/**
 * 资源策略验收。
 *
 * 分两层：
 *   纯逻辑（占比/全屏判定/决策）—— 用构造数据跑遍边界
 *   真实探测（foreground.ps1）—— 至少确认它能在本机跑出结构合法的 JSON
 *
 * 用法：node tools/verify-resources.mjs
 */
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'

import { coverageRatio, isFullscreen, decide, RESOURCE_DEFAULTS } from '../packages/system/resource-policy.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

const screen = { screenLeft: 0, screenTop: 0, screenW: 1920, screenH: 1040, monLeft: 0, monTop: 0, monW: 1920, monH: 1080 }
const full = { title: 'ELDEN RING', process: 'eldenring', left: 0, top: 0, right: 1920, bottom: 1080, ...screen }
const windowed = { title: '记事本', process: 'notepad', left: 100, top: 100, right: 900, bottom: 700, ...screen }
// 最大化窗口只铺满工作区（不含任务栏 40px），按整块显示器算约 96%
const maximized = { title: 'VS Code', process: 'Code', left: 0, top: 0, right: 1920, bottom: 1040, ...screen }
const taskbarOnly = { title: '', process: 'explorer', left: 0, top: 0, right: 1920, bottom: 1080, ...screen }

check('占比：全屏 = 100%', coverageRatio(full) === 1, `${coverageRatio(full)}`)
check('占比：窗口 ≈ 24%', Math.abs(coverageRatio(windowed) - 0.2407) < 0.01, `${coverageRatio(windowed).toFixed(3)}`)
check('占比：越界被夹掉', coverageRatio({ title: 'x', process: 'x', left: -200, top: -100, right: 500, bottom: 500, ...screen }) <= 1, '')

check('全屏判定：盖住整块显示器 → true', isFullscreen(full) === true, '')
check('全屏判定：普通窗口 → false', isFullscreen(windowed) === false, '')
// 关键：最大化的浏览器不该被当成游戏——否则语音会一直被关掉
check('全屏判定：最大化窗口 → false', isFullscreen(maximized) === false, `${(coverageRatio(maximized) * 100).toFixed(1)}%`)
check('全屏判定：explorer 豁免', isFullscreen(taskbarOnly) === false, '')
check('全屏判定：空标题豁免', isFullscreen({ ...full, title: '' }) === false, '')
check('全屏判定：只铺满上半屏 → false', isFullscreen({ ...full, bottom: 540 }) === false, '')

check('决策：忙时一律不动', decide({ fullscreen: true, freeMemMb: 100, voiceLoaded: true, asrLoaded: true, busy: true }).reason === 'busy', '')
check('决策：全屏 → 只释放语音',
  (() => { const d = decide({ fullscreen: true, freeMemMb: 8000, voiceLoaded: true, asrLoaded: true }); return d.releaseVoice && !d.releaseAsr })(), '')
check('决策：内存低 → 只卸 ASR',
  (() => { const d = decide({ fullscreen: false, freeMemMb: 600, voiceLoaded: true, asrLoaded: true }); return !d.releaseVoice && d.releaseAsr })(), '')
check('决策：都没加载 → 无动作', decide({ fullscreen: true, freeMemMb: 300, voiceLoaded: false, asrLoaded: false }).reason === 'ok', '')
check('决策：内存阈值可配', decide({ freeMemMb: 1500, asrLoaded: true }, { memoryFloorMb: 2000 }).releaseAsr === true, '')

// 真实探测
const script = join(process.cwd(), 'packages', 'system', 'foreground.ps1')
const probe = spawnSync('powershell.exe', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', script], {
  encoding: 'utf8', windowsHide: true, timeout: 15000,
})
let parsed = null
try { parsed = JSON.parse((probe.stdout ?? '').trim()) } catch { parsed = null }
check('真实探测：输出合法 JSON', parsed !== null && typeof parsed.hwnd === 'string',
  parsed ? `前台=${parsed.process || '?'} 占比=${(coverageRatio(parsed) * 100).toFixed(0)}%` : `失败: ${(probe.stderr ?? '').slice(0, 80)}`)
check('真实探测：字段完整', parsed !== null && ['left', 'top', 'right', 'bottom', 'screenW', 'screenH'].every((k) => typeof parsed[k] === 'number'), '')

console.log('资源策略验收（R14）\n' + '='.repeat(66))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(30, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(66))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
