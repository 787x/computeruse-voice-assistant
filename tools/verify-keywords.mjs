/**
 * 唤醒词/KWS 关键词验收：两个关键词都要能被识别，其他话不能误触发。
 *
 * 为什么要单测这一层：唤醒词是"外部世界进入系统"的唯一入口，
 * 它的两个错误方向代价不对称——漏检只是让人多说一遍，
 * 误检会让助手在别人聊天时自己动起来。所以"别的语音不触发"必须逐条验。
 *
 * 用法：node tools/verify-keywords.mjs
 */
import { createRequire } from 'node:module'
import { existsSync } from 'node:fs'

import { Kws } from '../packages/voice/kws.mjs'
import { readWav } from '../packages/voice/wav.mjs'

const require = createRequire(import.meta.url)
const { LinearResampler } = require('sherpa-onnx-node/resampler.js')

const kws = new Kws()

function load16k(path) {
  const { samples, sampleRate } = readWav(path)
  if (sampleRate === 16000) return samples
  const resampler = new LinearResampler(sampleRate, 16000)
  const parts = []
  for (let i = 0; i < samples.length; i += 2205) {
    const piece = samples.subarray(i, Math.min(i + 2205, samples.length))
    const isLast = i + 2205 >= samples.length
    parts.push(isLast ? resampler.flush(piece) : resampler.resample(piece))
  }
  const merged = new Float32Array(parts.reduce((sum, p) => sum + p.length, 0))
  let cursor = 0
  for (const part of parts) { merged.set(part, cursor); cursor += part.length }
  return merged
}

/** 全程喂一遍，返回命中的关键词集合与首次命中的位置（毫秒） */
function detect(path) {
  const pcm = load16k(path)
  kws.reset()
  const hits = []
  let firstAtMs = null
  for (let i = 0; i < pcm.length; i += 1600) {
    const piece = pcm.subarray(i, Math.min(i + 1600, pcm.length))
    const hit = kws.accept(piece, 16000)
    if (hit !== null) {
      if (!hits.includes(hit)) hits.push(hit)
      if (firstAtMs === null) firstAtMs = Math.round(((i + piece.length) / 16000) * 1000)
    }
  }
  const tail = kws.finish()
  if (tail !== null && !hits.includes(tail)) hits.push(tail)
  return { hits, firstAtMs, audioMs: Math.round((pcm.length / 16000) * 1000) }
}

const cases = [
  { file: 'assets/test/nihao-langbao.wav', expect: '你好狼宝', note: '唤醒词' },
  { file: 'assets/test/dengyixia.wav', expect: null, note: '打断词（不是KWS）' },
  { file: 'assets/test/other-speech.wav', expect: null, note: '无关语音' },
  { file: 'assets/test/asr-zh.wav', expect: null, note: '无关语音（指令句）' },
]

const rows = []
for (const item of cases) {
  if (!existsSync(item.file)) {
    rows.push({ ...item, pass: false, detail: '缺文件' })
    continue
  }
  const result = detect(item.file)
  const pass = item.expect === null
    ? result.hits.length === 0
    : result.hits.includes(item.expect)
  rows.push({ ...item, pass, detail: result.hits.length === 0 ? '未命中' : `${result.hits.join(' + ')} @${result.firstAtMs}ms` })
}

console.log('KWS 关键词验收\n' + '='.repeat(62))
for (const row of rows) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.note.padEnd(14, ' ')} 期望 ${String(row.expect ?? '不触发').padEnd(6)} → ${row.detail}`)
}
const failed = rows.filter((r) => !r.pass)
console.log('='.repeat(62))
console.log(JSON.stringify({ cases: rows.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
