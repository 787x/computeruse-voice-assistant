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
 * 判定是 fail-closed 的（PR #2 评审修复）：每个必需用例都有唯一 id 与预先
 * 声明的期望；异常、缺文件、缺录音都记录为 pass=false 并参与最终判定，
 * 不做任何"过滤后 every()"的空集合真值判断。
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
  LEGACY_DEFAULT_LINES,
  LEGACY_WAKE_WORDS,
  keywordLabel,
  readKeywordLines,
} from '../packages/voice/keywords.mjs'

/** 判定（同 verify-wake-runtime 的规则，见其 --selftest 回归）。 */
function keywordsVerdict(rows, requiredIds) {
  const byId = new Map(rows.map((r) => [r.id, r]))
  const checks = {}
  for (const id of requiredIds) {
    const r = byId.get(id)
    checks[id] = r !== undefined && r.required === true && r.pass === true
  }
  return { checks, pass: requiredIds.every((id) => checks[id] === true) }
}

const require = createRequire(import.meta.url)
const { LinearResampler } = require('sherpa-onnx-node/resampler.js')

// 测试不能读取/覆盖本机用户的自定义关键词，用独立数据目录跑默认解析路径。
const dataDir = mkdtempSync(join(tmpdir(), 'cua-va-keywords-'))
process.env.CUA_VA_DATA_DIR = dataDir

let kws = null
let keywordInfo = null
let defaultOnlyNew = false
let linesLoaded = []
try {
  kws = new Kws()
  keywordInfo = kws.keywordInfo
  const labels = keywordInfo.labels ?? []
  linesLoaded = readKeywordLines(keywordInfo.file)
  const oldLabels = linesLoaded.map(keywordLabel).filter((label) => LEGACY_WAKE_WORDS.includes(label))
  defaultOnlyNew = labels.length === 1
    && labels[0] === DEFAULT_WAKE_WORD
    && oldLabels.length === 0
    // 不允许残留"整份旧默认"（迁移没生效的表征）
    && !(linesLoaded.length === LEGACY_DEFAULT_LINES.length
      && linesLoaded.every((line, i) => line === LEGACY_DEFAULT_LINES[i]))
} catch (error) {
  console.log(`✗ KWS 初始化失败: ${error.message}`)
}

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
  ? { id: 'A', file: realRecording, synthetic: false, note: '新唤醒词（真人录音）' }
  : existsSync(syntheticRecording)
    ? { id: 'A', file: syntheticRecording, synthetic: true, note: '新唤醒词（TTS 合成）' }
    : null

// 必需用例：正向 A + 默认词表 + 全部负向（缺任何一个 = 验收没发生 = 失败）
const NEGATIVES = [
  { id: 'N1', file: 'assets/test/nihao-langbao.wav', note: `旧唤醒词「${LEGACY_WAKE_WORDS.join('/')}」不得触发` },
  { id: 'N2', file: 'assets/test/wake-1.wav', note: '更早唤醒词不得触发' },
  { id: 'N3', file: 'assets/test/dengyixia.wav', note: '打断词「等一下」不得触发' },
  { id: 'N4', file: 'assets/test/other-speech.wav', note: '无关语音' },
  { id: 'N5', file: 'assets/test/asr-zh.wav', note: '无关语音（指令句）' },
  { id: 'N6', file: 'assets/test/not-wake-word.wav', note: '普通语句' },
]
const REQUIRED_IDS = ['A', 'KWS', ...NEGATIVES.map((n) => n.id)]

const rows = []
function pushCheck(id, note, fn) {
  if (kws === null) {
    rows.push({ id, note, required: true, pass: false, error: 'KWS 初始化失败' })
    return
  }
  try {
    rows.push({ id, note, required: true, ...fn() })
  } catch (error) {
    rows.push({ id, note, required: true, pass: false, error: error.message })
  }
}

// A) 新唤醒词必须命中（缺新录音 = 失败，不是跳过）
if (positive === null) {
  rows.push({ id: 'A', note: `新唤醒词「${DEFAULT_WAKE_WORD}」`, required: true, expect: DEFAULT_WAKE_WORD, pass: false, detail: '缺新录音（真人 xiaoai-tongxue.wav / 合成 xiaoai-tongxue-synthetic.wav）' })
} else {
  pushCheck('A', positive.note, () => {
    const result = detect(positive.file)
    return { expect: DEFAULT_WAKE_WORD, pass: result.hits.includes(DEFAULT_WAKE_WORD), detail: result.hits.length === 0 ? '未命中' : `${result.hits.join(' + ')} @${result.firstAtMs}ms` }
  })
}

// KWS) 默认解析出的词表必须只含新词
pushCheck('KWS', '生效关键词只含「小爱同学」', () => ({
  pass: defaultOnlyNew,
  detail: `labels=${(keywordInfo?.labels ?? []).join('/') || '(空)'} source=${keywordInfo?.source ?? 'n/a'}`,
}))

// 负向）全部必需；缺文件也算失败
for (const neg of NEGATIVES) {
  if (!existsSync(neg.file)) {
    rows.push({ id: neg.id, note: neg.note, required: true, expect: null, pass: false, detail: '缺文件' })
    continue
  }
  pushCheck(neg.id, neg.note, () => {
    const result = detect(neg.file)
    return { expect: null, pass: result.hits.length === 0, detail: result.hits.length === 0 ? '未命中' : `误触发: ${result.hits.join(' + ')} @${result.firstAtMs}ms` }
  })
}

const verdict = keywordsVerdict(rows, REQUIRED_IDS)

console.log('KWS 关键词验收（fail-closed）\n' + '='.repeat(62))
if (keywordInfo !== null) {
  console.log(`关键词文件: ${keywordInfo.file}`)
  console.log(`来源: ${keywordInfo.source}${keywordInfo.migrated ? '（已从旧默认迁移）' : ''}${keywordInfo.warning ? `（warning: ${keywordInfo.warning}）` : ''}`)
}
console.log('-'.repeat(62))
for (const row of rows) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.note.padEnd(28, ' ')} 期望 ${String(row.expect ?? '不触发').padEnd(6)} → ${row.detail ?? row.error ?? ''}`)
}
console.log('='.repeat(62))
if (positive?.synthetic === true) {
  console.log('⚠ 未提供真人「小爱同学」录音，当前正向用例使用项目 TTS 合成样本。')
  console.log('  真人正向验证尚未完成；将真人录音放到 assets/test/xiaoai-tongxue.wav 后重跑即可。')
}
console.log(JSON.stringify({ keywordFile: keywordInfo?.file ?? null, cases: rows.length, verdict, realRecordingVerified: positive !== null && !positive.synthetic }))
rmSync(dataDir, { recursive: true, force: true })
process.exitCode = verdict.pass ? 0 : 1
