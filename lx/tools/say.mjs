/**
 * 用 TTS 合成一句话到 WAV（调试/造测试素材用）。
 *
 * 用法：node tools/say.mjs "小爱同学" [输出路径] [语速] [情绪]
 * 默认输出 assets/test/say.wav，末尾会补静音——流式 KWS/ASR 需要上下文。
 */
import { Tts } from '../packages/voice/tts.mjs'
import { writeWav } from '../packages/voice/wav.mjs'

const text = process.argv[2]
if (!text) {
  console.error('用法: node tools/say.mjs "文本" [输出路径] [语速] [情绪]')
  process.exit(2)
}
const out = process.argv[3] ?? 'assets/test/say.wav'
const speed = Number(process.argv[4] ?? 1)
const emotion = process.argv[5]

const tts = new Tts()
await tts.ready()
const result = await tts.speak(text, { speed, emotion })
await tts.dispose()

// 前后各 0.4s 静音：KWS/ASR 都是流式模型，没有静音边界容易漏检
const pad = new Float32Array(Math.round(0.4 * result.sampleRate))
const padded = new Float32Array(pad.length * 2 + result.samples.length)
padded.set(pad, 0)
padded.set(result.samples, pad.length)
padded.set(pad, pad.length + result.samples.length)

writeWav(out, padded, result.sampleRate)
console.log(JSON.stringify({
  text,
  out,
  seconds: Math.round((padded.length / result.sampleRate) * 10) / 10,
  firstChunkMs: result.stats.firstChunkMs,
  rtf: result.stats.rtf,
}, null, 1))
