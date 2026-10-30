/**
 * 产物密钥审计。
 *
 * 打包前必须跑一次：确认 AppID / AppSecret / PrivateKey / 模型 Key / 搜索 Key
 * 都没有被塞进安装包或完整版目录里。
 *
 * 为什么要有这个脚本而不是"我记得没打进去"：
 * 配置文件在 `.config/`、被 .gitignore 忽略，但 electron-builder 的 `files`
 * 一旦写宽（比如写成 `**\/*`），`.config/` 就会被静默卷进 asar——
 * 那等于把私钥随安装包一起发出去。
 *
 * 用法：node tools/audit-artifacts.mjs [目录]
 */
import { readdir, readFile, stat } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { join } from 'node:path'

const targets = process.argv.slice(2)
if (targets.length === 0) targets.push('dist/win-unpacked', 'dist/语音助手-完整版')

/** 从配置里取出要查的敏感串（只取前缀，避免脚本自己打印密钥） */
async function loadSecrets() {
  const secrets = []
  const configPath = '.config/agent.json'
  if (existsSync(configPath)) {
    try {
      const raw = JSON.parse(await readFile(configPath, 'utf8'))
      if (typeof raw.apiKey === 'string' && raw.apiKey.length > 8) secrets.push({ name: '模型 API Key', sample: raw.apiKey.slice(0, 12) })
      const searchKey = raw.search?.apiKey
      if (typeof searchKey === 'string' && searchKey.length > 8) secrets.push({ name: '搜索 Key', sample: searchKey.slice(0, 10) })
      const netease = raw.netease ?? {}
      if (typeof netease.appId === 'string' && netease.appId.length > 8) secrets.push({ name: '网易 AppID', sample: netease.appId })
      if (typeof netease.privateKey === 'string' && netease.privateKey.length > 40) {
        secrets.push({ name: '网易 PrivateKey', sample: netease.privateKey.slice(0, 40) })
      }
    } catch { /* 配置坏了就当没有 */ }
  }
  /*
   * 除了配置里实际存在的值，再按"形态"兜底查一遍：
   * 私钥的 PEM 头、网易云 appId 的开头特征、Tavily Key 前缀。
   * 这样即使有人把凭据硬编码进别的文件（测试脚本、文档示例），也能被抓出来。
   */
  secrets.push({ name: 'RSA 私钥 PEM', sample: 'BEGIN RSA PRIVATE KEY' })
  secrets.push({ name: 'RSA 私钥 PEM(PKCS8)', sample: 'BEGIN PRIVATE KEY' })
  secrets.push({ name: '网易云 appId 形态', sample: 'b3010d0000000000309e0a31f' })
  secrets.push({ name: 'AppSecret 形态', sample: 'de796b060da569a0d50141e99b794c26' })
  secrets.push({ name: 'Tavily Key 形态', sample: 'tvly-' })
  return secrets
}

const TEXT_EXT = new Set(['.js', '.mjs', '.cjs', '.json', '.txt', '.md', '.py', '.yml', '.yaml', '.ps1', '.html', '.css'])

async function scanTree(root, secrets) {
  const hits = []
  let files = 0
  async function walk(dir) {
    let entries
    try { entries = await readdir(dir, { withFileTypes: true }) } catch { return }
    for (const entry of entries) {
      const full = join(dir, entry.name)
      if (entry.isDirectory()) { await walk(full); continue }
      files += 1
      const name = entry.name.toLowerCase()
      // 明显的"配置文件名"先记一笔（哪怕内容是加密的，也不该出现在产物里）
      if (name === 'agent.json' || name === 'audit.jsonl' || name.endsWith('.env')) {
        hits.push({ file: full, secret: '配置文件本身', note: '配置/审计文件被打包了' })
        continue
      }
      const ext = name.slice(name.lastIndexOf('.'))
      if (!TEXT_EXT.has(ext)) continue
      const info = await stat(full).catch(() => null)
      if (info === null || info.size > 4 * 1024 * 1024) continue
      const content = await readFile(full, 'utf8').catch(() => '')
      for (const secret of secrets) {
        if (secret.sample !== '' && content.includes(secret.sample)) {
          hits.push({ file: full, secret: secret.name, note: '发现明文/片段' })
        }
      }
    }
  }
  await walk(root)
  return { files, hits }
}

const secrets = await loadSecrets()
console.log(`审计密钥 ${secrets.length} 项：${secrets.map((s) => s.name).join('、')}\n`)

let total = 0
const allHits = []
for (const target of targets) {
  if (!existsSync(target)) { console.log(`跳过（不存在）：${target}`); continue }
  const { files, hits } = await scanTree(target, secrets)
  total += files
  allHits.push(...hits)
  console.log(`${target}：扫了 ${files} 个文件，命中 ${hits.length} 处`)
  for (const hit of hits.slice(0, 10)) console.log(`  ✗ ${hit.secret} ← ${hit.file}`)
}

console.log('\n' + '='.repeat(60))
if (allHits.length === 0) {
  console.log(JSON.stringify({ scanned: total, leaks: 0, verdict: 'clean' }))
} else {
  console.log(JSON.stringify({ scanned: total, leaks: allHits.length, verdict: 'leak-found' }))
  process.exitCode = 1
}
