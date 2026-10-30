/**
 * 配置存储（R9）。
 *
 * 位置：`<项目>/.config/agent.json`（可用环境变量 CUA_VA_CONFIG_DIR 覆盖）
 * 规则：
 *  - 读出来给界面用时要打码密钥，绝不回显明文
 *  - 写入走临时文件 + rename，避免写一半崩了留下坏配置
 *  - 环境变量 CUA_VA_API_KEY 优先级高于文件（方便临时覆盖，不必落盘）
 */
import { readFile, writeFile, mkdir, rename } from 'node:fs/promises'
import { join } from 'node:path'

import { defaultConfig, getProvider, PROVIDERS } from './registry.mjs'
import { normalizeSystemPromptForStorage } from '../agent/system-prompt.mjs'

const CONFIG_DIR = process.env.CUA_VA_CONFIG_DIR ?? join(process.cwd(), '.config')
const CONFIG_PATH = join(CONFIG_DIR, 'agent.json')

/** 当前配置目录（可动态修改，供 --settings-test 隔离使用） */
let currentConfigDir = CONFIG_DIR
let currentConfigPath = CONFIG_PATH

/** 设置配置目录（供测试隔离使用）。必须在任何 loadConfig/saveConfig 调用前设置。 */
export function setConfigDir(dir) {
  currentConfigDir = dir
  currentConfigPath = join(dir, 'agent.json')
}

/** 获取当前配置路径（供日志/审计使用） */
export function getConfigPath() {
  return currentConfigPath
}

/** 读取原始配置（含明文密钥），供内部调用。落盘的 `enc:` 密文在这里解回明文。 */
export async function loadConfig() {
  try {
    const raw = await readFile(currentConfigPath, 'utf8')
    const parsed = JSON.parse(raw)
    const merged = { ...defaultConfig(), ...parsed }
    merged.apiKey = decryptField(merged.apiKey)
    if (merged.search !== undefined && merged.search !== null) {
      merged.search = { ...merged.search, apiKey: decryptField(merged.search.apiKey) }
    }
    return merged
  } catch {
    return defaultConfig()
  }
}

/** 读取给界面/日志用的配置：密钥打码。 */
export async function loadPublicConfig() {
  const config = await loadConfig()
  const searchKey = config.search?.apiKey ?? ''
  return {
    ...config,
    apiKey: maskKey(config.apiKey),
    hasApiKey: Boolean(config.apiKey || process.env.CUA_VA_API_KEY),
    search: { ...(config.search ?? {}), apiKey: maskKey(searchKey), hasApiKey: Boolean(searchKey || process.env.TAVILY_API_KEY) },
    // 网易云：appId 可以明示，私钥只报长度（1600 字符塞进界面毫无意义）
    modelProfiles: (config.modelProfiles ?? []).map((profile) => ({ ...profile, apiKey: maskKey(profile.apiKey), hasApiKey: Boolean(profile.apiKey) })),
  }
}

export function maskKey(key) {
  if (typeof key !== 'string' || key === '') return ''
  if (key.length <= 8) return '*'.repeat(key.length)
  return `${key.slice(0, 4)}${'*'.repeat(Math.min(12, key.length - 8))}${key.slice(-4)}`
}

/**
 * 密钥加密钩子。
 *
 * 为什么不在 store 里直接调 Electron 的 safeStorage：store 也被命令行工具用（没有 Electron）。
 * 由主进程在启动时注入加解密函数，store 只负责"写之前加密、读之后解密"。
 * 落盘格式：`enc:<base64>`。没有注入时就是明文（工具脚本、测试场景）。
 */
let cryptoHooks = null
export function setCryptoHooks(hooks) {
  cryptoHooks = hooks
}

/**
 * 判断一个字段值是不是"真的新密钥"。
 *
 * 界面回显的是掩码（`已保存（1624 字符）` / `sk-c***abcd`），
 * 如果把它当成新值写回去，密钥就被一行说明文字覆盖了——实测踩过一次。
 */
function isRealSecret(value) {
  if (typeof value !== 'string') return false
  const text = value.trim()
  if (text === '') return false
  if (text.includes('*')) return false
  if (text.startsWith('已保存') || text.includes('字符）')) return false
  if (text === '(未设置)' || text === '未配置') return false
  return true
}
const ENC_PREFIX = 'enc:'

function isEncrypted(value) {
  return typeof value === 'string' && value.startsWith(ENC_PREFIX)
}

function encryptField(value) {
  if (!cryptoHooks || typeof value !== 'string' || value === '' || isEncrypted(value)) return value
  try {
    return ENC_PREFIX + cryptoHooks.encrypt(value)
  } catch {
    return value
  }
}

function decryptField(value) {
  if (!cryptoHooks || !isEncrypted(value)) return value
  try {
    return cryptoHooks.decrypt(value.slice(ENC_PREFIX.length))
  } catch {
    // 解不开（换了机器/用户）就当空，让用户重填，不要把密文当密钥发出去
    return ''
  }
}

/** 更新配置。
 * 空字符串或掩码值（含 *）表示"不修改密钥"，避免界面把打码值写回去。
 */
export async function saveConfig(patch) {
  const current = await loadConfig()
  const next = { ...current, ...patch }

  if (typeof patch.apiKey === 'string') {
    if (patch.apiKey === '' || patch.apiKey.includes('*')) next.apiKey = current.apiKey
    else next.apiKey = patch.apiKey.trim()
  }
  // 显式清空：apiKey: null。空串表示"不修改"，null 表示"删掉"——两者语义不同。
  if (patch.apiKey === null) next.apiKey = ''

  // 搜索密钥同样遵守"空串=不修改、掩码=不修改、null=清除"
  if (patch.search !== undefined && patch.search !== null) {
    const currentSearch = current.search ?? { provider: 'tavily', apiKey: '' }
    const merged = { ...currentSearch, ...patch.search }
    const incoming = patch.search.apiKey
    if (incoming === null) merged.apiKey = ''
    else if (typeof incoming !== 'string' || incoming === '' || incoming.includes('*')) merged.apiKey = currentSearch.apiKey
    else merged.apiKey = incoming.trim()
    next.search = merged
  }

  if (typeof patch.providerId === 'string') {
    const provider = getProvider(patch.providerId)
    if (provider === undefined) throw new Error(`未知提供商: ${patch.providerId}`)
    // 切提供商且没显式给 baseURL → 用该提供商默认端点
    if (patch.baseURL === undefined) next.baseURL = provider.baseURL
    if (patch.model === undefined && provider.models.length > 0) next.model = provider.models[0]
  }

  if (typeof next.baseURL !== 'string' || !/^https?:\/\//u.test(next.baseURL)) {
    throw new Error('baseURL 必须是 http(s) 地址')
  }
  if (typeof next.model !== 'string' || next.model.trim() === '') {
    throw new Error('model 不能为空')
  }
  const steps = Number(next.maxSteps)
  next.maxSteps = Number.isFinite(steps) ? Math.min(20, Math.max(1, Math.round(steps))) : 8
  const temp = Number(next.temperature)
  next.temperature = Number.isFinite(temp) ? Math.min(2, Math.max(0, temp)) : 0.3

  /*
   * 系统提示词（人设）。语义和密钥**故意不同**，因为这里没有"回显掩码"的问题：
   *  - patch 里没有这个字段 → 保持原值（保存别的设置不会把用户的提示词冲掉）
   *  - null → 显式恢复内置默认提示词
   *  - 其它情况交给 normalizeSystemPromptForStorage：非空文本原样保存（换行、缩进、
   *    尾随空格都是用户写的内容），非法值抛错 → 整次保存失败、磁盘不动
   * 注意：空字符串在密钥字段里是"不修改"，在这里是"非法值"——传空串说明调用方没搞清
   * "恢复默认要传 null"，默默收下只会让用户以为保存成功了。
   */
  if (patch.systemPrompt === undefined) {
    /*
     * 不修改。这里必须把值显式放回去：`{...current, ...patch}` 在 patch 带
     * `systemPrompt: undefined`（IPC 结构化克隆会保留 undefined）时会把已存的
     * 自定义提示词冲成 undefined——落盘后字段消失，用户下次打开设置就"变回默认"了。
     */
    next.systemPrompt = current.systemPrompt ?? null
  } else if (patch.systemPrompt === null) {
    next.systemPrompt = null
  } else {
    next.systemPrompt = normalizeSystemPromptForStorage(patch.systemPrompt)
  }

  /*
   * 模型方案：每条方案的 apiKey 单独打码；掩码值（含 * 或「已保存」）不覆盖原值。
   */
  if (Array.isArray(patch.modelProfiles)) {
    const currentProfiles = Array.isArray(current.modelProfiles) ? current.modelProfiles : []
    next.modelProfiles = patch.modelProfiles.map((profile) => {
      const merged = {
        id: typeof profile?.id === 'string' && profile.id !== '' ? profile.id : `p${Math.random().toString(36).slice(2, 8)}`,
        name: String(profile?.name ?? '').trim(),
        providerId: String(profile?.providerId ?? '').trim(),
        baseURL: String(profile?.baseURL ?? '').trim(),
        model: String(profile?.model ?? '').trim(),
        apiKey: '',
      }
      const previous = currentProfiles.find((item) => item?.id === merged.id)
      if (isRealSecret(profile?.apiKey)) merged.apiKey = String(profile.apiKey).trim()
      else if (previous !== undefined) merged.apiKey = previous.apiKey ?? ''
      return merged
    })
  }
  // 落盘前加密（主进程注入了 safeStorage 才生效；工具脚本里就是明文）
  next.apiKey = encryptField(next.apiKey)
  if (next.search !== undefined && next.search !== null) {
    next.search = { ...next.search, apiKey: encryptField(next.search.apiKey) }
  }

  await mkdir(currentConfigDir, { recursive: true })
  const tmp = `${currentConfigPath}.tmp`
  await writeFile(tmp, JSON.stringify(next, null, 2), 'utf8')
  await rename(tmp, currentConfigPath)
  return loadPublicConfig()
}

/** 实际发请求时用的密钥：环境变量优先。 */
export async function resolveApiKey() {
  if (process.env.CUA_VA_API_KEY) return process.env.CUA_VA_API_KEY
  const config = await loadConfig()
  if (isEncrypted(config.apiKey)) {
    // 没有 safeStorage 钩子又读到密文：宁可报错，也不要把密文当密钥发出去
    throw new Error('配置里的 API Key 已加密（由应用写入），请在应用里重新填写，或设 CUA_VA_API_KEY 环境变量')
  }
  return config.apiKey
}

/** 搜索用的密钥：环境变量 TAVILY_API_KEY 优先于配置文件。 */
export async function resolveSearchKey() {
  if (process.env.TAVILY_API_KEY) return process.env.TAVILY_API_KEY
  const config = await loadConfig()
  if (isEncrypted(config.search?.apiKey)) {
    throw new Error('配置里的搜索 Key 已加密（由应用写入），请在应用里重新填写，或设 TAVILY_API_KEY 环境变量')
  }
  return config.search?.apiKey ?? ''
}

export function listProviders() {
  return PROVIDERS
}

export function getConfigFile() {
  return currentConfigPath
}

/** 兼容旧代码：动态获取当前配置文件路径 */
export const CONFIG_FILE = CONFIG_PATH
