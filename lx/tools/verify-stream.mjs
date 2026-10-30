/**
 * P2-d 验收：流式管线（AGC + VAD + KWS 一起）离线跑通。
 *
 * 为什么单独测：实机"扬声器放唤醒词 → 麦克风收"受音量、AEC、房间影响，
 * 不适合作为管线正确性的判据。这里用固定音频验证管线行为是确定的。
 *
 * 唤醒词「小爱同学」：正向用例使用新唤醒词录音（真人优先，缺失时用合成样本），
 * 旧唤醒词录音（wake-1.wav 等）只作为负向用例。
 */
import { existsSync } from 'node:fs'

import { StreamingPipeline } from '../packages/voice/stream.mjs'
import { readWav } from '../packages/voice/wav.mjs'
import { DEFAULT_WAKE_WORD } from '../packages/voice/keywords.mjs'

function run(path, attenuateDb = 0) {
  const { samples, sampleRate } = readWav(path)
  let x = samples
  if (attenuateDb !== 0) {
    const factor = 10 ** (attenuateDb / 20)
    x = Float32Array.from(x, (v) => v * factor)
  }
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

  const pipeline = new StreamingPipeline()
  const wakes = []
  let speechChunks = 0
  let peakLevel = -120
  const gains = []

  // 按 100ms 一块喂（与真实采集节奏一致）
  for (let i = 0; i < x.length; i += 1600) {
    const result = pipeline.push(x.subarray(i, Math.min(x.length, i + 1600)))
    if (result.wake) wakes.push(result.wake)
    if (result.speech) speechChunks += 1
    if (result.levelDb > peakLevel) peakLevel = result.levelDb
    gains.push(result.gainDb)
  }
  // 收尾：流式模型要排空
  const finalHit = pipeline.snapshot()

  return {
    file: path,
    seconds: Math.round((x.length / 16000) * 10) / 10,
    peakLevelDb: peakLevel,
    speechChunks,
    totalChunks: Math.ceil(x.length / 1600),
    wakes: [...new Set(wakes)],
    gainMedianDb: gains.sort((a, b) => a - b)[Math.floor(gains.length / 2)],
    finalHit: finalHit.speaking,
  }
}

const realRecording = 'assets/test/xiaoai-tongxue.wav'
const syntheticRecording = 'assets/test/xiaoai-tongxue-synthetic.wav'
const wakeFile = existsSync(realRecording) ? realRecording : syntheticRecording
const wakeIsSynthetic = wakeFile === syntheticRecording

const report = { wakeFile, wakeIsSynthetic, cases: [] }
report.cases.push(run(wakeFile))
report.cases.push(run('assets/test/not-wake-word.wav'))
// 旧唤醒词录音：默认关键词下必须不触发
report.cases.push(run('assets/test/wake-1.wav'))
// 弱信号：把唤醒词衰减 24dB，模拟麦克风阵列那种 -45dBFS 输入
report.cases.push({ ...run(wakeFile, -24), name: '弱信号 -24dB' })

const wakeCase = report.cases[0]
const otherCase = report.cases[1]
const legacyCase = report.cases[2]

const quietCase = report.cases[3]
report.verdict = {
  wakeDetected: wakeCase.wakes.includes(DEFAULT_WAKE_WORD),
  otherNotDetected: otherCase.wakes.length === 0,
  legacyNotDetected: legacyCase.wakes.length === 0,
  // AGC 的正确行为：强信号不加热，弱信号才加
  loudNoBoost: Math.abs(wakeCase.gainMedianDb) < 3,
  quietBoosted: quietCase.gainMedianDb > 5,
  quietStillDetected: quietCase.wakes.includes(DEFAULT_WAKE_WORD),
}
report.verdict.pass = Object.values(report.verdict).every((v) => v === true)

console.log(JSON.stringify(report, null, 1))
if (wakeIsSynthetic && existsSync(syntheticRecording)) {
  console.error('⚠ 正向用例使用 TTS 合成样本；真人「小爱同学」录音验证尚未完成。')
}
if (!existsSync(wakeFile)) console.error(`✗ 缺新唤醒词录音 ${wakeFile}：KWS 流式验收未执行`)
process.exitCode = report.verdict.pass ? 0 : 1
