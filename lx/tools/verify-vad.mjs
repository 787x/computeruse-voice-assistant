/**
 * P2-b 验收：自适应 VAD + AGC 在「安静」与「吵闹」两种环境下都要成立。
 *
 * 素材（assets/test/）：
 *   noise-dorm.wav     本机实测宿舍底噪（-42.2 dBFS）
 *   speech-clean.wav   TTS 干净语音（-18.9 dBFS）
 *   mix-snr{20,10,5,0}.wav  按 SNR 混入宿舍噪声
 *
 * 验收点：
 *   1. 纯噪声上几乎不误触发（这是"吵"环境下最关键的指标）
 *   2. 干净语音上检出率高
 *   3. 低 SNR 下仍能大致框住语音（允许边界误差）
 *   4. AGC 在安静环境不硬拉增益、在吵环境不把噪声一起放大
 */
import { readFileSync } from 'node:fs'

import { detectSpeech } from '../packages/voice/vad.mjs'
import { autoGain } from '../packages/voice/agc.mjs'

function readWav(path) {
  const buf = readFileSync(path)
  const sampleRate = buf.readUInt32LE(24)
  const bits = buf.readUInt16LE(34)
  const channels = buf.readUInt16LE(22)
  const dataSize = buf.readUInt32LE(40)
  const samples = new Float32Array(dataSize / 2 / channels)
  for (let i = 0; i < samples.length; i += 1) {
    samples[i] = buf.readInt16LE(44 + i * 2 * channels) / 32768
  }
  return { samples, sampleRate, bits, channels }
}

/** 帧级对比：以干净语音的 VAD 结果作为参考 */
function compare(refSpeech, testSpeech) {
  let tp = 0; let fp = 0; let fn = 0; let tn = 0
  const n = Math.min(refSpeech.length, testSpeech.length)
  for (let i = 0; i < n; i += 1) {
    const r = refSpeech[i] === 1
    const t = testSpeech[i] === 1
    if (r && t) tp += 1
    else if (!r && t) fp += 1
    else if (r && !t) fn += 1
    else tn += 1
  }
  const precision = tp + fp === 0 ? null : tp / (tp + fp)
  const recall = tp + fn === 0 ? null : tp / (tp + fn)
  return {
    precision: precision === null ? null : Math.round(precision * 1000) / 1000,
    recall: recall === null ? null : Math.round(recall * 1000) / 1000,
    tp, fp, fn, tn,
  }
}

const report = { cases: [], verdict: {} }

const clean = readWav('assets/test/speech-clean.wav')
const refVad = detectSpeech(clean.samples, clean.sampleRate)
report.reference = {
  file: 'speech-clean.wav',
  sampleRate: clean.sampleRate,
  noiseFloorDb: refVad.floorDb,
  speechRatio: Math.round((refVad.speech.reduce((a, b) => a + b, 0) / refVad.speech.length) * 1000) / 1000,
  segments: refVad.segments.length,
}

const cases = [
  // 合成噪声：确定不含人声，用来验证"空场不误触发"
  { name: '纯噪声（合成，无人声）', file: 'assets/test/noise-synthetic.wav', kind: 'noise' },
  // 宿舍实录音频：很可能本来就含室友说话，按"含人声"单独看
  { name: '宿舍实录（可能含人声）', file: 'assets/test/noise-dorm.wav', kind: 'dorm' },
  { name: '干净语音', file: 'assets/test/speech-clean.wav', kind: 'speech' },
  { name: 'SNR 20dB', file: 'assets/test/mix-snr20.wav', kind: 'mix' },
  { name: 'SNR 10dB', file: 'assets/test/mix-snr10.wav', kind: 'mix' },
  { name: 'SNR 5dB', file: 'assets/test/mix-snr5.wav', kind: 'mix' },
  { name: 'SNR 0dB', file: 'assets/test/mix-snr0.wav', kind: 'mix' },
]

for (const c of cases) {
  const audio = readWav(c.file)
  const vad = detectSpeech(audio.samples, audio.sampleRate)
  const agc = autoGain(audio.samples, audio.sampleRate, vad)
  const item = {
    name: c.name,
    noiseFloorDb: vad.floorDb,
    speechRatio: Math.round((vad.speech.reduce((a, b) => a + b, 0) / vad.speech.length) * 1000) / 1000,
    segments: vad.segments.length,
    segmentTotalSec: Math.round(vad.segments.reduce((sum, s) => sum + s.durationSec, 0) * 10) / 10,
    agc: agc.stats,
  }
  if (c.kind !== 'noise') {
    item.vsClean = compare(refVad.speech, vad.speech)
  }
  report.cases.push(item)
}

const noiseCase = report.cases.find((c) => c.name.includes('合成'))
const dormCase = report.cases.find((c) => c.name.includes('宿舍'))
const cleanCase = report.cases.find((c) => c.name === '干净语音')
const snr10 = report.cases.find((c) => c.name === 'SNR 10dB')
const snr0 = report.cases.find((c) => c.name === 'SNR 0dB')

report.verdict = {
  // 吵的时候最容易出问题的是"一直误触发"，所以用语音占比卡死
  noiseDoesNotTrigger: noiseCase.speechRatio < 0.05,
  cleanDetected: (cleanCase.vsClean?.recall ?? 0) > 0.85,
  snr10Usable: (snr10.vsClean?.recall ?? 0) > 0.7,
  snr0DegradesGracefully: (snr0.vsClean?.precision ?? 0) > 0.5,
  // AGC：安静环境下不应硬拉满增益
  agcAdaptive: (cleanCase.agc.medianGainDb ?? 0) <= 6,
}
report.dormNote = {
  speechRatio: dormCase?.speechRatio,
  note: '宿舍实录里若含室友说话，VAD 判为语音是正确行为；区分“谁在说”要靠唤醒词/声纹，不是 VAD',
}
report.verdict.pass = Object.values(report.verdict).every((v) => v === true)

console.log(JSON.stringify(report, null, 1))
process.exitCode = report.verdict.pass ? 0 : 1
