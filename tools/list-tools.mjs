/**
 * 打印"现在助手真的能调用的工具"——从代码里现算，不靠手写清单。
 *
 * 手写的清单一定会过期：改了白名单、加了工具、没填搜索 key，都该反映出来。
 *
 * 用法：node tools/list-tools.mjs
 */
import { CuaToolkit, DEFAULT_TOOL_NAMES } from '../packages/cua/adapter.mjs'
import { WEB_SEARCH_TOOL } from '../packages/tools/web-search.mjs'
import { LAUNCH_APP_TOOL } from '../packages/tools/app-launch.mjs'
import { SHELL_TOOL } from '../packages/tools/shell.mjs'
import { FILES_TOOL } from '../packages/tools/files.mjs'
import { PROC_TOOL, KILL_TOOL } from '../packages/tools/procs.mjs'
import { SYSINFO_TOOL } from '../packages/tools/sysinfo.mjs'
import { CAPABILITIES, TOOL_CAPABILITY, resolveLevel } from '../packages/agent/permissions.mjs'
import { loadConfig } from '../packages/config/store.mjs'

const toolkit = new CuaToolkit({ frameDir: '.runtime/frames' })
const config = await loadConfig()
// 密钥在主进程里用 DPAPI 加密过，命令行（没有 safeStorage）读不出来。
// 这里只要知道"配没配"，不要去解密——解不开是设计如此，不是错误。
let searchState = '未配置（web_search 不出现）'
const rawSearchKey = config.search?.apiKey ?? ''
if (typeof rawSearchKey === 'string' && rawSearchKey.startsWith('enc:')) searchState = '已配置（DPAPI 加密，命令行不可读，应用内可用）'
else if (rawSearchKey !== '') searchState = '已配置'
let hasSearch = rawSearchKey !== ''

const rows = []
for (const definition of toolkit.listTools(DEFAULT_TOOL_NAMES)) {
  rows.push({ name: definition.function.name, description: definition.function.description, source: 'cua' })
}
rows.push({ name: LAUNCH_APP_TOOL.function.name, description: LAUNCH_APP_TOOL.function.description, source: 'local' })
rows.push({ name: SYSINFO_TOOL.function.name, description: SYSINFO_TOOL.function.description, source: 'local' })
rows.push({ name: FILES_TOOL.function.name, description: FILES_TOOL.function.description, source: 'local' })
rows.push({ name: PROC_TOOL.function.name, description: PROC_TOOL.function.description, source: 'local' })
rows.push({ name: KILL_TOOL.function.name, description: KILL_TOOL.function.description, source: 'local' })
rows.push({ name: SHELL_TOOL.function.name, description: SHELL_TOOL.function.description, source: 'local' })
if (hasSearch) {
  rows.push({ name: WEB_SEARCH_TOOL.function.name, description: WEB_SEARCH_TOOL.function.description, source: 'local' })
}

const LEVEL_TEXT = { allow: '允许', ask: '每次询问', deny: '拒绝', allow_once: '临时允许' }
function describe(capability) {
  const meta = CAPABILITIES[capability]
  if (meta === undefined) return { label: capability, level: 'allow' }
  const level = resolveLevel(capability, config.permissions ?? {})
  return { label: meta.label, level: LEVEL_TEXT[level] ?? level }
}

/** 带 action 的工具按动作展开权限：同一个 files，列表和删除差着量级 */
const ACTION_LEVELS = {
  files: '读/列=允许 · 写/移动=询问 · 删除=询问',
  app_control: '只读=允许',
  kill_app: '每次询问（免确认模式下直接关）',
}

const lines = []
lines.push(`模型：${config.model}   档位：${config.permissions?.mode ?? 'standard'}`)
lines.push(`搜索：${searchState}`)
lines.push('')
lines.push(`${'工具'.padEnd(18)}${'能力'.padEnd(14)}${'默认权限'.padEnd(10)}说明`)
lines.push('-'.repeat(96))
for (const row of rows) {
  const capability = TOOL_CAPABILITY[row.name] ?? 'unknown'
  const info = describe(capability)
  const level = ACTION_LEVELS[row.name] ?? info.level
  const desc = String(row.description ?? '').replace(/\s+/gu, ' ').slice(0, 42)
  lines.push(`${row.name.padEnd(18)}${info.label.padEnd(14)}${level.padEnd(10)}${desc}`)
}
lines.push('')
lines.push(`合计 ${rows.length} 个工具（CUA ${rows.filter((r) => r.source === 'cua').length} + 本地 ${rows.filter((r) => r.source === 'local').length}）`)

const text = lines.join('\n')
console.log(text)

// 同时写一份 Markdown，方便贴到文档/说明书里
const md = [
  '# 当前可调用的工具',
  '',
  `- 模型：\`${config.model}\``,
  `- 权限档位：\`${config.permissions?.mode ?? 'standard'}\``,
  `- 联网搜索：${searchState}`,
  '',
  '| 工具 | 能力 | 默认权限 | 说明 |',
  '|---|---|---|---|',
  ...rows.map((row) => {
    const capability = TOOL_CAPABILITY[row.name] ?? 'unknown'
    const info = describe(capability)
    const desc = String(row.description ?? '').replace(/\s+/gu, ' ').replace(/\|/gu, '\\|').slice(0, 60)
    return `| \`${row.name}\` | ${info.label} | ${info.level} | ${desc} |`
  }),
  '',
].join('\n')
const { writeFile } = await import('node:fs/promises')
await writeFile('docs/09-工具清单.md', md, 'utf8')
await toolkit.dispose()
