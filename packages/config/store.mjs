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
    if (merged.search !== undefined && merged.search !== null) {
      merged.search = { ...merged.search, apiKey: decryptField(merged.search.apiKey) }
    }
    /*
     * 兼容旧配置（PR#4 之前只有 `apiKey` 一个槽）：把当时选中提供商的 Key
     * 记进 providerKeys，用户切走再切回来时不用重填。
     * 未知/手改的 providerId 也照这份来：它的 baseURL 同样是用户自己写的，
     * 这份 Key 本来就属于那个地址，不存在"发到别家"的问题。
     * 这一步只在**读**的时候补齐，不改 providerId / baseURL / model，
     * 所以已有 DeepSeek、自定义端点的配置升级后行为完全不变。
     */
    if (typeof merged.providerKeys !== 'object' || merged.providerKeys === null) merged.providerKeys = {}
    if (typeof merged.providerKeys[merged.providerId] !== 'string' && typeof merged.apiKey === 'string' && merged.apiKey !== '') {
      merged.providerKeys = { ...merged.providerKeys, [merged.providerId]: merged.apiKey }
    }
    // 每次读都重算生效摘要：档位是纯函数，磁盘上那份只是给人看的，不能当真值源
    merged.effectiveReasoning = reasoningSummary(merged)
    return merged
  } catch {
    const fresh = defaultConfig()
    fresh.providerKeys = {}
    fresh.effectiveReasoning = reasoningSummary(fresh)
    return fresh
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
    // 每个提供商的 Key 只报掩码 + 有没有，界面据此决定"切回去要不要重新填"
    providerKeys: Object.fromEntries(Object.entries(config.providerKeys ?? {}).map(([id, key]) => [id, maskKey(key)])),
    hasProviderKey: Object.fromEntries(Object.entries(config.providerKeys ?? {}).map(([id, key]) => [id, Boolean(key)])),
    search: { ...(config.search ?? {}), apiKey: maskKey(searchKey), hasApiKey: Boolean(searchKey || process.env.TAVILY_API_KEY) },
    // 网易云：appId 可以明示，私钥只报长度（1600 字符塞进界面毫无意义）
    modelProfiles: (config.modelProfiles ?? []).map((profile) => ({ ...profile, apiKey: maskKey(profile.apiKey), hasApiKey: Boolean(profile.apiKey) })),
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

  /*
   * 「提供商 → Key」映射：每个提供商自己那份凭据。
   * 语义和其它密钥字段一致：空串/掩码=**不修改**、null=清除、真值=替换。
   * 从当前（已解密的）映射出发逐条覆盖，所以调用方就算把打码后的整份配置回传，
   * 也只会把掩码丢掉、不会把真 Key 抹了。
   */
  const providerKeys = { ...(current.providerKeys ?? {}) }
  const incomingKeys = patch.providerKeys && typeof patch.providerKeys === 'object' && !Array.isArray(patch.providerKeys)
    ? patch.providerKeys
    : null
  if (incomingKeys !== null) {
    for (const [rawId, value] of Object.entries(incomingKeys)) {
      // id 也要 trim：留着空格会开出一个"查不到的槽位"，等于把用户的 Key 弄丢
      const id = typeof rawId === 'string' ? rawId.trim() : ''
      if (id === '') continue
      if (value === null) delete providerKeys[id]
      else if (isRealSecret(value)) providerKeys[id] = String(value).trim()
      // 空串/掩码：不修改
    }
  }
  if (typeof patch.apiKey === 'string') {
    if (patch.apiKey === '' || patch.apiKey.includes('*')) {
      /*
       * 不修改。界面回显的是掩码，把它当新值写回去就等于把 Key 抹了（实测踩过）。
       * 只有"还在同一个提供商上"时，才用 global apiKey 补齐该槽位（老配置只有一份 Key）；
       * **切到别的提供商时绝不能这么补**——那等于把上一家的 Key 塞进新家的槽位，
       * 下一次请求就会拿 DeepSeek 的 Key 去打 MiMo（401，而且凭据越境）。
       */
      if (targetProviderId === current.providerId && providerKeys[targetProviderId] === undefined
        && isRealSecret(current.apiKey)) {
        providerKeys[targetProviderId] = current.apiKey
      }
    } else {
      providerKeys[targetProviderId] = patch.apiKey.trim()
    }
  }
  // 显式清空：apiKey: null。空串表示"不修改"，null 表示"删掉"——两者语义不同。
  if (patch.apiKey === null) delete providerKeys[targetProviderId]
  next.providerKeys = providerKeys

  /*
   * 真正生效的 apiKey **由提供商推导**，不做"顺手保留上一个提供商的 Key"。
   * 这是切提供商时的凭据隔离：DeepSeek 的 Key 绝不能跟着配置一起打到 MiMo 的端点上
   * （反过来也一样，而且两家 Key 前缀不同，发错就是 401）。
   * 回切便利性靠 providerKeys：切回来时那一份还在。
   */
  next.apiKey = typeof providerKeys[next.providerId] === 'string' ? providerKeys[next.providerId] : ''

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
      if (isRealSecret(profile?.apiKey)) merged.apiKey = String(profile.apiKey).trim()
      // 方案指向某个提供商时，没填 Key 就沿用**这条方案自己**原来的 Key；
      // 绝不用"当前全局 Key"去顶——那可能是另一个提供商的凭据。
      else if (typeof previous?.apiKey === 'string') merged.apiKey = previous.apiKey
      return merged
    })
  }
  // 落盘前加密（主进程注入了 safeStorage 才生效；工具脚本里就是明文）
  next.apiKey = encryptField(next.apiKey)
  next.providerKeys = encryptKeyMap(next.providerKeys)
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
 * 取「某个提供商」该用的 Key。设置页的下拉可以先选中一家还没保存的提供商，
 * 这时**绝不能**拿当前生效的那一份去请求——那是另一家的凭据（凭据越境 + 必定 401）。
 *
 * - 目标就是当前提供商：走 resolveApiKey()（环境变量优先、密文检查都在那里）
 * - 目标是别家：只用它自己槽位里的 Key，没有就返回空（让调用方提示"请先填 Key"）
 */
export async function resolveApiKeyFor(providerId, { config } = {}) {
  const target = typeof providerId === 'string' ? providerId.trim() : ''
  const current = config ?? (await loadConfig())
  if (target === '' || target === current.providerId) return await resolveApiKey()
  const own = (current.providerKeys ?? {})[target]
  return typeof own === 'string' ? own : ''
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
