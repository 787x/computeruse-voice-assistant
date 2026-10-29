/**
 * 跑一条命令（抄自 cherry-remote-app 的 `exec`，收紧了超时与输出长度）。
 *
 * 这是最危险的一个工具：一条 cmd 能做任何事。所以
 *   - 权限默认 `ask`（每次都要用户点头，见 permissions.mjs 的 system.shell）
 *   - 输出截断到 4000 字，避免把上下文撑爆
 *   - 超时默认 30 秒，防止挂住的命令把整个回合卡死
 */
import { spawn } from 'node:child_process'

export const SHELL_DEFAULTS = {
  timeoutMs: 30_000,
  maxOutputChars: 4000,
}

export const SHELL_TOOL = {
  type: 'function',
  function: {
    name: 'run_shell',
    description: '在本机执行一条 PowerShell 命令并返回输出。'
      + '适合查系统状态、跑命令行工具；不可逆的操作（删除、安装、改配置）先跟用户说清楚再执行。'
      + '输出会截断到 4000 字。',
    parameters: {
      type: 'object',
      properties: {
        command: { type: 'string', description: '要执行的命令（PowerShell 语法）' },
        cwd: { type: 'string', description: '工作目录，默认用户目录' },
        timeout_ms: { type: 'number', description: '超时毫秒，默认 30000，最大 120000' },
      },
      required: ['command'],
      additionalProperties: false,
    },
  },
}

/** 危险命令：即使有确认卡片，也把这几类标出来，让用户看清楚要做什么 */
export function assessRisk(command) {
  const text = String(command ?? '')
  const patterns = [
    { pattern: /Remove-Item|del\s|rd\s|rmdir/iu, why: '删除文件' },
    { pattern: /Format-Volume|format\s+[a-z]:/iu, why: '格式化磁盘' },
    { pattern: /shutdown|Restart-Computer|Stop-Computer/iu, why: '关机/重启' },
    { pattern: /Set-ExecutionPolicy|New-LocalUser|net\s+user/iu, why: '修改系统安全设置' },
    { pattern: /Invoke-WebRequest|curl|wget|iwr/iu, why: '从网络下载内容' },
  ]
  return patterns.filter((item) => item.pattern.test(text)).map((item) => item.why)
}

export async function runShell(args = {}, options = {}) {
  const cfg = { ...SHELL_DEFAULTS, ...options }
  const command = String(args.command ?? '').trim()
  if (command === '') throw new Error('run_shell 需要 command')
  const timeout = Math.min(120_000, Math.max(1000, Number(args.timeout_ms ?? cfg.timeoutMs)))
  const cwd = typeof args.cwd === 'string' && args.cwd !== '' ? args.cwd : process.env.USERPROFILE ?? process.cwd()

  const started = Date.now()
  const result = await new Promise((resolve) => {
    const child = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], {
      cwd,
      windowsHide: true,
      env: { ...process.env, PYTHONIOENCODING: 'utf-8' },
    })
    let stdout = ''
    let stderr = ''
    let killed = false
    const timer = setTimeout(() => {
      killed = true
      try { child.kill() } catch { /* 已退出 */ }
    }, timeout)

    child.stdout.on('data', (chunk) => { stdout += chunk.toString('utf8') })
    child.stderr.on('data', (chunk) => { stderr += chunk.toString('utf8') })
    child.on('error', (error) => {
      clearTimeout(timer)
      resolve({ stdout, stderr: `${stderr}${error.message}`, code: -1, killed })
    })
    child.on('close', (code) => {
      clearTimeout(timer)
      resolve({ stdout, stderr, code: code ?? -1, killed })
    })
  })

  const elapsed = Date.now() - started
  const risks = assessRisk(command)
  const trim = (text) => (text.length > cfg.maxOutputChars ? `${text.slice(0, cfg.maxOutputChars)}…（截断，共 ${text.length} 字）` : text)

  const parts = [`命令：${command}`, `退出码：${result.code}${result.killed ? `（超时 ${timeout}ms 被杀）` : ''}，耗时 ${elapsed}ms`]
  if (risks.length > 0) parts.push(`风险：${risks.join('、')}`)
  if (result.stdout.trim() !== '') parts.push(`输出：\n${trim(result.stdout.trim())}`)
  if (result.stderr.trim() !== '') parts.push(`错误输出：\n${trim(result.stderr.trim())}`)
  if (result.stdout.trim() === '' && result.stderr.trim() === '') parts.push('（没有输出）')

  return {
    text: parts.join('\n'),
    data: { code: result.code, killed: result.killed, elapsedMs: elapsed, risks },
  }
}
