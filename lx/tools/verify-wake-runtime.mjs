/**
 * 唤醒链路验收：走**运行时同一条路径**（StreamingPipeline.push），
 * 而不是只测 Kws 类——开机自启那次出问题的正是"喂音频→判定唤醒"这段。
 *
 * 用法：node tools/verify-wake-runtime.mjs
 */
import { StreamingPipeline } from '../packages/voice/stream.mjs'
import { readWav } from '../packages/voice/wav.mjs'
import { createRequire } from 'node:module'

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

const cases = []

// A) 唤醒词应当命中
try {
  const audio = readWav('assets/test/nihao-langbao.wav')
  const samples = resample(audio.samples, audio.sampleRate, 16000)
  const pipeline = new StreamingPipeline()
  const hit = feed(pipeline, padSilence(samples))
  cases.push({
    name: 'A 唤醒词「你好狼宝」',
    expectWake: true,
    ...hit,
    pass: hit.firstWake !== null,
  })
} catch (error) {
  cases.push({ name: 'A 唤醒词「你好狼宝」', error: error.message, pass: false })
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

// C) 连续两次唤醒：证明 pipeline 复用后还能再次命中（开机自启场景会持续跑很久）
try {
  const audio = readWav('assets/test/nihao-langbao.wav')
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
    pass: first.firstWake !== null && second.firstWake !== null,
  })
} catch (error) {
  cases.push({ name: 'C 同一管线二次唤醒', error: error.message, pass: false })
}

const report = {
  cases,
  verdict: {
    wakeDetected: cases.find((c) => c.name.startsWith('A'))?.pass === true,
    noFalseWake: cases.find((c) => c.name.startsWith('B'))?.pass === true,
    repeatable: cases.find((c) => c.name.startsWith('C'))?.pass === true,
  },
}
report.verdict.pass = Object.values(report.verdict).every((value) => value === true)

console.log('唤醒链路验收（运行时同路径）\n' + '='.repeat(62))
for (const item of cases) {
  console.log(`${item.pass ? '✓' : '✗'} ${item.name}  ${item.error ?? ''}`
    + `${item.firstWake === undefined ? '' : ` wake=${item.firstWake} @${item.hitAtMs}ms`}`
    + `${item.second === undefined ? '' : ` 二次=${item.second}`}`)
}
console.log('='.repeat(62))
console.log(JSON.stringify({ cases: cases.length, verdict: report.verdict }))
process.exitCode = report.verdict.pass ? 0 : 1
