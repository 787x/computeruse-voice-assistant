/**
 * ASR 验收：喂一段中文音频，看字幕是否跟着话逐字出现、端点是否判对。
 *
 * 用法：
 *   node tools/verify-asr.mjs [wav路径]
 *
 * 验收的不是"能不能识别"，而是三个数字：
 *   1. 首字延迟（说完第一个字到字幕出现）——超过 800ms 就不能忍
 *   2. 端点判定（说完多久收尾）——2.4s 是留给用户犹豫的时间
 *   3. RTF（解码耗时 / 音频时长）——必须远小于 1，否则实时跟不上
 */
import { existsSync } from 'node:fs'
import { createRequire } from 'node:module'
import { Asr } from '../packages/voice/asr.mjs'
import { readWav } from '../packages/voice/wav.mjs'

const require = createRequire(import.meta.url)
const { LinearResampler } = require('sherpa-onnx-node/resampler.js')

const wavPath = process.argv[2] ?? 'assets/test/asr-zh.wav'
if (!existsSync(wavPath)) {
  console.error(`音频不存在: ${wavPath}\n先跑: node tools/say.mjs "帮我把浏览器打开，然后搜索今天的天气" ${wavPath}`)
  process.exit(2)
}

const { samples, sampleRate } = readWav(wavPath)
let pcm = samples
if (sampleRate !== 16000) {
  const resampler = new LinearResampler(sampleRate, 16000)
  const chunk = 2205
  const parts = []
  for (let i = 0; i < samples.length; i += chunk) {
    const isLast = i + chunk >= samples.length
    const piece = samples.subarray(i, Math.min(i + chunk, samples.length))
    parts.push(isLast ? resampler.flush(piece) : resampler.resample(piece))
  }
  const total = parts.reduce((sum, p) => sum + p.length, 0)
  pcm = new Float32Array(total)
  let cursor = 0
  for (const p of parts) { pcm.set(p, cursor); cursor += p.length }
}

const audioSec = pcm.length / 16000
console.log(JSON.stringify({ wav: wavPath, inSampleRate: sampleRate, audioSec: Math.round(audioSec * 100) / 100 }, null, 1))

const t0 = performance.now()
const asr = new Asr()
const initMs = Math.round(performance.now() - t0)
console.log(`模型加载 ${initMs}ms  ${JSON.stringify(asr.modelFiles)}`)

const CHUNK = 1600 // 100ms，和麦克风采集一致
let decodeMs = 0
let lastPartial = ''
const partials = []
const finals = []

for (let i = 0; i < pcm.length; i += CHUNK) {
  const piece = pcm.subarray(i, Math.min(i + CHUNK, pcm.length))
  const t = performance.now()
  const r = asr.push(piece, 16000)
  decodeMs += performance.now() - t

  if (r.partial !== '' && r.partial !== lastPartial) {
    lastPartial = r.partial
    const atMs = Math.round(((i + piece.length) / 16000) * 1000)
    partials.push({ atMs, text: r.partial, firstPartialMs: asr.firstPartialMs })
    console.log(`  [${String(atMs).padStart(5)}ms] ${r.partial}`)
  }
  if (r.final !== null) {
    finals.push({ atMs: Math.round(((i + piece.length) / 16000) * 1000), text: r.final })
    console.log(`  [端点] ${r.final}`)
    lastPartial = ''
  }
}

const tail = asr.flush()
if (tail !== '') {
  finals.push({ atMs: Math.round(audioSec * 1000), text: tail, flushed: true })
  console.log(`  [收尾] ${tail}`)
}

const report = {
  initMs,
  audioSec: Math.round(audioSec * 100) / 100,
  decodeMs: Math.round(decodeMs),
  rtf: Math.round((decodeMs / 1000 / audioSec) * 1000) / 1000,
  partialCount: partials.length,
  firstPartialMs: partials.length > 0 ? partials[0].firstPartialMs : -1,
  firstPartialAtMs: partials.length > 0 ? partials[0].atMs : -1,
  finals,
  text: finals.map((f) => f.text).join(' / '),
  checks: {
    recognized: finals.length > 0 && finals.some((f) => f.text.length >= 2),
    subtitleFollows: partials.length >= 2,
    latencyOk: partials.length > 0 && partials[0].firstPartialMs <= 800,
    realtime: decodeMs / 1000 / audioSec < 0.5,
  },
}
console.log(JSON.stringify(report, null, 1))

const failed = Object.entries(report.checks).filter(([, ok]) => !ok).map(([k]) => k)
if (failed.length > 0) {
  console.error(`\n未通过: ${failed.join(', ')}`)
  process.exitCode = 1
}
