/**
 * 唤醒链路验收：走**运行时同一条路径**（StreamingPipeline.push），
 * 而不是只测 Kws 类——开机自启那次出问题的正是"喂音频→判定唤醒"这段。
 *
 * 唤醒词为「小爱同学」：正向用例必须有新唤醒词的音频，
 * 旧录音（「你好狼宝」等）只能作为负向用例。
 *
 * 用法：node tools/verify-wake-runtime.mjs
 * 可选：把真人录音放到 assets/test/xiaoai-tongxue.wav，脚本会优先使用它。
 */
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'

import { StreamingPipeline } from '../packages/voice/stream.mjs'
import { readWav } from '../packages/voice/wav.mjs'
import { DEFAULT_WAKE_WORD, LEGACY_WAKE_WORDS } from '../packages/voice/keywords.mjs'

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

const realRecording = 'assets/test/xiaoai-tongxue.wav'
const syntheticRecording = 'assets/test/xiaoai-tongxue-synthetic.wav'
const positive = existsSync(realRecording) ? realRecording : existsSync(syntheticRecording) ? syntheticRecording : null
const positiveIsSynthetic = positive === syntheticRecording

const cases = []

// A) 新唤醒词「小爱同学」应当命中（必须是新录音，不接受旧词的期望改写）
if (positive === null) {
  cases.push({
    name: `A 唤醒词「${DEFAULT_WAKE_WORD}」`,
    expectWake: true,
    incomplete: true,
    detail: '缺新唤醒词录音（真人 xiaoai-tongxue.wav / 合成 xiaoai-tongxue-synthetic.wav）',
    pass: false,
  })
} else {
  try {
    const audio = readWav(positive)
    const samples = resample(audio.samples, audio.sampleRate, 16000)
    const pipeline = new StreamingPipeline()
    const hit = feed(pipeline, padSilence(samples))
    cases.push({
      name: `A 唤醒词「${DEFAULT_WAKE_WORD}」${positiveIsSynthetic ? '（合成）' : '（真人）'}`,
      expectWake: true,
      ...hit,
      pass: hit.firstWake === DEFAULT_WAKE_WORD,
    })
  } catch (error) {
    cases.push({ name: `A 唤醒词「${DEFAULT_WAKE_WORD}」`, error: error.message, pass: false })
  }
}

// B) 普通说话不应唤醒
try {
  const audio = readWav('assets/test/not-wake-word.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  cases.push({
    name: 'B 普通语句',
    expectWake: false,
    ...hit,
    pass: hit.firstWake === null,
  })
} catch (error) {
  cases.push({ name: 'B 普通语句', error: error.message, pass: false })
}

// B2) 旧唤醒词「你好狼宝」在默认配置下不应唤醒
try {
  const audio = readWav('assets/test/nihao-langbao.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  cases.push({
    name: `B2 旧唤醒词「${LEGACY_WAKE_WORDS.join('/')}」`,
    expectWake: false,
    ...hit,
    pass: hit.firstWake === null,
  })
} catch (error) {
  cases.push({ name: 'B2 旧唤醒词', error: error.message, pass: false })
}

// B3) 打断词「等一下」不应误触发唤醒
try {
  const audio = readWav('assets/test/dengyixia.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  cases.push({
    name: 'B3 打断词「等一下」',
    expectWake: false,
    ...hit,
    pass: hit.firstWake === null,
  })
} catch (error) {
  cases.push({ name: 'B3 打断词「等一下」', error: error.message, pass: false })
}

// C) 连续两次唤醒：证明 pipeline 复用后还能再次命中（开机自启场景会持续跑很久）
if (positive === null) {
  cases.push({ name: 'C 同一管线二次唤醒', incomplete: true, detail: '缺新唤醒词录音', pass: false })
} else {
  try {
    const audio = readWav(positive)
    const samples = resample(audio.samples, audio.sampleRate, 16000)
    const pipeline = new StreamingPipeline()
    const first = feed(pipeline, samples)
    const silence = new Float32Array(16000 * 1) // 中间隔 1 秒静音
    for (let i = 0; i < silence.length; i += 1600) pipeline.push(silence.subarray(i, i + 1600))
    const second = feed(pipeline, samples)
    cases.push({
      name: 'C 同一管线二次唤醒',
      first: first.firstWake,
      second: second.firstWake,
      pass: first.firstWake === DEFAULT_WAKE_WORD && second.firstWake === DEFAULT_WAKE_WORD,
    })
  } catch (error) {
    cases.push({ name: 'C 同一管线二次唤醒', error: error.message, pass: false })
  }
}

// D) 长时间静音后的首次唤醒（>3 秒会触发 KWS 流定期重置）
if (positive === null) {
  cases.push({ name: 'D 长静音后首次唤醒', incomplete: true, detail: '缺新唤醒词录音', pass: false })
} else {
  try {
    const audio = readWav(positive)
    const samples = resample(audio.samples, audio.sampleRate, 16000)
    const pipeline = new StreamingPipeline()
    const longSilence = new Float32Array(16000 * 10) // 10 秒静音
    for (let i = 0; i < longSilence.length; i += 1600) pipeline.push(longSilence.subarray(i, i + 1600))
    const hit = feed(pipeline, padSilence(samples, 0.5))
    cases.push({
      name: 'D 静音 10 秒后首次唤醒',
      expectWake: true,
      ...hit,
      pass: hit.firstWake === DEFAULT_WAKE_WORD,
    })
  } catch (error) {
    cases.push({ name: 'D 静音 10 秒后首次唤醒', error: error.message, pass: false })
  }
}

const report = {
  positiveRecording: positive,
  positiveSynthetic: positiveIsSynthetic,
  cases,
  verdict: {
    wakeDetected: cases.find((c) => c.name.startsWith('A'))?.pass === true,
    noFalseWake: cases.filter((c) => c.expectWake === false).every((c) => c.pass === true),
    legacyNoWake: cases.find((c) => c.name.startsWith('B2'))?.pass === true,
    repeatable: cases.find((c) => c.name.startsWith('C'))?.pass === true,
    afterLongSilence: cases.find((c) => c.name.startsWith('D'))?.pass === true,
  },
}
report.verdict.pass = Object.values(report.verdict).every((value) => value === true)
report.verdict.incomplete = positive === null || positiveIsSynthetic

console.log('唤醒链路验收（运行时同路径）\n' + '='.repeat(62))
for (const item of cases) {
  console.log(`${item.pass ? '✓' : '✗'} ${item.name}  ${item.error ?? item.detail ?? ''}`
    + `${item.firstWake === undefined ? '' : ` wake=${item.firstWake} @${item.hitAtMs}ms`}`
    + `${item.second === undefined ? '' : ` 二次=${item.second}`}`)
}
console.log('='.repeat(62))
if (positiveIsSynthetic) console.log('⚠ 正向用例使用 TTS 合成样本，真人「小爱同学」录音验证尚未完成。')
console.log(JSON.stringify({ cases: cases.length, verdict: report.verdict }))
rmSync(process.env.CUA_VA_DATA_DIR, { recursive: true, force: true })
process.exitCode = report.verdict.pass ? 0 : 1
