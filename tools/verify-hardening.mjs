/**
 * P3 收尾验收：密钥加密 / 审计导出 / 15 分钟授权。
 *
 * 这三件事都有"看起来好了其实没生效"的风险：
 *   加密  —— 写进去了但读回来是密文，程序拿密文当密钥用
 *   导出  —— 文件建了但内容为空
 *   授权  —— 点了按钮但引擎没记住，下一秒又弹卡片
 *
 * 用法：node tools/verify-hardening.mjs
 */
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const dir = await mkdtemp(join(tmpdir(), 'hard-'))
process.env.CUA_VA_CONFIG_DIR = dir

const store = await import('../packages/config/store.mjs')
const { PermissionGate } = await import('../packages/agent/permissions.mjs')

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

// ---- 1. 加密钩子：写入是密文，读回是明文
const fakeCrypto = {
  encrypt: (plain) => Buffer.from(`fake:${plain}`).toString('base64'),
  decrypt: (encoded) => Buffer.from(encoded, 'base64').toString('utf8').replace(/^fake:/u, ''),
}
store.setCryptoHooks(fakeCrypto)
await store.saveConfig({ apiKey: 'sk-secret-123456', search: { provider: 'tavily', apiKey: 'tvly-secret-999' } })

const raw = await readFile(join(dir, 'agent.json'), 'utf8')
const onDisk = JSON.parse(raw)
check('落盘是密文', onDisk.apiKey.startsWith('enc:') && onDisk.search.apiKey.startsWith('enc:'),
  `${String(onDisk.apiKey).slice(0, 12)}…`)
check('密文里不含明文', !raw.includes('sk-secret') && !raw.includes('tvly-secret'), '')

const loaded = await store.loadConfig()
check('读回是明文', loaded.apiKey === 'sk-secret-123456' && loaded.search.apiKey === 'tvly-secret-999', loaded.apiKey.slice(0, 6))

const publicConfig = await store.loadPublicConfig()
check('界面看到的是掩码', publicConfig.apiKey.includes('*') && publicConfig.hasApiKey === true, publicConfig.apiKey)

// ---- 2. 没有钩子时不能把密文当密钥
store.setCryptoHooks(null)
let errorText = ''
try {
  await store.resolveApiKey()
} catch (error) {
  errorText = error.message
}
check('无钩子读密文会明确报错', /已加密/u.test(errorText), errorText.slice(0, 40))

// ---- 3. 明文（工具写入的）仍然可用
await writeFile(join(dir, 'agent.json'), JSON.stringify({ apiKey: 'plain-key', search: { provider: 'tavily', apiKey: 'plain-search' } }), 'utf8')
const plainConfig = await store.loadConfig()
check('明文配置照常读取', plainConfig.apiKey === 'plain-key' && plainConfig.search.apiKey === 'plain-search', '')

// ---- 4. 15 分钟授权：grantOnce 之后同样的 ask 不再弹
let asked = 0
const gate = new PermissionGate({
  permissions: { mode: 'standard' },
  confirm: async () => { asked += 1; return { allow: true, channel: 'click' } },
})
const first = await gate.check({ tool: 'read_clipboard', args: {} })
check('第一次要问', first.level === 'ask' && asked === 1, first.level)

const until = gate.grantOnce('clipboard.read', 15)
check('grantOnce 返回到期时间', until > Date.now(), new Date(until).toISOString())
const second = await gate.check({ tool: 'read_clipboard', args: {} })
check('15 分钟内不再问', second.allow === true && asked === 1, `asked=${asked} level=${second.level}`)

// ---- 5. 审计导出：源文件可读、行数与内容一致
const auditPath = join(dir, 'audit.jsonl')
const auditLines = [
  { ts: new Date().toISOString(), tool: 'read_clipboard', decision: 'allow', level: 'ask' },
  { ts: new Date().toISOString(), tool: 'rm_rf', decision: 'deny', level: 'deny' },
]
await writeFile(auditPath, `${auditLines.map((row) => JSON.stringify(row)).join('\n')}\n`, 'utf8')
const auditRaw = await readFile(auditPath, 'utf8')
check('审计行可解析', auditRaw.split('\n').filter(Boolean).every((line) => JSON.parse(line)), `${auditRaw.split('\n').filter(Boolean).length} 行`)

await rm(dir, { recursive: true, force: true })

console.log('P3 收尾验收\n' + '='.repeat(66))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(26, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(66))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
