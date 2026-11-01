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

import { defaultConfig, getProvider, PROVIDERS, defaultModelFor } from './registry.mjs'
import { normalizeSystemPromptForStorage } from '../agent/system-prompt.mjs'
import { buildProfileFor, buildRequestFields, normalizeEffortLevel, normalizeThinking } from '../agent/ll-params.mjs'

const CONFIG_DIR = process.env.CUA_VA_CONFIG_DIR ?? join(process.cwd(), '.config')
const CONFIG_PATH = join(CONFIG_DIR, 'agent.json')

/**
 * P2: 配置路径一致性——不再使用模块级可变变量。
 * store 在 import 时读取 CUA_VA_CONFIG_DIR，之后不再改变。
 * 这保证一次保存操作（loadConfig → 校验 → 临时写入 → rename）始终操作同一目录。
 * --settings-test 在启动最早期设置环境变量，所有后续操作都走临时目录。
 */

/** 读取原始配置（含明文密钥），供内部调用。落盘的 `enc:` 密文在这里解回明文。 */
export async function loadConfig() {
  try {
    const raw = await readFile(CONFIG_PATH, 'utf8')
    const parsed = JSON.parse(raw)
    const merged = { ...defaultConfig(), ...parsed }
    merged.apiKey = decryptField(merged.apiKey)
    // 每个提供商自己的 Key 也逐个解回（缺这个就会把密文当 Key 发出去）
    merged.providerKeys = decryptKeyMap(merged.providerKeys)
    // 认证目标 Key（providerId|baseURL）也解回
    merged.providerTargets = decryptKeyMap(merged.providerTargets)
    // 模型方案的 Key 同样解回：可能来自旧明文配置，也可能是 safeStorage 加密后的 enc: 值
    if (Array.isArray(merged.modelProfiles)) {
      merged.modelProfiles = merged.modelProfiles.map((profile) => ({
        ...profile,
        apiKey: decryptField(profile?.apiKey ?? ''),
      }))
    }
    if (merged.search !== undefined && merged.search !== null) {
      merged.search = { ...merged.search, apiKey: decryptField(merged.search.apiKey) }
    }
    /*
     * 旧配置兼容（PR#4 之前只有 `apiKey` 一个槽 / `providerKeys[providerId]`）：
     * 把「当时选中提供商 + 当时 baseURL」的 Key 迁移到 providerTargets[providerId|baseURL]。
     *
     * 这是**一次性迁移**：只发生在读入时、目标键不存在时。之后用户修改 baseURL，
     * 新目标没有键就是没有 Key，绝不自动把旧 Key 绑到新地址（禁止跨目标回退路径）。
     */
    if (typeof merged.providerKeys !== 'object' || merged.providerKeys === null) merged.providerKeys = {}
    if (typeof merged.providerTargets !== 'object' || merged.providerTargets === null) merged.providerTargets = {}
    const migrationTarget = targetKey(merged.providerId, merged.baseURL)
    // 旧配置的 apiKey 主槽就是“当时选中目标”的 Key；providerKeys[providerId] 同理。
    // 只要当前目标键缺失，且 apiKey / providerKeys[providerId] 至少有一个明文可迁移，
    // 就把它绑定到**迁移时的认证目标**（之后改地址不会跟随）。
    const legacyValue = merged.providerKeys[merged.providerId]
    const sourceValue = (typeof merged.apiKey === 'string' && merged.apiKey !== '' && !isEncrypted(merged.apiKey))
      ? merged.apiKey
      : (typeof legacyValue === 'string' && legacyValue !== '' ? legacyValue : '')
    if (sourceValue !== '' && merged.providerTargets[migrationTarget] === undefined) {
      merged.providerTargets = { ...merged.providerTargets, [migrationTarget]: sourceValue }
    }
    // 真正生效的 apiKey 按「认证目标」推导：providerTargets[currentTarget]（无其它回退路径）
    const currentTarget = targetKey(merged.providerId, merged.baseURL)
    const targets = effectiveTargetKeys(merged)
    merged.apiKey = typeof targets[currentTarget] === 'string' ? targets[currentTarget] : ''
    // 每次读都重算生效摘要：档位是纯函数，磁盘上那份只是给人看的，不能当真值源
    merged.effectiveReasoning = reasoningSummary(merged)
    return merged
  } catch {
    const fresh = defaultConfig()
    fresh.providerKeys = {}
    fresh.providerTargets = {}
    fresh.effectiveReasoning = reasoningSummary(fresh)
    return fresh
  }
}

/** 读取给界面/日志用的配置：密钥打码，任何情况都不暴露明文。 */
export async function loadPublicConfig() {
  const config = await loadConfig()
  const searchKey = config.search?.apiKey ?? ''
  const targets = effectiveTargetKeys(config)
  /*
   * 用显式白名单构造返回对象，禁止 `...config` 全量展开：
   * `config` 里的 providerTargets / providerKeys / modelProfiles / search.apiKey
   * 都是解密后的明文，绝不能进入公开配置（会被 settings:load 直接送回渲染层）。
   */
  return {
    schemaVersion: config.schemaVersion,
    providerId: config.providerId,
    baseURL: config.baseURL,
    model: config.model,
    // apiKey 只报掩码 + 有没有；不包含明文
    apiKey: maskKey(config.apiKey),
    hasApiKey: Boolean(config.apiKey || process.env.CUA_VA_API_KEY),
    // 每个提供的 Key 只报掩码 + 有没有，界面据此决定"切回去要不要重新填"
    providerKeys: Object.fromEntries(Object.entries(config.providerKeys ?? {}).map(([id, key]) => [id, maskKey(key)])),
    hasProviderKey: Object.fromEntries(Object.entries(config.providerKeys ?? {}).map(([id, key]) => [id, Boolean(key)])),
    // 认证目标维度：纯数据 map（掩码），供界面按 providerId+baseURL 判断是否有可用凭据
    targetKeys: Object.fromEntries(Object.entries(targets).map(([t, key]) => [t, maskKey(key)])),
    hasTargetKey: Object.fromEntries(Object.entries(targets).map(([t, key]) => [t, Boolean(key)])),
    temperature: config.temperature,
    mimoThinking: config.mimoThinking,
    deepseekReasoningEffort: config.deepseekReasoningEffort,
    providerHint: config.providerHint,
    maxSteps: config.maxSteps,
    timeoutMs: config.timeoutMs,
    systemPrompt: config.systemPrompt ?? null,
    effectiveReasoning: config.effectiveReasoning ?? null,
    ui: { ...(config.ui ?? {}) },
    tts: { ...(config.tts ?? {}) },
    permissions: { ...(config.permissions ?? {}) },
    resources: { ...(config.resources ?? {}) },
    followup: JSON.parse(JSON.stringify(config.followup ?? {})),
    // 模型方案 Key 打码
    modelProfiles: (config.modelProfiles ?? []).map((profile) => ({ ...profile, apiKey: maskKey(profile.apiKey), hasApiKey: Boolean(profile.apiKey) })),
    modelSchedule: JSON.parse(JSON.stringify(config.modelSchedule ?? { enabled: false, rules: [] })),
    // 搜索 Key 打码
    search: { ...(config.search ?? {}), apiKey: maskKey(searchKey), hasApiKey: Boolean(searchKey || process.env.TAVILY_API_KEY) },
    // 网易云：appId 可以明示，私钥只报长度（1600 字符塞进界面毫无意义）
    netease: {
      appId: config.netease?.appId ?? '',
      privateKey: config.netease?.privateKey === undefined || config.netease.privateKey === ''
        ? ''
        : `已保存（${String(config.netease.privateKey).length} 字符）`,
      hasPrivateKey: Boolean(config.netease?.privateKey),
      // 界面要显示「已保存（N 字符）」；长度必须由这里给，掩码串自己的长度没有意义
      privateKeyLength: String(config.netease?.privateKey ?? '').length,
    },
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

/**
 * 认证目标键：`providerId|baseURL`。
 *
 * 凭据按「认证目标」隔离：同一 providerId 但不同 baseURL（例如官方 MiMo 与
 * 自研代理、不同账号入口）是不同认证目标，绝不能自动把 A 目标的 Key 发到 B 目标。
 */
export function targetKey(providerId, baseURL) {
  return `${String(providerId ?? '').trim()}|${String(baseURL ?? '').trim()}`
}

/**
 * 取「认证目标 → Key」映射（明文）。
 *
 * 只读 `providerTargets`（键 `providerId|baseURL`）。旧 `providerKeys[providerId]`
 * 已在 loadConfig 迁移阶段绑定到迁移时的认证目标，这里**不做任何 legacy 回退**——
 * 否则用户改地址后旧 Key 会被再次绑到新地址（跨目标泄漏路径）。
 */
function effectiveTargetKeys(config) {
  const targets = config?.providerTargets && typeof config.providerTargets === 'object' && !Array.isArray(config.providerTargets)
    ? config.providerTargets
    : {}
  const out = {}
  for (const [rawKey, value] of Object.entries(targets)) {
    if (typeof value === 'string' && value !== '') out[rawKey] = value
  }
  return out
}

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

/** 逐条解密「提供商 → Key」映射（整块加解密会丢单条为空的情况） */
function decryptKeyMap(map) {
  if (typeof map !== 'object' || map === null || Array.isArray(map)) return {}
  const out = {}
  for (const [id, value] of Object.entries(map)) {
    if (typeof value === 'string') out[id] = decryptField(value)
  }
  return out
}

function encryptKeyMap(map) {
  if (typeof map !== 'object' || map === null || Array.isArray(map)) return {}
  const out = {}
  for (const [id, value] of Object.entries(map)) {
    if (typeof value === 'string') out[id] = encryptField(value)
  }
  return out
}

/** 更新配置。
 * 空字符串或掩码值（含 *）表示"不修改密钥"，避免界面把打码值写回去。
 */
let saveQueue = Promise.resolve()
export function saveConfig(patch, options = {}) {
  const pending = saveQueue.then(() => saveConfigOnce(patch, options))
  saveQueue = pending.catch(() => {})
  return pending
}
async function saveConfigOnce(patch, { signal, expectedCredentials } = {}) {
  signal?.throwIfAborted()
  const current = await loadConfig()
  signal?.throwIfAborted()
  if (expectedCredentials && (current.netease?.appId !== expectedCredentials.appId
      || current.netease?.privateKey !== expectedCredentials.privateKey)) {
    throw Object.assign(new Error('音乐配置版本已变化'), { code: 'STALE_CREDENTIALS' })
  }
  const next = { ...current, ...patch }

  /*
   * 提供商先定下来（并校验），后面「这条 Key 属于哪一家」「生效的是哪一份 Key」
   * 都只用这一个结果。两边各算各的会出现：Key 存进 `' mimo '` 的槽位，
   * 而 providerId 是 `'mimo'`，查不到就等于把用户的 Key 弄丢了。
   *
   * 切提供商时：地址跟着默认端点走，模型换成该提供商的推荐初始值；
   * 用户显式给了 baseURL / model 就以用户给的为准（在线模型列表刷新后就是这么提交的）。
   * 这里**不改**推理档位：两个档位在磁盘上各自存着，切走再切回来还是用户原来选的。
   */
  if (typeof patch.providerId === 'string' && patch.providerId.trim() !== '') {
    const provider = getProvider(patch.providerId.trim())
    if (provider === undefined) throw new Error(`未知提供商: ${patch.providerId}`)
    next.providerId = provider.id
    if (patch.baseURL === undefined) next.baseURL = provider.baseURL
    if (patch.model === undefined) {
      const suggested = defaultModelFor(provider)
      if (suggested !== '') next.model = suggested
    }
  } else {
    /*
     * providerId 为空/缺省：保持原值。不能让它变成 ''——
     * 那样下面推导 Key 时会查到一个不存在的槽位，把用户已存的 Key"清没"，
     * 界面却还显示"已保存"，实际请求全是 401。
     */
    next.providerId = current.providerId
  }
  const targetProviderId = next.providerId

  /**
   * 凭据按「认证目标」（providerId|baseURL）隔离：
   *  - `providerTargets` 是真正生效的凭据表（键 = targetKey(providerId, baseURL)）；
   *  - `providerKeys` 保留为向后兼容的 providerId 键（旧界面/旧测试依赖），
   *    保存时与 providerTargets 同步维护，读取时 priority 为 providerTargets。
   *
   * 语义和其它密钥字段一致：空串/掩码=**不修改**、null=清除、真值=替换。
   */
  const providerTargets = { ...(current.providerTargets ?? {}) }
  const providerKeys = { ...(current.providerKeys ?? {}) }
  // 旧 providerKeys patch 的兼容：逐 id 映射到「该 id 当前默认 baseURL」的目标键
  const incomingKeys = patch.providerKeys && typeof patch.providerKeys === 'object' && !Array.isArray(patch.providerKeys)
    ? patch.providerKeys
    : null
  if (incomingKeys !== null) {
    for (const [rawId, value] of Object.entries(incomingKeys)) {
      const id = typeof rawId === 'string' ? rawId.trim() : ''
      if (id === '') continue
      const provider = getProvider(id)
      const baseURL = provider?.baseURL ?? next.baseURL ?? ''
      const t = targetKey(id, baseURL)
      if (value === null) {
        delete providerTargets[t]
        delete providerKeys[id]
      } else if (isRealSecret(value)) {
        const secret = String(value).trim()
        providerTargets[t] = secret
        providerKeys[id] = secret
      }
      // 空串/掩码：不修改
    }
  }
  // 当前认证目标
  const currentTarget = targetKey(targetProviderId, next.baseURL)
  if (typeof patch.apiKey === 'string') {
    if (patch.apiKey === '' || patch.apiKey.includes('*')) {
      /*
       * 不修改。界面回显的是掩码，把它当新值写回去就等于把 Key 抹了（实测踩过）。
       * 只有"还在同一个认证目标"时，才用 global apiKey 补齐该目标（老配置只有一份 Key）；
       * **切换到别的目标时绝不能这么补**——那等于把旧目标的 Key 塞进新目标的槽位。
       */
      if (targetProviderId === current.providerId
        && String(next.baseURL) === String(current.baseURL)
        && providerTargets[currentTarget] === undefined
        && isRealSecret(current.apiKey)) {
        providerTargets[currentTarget] = current.apiKey
        if (providerKeys[targetProviderId] === undefined) providerKeys[targetProviderId] = current.apiKey
      }
    } else {
      const secret = patch.apiKey.trim()
      providerTargets[currentTarget] = secret
      providerKeys[targetProviderId] = secret
    }
  }
  // 显式清空：apiKey: null。空串表示"不修改"，null 表示"删掉"——两者语义不同。
  if (patch.apiKey === null) {
    delete providerTargets[currentTarget]
    // 仅当该 providerId 没有其它目标凭据时才删兼容槽（避免误删同 provider 别处 Key）
    const hasOtherTarget = Object.keys(providerTargets).some((t) =>
      t.startsWith(`${targetProviderId}|`) && providerTargets[t] !== undefined)
    if (!hasOtherTarget) delete providerKeys[targetProviderId]
  }
  next.providerTargets = providerTargets
  next.providerKeys = providerKeys

  /*
   * 真正生效的 apiKey **按认证目标推导**，只有当前目标的 providerTargets 有键才会用。
   * 旧 `providerKeys[providerId]` 已经迁移到当时的目标键；这里不做任何跨目标回退，
   * 否则改成新地址会错误地把旧 Key 发往新端点（凭据越境）。
   */
  next.apiKey = typeof providerTargets[currentTarget] === 'string' ? providerTargets[currentTarget] : ''

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

  /*
   * 推理档位。语义和 systemPrompt 一样：
   *  - patch 里没这个字段 → 保原值（保存无关设置不会冲掉用户选的模式）
   *  - 有 → 规整后落盘（非法值回 default，绝不猜成"关闭思考"）
   * 必须显式放回去：`{...current, ...patch}` 在 patch 带 `field: undefined`
   * （IPC 结构化克隆会保留 undefined）时会把已存的档位冲成 undefined。
   */
  next.mimoThinking = patch.mimoThinking === undefined
    ? normalizeThinking(current.mimoThinking)
    : normalizeThinking(patch.mimoThinking)
  next.deepseekReasoningEffort = patch.deepseekReasoningEffort === undefined
    ? normalizeEffortLevel(current.deepseekReasoningEffort)
    : normalizeEffortLevel(patch.deepseekReasoningEffort)
  next.providerHint = patch.providerHint === undefined
    ? normalizeHint(current.providerHint)
    : normalizeHint(patch.providerHint)
  /*
   * 当前生效的推理参数摘要（诊断与界面提示用）。
   * 由提供商推导，**不信**模型名：`deepseek-flash` 挂到自定义端点上不该带 DeepSeek 参数。
   */
  next.effectiveReasoning = reasoningSummary(next)

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
   * 网易云凭据：两个字段各自"空/掩码=不修改、null=清除"，跟 API Key 同一套语义。
   * 私钥很长，界面读回来只给"已保存（N 字符）"，绝不能把明文当占位符写回去。
   */
  if (patch.netease !== undefined && patch.netease !== null) {
    const currentNetease = current.netease ?? { appId: '', privateKey: '' }
    const incoming = patch.netease
    const merged = { ...currentNetease }
    if (incoming.appId === null) merged.appId = ''
    else if (isRealSecret(incoming.appId)) merged.appId = incoming.appId.trim()
    if (incoming.privateKey === null) merged.privateKey = ''
    else if (isRealSecret(incoming.privateKey)) merged.privateKey = incoming.privateKey.trim()
    /*
     * 记下"同步过哪一版凭据"。这个字段丢了会导致每次启动都重写 CLI 凭据，
     * 而重写会让已有的网易云登录态失效（token 变 Expired）。
     */
    if (typeof incoming.syncedHash === 'string') merged.syncedHash = incoming.syncedHash
    next.netease = merged
  }
  /*
   * 模型方案：每条方案的 apiKey 单独打码；掩码值（含 * 或「已保存」）不覆盖原值。
   * 方案还要带上**推理策略**（提供商 + 地址 + Key + 模型 + 档位是一套），
   * 否则定时切到 MiMo 方案却仍用 DeepSeek 的参数发请求，正是这次要修的毛病。
   */
  if (Array.isArray(patch.modelProfiles)) {
    const currentProfiles = Array.isArray(current.modelProfiles) ? current.modelProfiles : []
    next.modelProfiles = patch.modelProfiles.map((profile) => {
      const previous = currentProfiles.find((item) => item?.id === profile?.id)
      const merged = {
        id: typeof profile?.id === 'string' && profile.id !== '' ? profile.id : `p${Math.random().toString(36).slice(2, 8)}`,
        name: String(profile?.name ?? '').trim(),
        providerId: String(profile?.providerId ?? '').trim(),
        baseURL: String(profile?.baseURL ?? '').trim(),
        model: String(profile?.model ?? '').trim(),
        apiKey: '',
        /*
         * 档位三态：界面给了就用界面的；没给（undefined/空串）就沿用这条方案原来存的；
         * 这条方案也没有（老配置新建的方案行）才回默认。
         * 「没给」和「选了 default」必须区分开：default 是用户明确选的合法档位。
         */
        mimoThinking: keepOr(profile?.mimoThinking, previous?.mimoThinking, normalizeThinking),
        deepseekReasoningEffort: keepOr(profile?.deepseekReasoningEffort, previous?.deepseekReasoningEffort, normalizeEffortLevel),
        providerHint: keepOr(profile?.providerHint, previous?.providerHint, normalizeHint),
      }
      /*
       * 方案 Key 的凭据边界：
       *  - 用户主动给了新 Key → 用新的；
       *  - 没给新 Key，且**认证目标没变**（providerId 与 baseURL 都与原方案一致）→ 沿用原 Key；
       *  - 没给新 Key，但方案换了提供商或换了 Base URL → 这是新的认证目标，
       *    绝不能把旧 Key 发到新主机，Key 清空并留待用户填写。
       * 这与顶层 providerKeys 的语义一致：凭据跟着认证源走，不跟着方案 ID 走。
       */
      const sameTarget = previous !== undefined
        && String(previous.providerId ?? '').trim() === merged.providerId
        && String(previous.baseURL ?? '').trim() === merged.baseURL
      if (isRealSecret(profile?.apiKey)) {
        merged.apiKey = String(profile.apiKey).trim()
      } else if (sameTarget && typeof previous?.apiKey === 'string') {
        merged.apiKey = previous.apiKey
      } else if (sameTarget && previous !== undefined && previous.apiKey !== undefined) {
        // previous.apiKey 已是解密后的明文（loadConfig 解过），保留
        merged.apiKey = typeof previous.apiKey === 'string' ? previous.apiKey : ''
      }
      return merged
    })
  }
  // 落盘前加密（主进程注入了 safeStorage 才生效；工具脚本里就是明文）
  next.apiKey = encryptField(next.apiKey)
  next.providerKeys = encryptKeyMap(next.providerKeys)
  if (Array.isArray(next.modelProfiles)) {
    next.modelProfiles = next.modelProfiles.map((profile) => ({
      ...profile,
      apiKey: encryptField(profile?.apiKey ?? ''),
    }))
  }
  next.providerTargets = encryptKeyMap(next.providerTargets)
  if (next.search !== undefined && next.search !== null) {
    next.search = { ...next.search, apiKey: encryptField(next.search.apiKey) }
  }

  signal?.throwIfAborted()
  await mkdir(CONFIG_DIR, { recursive: true })
  const tmp = `${CONFIG_PATH}.tmp`
  signal?.throwIfAborted()
  await writeFile(tmp, JSON.stringify(next, null, 2), 'utf8')
  signal?.throwIfAborted()
  await rename(tmp, CONFIG_PATH)
  return loadPublicConfig()
}

/**
 * 「界面上有没有表态」的三态取舍：
 * 给了合法值 → 用给的；没给（undefined / 空串 / null）→ 用原来存的；
 * 两个都没有 → 回默认档位。
 * `default` 是用户**明确选过**的合法档位，绝不能当成"没给"。
 */
function keepOr(incoming, previous, normalize) {
  if (typeof incoming === 'string' && incoming.trim() !== '') return normalize(incoming)
  if (typeof previous === 'string' && previous.trim() !== '') return normalize(previous)
  return normalize('default')
}

function normalizeHint(value) {
  const hint = typeof value === 'string' ? value.trim().toLowerCase() : ''
  if (hint === '' || hint === null) return 'auto'
  return hint === 'deepseek' || hint === 'mimo' || hint === 'auto' ? hint : 'auto'
}

/**
 * 当前生效的推理参数摘要（写进配置，供界面与诊断对照）。
 * 由**当前提供商**推导，不信模型名。它只是说明性的：真正发请求时
 * LLMClient 会重新按同一套规则组装（见 agent/ll-params.mjs）。
 */
function reasoningSummary(config) {
  const profile = buildProfileFor(config)
  const built = buildRequestFields(profile, { temperature: config.temperature })
  return {
    capability: profile.capability,
    thinking: built.fields.thinking ?? null,
    reasoningEffort: built.fields.reasoning_effort ?? null,
    thinkingActive: built.thinkingActive,
    temperatureSent: built.temperature !== null,
    ignoredSampling: built.ignoredSampling,
  }
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

/**
 * 取「某个提供商 + 认证目标」该用的 Key。设置页的下拉可以先选中一家还没保存的提供商，
 * 这时**绝不能**拿当前生效的那一份去请求——那是另一家的凭据（凭据越境 + 必定 401）。
 *
 * - 目标就是当前提供商且未另行指定 baseURL：走 resolveApiKey()（环境变量优先、密文检查都在那里）
 * - 目标是别家（或别家 + 不同 baseURL）：只用它自己目标槽位里的 Key，没有就返回空
 *   （让调用方提示"请先填 Key"）
 * - 任何情况下都不回退到 `providerKeys[providerId]`（旧 providerId 槽不是认证目标，
 *   回退会导致把旧目标 Key 发给新目标）。
 */
export async function resolveApiKeyFor(providerId, { config, baseURL } = {}) {
  const target = typeof providerId === 'string' ? providerId.trim() : ''
  const current = config ?? (await loadConfig())
  if (target === '' || (target === current.providerId && (baseURL === undefined || String(baseURL).trim() === ''))) {
    return await resolveApiKey()
  }
  // 按认证目标取：providerTargets[targetKey]（无 legacy 回退）
  const t = targetKey(target, baseURL ?? current.baseURL ?? '')
  const targets = effectiveTargetKeys(current)
  return typeof targets[t] === 'string' ? targets[t] : ''
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
  return CONFIG_PATH
}

/** 兼容旧代码：动态获取当前配置文件路径 */
export const CONFIG_FILE = CONFIG_PATH
