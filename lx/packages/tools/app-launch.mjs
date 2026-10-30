/**
 * 启动已安装应用（按名字）。
 *
 * 为什么需要它：CUA 的 `open_application` 只接受**真实路径**（Start-Process 直接跑），
 * 而"打开无畏契约"需要的是一次 Start 菜单级的名字解析。
 * 之前模型只能自己摸索：Win 键 → 打字 → 截图 → 点结果，六七个动作还经常在搜索结果出来前就用完步数。
 *
 * 索引来自两处，缺一不可：
 *   - `Get-StartApps`：UWP/商店应用 + 一部分桌面应用（AppID 可能是 shell:AppsFolder 形式）
 *   - 两个 Start Menu 目录里的 .lnk：桌面应用的中文显示名往往只在这里
 */
import { spawn } from 'node:child_process'
import { readdir } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { join } from 'node:path'

export const LAUNCH_APP_DEFAULTS = {
  /** 索引多久重建一次（装了新软件不用重启应用） */
  indexTtlMs: 10 * 60 * 1000,
  /** 匹配不上时最多返回几个候选 */
  maxCandidates: 5,
  timeoutMs: 8000,
}

export const LAUNCH_APP_TOOL = {
  type: 'function',
  function: {
    name: 'launch_app',
    description: '按名字启动已安装的程序（支持中文名，如"无畏契约""微信"）。'
      + '打开软件优先用它，不要猜 exe 路径、也不要靠 Win 键搜索一步步点。'
      + '返回 matched（实际启动的名字）；没匹配上会返回 candidates，可以从里面挑一个再调。',
    parameters: {
      type: 'object',
      properties: {
        query: { type: 'string', description: '程序名，中文或英文都行，比如"无畏契约"或"VALORANT"' },
      },
      required: ['query'],
      additionalProperties: false,
    },
  },
}

/** 去掉空格、标点、大小写差异，便于"无畏契约"/"VALORANT"/"valorant"互相命中 */
function normalize(text) {
  return String(text ?? '')
    .toLowerCase()
    // "帮我打开无畏契约" → "无畏契约"：模型经常把动词一起塞进 query
    .replace(/^(请|帮我|帮忙|麻烦|给我)?(打开|启动|运行|开启|开一下|开个|开)/u, '')
    .replace(/[\s\-_·・.,，。:：()（）[\]【】]/gu, '')
}

/** 编辑距离（只用来兜住同音错字：无谓契约 / 无畏契约） */
function editDistance(a, b) {
  if (a === b) return 0
  const rows = a.length + 1
  const cols = b.length + 1
  let prev = Array.from({ length: cols }, (_, i) => i)
  for (let i = 1; i < rows; i += 1) {
    const cur = [i]
    for (let j = 1; j < cols; j += 1) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1
      cur[j] = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost)
    }
    prev = cur
  }
  return prev[cols - 1]
}

/** 打分：精确 > 包含 > 近似（同音/错字），0 表示不匹配 */
export function scoreMatch(name, query) {
  const n = normalize(name)
  const q = normalize(query)
  if (n === '' || q === '') return 0
  if (n === q) return 100
  if (n.startsWith(q)) return 85
  if (n.includes(q)) return 70
  if (q.includes(n) && n.length >= 2) return 55
  // 英文缩写：VLR 之类
  if (/^[a-z]+$/u.test(q) && n.split(/[^a-z]+/u).some((word) => word.startsWith(q))) return 45
  // 近似匹配：ASR 把「无畏契约」听成「无谓契约」时，编辑距离只有 1
  if (q.length >= 2 && n.length >= 2) {
    const distance = editDistance(n, q)
    const similarity = 1 - distance / Math.max(n.length, q.length)
    if (similarity >= 0.6) return Math.round(60 * similarity)
  }
  return 0
}

/**
 * 从 Start 菜单和 Get-StartApps 建索引。
 * 只在第一次用到时跑（约 1~2 秒），之后按 TTL 复用。
 */
export class AppLauncher {
  #options
  #index = null
  #builtAt = 0
  #building = null

  constructor(options = {}) {
    this.#options = { ...LAUNCH_APP_DEFAULTS, ...options }
  }

  get ready() { return this.#index !== null }

  async ensureIndex(force = false) {
    if (!force && this.#index !== null && Date.now() - this.#builtAt < this.#options.indexTtlMs) return this.#index
    if (this.#building !== null) return this.#building
    this.#building = (async () => {
      const entries = []
      const seen = new Set()
      const push = (name, kind, target, alias) => {
        const clean = String(name ?? '').trim()
        const key = `${kind}:${target}`
        if (clean === '' || target === undefined || seen.has(key)) return
        seen.add(key)
        entries.push({ name: clean, kind, target, alias: alias ?? '' })
      }

      // 一次 PowerShell 同时拿两样东西：
      //   links     —— Start 菜单 .lnk（中文显示名在这里）以及它指向的 exe（英文名在这里）
      //   startApps —— UWP/商店应用的 AppID
      const script = [
        '[Console]::OutputEncoding=[Text.Encoding]::UTF8;',
        '$sh = New-Object -ComObject WScript.Shell;',
        '$roots = @($env:ProgramData + "\\Microsoft\\Windows\\Start Menu\\Programs",',
        '  $env:APPDATA + "\\Microsoft\\Windows\\Start Menu\\Programs") | Where-Object { Test-Path $_ };',
        '$links = Get-ChildItem $roots -Recurse -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object {',
        '  $target = "";',
        '  try { $target = $sh.CreateShortcut($_.FullName).TargetPath } catch { };',
        '  [pscustomobject]@{ name = $_.BaseName; path = $_.FullName; target = $target }',
        '};',
        '$apps = Get-StartApps | ForEach-Object { [pscustomobject]@{ name = $_.Name; appId = $_.AppID } };',
        '[pscustomobject]@{ links = @($links); startApps = @($apps) } | ConvertTo-Json -Compress -Depth 4',
      ].join(' ')

      let payload = null
      try {
        const json = await this.#runPowerShell(script)
        payload = JSON.parse(json.replace(/^\uFEFF/u, '').trim())
      } catch {
        // PowerShell 挂了就退回纯 Node 扫描（没有别名，但能启动）
        payload = null
      }

      if (payload !== null) {
        const links = Array.isArray(payload.links) ? payload.links : [payload.links]
        for (const link of links ?? []) {
          if (link === null || typeof link !== 'object') continue
          const alias = String(link.target ?? '').split('\\').pop().replace(/\.exe$/iu, '')
          push(link.name, 'lnk', link.path, alias)
        }
        const apps = Array.isArray(payload.startApps) ? payload.startApps : [payload.startApps]
        for (const app of apps ?? []) {
          if (app === null || typeof app !== 'object') continue
          push(app.name, 'uwp', app.appId)
        }
      } else {
        const roots = [
          join(process.env.ProgramData ?? 'C:\\ProgramData', 'Microsoft', 'Windows', 'Start Menu', 'Programs'),
          join(process.env.APPDATA ?? '', 'Microsoft', 'Windows', 'Start Menu', 'Programs'),
        ].filter((p) => p !== '' && existsSync(p))
        for (const root of roots) {
          for (const file of await walk(root, 3)) {
            if (!file.toLowerCase().endsWith('.lnk')) continue
            push(file.split('\\').pop().replace(/\.lnk$/iu, ''), 'lnk', file)
          }
        }
      }

      this.#index = entries
      this.#builtAt = Date.now()
      return entries
    })()
    try {
      return await this.#building
    } finally {
      this.#building = null
    }
  }

  /** 只查不启动（模型用它挑候选） */
  async find(query, limit = this.#options.maxCandidates) {
    const entries = await this.ensureIndex()
    return entries
      .map((entry) => {
        // 名字和 exe 名都算：中文名在 .lnk，英文名往往只在 exe 上
        const score = Math.max(scoreMatch(entry.name, query), scoreMatch(entry.alias ?? '', query))
        return { ...entry, score }
      })
      .filter((entry) => entry.score > 0)
      .sort((a, b) => b.score - a.score || a.name.length - b.name.length)
      .slice(0, limit)
  }

  /**
   * 查并启动。
   * @returns {Promise<{text:string, meta:object, data:object}>}
   */
  async launch(query) {
    const started = Date.now()
    const candidates = await this.find(query)
    if (candidates.length === 0) {
      const entries = await this.ensureIndex()
      return {
        text: `没有找到叫「${query}」的程序（已索引 ${entries.length} 个）。`
          + '\n两种办法：换个更接近的名字再调一次；或者用 key 按 Win 键搜索，'
          + '但注意搜索只是开始，还要在结果里点一下才算打开。',
        meta: { ms: Date.now() - started, matched: null, count: 0 },
        data: { found: false },
      }
    }

    const best = candidates[0]
    try {
      await this.#launchEntry(best)
    } catch (error) {
      return {
        text: `找到「${best.name}」但启动失败：${error.message}`,
        meta: { ms: Date.now() - started, matched: best.name, count: candidates.length },
        data: { found: true, launched: false, candidates },
      }
    }

    const others = candidates.slice(1).map((c) => c.name).join('、')
    return {
      text: `已启动「${best.name}」`
        + (others === '' ? '' : `（相近的还有：${others}）`)
        + '\n启动后建议截图确认窗口真的起来了。',
      meta: { ms: Date.now() - started, matched: best.name, kind: best.kind, count: candidates.length },
      data: { found: true, launched: true, matched: best.name, candidates },
    }
  }

  async #launchEntry(entry) {
    // UWP 用 shell:AppsFolder 启动；.lnk 直接 Start-Process
    const command = entry.kind === 'uwp'
      ? `Start-Process explorer.exe -ArgumentList 'shell:AppsFolder\\${entry.target.replace(/'/gu, "''")}'`
      : `Start-Process -FilePath '${entry.target.replace(/'/gu, "''")}'`
    await this.#runPowerShell(command)
  }

  #runPowerShell(command) {
    return new Promise((resolve, reject) => {
      const child = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], {
        windowsHide: true,
      })
      let stdout = ''
      let stderr = ''
      const timer = setTimeout(() => {
        try { child.kill() } catch { /* 已退出 */ }
        reject(new Error(`PowerShell 超时（${this.#options.timeoutMs}ms）`))
      }, this.#options.timeoutMs)

      child.stdout.on('data', (chunk) => { stdout += chunk })
      child.stderr.on('data', (chunk) => { stderr += chunk })
      child.on('error', (error) => { clearTimeout(timer); reject(error) })
      child.on('close', (code) => {
        clearTimeout(timer)
        if (code === 0) resolve(stdout)
        else reject(new Error(stderr.trim() || `PowerShell 退出码 ${code}`))
      })
    })
  }
}

/** 递归列文件（限制深度，Start 菜单里不会有很深的目录） */
async function walk(dir, depth) {
  if (depth <= 0) return []
  let entries
  try {
    entries = await readdir(dir, { withFileTypes: true })
  } catch {
    return []
  }
  const out = []
  for (const entry of entries) {
    const full = join(dir, entry.name)
    if (entry.isDirectory()) out.push(...await walk(full, depth - 1))
    else out.push(full)
  }
  return out
}
