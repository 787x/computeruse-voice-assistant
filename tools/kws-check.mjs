/**
 * 对任意 WAV 跑唤醒词检测。
 *
 * 用途：用真人录音验证唤醒词（合成语音与真人声学差异大，命中率不代表真实表现）。
 * 用法：node tools/kws-check.mjs assets/test/my-voice.wav
 */
import { Kws } from '../packages/voice/kws.mjs'
import { readWav } from '../packages/voice/wav.mjs'

const path = process.argv[2]
if (!path) {
  console.error('用法: node tools/kws-check.mjs <wav>')
  process.exit(2)
}

const { samples, sampleRate } = readWav(path)
console.log(`音频: ${path}  ${sampleRate} Hz  ${(samples.length / sampleRate).toFixed(2)} s`)

// 重采样到 16k（KWS 只接受 16k）
let x = samples
if (sampleRate !== 16000) {
  const n = Math.round((samples.length * 16000) / sampleRate)
  x = new Float32Array(n)
  for (let i = 0; i < n; i += 1) {
    const pos = (i * (sampleRate - 1)) / Math.max(1, n - 1)
    const i0 = Math.floor(pos)
    const i1 = Math.min(samples.length - 1, i0 + 1)
    const f = pos - i0
    x[i] = samples[i0] * (1 - f) + samples[i1] * f
  }
}

const kws = new Kws()
let hit = null
const chunk = 1600
for (let i = 0; i < x.length; i += chunk) {
  const h = kws.accept(x.subarray(i, Math.min(x.length, i + chunk)), 16000)
  if (h) { hit = h; break }
}
if (!hit) hit = kws.finish()

console.log(hit ? `命中唤醒词: ${hit}` : '未命中唤醒词')
process.exitCode = hit ? 0 : 1
