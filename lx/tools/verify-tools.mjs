/**
 * 新工具验收（shell / files / app_control / system_info）。
 *
 * 这几个工具都是"能真的改系统"的，所以验的重点不是"能不能跑通"，
 * 而是"危险动作会不会被挡住"：
 *   - files 删除盘符根、Windows 目录 → 必须拒绝
 *   - app_control 杀系统关键进程 → 必须拒绝
 *   - run_shell 超时 → 必须被杀掉而不是挂住
 *
 * 用法：node tools/verify-tools.mjs
 */
import { mkdtemp, rm, readFile, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { runFiles } from '../packages/tools/files.mjs'
import { runShell, assessRisk } from '../packages/tools/shell.mjs'
import { runAppControl, runKillApp } from '../packages/tools/procs.mjs'
import { runSystemInfo } from '../packages/tools/sysinfo.mjs'
import { classify } from '../packages/agent/permissions.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

const dir = await mkdtemp(join(tmpdir(), 'tools-'))
const file = join(dir, 'note.txt')

// ---- files：写 → 读 → 列 → 复制 → 改信息 → 删除
const written = await runFiles({ action: 'write', path: file, content: '第一行\n第二行' })
check('files.write', written.data.ok === true && (await readFile(file, 'utf8')).includes('第二行'), file.split('\\').pop())

const read = await runFiles({ action: 'read', path: file })
check('files.read', read.text.includes('第一行'), `${read.data.bytes} 字节`)

const listed = await runFiles({ action: 'list', path: dir })
check('files.list', listed.data.entries.some((entry) => entry.path.endsWith('note.txt')), `${listed.data.entries.length} 项`)

const copied = await runFiles({ action: 'copy', path: file, dest: join(dir, 'copy.txt') })
check('files.copy', copied.data.ok === true, '')

const info = await runFiles({ action: 'info', path: file })
check('files.info', info.data.size > 0 && info.data.isDirectory === false, `${info.data.size} 字节`)

const deleted = await runFiles({ action: 'delete', path: join(dir, 'copy.txt') })
check('files.delete', deleted.data.ok === true, '')

// ---- files：危险目标必须被拒
for (const [name, path] of [['盘符根', 'C:\\'], ['Windows 目录', process.env.SystemRoot ?? 'C:\\Windows']]) {
  let blocked = false
  try { await runFiles({ action: 'delete', path }) } catch (error) { blocked = /拒绝/u.test(error.message) }
  check(`files.delete 拒绝${name}`, blocked, '')
}

// ---- files：不存在的路径要报错而不是静默成功
let missing = false
try { await runFiles({ action: 'delete', path: join(dir, '不存在.txt') }) } catch { missing = true }
check('files.delete 不存在要报错', missing, '')

// ---- run_shell：正常执行
const echo = await runShell({ command: 'Write-Output "hello-tools"' })
check('run_shell 输出', echo.text.includes('hello-tools') && echo.data.code === 0, `${echo.data.elapsedMs}ms`)

// ---- run_shell：超时会被杀
const slow = await runShell({ command: 'Start-Sleep -Seconds 5', timeout_ms: 1200 })
check('run_shell 超时被杀', slow.data.killed === true, `${slow.data.elapsedMs}ms`)

// ---- run_shell：风险识别
check('风险识别：删除命令', assessRisk('Remove-Item C:\\x').includes('删除文件'), '')
check('风险识别：普通命令无风险', assessRisk('Get-Date').length === 0, '')

// ---- app_control：只读
const procs = await runAppControl({ action: 'list' })
check('app_control 能列出进程', Array.isArray(procs.data.processes), `${procs.data.count} 个窗口`)

// ---- app_control：只读工具，不再有关闭动作
{
  const malformed = await runAppControl({ action: 'stop', name: 'notepad' }).then(() => 'no-throw').catch((error) => error.message)
  check('app_control 已经没有 stop', /未知 action/u.test(malformed), malformed.slice(0, 24))
}

// ---- kill_app：总是 taskkill，且验证真的没了
{
  const { spawn } = await import('node:child_process')
  const stubborn = spawn('python', ['-c', 'import time; time.sleep(45)'], { detached: true, stdio: 'ignore' })
  stubborn.unref()
  await new Promise((resolve) => setTimeout(resolve, 1200))

  const found = await runAppControl({ action: 'find', name: 'python' })
  check('app_control.find 找到后台进程', found.data.found >= 1, `${found.data.found} 个`)

  const closed = await runKillApp({ name: 'python' })
  check('kill_app 直接 taskkill 关掉顽固进程', closed.data.closed === true, closed.text.slice(0, 34))

  const after = await runAppControl({ action: 'find', name: 'python' })
  check('关完之后确实查不到', after.data.found === 0, `${after.data.found} 个`)

  let protectedBlocked = false
  try { await runKillApp({ name: 'lsass' }) } catch (error) { protectedBlocked = /系统关键进程/u.test(error.message) }
  check('kill_app 拒绝系统进程', protectedBlocked, '')
}

// ---- system_info
const sys = await runSystemInfo({})
check('system_info 字段完整',
  typeof sys.data.cpuPercent === 'number' && sys.data.memoryTotalGb > 0 && Array.isArray(sys.data.disks),
  `CPU ${sys.data.cpuPercent}% · 内存 ${sys.data.memoryUsedPercent}% · 磁盘 ${sys.data.disks.length} 个`)

// ---- 权限分类：风险动作必须落在 ask 档
const classifyCases = [
  ['run_shell', { command: 'dir' }, 'system.shell'],
  ['files', { action: 'delete', path: 'x' }, 'file.delete'],
  ['files', { action: 'write', path: 'x' }, 'file.write'],
  ['files', { action: 'list', path: 'x' }, 'file.read'],
  ['kill_app', { name: 'x' }, 'app.close'],
  ['app_control', { action: 'list' }, 'window.read'],
]
for (const [tool, args, expected] of classifyCases) {
  const verdict = classify(tool, args)
  check(`权限分类 ${tool}/${args.action ?? ''}`, verdict.capability === expected, verdict.capability)
}

await rm(dir, { recursive: true, force: true })

console.log('新工具验收\n' + '='.repeat(68))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(30, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(68))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
