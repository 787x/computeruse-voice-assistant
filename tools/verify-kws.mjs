/**
 * P2-c 验收：唤醒词（KWS）。
 *
 * 默认唤醒词为「小爱同学」（音素：x iǎo ài t óng x ué @小爱同学）。
 *
 * 用例：
 *   A 官方测试音频 + 官方关键词 —— 先证明 KWS 管线本身能工作
 *   B 默认关键词文件只含「小爱同学」，且「你好狼宝」录音不再触发 —— 证明旧词已失效
 *   C 用 TTS 合成"小爱同学"，跑默认关键词 —— 证明新唤醒词生效（合成语音仅作旁证）
 *   D 合成一句不含唤醒词的话 —— 证明不会误触发
 *
 * 真人正向验证仍是硬缺口：把录音放到 assets/test/xiaoai-tongxue.wav，
 * 然后跑 node tools/verify-keywords.mjs（它会优先使用真人录音）。
 */
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { Kws } from '../packages/voice/kws.mjs'
import { Tts } from '../packages/voice/tts.mjs'
import { readWav, writeWav } from '../packages/voice/wav.mjs'
import { DEFAULT_WAKE_WORD, LEGACY_WAKE_WORDS, readKeywordLabels } from '../packages/voice/keywords.mjs'

// 独立数据目录：验收默认解析路径，不吃也不写本机自定义关键词。
process.env.CUA_VA_DATA_DIR = mkdtempSync(join(tmpdir(), 'cua-va-kws-'))

const MODEL_DIR = 'models/sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01'

function resample(x, srIn, srOut) {
  if (srIn === srOut) return x
  const n = Math.round((x.length * srOut) / srIn)
  const out = new Float32Array(n)
  for (let i = 0; i < n; i += 1) {
    const pos = (i * (srIn - 1)) / Math.max(1, n - 1)
    const i0 = Math.floor(pos)
    const i1 = Math.min(x.length - 1, i0 + 1)
    const frac = pos - i0
    out[i] = x[i0] * (1 - frac) + x[i1] * frac
  }
  return out
}

/** 分块喂入，模拟实时流 */
function feed(spotter, samples, chunk = 1600) {
  for (let i = 0; i < samples.length; i += chunk) {
    const hit = spotter.accept(samples.subarray(i, Math.min(samples.length, i + chunk)), 16000)
    if (hit) return hit
  }
  // 流式模型需要显式收尾，否则最后一段的命中不会吐出（实测）
  return spotter.finish()
}

const report = { cases: [], verdict: {} }

// ---- A：官方样例 ----
try {
  const spotter = new Kws({ keywordsFile: `${MODEL_DIR}/test_wavs/test_keywords.txt` })
  const audio = readWav(`${MODEL_DIR}/test_wavs/3.wav`)
  const hit = feed(spotter, resample(audio.samples, audio.sampleRate, 16000))
  report.cases.push({ name: 'A 官方样例 3.wav', hit, expectHit: true, pass: hit !== null })
} catch (error) {
  report.cases.push({ name: 'A 官方样例 3.wav', error: error.message, pass: false })
}

// ---- 用 TTS 造"唤醒词"音频（合成完就关掉 worker，否则它会常驻占显存） ----
const tts = new Tts()
await tts.ready()
function padSilence(x, seconds = 0.4, sampleRate = 16000) {
  const pad = new Float32Array(Math.round(seconds * sampleRate))
  const out = new Float32Array(pad.length * 2 + x.length)
  out.set(pad, 0)
  out.set(x, pad.length)
  out.set(pad, pad.length + x.length)
  return out
}

const wake = await tts.speak(DEFAULT_WAKE_WORD)
// KWS 是流式模型：前后留静音让它有上下文，实测不加会漏检
const wake16k = padSilence(resample(wake.samples, wake.sampleRate, 16000))
writeWav('assets/test/wake-word.wav', wake16k, 16000)

const other = await tts.speak('今天天气不错，适合出门走走。')
const other16k = padSilence(resample(other.samples, other.sampleRate, 16000))
writeWav('assets/test/not-wake-word.wav', other16k, 16000)

// ---- B：默认关键词文件应只含「小爱同学」；旧唤醒词录音必须不再触发 ----
const defaultSpotter = new Kws()
const defaultLabels = readKeywordLabels(defaultSpotter.keywordInfo.file)
const legacyNotTriggered = []
for (const file of ['assets/test/nihao-langbao.wav', 'assets/test/wake-1.wav']) {
  if (!existsSync(file)) continue
  const audio = readWav(file)
  defaultSpotter.reset()
  const hit = feed(defaultSpotter, resample(audio.samples, audio.sampleRate, 16000))
  legacyNotTriggered.push({ file, hit, ok: hit === null })
}
report.cases.push({
  name: `B 默认关键词仅「${DEFAULT_WAKE_WORD}」且旧词不触发`,
  labels: defaultLabels,
  legacy: legacyNotTriggered,
  expectHit: true,
  pass: defaultLabels.length === 1 && defaultLabels[0] === DEFAULT_WAKE_WORD
    && legacyNotTriggered.length > 0 && legacyNotTriggered.every((t) => t.ok),
})

// ---- C：合成"小爱同学"应命中（多生成几次，合成语音本身有随机性） ----
const trials = []
for (let i = 0; i < 3; i += 1) {
  try {
    const a = await tts.speak(DEFAULT_WAKE_WORD, { speed: i === 1 ? 0.9 : 1.0 })
    const x = padSilence(resample(a.samples, a.sampleRate, 16000))
    const spotter = new Kws()
    trials.push({ hit: feed(spotter, x) })
  } catch (error) {
    trials.push({ error: error.message })
  }
}
const cHits = trials.filter((t) => t.hit === DEFAULT_WAKE_WORD).length
report.cases.push({
  name: `C 合成「${DEFAULT_WAKE_WORD}」×3 次`,
  hits: cHits,
  of: trials.length,
  details: trials,
  expectHit: true,
  /*
   * 合成语音**只作信息项，不作为判据**：本仓库 §20.1 的实测结论是
   * "合成语音验证不可信"（旧词同样只有 1/3），拿它当硬门槛会让验收
   * 随机红掉。正向证据以仓库内 xiaoai-tongxue-synthetic.wav 的固定命中
   * （verify-keywords / verify-wake-runtime / verify-stream 三个脚本）
   * 与真人录音（用例 E）为准。
   */
  informational: true,
  pass: true,
})

// ---- D：非唤醒词不应触发 ----
try {
  const spotter = new Kws()
  const hit = feed(spotter, other16k)
  report.cases.push({ name: 'D 非唤醒词不应触发', hit, expectHit: false, pass: hit === null })
} catch (error) {
  report.cases.push({ name: 'D 非唤醒词不应触发', error: error.message, pass: false })
}

// ---- E（可选）：真人录音的正向命中 ----
const realRecording = 'assets/test/xiaoai-tongxue.wav'
if (existsSync(realRecording)) {
  const spotter = new Kws()
  const audio = readWav(realRecording)
  const hit = feed(spotter, resample(audio.samples, audio.sampleRate, 16000))
  report.cases.push({ name: 'E 真人「小爱同学」录音', hit, expectHit: true, pass: hit === DEFAULT_WAKE_WORD })
} else {
  report.cases.push({
    name: 'E 真人「小爱同学」录音',
    pass: false,
    incomplete: true,
    note: `缺真人录音；旧词「${LEGACY_WAKE_WORDS.join('/')}」与合成旁证不能替代它（用 tools/record-mic.py 录制）`,
  })
}

report.verdict = {
  officialWorks: report.cases.find((c) => c.name.startsWith('A'))?.pass === true,
  defaultIsNewOnly: report.cases.find((c) => c.name.startsWith('B'))?.pass === true,
  noFalseTrigger: report.cases.find((c) => c.name.startsWith('D'))?.pass === true,
  realWakeVerified: report.cases.find((c) => c.name.startsWith('E'))?.pass === true,
}
report.verdict.syntheticTrialsHits = report.cases.find((c) => c.name.startsWith('C'))?.hits ?? 0
report.verdict.pass = Object.values(report.verdict).every((v) => v === true)
report.verdict.realRecordingMissing = !existsSync(realRecording)

await tts.dispose()
rmSync(process.env.CUA_VA_DATA_DIR, { recursive: true, force: true })

console.log(JSON.stringify(report, null, 1))
if (report.verdict.realRecordingMissing) {
  console.error('⚠ 真人「小爱同学」录音缺失：KWS 验收未完成（A-D 通过也不能替代真人正向验证）。')
}
process.exitCode = report.verdict.pass ? 0 : 1
