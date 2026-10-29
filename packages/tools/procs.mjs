/**
 * 进程列表 / 结束进程（抄自 cherry-remote-app 的 `app` 动作）。
 *
 * 关闭应用属于"可能丢用户东西"的操作，所以默认 `ask`；
 * 另外挡掉系统关键进程——杀错一个会让 Windows 直接蓝屏或注销。
 */
import { spawnSync } from 'node:child_process'

/** 系统关键进程：绝不允许模型去杀 */
const PROTECTED = new Set([
  'csrss', 'wininit', 'winlogon', 'services', 'lsass', 'smss', 'svchost',
  'dwm', 'explorer', 'system', 'registry', 'memory compression', 'fontdrvhost',
  'audiodg', 'sihost', 'taskhostw', 'ctfmon', 'searchhost', 'startmenuexperiencehost',
])

export const PROC_TOOL = {
  type: 'function',
  function: {
    name: 'app_control',
    description: '查看正在运行的程序（只读）。action=list 列出有窗口的进程；'
      + 'action=find 按名字查**全部**进程（含后台/托盘，用来确认"到底还在不在"）。'
      + '要关程序请用 kill_app。',
    parameters: {
      type: 'object',
      properties: {
        action: { type: 'string', enum: ['list', 'find'] },
        name: { type: 'string', description: 'action=find 时的进程名（不带 .exe）' },
      },
      required: ['action'],
      additionalProperties: false,
    },
  },
}

/**
 * 关程序：只有这一条路，而且**永远是 taskkill /F /T**。
 *
 * 为什么不再留"温和关闭"的选项：之前那个工具先发 WM_CLOSE、等 3 秒、
 * 还活着就回来问用户"要不要强杀"——用户的原话是优柔寡断。
 * 说"关掉"就是关掉，没有第二个选项，也没有"要不要用力"这一问。
 */
export const KILL_TOOL = {
  type: 'function',
  function: {
    name: 'kill_app',
    description: '结束一个程序（总是 taskkill /F /T，结束整棵进程树）。'
      + '传进程名，如 notepad、chrome、cloudmusic。执行后会验证进程真的没了才回话。'
      + '系统关键进程（explorer、svchost 等）会被拒绝。未保存的内容会丢失。',
    parameters: {
      type: 'object',
      properties: {
        name: { type: 'string', description: '进程名（不带 .exe）' },
      },
      required: ['name'],
      additionalProperties: false,
    },
  },
}

/**
 * 查所有同名进程（含后台），返回 [{ Id, ProcessName, MainWindowTitle, Path }]。
 *
 * 会**过滤掉自己的进程**：TTS worker 也是 python.exe、CUA 桥接也是 powershell.exe，
 * 用户说"关掉 python"时不希望顺手把助手的嗓子也关了（真实测过：一次 stop 端掉了 worker）。
 */
function queryProcesses(name) {
  const result = spawnSync('powershell.exe', ['-NoProfile', '-Command',
    '[Console]::OutputEncoding=[Text.Encoding]::UTF8;'
    + `Get-Process -Name '${name.replace(/'/gu, "''")}' -ErrorAction SilentlyContinue | `
    + 'Select-Object Id, ProcessName, MainWindowTitle, Path | ConvertTo-Json -Compress',
  ], { encoding: 'utf8', windowsHide: true, timeout: 15000 })
  let rows = []
  try {
    const parsed = JSON.parse((result.stdout ?? '').trim() || '[]')
    rows = (Array.isArray(parsed) ? parsed : [parsed]).filter((row) => row !== null && typeof row === 'object')
  } catch {
    return []
  }
  const roots = [process.env.CUA_VA_ROOT, process.env.CUA_VA_RUNTIME_ROOT]
    .filter((value) => typeof value === 'string' && value !== '')
    .map((value) => value.toLowerCase())
  return rows.filter((row) => {
    if (Number(row.Id) === process.pid) return false
    const path = String(row.Path ?? '').toLowerCase()
    if (path === '') return true
    // 自己的子进程（venv 里的 python、resources 里的 powershell 脚本）一律不动
    return !roots.some((root) => path.startsWith(root))
  })
}

/** 等进程消失；返回还在跑的那些 */
function waitGone(name, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const alive = queryProcesses(name)
    if (alive.length === 0) return []
    if (Date.now() >= deadline) return alive
    spawnSync('powershell.exe', ['-NoProfile', '-Command', 'Start-Sleep -Milliseconds 200'], { windowsHide: true, timeout: 5000 })
  }
}

export async function runAppControl(args = {}) {
  const action = String(args.action ?? '')

  if (action === 'list') {
    const result = spawnSync('powershell.exe', ['-NoProfile', '-Command',
      '[Console]::OutputEncoding=[Text.Encoding]::UTF8;'
      + 'Get-Process | Where-Object { $_.MainWindowTitle -ne "" } | '
      + 'Select-Object -First 40 ProcessName, Id, MainWindowTitle | ConvertTo-Json -Compress',
    ], { encoding: 'utf8', windowsHide: true, timeout: 15000 })
    let rows = []
    try {
      const parsed = JSON.parse((result.stdout ?? '').trim() || '[]')
      rows = Array.isArray(parsed) ? parsed : [parsed]
    } catch {
      rows = []
    }
    const lines = rows.map((row) => `${row.ProcessName} (pid ${row.Id}) — ${String(row.MainWindowTitle ?? '').slice(0, 40)}`)
    return {
      text: rows.length === 0
        ? '没有正在运行的可视窗口程序。（注意：这条只列有窗口的进程，后台进程用 action=find）'
        : `正在运行（${rows.length} 个，只有有窗口的）：\n${lines.join('\n')}`,
      data: { count: rows.length, processes: rows },
    }
  }

  // 按名字查所有同名进程（含后台、含托盘），用于"它到底还在不在"
  if (action === 'find') {
    const name = String(args.name ?? '').trim().replace(/\.exe$/iu, '')
    if (name === '') throw new Error('action=find 需要 name')
    const rows = queryProcesses(name)
    if (rows.length === 0) {
      return { text: `没有找到名为「${name}」的进程（已查全部进程，不只是有窗口的）。`, data: { name, found: 0, processes: [] } }
    }
    const lines = rows.map((row) => `pid ${row.Id} · ${row.ProcessName}${row.MainWindowTitle === '' ? '（无窗口/后台）' : ` — ${String(row.MainWindowTitle).slice(0, 30)}`}`)
    return {
      text: `「${name}」还有 ${rows.length} 个进程在跑：\n${lines.join('\n')}`,
      data: { name, found: rows.length, processes: rows },
    }
  }

  throw new Error(`未知 action：${action}（关程序请用 kill_app）`)
}

/**
 * 关程序：总是 taskkill /F /T，然后验证真的没了再回话。
 * 没有"温和关闭"的选项——用户说关就是关。
 */
export async function runKillApp(args = {}) {
  const name = String(args.name ?? '').trim().replace(/\.exe$/iu, '')
  if (name === '') throw new Error('kill_app 需要 name')
  if (PROTECTED.has(name.toLowerCase())) throw new Error(`「${name}」是系统关键进程，拒绝结束`)

  const before = queryProcesses(name)
  if (before.length === 0) throw new Error(`没有找到正在运行的「${name}」（已查全部进程，不只是有窗口的）`)

  const killed = []
  for (const proc of before) {
    const result = spawnSync('taskkill.exe', ['/F', '/T', '/PID', String(proc.Id)], {
      encoding: 'utf8', windowsHide: true, timeout: 15000,
    })
    if (result.status === 0) killed.push(proc.Id)
  }

  const alive = waitGone(name, 3000)
  if (alive.length === 0) {
    return {
      text: `「${name}」已结束（taskkill /F /T，${killed.length} 个进程树）。`,
      data: { name, closed: true, killed, before: before.length },
    }
  }

  const pids = alive.map((row) => `${row.ProcessName}(${row.Id})`).join('、')
  return {
    text: `「${name}」**没关掉**：还剩 ${alive.length} 个进程（${pids}）。`
      + '可能是权限不够（需要管理员）或被保护进程挡住了。',
    data: { name, closed: false, killed, alive: alive.map((row) => row.Id) },
  }
}

/**
 * 兼容旧的 action=stop 调用：仍然走 taskkill，但不再出现在工具清单里。
 * 留着是为了模型偶尔复用上一轮的参数形状时不会直接报错。
 */
export async function runAppControlLegacyStop(args = {}) {
  return runKillApp(args)
}
