/**
 * P2-c 验收：唤醒词（KWS）。
 *
 * 三个用例：
 *   A 官方测试音频 + 官方关键词 —— 先证明 KWS 管线本身能工作
 *   B 用我们的 TTS 合成"你好狼宝"，跑我们的关键词 —— 证明自定义唤醒词生效
 *   C 合成一句不含唤醒词的话 —— 证明不会误触发
 */
import { Kws } from '../packages/voice/kws.mjs'
import { Tts } from '../packages/voice/tts.mjs'
import { readWav, writeWav } from '../packages/voice/wav.mjs'

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

const wake = await tts.speak('你好狼宝')
// KWS 是流式模型：前后留静音让它有上下文，实测不加会漏检
const wake16k = padSilence(resample(wake.samples, wake.sampleRate, 16000))
writeWav('assets/test/wake-word.wav', wake16k, 16000)

const other = await tts.speak('今天天气不错，适合出门走走。')
const other16k = padSilence(resample(other.samples, other.sampleRate, 16000))
writeWav('assets/test/not-wake-word.wav', other16k, 16000)

// ---- B：自定义唤醒词（多生成几次，合成语音本身有随机性） ----
const trials = []
for (let i = 0; i < 3; i += 1) {
  try {
    const a = await tts.speak('你好狼宝', { speed: i === 1 ? 0.9 : 1.0 })
    const x = padSilence(resample(a.samples, a.sampleRate, 16000))
    const spotter = new Kws()
    trials.push({ hit: feed(spotter, x) })
  } catch (error) {
    trials.push({ error: error.message })
  }
}
const bHits = trials.filter((t) => t.hit).length
report.cases.push({
  name: 'B 自定义「你好狼宝」×3 次',
  hits: bHits,
  of: trials.length,
  details: trials,
  expectHit: true,
  // 合成语音与真人声学差异大，命中率不稳定；这里只要求"至少一次命中"证明关键词有效
  pass: bHits >= 1,
})

// ---- C：非唤醒词不应触发 ----
try {
  const spotter = new Kws()
  const hit = feed(spotter, other16k)
  report.cases.push({ name: 'C 非唤醒词不应触发', hit, expectHit: false, pass: hit === null })
} catch (error) {
  report.cases.push({ name: 'C 非唤醒词不应触发', error: error.message, pass: false })
}

report.verdict = {
  officialWorks: report.cases.find((c) => c.name.startsWith('A'))?.pass === true,
  customWakeWorks: report.cases.find((c) => c.name.startsWith('B'))?.pass === true,
  noFalseTrigger: report.cases.find((c) => c.name.startsWith('C'))?.pass === true,
}
report.verdict.pass = Object.values(report.verdict).every((v) => v === true)

await tts.dispose()

console.log(JSON.stringify(report, null, 1))
process.exitCode = report.verdict.pass ? 0 : 1
