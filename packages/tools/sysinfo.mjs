/**
 * 系统信息（抄自 cherry-remote-app 的 `sys`，用 Node 自带能力 + 少量 PowerShell 实现）。
 *
 * 为什么不引入 psutil：Node 的 os 模块已经给 CPU/内存/开机时长，
 * 只剩磁盘与电池需要问 Windows。为了这点信息装一个 Python 依赖不值得。
 */
import { cpus, freemem, hostname, loadavg, platform, release, totalmem, uptime } from 'node:os'
import { spawnSync } from 'node:child_process'

export const SYSINFO_TOOL = {
  type: 'function',
  function: {
    name: 'system_info',
    description: '查看本机状态：CPU 占用、内存、磁盘剩余、电池、开机时长。'
      + '用户问"电脑卡不卡""硬盘还剩多少""电池还有多少"时用它。',
    parameters: {
      type: 'object',
      properties: {
        detail: { type: 'boolean', description: '为 true 时返回每个分区的明细' },
      },
      additionalProperties: false,
    },
  },
}

function gb(bytes) {
  return Math.round((bytes / 1073741824) * 10) / 10
}

/** 采样两次 CPU 使用率：os.cpus() 只给累计时间，单次算不出来 */
export function cpuUsage(sampleMs = 300) {
  const first = cpus().map((core) => ({ ...core.times }))
  const started = Date.now()
  // 同步忙等会卡主线程；这里用 Promise 让调用方 await
  return new Promise((resolve) => {
    setTimeout(() => {
      const second = cpus().map((core) => ({ ...core.times }))
      let idleDelta = 0
      let totalDelta = 0
      for (let i = 0; i < second.length; i += 1) {
        const before = first[i] ?? { idle: 0, user: 0, nice: 0, sys: 0, irq: 0 }
        const after = second[i]
        const idle = after.idle - before.idle
        const total = (after.user + after.nice + after.sys + after.irq + after.idle)
          - (before.user + before.nice + before.sys + before.irq + before.idle)
        idleDelta += idle
        totalDelta += total
      }
      const percent = totalDelta <= 0 ? 0 : Math.round((1 - idleDelta / totalDelta) * 100)
      resolve({ percent, elapsedMs: Date.now() - started })
    }, sampleMs)
  })
}

function diskInfo(detail) {
  const result = spawnSync('powershell.exe', ['-NoProfile', '-Command',
    '[Console]::OutputEncoding=[Text.Encoding]::UTF8;'
    + 'Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Used -ne $null } | '
    + 'Select-Object Name, Used, Free | ConvertTo-Json -Compress',
  ], { encoding: 'utf8', windowsHide: true, timeout: 12000 })
  let rows = []
  try {
    const parsed = JSON.parse((result.stdout ?? '').trim() || '[]')
    rows = (Array.isArray(parsed) ? parsed : [parsed]).filter((row) => row !== null && typeof row === 'object')
  } catch {
    rows = []
  }
  return rows.map((row) => ({
    drive: `${row.Name}:`,
    totalGb: gb((row.Used ?? 0) + (row.Free ?? 0)),
    usedGb: gb(row.Used ?? 0),
    freeGb: gb(row.Free ?? 0),
    detail,
  }))
}

function batteryInfo() {
  const result = spawnSync('powershell.exe', ['-NoProfile', '-Command',
    '$b = Get-CimInstance Win32_Battery | Select-Object -First 1;'
    + 'if ($b) { [pscustomobject]@{ percent = $b.EstimatedChargeRemaining; status = $b.BatteryStatus } | ConvertTo-Json -Compress } else { "" }',
  ], { encoding: 'utf8', windowsHide: true, timeout: 12000 })
  try {
    const parsed = JSON.parse((result.stdout ?? '').trim() || 'null')
    return parsed === null ? null : { percent: parsed.percent, charging: parsed.status === 2 }
  } catch {
    return null
  }
}

export async function runSystemInfo(args = {}) {
  const cpu = await cpuUsage()
  const memoryTotal = totalmem()
  const memoryFree = freemem()
  const memoryUsedPercent = Math.round((1 - memoryFree / memoryTotal) * 100)
  const disks = diskInfo(args.detail === true)
  const battery = batteryInfo()

  const lines = [
    `主机：${hostname()}（${platform()} ${release()}）`,
    `CPU：${cpu.percent}% 占用，${cpus().length} 个逻辑核`,
    `内存：${gb(memoryTotal - memoryFree)} / ${gb(memoryTotal)} GB（${memoryUsedPercent}% 已用，可用 ${gb(memoryFree)} GB）`,
  ]
  if (disks.length > 0) {
    lines.push(`磁盘：${disks.map((disk) => `${disk.drive} 剩 ${disk.freeGb}GB / 共 ${disk.totalGb}GB`).join('；')}`)
  }
  if (battery !== null) {
    lines.push(`电池：${battery.percent}%${battery.charging ? '（充电中）' : ''}`)
  }
  lines.push(`开机时长：${Math.floor(uptime() / 3600)} 小时 ${Math.floor((uptime() % 3600) / 60)} 分`)

  return {
    text: lines.join('\n'),
    data: {
      cpuPercent: cpu.percent,
      cores: cpus().length,
      memoryTotalGb: gb(memoryTotal),
      memoryFreeGb: gb(memoryFree),
      memoryUsedPercent,
      disks,
      battery,
      uptimeSec: Math.round(uptime()),
      loadavg: loadavg(),
    },
  }
}
