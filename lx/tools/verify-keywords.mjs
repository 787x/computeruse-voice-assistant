/**
 * 唤醒词/KWS 关键词验收。
 *
 * 默认只允许「小爱同学」唤醒：
 *   - 新唤醒词必须真的命中（优先真人录音，缺录音时用明确标注的 TTS 样本）；
 *   - 旧唤醒词「你好狼宝」必须不再触发；
 *   - 普通对话、打断词、无关语音不能误触发。
 *
 * 为什么不能把旧录音的期望文字一改了事：旧录音里说的是「你好狼宝」，
 * 它只能作为**负向**用例。新唤醒词必须有新的音频输入，否则验证没有发生。
 *
 * 用法：node tools/verify-keywords.mjs
 * 可选：把真人录音放到 assets/test/xiaoai-tongxue.wav，脚本会优先使用它。
 */
import { createRequire } from 'node:module'
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { Kws } from '../packages/voice/kws.mjs'
import { readWav } from '../packages/voice/wav.mjs'
import {
  DEFAULT_WAKE_WORD,
  LEGACY_WAKE_WORDS,
  keywordLabel,
  readKeywordLines,
} from '../packages/voice/keywords.mjs'

const require = createRequire(import.meta.url)
const { LinearResampler } = require('sherpa-onnx-node/resampler.js')

// 测试不能读取/覆盖本机用户的自定义关键词，用独立数据目录跑默认解析路径。
const dataDir = mkdtempSync(join(tmpdir(), 'cua-va-keywords-'))
process.env.CUA_VA_DATA_DIR = dataDir

const kws = new Kws()
const keywordInfo = kws.keywordInfo
const labels = keywordInfo.labels ?? []
const lines = readKeywordLines(keywordInfo.file)
const oldLabels = lines.map(keywordLabel).filter((label) => LEGACY_WAKE_WORDS.includes(label))
const defaultOnlyNew = labels.length === 1
  && labels[0] === DEFAULT_WAKE_WORD
  && oldLabels.length === 0

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

const realRecording = 'assets/test/xiaoai-tongxue.wav'
const syntheticRecording = 'assets/test/xiaoai-tongxue-synthetic.wav'
const positive = existsSync(realRecording)
  ? { file: realRecording, synthetic: false, note: '新唤醒词（真人录音）' }
  : existsSync(syntheticRecording)
    ? { file: syntheticRecording, synthetic: true, note: '新唤醒词（TTS 合成）' }
    : null

const cases = []
if (positive === null) {
  cases.push({ note: '新唤醒词「小爱同学」', expect: DEFAULT_WAKE_WORD, status: 'incomplete', detail: '缺新录音（真人 xiaoai-tongxue.wav / 合成 xiaoai-tongxue-synthetic.wav）' })
} else {
  cases.push({ ...positive, expect: DEFAULT_WAKE_WORD, required: true })
}
cases.push(
  { file: 'assets/test/nihao-langbao.wav', expect: null, note: '旧唤醒词「你好狼宝」不得触发', required: true },
  { file: 'assets/test/wake-1.wav', expect: null, note: '更早唤醒词不得触发' },
  { file: 'assets/test/dengyixia.wav', expect: null, note: '打断词「等一下」不得触发' },
  { file: 'assets/test/other-speech.wav', expect: null, note: '无关语音' },
  { file: 'assets/test/asr-zh.wav', expect: null, note: '无关语音（指令句）' },
  { file: 'assets/test/not-wake-word.wav', expect: null, note: '普通语句' },
)

const rows = []
for (const item of cases) {
  if (item.status === 'incomplete') {
    rows.push(item)
    continue
  }
  if (!existsSync(item.file)) {
    rows.push({ ...item, status: item.required ? 'fail' : 'skip', detail: '缺文件' })
    continue
  }
  const result = detect(item.file)
  const pass = item.expect === null
    ? result.hits.length === 0
    : result.hits.includes(item.expect)
  rows.push({
    ...item,
    status: pass ? 'pass' : 'fail',
    hits: result.hits,
    detail: result.hits.length === 0 ? '未命中' : `${result.hits.join(' + ')} @${result.firstAtMs}ms`,
  })
}

const failed = rows.filter((row) => row.status === 'fail')
const skipped = rows.filter((row) => row.status === 'skip')
const incomplete = positive === null || positive.synthetic
const requireReal = process.env.CUA_VA_REQUIRE_REAL_WAKE_RECORDING === '1'

console.log('KWS 关键词验收\n' + '='.repeat(62))
console.log(`关键词文件: ${keywordInfo.file}`)
console.log(`来源: ${keywordInfo.source}${keywordInfo.migrated ? '（已从旧默认迁移）' : ''}`)
console.log(`实际关键词: ${labels.join(' / ') || '(空)'}`)
console.log(`仅含新默认词: ${defaultOnlyNew ? '是' : '否'}`)
console.log('-'.repeat(62))
for (const row of rows) {
  const mark = row.status === 'pass' ? '✓' : row.status === 'fail' ? '✗' : row.status === 'skip' ? '-' : '…'
  console.log(`${mark} ${row.note.padEnd(28, ' ')} 期望 ${String(row.expect ?? '不触发').padEnd(6)} → ${row.detail}`)
}
console.log('='.repeat(62))
if (positive?.synthetic === true) {
  console.log('⚠ 未提供真人「小爱同学」录音，当前正向用例使用项目 TTS 合成样本。')
  console.log('  真人正向验证尚未完成；将真人录音放到 assets/test/xiaoai-tongxue.wav 后重跑即可。')
}
if (skipped.length > 0) console.log(`- 跳过 ${skipped.length} 个可选用例（缺文件）`)
if (failed.length > 0) console.log(`✗ 失败 ${failed.length} 个用例`)

const verdict = {
  defaultOnlyNew,
  newWakeDetected: rows.find((row) => row.note?.startsWith('新唤醒词'))?.status === 'pass',
  legacyNotDetected: rows.find((row) => row.note?.includes('旧唤醒词'))?.status === 'pass',
  noFalseTrigger: rows.filter((row) => row.expect === null).every((row) => row.status === 'pass' || row.status === 'skip'),
  realRecordingVerified: positive !== null && !positive.synthetic,
  pass: defaultOnlyNew && failed.length === 0 && positive !== null,
  incomplete,
}
console.log(JSON.stringify({ keywordFile: keywordInfo.file, labels, cases: rows.length, failed: failed.length, skipped: skipped.length, verdict }))

rmSync(dataDir, { recursive: true, force: true })

if (!verdict.pass) process.exitCode = failed.length > 0 ? 1 : 2
else if (requireReal && !verdict.realRecordingVerified) process.exitCode = 2
