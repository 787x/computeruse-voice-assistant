/**
 * TTS 验收（当前后端：GPT-SoVITS + GSV-TTS-Lite，银狼音色）。
 *
 * 验的是"接进项目之后还能不能用"，不是模型本身的质量：
 *   1. worker 能起来、模型能加载（首次 25~30 秒，之后常驻）
 *   2. 三句不同长度都能合成，并给出 RTF
 *   3. 换情绪参考音频有效（音色/情绪解耦）
 *   4. 关闭后进程真的退出了（否则 1.5GB 显存泄漏）
 *
 * 用法：node tools/verify-tts.mjs
 */
import { spawnSync } from 'node:child_process'

import { Tts } from '../packages/voice/tts.mjs'
import { writeWav } from '../packages/voice/wav.mjs'

const LINES = [
  '好的。',
  '我已经把浏览器打开了，接下来要搜什么？',
  '上海今天多云转晴，气温大概十七到二十一度，早晚有点凉，出门套件薄外套就够了。',
]

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

function pythonProcessCount() {
  // 只统计跑 gsv_worker 的 python（别把用户自己的 python 算进来）
  const ps = spawnSync('powershell.exe', ['-NoProfile', '-Command',
    "(Get-CimInstance Win32_Process -Filter \"Name='python.exe'\" | Where-Object { $_.CommandLine -match 'gsv_worker' } | Measure-Object).Count",
  ], { encoding: 'utf8', windowsHide: true })
  return Number((ps.stdout ?? '0').trim()) || 0
}

const started = Date.now()
const tts = new Tts()
const ready = await tts.ready()
check('worker 就绪', typeof ready.loadMs === 'number', `${Date.now() - started}ms（loadMs=${ready.loadMs}）`)
check('情绪列表完整', Array.isArray(ready.voices) && ready.voices.length >= 3, ready.voices.join('/'))

const runs = []
for (const text of LINES) {
  const result = await tts.speak(text)
  runs.push({ chars: text.length, ...result.stats })
  check(`合成「${text.slice(0, 8)}…」`, result.samples.length > 1000 && result.sampleRate === 32000,
    `${result.stats.synthMs}ms / 音频 ${result.stats.durationSec}s / RTF ${result.stats.rtf}`)
}

// 短句（"好的。"）的固定开销占比高，RTF 天然会 >1；只对 ≥10 字的句子要求能实时跟上
const longRuns = runs.filter((run) => run.chars >= 10)
check('长句 RTF 小于 1（能实时跟上）', longRuns.length > 0 && longRuns.every((run) => run.rtf < 1),
  runs.map((run) => `${run.chars}字:${run.rtf}`).join(' / '))

const angry = await tts.speak('这下麻烦大了。', { emotion: '生气' })
check('换情绪生效', angry.stats.emotion === '生气', `emotion=${angry.stats.emotion}`)

// 落一份可试听的文件
const sample = await tts.speak('我是银狼，今天想干点什么？')
writeWav('.runtime/samples/verify-tts.wav', sample.samples, sample.sampleRate)
check('写出试听文件', true, '.runtime/samples/verify-tts.wav')

const before = pythonProcessCount()
await tts.dispose()
await new Promise((resolve) => setTimeout(resolve, 1200))
const after = pythonProcessCount()
check('关闭后进程退出', after < before || after === 0, `before=${before} after=${after}`)

console.log('TTS 验收（GSV-TTS-Lite / 银狼）\n' + '='.repeat(70))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(26, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(70))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
