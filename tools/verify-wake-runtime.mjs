/**
 * 唤醒链路验收：走**运行时同一条路径**（StreamingPipeline.push），
 * 而不是只测 Kws 类——开机自启那次出问题的正是"喂音频→判定唤醒"这段。
 *
 * 唤醒词为「小爱同学」：正向用例必须有新唤醒词的音频，
 * 旧录音（「你好狼宝」等）只能作为负向用例。
 *
 * 判定是 fail-closed 的（PR #2 评审修复）：
 *   - 每个必需用例都有唯一 id 和**预先声明**的 expectWake；异常/缺文件也必须记录，
 *     绝不允许用 filter+every 在空集合上把"没跑的测试"算成通过。
 *   - 必需用例缺失、异常、缺输入 → 整体失败。
 *   - `--selftest` 用构造的用例列表回归检查判定函数本身。
 *
 * 用法：node tools/verify-wake-runtime.mjs [--selftest]
 * 可选：把真人录音放到 assets/test/xiaoai-tongxue.wav，脚本会优先使用它。
 */
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'

import { StreamingPipeline } from '../packages/voice/stream.mjs'
import { readWav } from '../packages/voice/wav.mjs'
import { DEFAULT_WAKE_WORD, LEGACY_WAKE_WORDS } from '../packages/voice/keywords.mjs'

/*
 * 判定函数（纯函数，可被 --selftest 单独回归）。
 * 规则：
 *   1. 每个 required id 必须在结果里出现，且 pass === true，否则整体失败；
 *   2. 负向用例（expectWake === false）异常时 pass=false，被规则 1 抓住；
 *   3. 不做任何"过滤后再 every()"的空集合真值判断。
 */
export function wakeVerdict(cases, requiredIds) {
  const byId = new Map(cases.map((c) => [c.id, c]))
  const checks = {}
  for (const id of requiredIds) {
    const c = byId.get(id)
    checks[id] = c !== undefined && c.required === true && c.pass === true
  }
  const pass = requiredIds.every((id) => checks[id] === true)
  return { checks, pass }
}

// ---- 判定函数自身的回归检查（异常/缺失输入不得被判为通过） ----
function runVerdictSelfTest() {
  const req = ['A', 'B', 'B2', 'B3', 'C', 'D']
  const mk = (id, pass, expectWake) => ({ id, required: true, pass, expectWake })
  const allPass = () => req.map((id) => mk(id, true, id === 'A' || id === 'C' || id === 'D'))
  const scenarios = [
    { name: '全部通过', cases: allPass(), want: true },
    // 旧行为的漏洞：异常记录没有 expectWake:false 字段，filter 会把这条负向用例整个漏掉
    { name: '负向用例异常（记录缺 expectWake 字段，pass=false）',
      cases: allPass().map((c) => (c.id === 'B2' ? { id: 'B2', required: true, pass: false, error: 'boom' } : c)),
      want: false },
    { name: '负向用例整条缺失', cases: allPass().filter((c) => c.id !== 'B'), want: false },
    { name: '空结果集（every 空集合陷阱）', cases: [], want: false },
    { name: '正向缺录音（记录为 pass:false）', cases: allPass().map((c) => (c.id === 'A' ? { ...c, pass: false } : c)), want: false },
    { name: '必需用例被标记为非 required（不允许）', cases: allPass().map((c) => (c.id === 'B3' ? { ...c, required: false } : c)), want: false },
  ]
  let ok = true
  for (const s of scenarios) {
    const got = wakeVerdict(s.cases, req).pass
    const mark = got === s.want ? '✓' : '✗'
    if (got !== s.want) ok = false
    console.log(`${mark} 判定自测 · ${s.name}: want=${s.want} got=${got}`)
  }
  console.log(JSON.stringify({ selftest: true, verdict: ok ? 'pass' : 'fail' }))
  process.exitCode = ok ? 0 : 1
}

if (process.argv.includes('--selftest')) {
  runVerdictSelfTest()
  process.exit(process.exitCode ?? 0)
}

// 独立数据目录：不吃本机用户的自定义关键词，也不污染它。
process.env.CUA_VA_DATA_DIR = mkdtempSync(join(tmpdir(), 'cua-va-wake-runtime-'))

/*
 * 重采样必须用 sherpa 自带的 LinearResampler。
 * 手写的线性插值在这段 44.1k 语音上会把关键词糊掉——实测同一个 wav，
 * 手写重采样喂进去不命中，官方重采样就命中。
 */
const { LinearResampler } = createRequire(import.meta.url)('sherpa-onnx-node/resampler.js')

function resample(x, srIn, srOut) {
  if (srIn === srOut) return x
  const resampler = new LinearResampler(srIn, srOut)
  const parts = []
  for (let i = 0; i < x.length; i += 2205) {
    const piece = x.subarray(i, Math.min(i + 2205, x.length))
    const isLast = i + 2205 >= x.length
    parts.push(isLast ? resampler.flush(piece) : resampler.resample(piece))
  }
  const merged = new Float32Array(parts.reduce((sum, part) => sum + part.length, 0))
  let cursor = 0
  for (const part of parts) { merged.set(part, cursor); cursor += part.length }
  return merged
}

function padSilence(x, seconds = 2, sampleRate = 16000) {
  const pad = new Float32Array(Math.round(seconds * sampleRate))
  const out = new Float32Array(pad.length + x.length + pad.length)
  out.set(x, pad.length)
  return out
}

function feed(pipeline, samples, chunk = 1600) {
  let firstWake = null
  let hitAtMs = null
  for (let i = 0; i < samples.length; i += chunk) {
    const slice = samples.subarray(i, Math.min(samples.length, i + chunk))
    const result = pipeline.push(slice)
    if (result.wake !== null && firstWake === null) {
      firstWake = result.wake
      hitAtMs = Math.round((i / 16000) * 1000)
    }
  }
  return { firstWake, hitAtMs }
}

/**
 * 用例表：id 唯一、expectWake 预先声明（负向=false）。
 * 每条都进判定；异常/缺文件记录为 pass=false，绝不缺席。
 */
const REQUIRED_IDS = ['A', 'B', 'B2', 'B3', 'C', 'D']

const realRecording = 'assets/test/xiaoai-tongxue.wav'
const syntheticRecording = 'assets/test/xiaoai-tongxue-synthetic.wav'
const positive = existsSync(realRecording) ? realRecording : existsSync(syntheticRecording) ? syntheticRecording : null
const positiveIsSynthetic = positive === syntheticRecording

function loadPositive16k() {
  const audio = readWav(positive)
  return resample(audio.samples, audio.sampleRate, 16000)
}

const cases = []
function runCase(id, name, expectWake, fn) {
  try {
    const extra = fn()
    cases.push({ id, name, required: true, expectWake, ...extra })
  } catch (error) {
    cases.push({ id, name, required: true, expectWake, pass: false, error: error.message })
  }
}

// A) 新唤醒词「小爱同学」应当命中（必须是新录音，不接受旧词的期望改写）
if (positive === null) {
  cases.push({
    id: 'A',
    name: `A 唤醒词「${DEFAULT_WAKE_WORD}」`,
    required: true,
    expectWake: true,
    incomplete: true,
    detail: '缺新唤醒词录音（真人 xiaoai-tongxue.wav / 合成 xiaoai-tongxue-synthetic.wav）',
    pass: false,
  })
} else {
  runCase('A', `A 唤醒词「${DEFAULT_WAKE_WORD}」${positiveIsSynthetic ? '（合成）' : '（真人）'}`, true, () => {
    const pipeline = new StreamingPipeline()
    const hit = feed(pipeline, padSilence(loadPositive16k()))
    return { ...hit, pass: hit.firstWake === DEFAULT_WAKE_WORD }
  })
}

// B) 普通说话不应唤醒
runCase('B', 'B 普通语句', false, () => {
  const audio = readWav('assets/test/not-wake-word.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  return { ...hit, pass: hit.firstWake === null }
})

// B2) 旧唤醒词「你好狼宝」在默认配置下不应唤醒
runCase('B2', `B2 旧唤醒词「${LEGACY_WAKE_WORDS.join('/')}」`, false, () => {
  const audio = readWav('assets/test/nihao-langbao.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  return { ...hit, pass: hit.firstWake === null }
})

// B3) 打断词「等一下」不应误触发唤醒
runCase('B3', 'B3 打断词「等一下」', false, () => {
  const audio = readWav('assets/test/dengyixia.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  return { ...hit, pass: hit.firstWake === null }
})

// C) 连续两次唤醒：证明 pipeline 复用后还能再次命中（开机自启场景会持续跑很久）
if (positive === null) {
  cases.push({ id: 'C', name: 'C 同一管线二次唤醒', required: true, expectWake: true, incomplete: true, detail: '缺新唤醒词录音', pass: false })
} else {
  runCase('C', 'C 同一管线二次唤醒', true, () => {
    const samples = loadPositive16k()
    const pipeline = new StreamingPipeline()
    const first = feed(pipeline, samples)
    const silence = new Float32Array(16000 * 1) // 中间隔 1 秒静音
    for (let i = 0; i < silence.length; i += 1600) pipeline.push(silence.subarray(i, i + 1600))
    const second = feed(pipeline, samples)
    return { first: first.firstWake, second: second.firstWake, pass: first.firstWake === DEFAULT_WAKE_WORD && second.firstWake === DEFAULT_WAKE_WORD }
  })
}

// D) 长时间静音后的首次唤醒（>3 秒会触发 KWS 流定期重置）
if (positive === null) {
  cases.push({ id: 'D', name: 'D 静音 10 秒后首次唤醒', required: true, expectWake: true, incomplete: true, detail: '缺新唤醒词录音', pass: false })
} else {
  runCase('D', 'D 静音 10 秒后首次唤醒', true, () => {
    const pipeline = new StreamingPipeline()
    const longSilence = new Float32Array(16000 * 10) // 10 秒静音
    for (let i = 0; i < longSilence.length; i += 1600) pipeline.push(longSilence.subarray(i, i + 1600))
    const hit = feed(pipeline, padSilence(loadPositive16k(), 0.5))
    return { ...hit, pass: hit.firstWake === DEFAULT_WAKE_WORD }
  })
}

const verdict = wakeVerdict(cases, REQUIRED_IDS)

console.log('唤醒链路验收（运行时同路径，fail-closed）\n' + '='.repeat(62))
for (const item of cases) {
  console.log(`${item.pass ? '✓' : '✗'} ${item.name}  ${item.error ?? item.detail ?? ''}`
    + `${item.firstWake === undefined ? '' : ` wake=${item.firstWake} @${item.hitAtMs}ms`}`
    + `${item.second === undefined ? '' : ` 二次=${item.second}`}`)
}
console.log('='.repeat(62))
if (positiveIsSynthetic) console.log('⚠ 正向用例使用 TTS 合成样本，真人「小爱同学」录音验证尚未完成。')
console.log(JSON.stringify({ cases: cases.length, required: REQUIRED_IDS, verdict }))
rmSync(process.env.CUA_VA_DATA_DIR, { recursive: true, force: true })
process.exitCode = verdict.pass ? 0 : 1
