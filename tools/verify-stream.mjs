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

/*
 * fail-closed：必需用例（正向/负向/旧词/弱信号）任何一个异常或缺输入
 * 都记录为 executed:false / pass:false 并参与判定——不允许"跑都没跑却被
 * wakes.length===0 算成没有误唤醒"。负向用例的通过条件是
 * "确实执行完 + 没有命中"，两者缺一不可。
 */
const report = { wakeFile, wakeIsSynthetic, cases: [] }

/** 判定单个用例：executed 且满足对应期望才算 pass。 */
function judge(c, expect) {
  if (!c.executed) return false
  if (expect.kind === 'hit') return c.wakes.includes(DEFAULT_WAKE_WORD)
  return c.wakes.length === 0 && c.gainMedianDb !== null
}

// --selftest：回归"负向用例异常但 wakes 为空不能算通过"这个历史假阳性。
if (process.argv.includes('--selftest')) {
  const scenarios = [
    { name: '负向正常执行零命中', c: { executed: true, wakes: [], gainMedianDb: 0.5 }, e: { kind: 'no-hit' }, want: true },
    { name: '负向异常（wakes 置空的旧记录形态）', c: { executed: false, wakes: [], gainMedianDb: null }, e: { kind: 'no-hit' }, want: false },
    { name: '负向执行了但误触发', c: { executed: true, wakes: [DEFAULT_WAKE_WORD], gainMedianDb: 0 }, e: { kind: 'no-hit' }, want: false },
    { name: '正向命中', c: { executed: true, wakes: [DEFAULT_WAKE_WORD], gainMedianDb: 0 }, e: { kind: 'hit' }, want: true },
    { name: '正向未命中', c: { executed: true, wakes: [], gainMedianDb: 0 }, e: { kind: 'hit' }, want: false },
    { name: '正向异常未执行', c: { executed: false, wakes: null, gainMedianDb: null }, e: { kind: 'hit' }, want: false },
    { name: '必需用例记录缺失（find 不到）', c: undefined, e: { kind: 'no-hit' }, want: false },
  ]
  let ok = true
  for (const s of scenarios) {
    const got = s.c === undefined ? false : judge(s.c, s.e)
    if (got !== s.want) ok = false
    console.log(`${got === s.want ? '✓' : '✗'} 判定自测 · ${s.name}: want=${s.want} got=${got}`)
  }
  console.log(JSON.stringify({ selftest: true, verdict: ok ? 'pass' : 'fail' }))
  process.exit(ok ? 0 : 1)
}

function pushRun(id, note, path, db = 0, expect = { kind: 'no-hit' }) {
  let c
  try {
    if (!existsSync(path)) throw new Error(`缺文件 ${path}`)
    c = { id, note, required: true, expect, executed: true, ...run(path, db) }
  } catch (error) {
    // 异常记录：wakes/gain 留 null，judge 里 executed=false 直接失败
    c = { id, note, required: true, expect, executed: false, error: error.message, file: path, wakes: null, gainMedianDb: null }
  }
  c.pass = judge(c, expect)
  report.cases.push(c)
}
pushRun('W', '新唤醒词（流式管线）', wakeFile, 0, { kind: 'hit' })
pushRun('O', '普通语句', 'assets/test/not-wake-word.wav')
// 旧唤醒词录音：默认关键词下必须不触发
pushRun('L', '旧唤醒词录音', 'assets/test/wake-1.wav')
// 弱信号：把唤醒词衰减 24dB，模拟麦克风阵列那种 -45dBFS 输入
pushRun('Q', '弱信号 -24dB', wakeFile, -24, { kind: 'hit' })

const byId = new Map(report.cases.map((c) => [c.id, c]))
const wakeCase = byId.get('W')
const quietCase = byId.get('Q')

report.verdict = {
  wakeDetected: wakeCase.executed && wakeCase.wakes.includes(DEFAULT_WAKE_WORD),
  // 负向：必须"确实执行且零命中"；异常用例直接 false（不再被空数组蒙过）
  otherNotDetected: byId.get('O').pass === true,
  legacyNotDetected: byId.get('L').pass === true,
  // AGC 的正确行为：强信号不加热，弱信号才加（异常时 gainMedianDb=null → 失败）
  loudNoBoost: wakeCase.executed && Math.abs(wakeCase.gainMedianDb) < 3,
  quietBoosted: quietCase.executed && quietCase.gainMedianDb > 5,
  quietStillDetected: quietCase.executed && quietCase.wakes.includes(DEFAULT_WAKE_WORD),
  allRequiredPass: ['W', 'O', 'L', 'Q'].every((id) => byId.get(id)?.pass === true),
}
report.verdict.pass = Object.values(report.verdict).every((v) => v === true)

console.log(JSON.stringify(report, null, 1))
if (wakeIsSynthetic && existsSync(syntheticRecording)) {
  console.error('⚠ 正向用例使用 TTS 合成样本；真人「小爱同学」录音验证尚未完成。')
}
if (!existsSync(wakeFile)) console.error(`✗ 缺新唤醒词录音 ${wakeFile}：KWS 流式验收未执行`)
process.exitCode = report.verdict.pass ? 0 : 1
