/**
 * 提供商推理参数装配（纯逻辑，便于单测）。
 *
 * 解决的问题：同一套 OpenAI 兼容代码，怎么按提供商发不同的推理控制参数。
 *
 *  - MiMo：Chat Completions 只有 `thinking: {type: enabled|disabled}`。官方参数表里
 *    **没有** `reasoning_effort`（强度档位只在 Responses API 上有，而且除 none 外
 *    全部映射到同一个实际强度），所以这里绝不伪造 MiMo 的强度档位。
 *  - DeepSeek：`thinking.type` 决定开关，且官方明确「提供 thinking 时其模式设置优先于
 *    reasoning_effort」；`reasoning_effort` 决定强度。官方把 medium 映射成 high、
 *    minimal 映射成 low、ultra 映射成 max，所以界面只展示有实际差别的 none/low/high/max。
 *  - 其它 OpenAI 兼容端点 / 本地 llama.cpp / mock：一个提供商专属字段都不发。
 *    给它们发 MiMo 或 DeepSeek 专属参数，轻则被忽略，重则参数校验报错（假失败）。
 *
 * 判断依据是**提供商能力标记**（registry 的 `capabilities`，自定义端点上的 `providerHint`），
 * 不是拿模型名做字符串猜。模型名只用于认「官方按型号声明的采样参数限制」。
 *
 * 两套发行版（根目录 / lx）共用同一份实现，行为必须一致。
 */

import { PROVIDERS } from '../config/registry.mjs'

export const MIMO_CAPABILITY = 'mimo'
export const DEEPSEEK_CAPABILITY = 'deepseek'
export const PLAIN_CAPABILITY = 'openai-compatible'

/** MiMo 思考模式档位：default = 不传 thinking，由服务端决定（V2.6 官方默认是开） */
export const MIMO_THINKING_LEVELS = ['default', 'enabled', 'disabled']
/**
 * DeepSeek 推理档位。
 * `medium` 官方映射为 `high`，没有独立实际效果，所以不作为一档展示。
 */
export const DEEPSEEK_EFFORT_LEVELS = ['default', 'none', 'low', 'high', 'max']

/** 官方档位别名 → 有实际差别的档位（用来把能力元数据里的别名并到展示档位上） */
const EFFORT_ALIASES = { minimal: 'low', medium: 'high', xhigh: 'high', ultra: 'max' }
/** 实际强度从低到高，用于「用户选了模型不支持的档位」时就近降级 */
const EFFORT_ORDER = ['low', 'high', 'max']

/**
 * 思考模式下官方会强制覆盖采样参数的 MiMo 型号系列。
 * 依据 MiMo《深度思考》《模型超参》：mimo-v2.6-flash / -pro / -pro-ultraspeed 思考下
 * temperature、top_p 即使传入也会被强制为 1.0 / 0.95。（V2.5 系列 2026-10-21 已下线。）
 * 这是「按型号声明的规则」，不是靠模型名猜提供商。
 */
const MIMO_THINKING_FORCED_SAMPLING = /^mimo-v2\.6(?:$|-)/u

/** 自定义端点上可选的协议提示（设置页用） */
export const PROVIDER_HINTS = [
  { id: 'auto', name: '自动（默认不发送提供商专属推理参数）' },
  { id: DEEPSEEK_CAPABILITY, name: 'DeepSeek 协议（thinking + reasoning_effort）' },
  { id: MIMO_CAPABILITY, name: 'MiMo 协议（仅 thinking 开关）' },
]

/**
 * 内置提供商里"没有专属推理参数"的那些。用户自己另建的 id 也一律按标准兼容处理：
 * 宁可不发专属参数，也不要拿某一家的私有字段去打别家。
 */
const KNOWN_CAPABILITIES = new Set([MIMO_CAPABILITY, DEEPSEEK_CAPABILITY, PLAIN_CAPABILITY, 'local', 'mock'])

function normalizeToken(value) {
  return typeof value === 'string' ? value.trim().toLowerCase() : ''
}

/**
 * 认端点类型。
 * @param {{capability?:string, providerId?:string, hint?:string}} options
 *   - capability：提供商注册表里声明的能力（**最高优先**）
 *   - providerId：配置里的提供商 id；没直接给 capability 时，用它去注册表查声明的能力
 *   - hint：用户在自定义端点上手填的协议提示（`auto` 视为没填）
 */
export function resolveCapability({ capability, providerId, hint } = {}) {
  const declared = normalizeToken(capability)
  if (declared !== '') return declared
  const id = normalizeToken(providerId)
  const fromRegistry = capabilityOfProviderId(id)
  const hinted = normalizeToken(hint)
  // 用户显式声明了协议（比如自建 OneAPI 转发 DeepSeek）→ 以声明为准
  if (hinted !== '' && hinted !== 'auto') return hinted
  if (fromRegistry !== '') return fromRegistry
  // 注册表里没有这个 id：按标准 OpenAI 兼容处理，什么专属参数都不发
  return KNOWN_CAPABILITIES.has(id) && id !== '' ? id : PLAIN_CAPABILITY
}

/**
 * 从提供商注册表查该 id 声明的能力。
 * 这样"新增一家只要把 capabilities 填对，推理参数与界面档位就自动跟上"，
 * 不必再来改这份文件里的硬编码名单。
 */
function capabilityOfProviderId(id) {
  if (id === '') return ''
  const provider = PROVIDERS.find((item) => item.id === id)
  if (provider === undefined) return ''
  return normalizeToken(provider.capabilities)
}

/** 该提供商的 Chat Completions 端点是否接受 reasoning_effort（MiMo 没有这个字段） */
export function supportsReasoningEffort(capability) {
  return normalizeToken(capability) === DEEPSEEK_CAPABILITY
}

/**
 * 该提供商是否要求「带 tools 的历史必须原样回传 reasoning_content」。
 * 两家的官方措辞不同（DeepSeek：只要请求带 tools，后续所有请求都要回传，即使那轮没调工具，
 * 否则 400；MiMo：思考模式多轮工具调用中，含 tool_calls 的 assistant 必须完整回传，否则 400），
 * 共同点是「漏了会 400」，所以两家都按最严格的方式回传。
 */
export function requiresReasoningEcho(capability) {
  const id = normalizeToken(capability)
  return id === MIMO_CAPABILITY || id === DEEPSEEK_CAPABILITY
}

/** 用户选的思考档位（非法值回 default，绝不猜成 disabled） */
export function normalizeThinking(value) {
  const level = normalizeToken(value)
  return MIMO_THINKING_LEVELS.includes(level) ? level : 'default'
}

/** 用户选的推理档位；该提供商不支持强度控制时一律回 default */
export function normalizeReasoningEffort(value, capability = '') {
  if (!supportsReasoningEffort(capability)) return 'default'
  return normalizeEffortLevel(value)
}

/**
 * 纯档位合法性检查（不看提供商）。
 * 落盘时要校验**存着的** DeepSeek 档位——那时用户可能正选中 MiMo，
 * 不能因为"当前提供商不支持强度"就把已存的 DeepSeek 设置清掉。
 */
export function normalizeEffortLevel(value) {
  const level = normalizeToken(value)
  return DEEPSEEK_EFFORT_LEVELS.includes(level) ? level : 'default'
}

/**
 * 由模型能力元数据（DeepSeek `GET /models` 的 `effort.supported_levels`）算出
 * 界面上真正可选的档位。拿不到元数据时返回 null = 「不知道，按官方当前档位全给」，
 * 不假定未来模型的能力永远相同。
 * @returns {{supports:boolean, levels:string[], defaultLevel:string|null}|null}
 */
export function effortCapability(metadata) {
  const supported = Array.isArray(metadata?.supported_levels)
    ? metadata.supported_levels.map(normalizeToken).filter(Boolean)
    : null
  if (supported === null) return null
  const levels = ['none', ...EFFORT_ORDER.filter((level) => supported.some((item) => canonicalEffort(item) === level))]
  const raw = normalizeToken(metadata?.default_level)
  return {
    supports: true,
    levels,
    defaultLevel: raw === '' ? null : canonicalEffort(raw),
  }
}

/** 官方档位别名并入实际档位；未知档位原样返回（不认识就不假装认识） */
export function canonicalEffort(level) {
  const token = normalizeToken(level)
  return EFFORT_ALIASES[token] ?? token
}

function finiteClamped(value, min, max) {
  const number = Number(value)
  if (!Number.isFinite(number)) return null
  return Math.min(max, Math.max(min, number))
}

/**
 * 组装一次请求的推理与采样字段。
 *
 * @param {object} options
 * @param {string} [options.capability] 提供商能力（registry 声明）
 * @param {string} [options.providerId] 配置里的提供商 id
 * @param {string} [options.hint] 自定义端点上的协议提示
 * @param {string} [options.thinking] MiMo 档位 default|enabled|disabled
 * @param {string} [options.reasoningEffort] DeepSeek 档位 default|none|low|high|max
 * @param {string} [options.model] 当前模型名（只用于型号级采样限制）
 * @param {object} [options.modelCapabilities] /models 带回的能力元数据
 * @param {{temperature?:number, topP?:number}} [sampling] 用户设的采样参数
 * @returns {{fields:object, temperature:number|null, ignoredSampling:string[],
 *            thinkingActive:boolean, effortApplied:string|null, effortDowngraded:boolean,
 *            capability:string}}
 *   `fields` 合进请求体；`temperature` 为 null 表示**别发这个字段**；
 *   `ignoredSampling` 记录官方会静默忽略/强制覆盖的参数，界面据此如实说明，
 *   不能一边显示「温度 0.3」一边让模型实际用 1.0。
 */
export function buildRequestFields(options = {}, sampling = {}) {
  const capability = resolveCapability(options)
  const model = normalizeToken(options.model)
  const out = {
    capability,
    fields: {},
    temperature: null,
    ignoredSampling: [],
    thinkingActive: false,
    effortApplied: null,
    effortDowngraded: false,
  }

  if (capability === MIMO_CAPABILITY) {
    const level = normalizeThinking(options.thinking)
    // 「默认」不传 thinking → 服务端决定。MiMo V2.6 官方默认**开**思考，
    // 所以把 default 实现成 disabled 是错的。
    out.thinkingActive = level !== 'disabled'
    if (level !== 'default') out.fields.thinking = { type: level }
    const forced = MIMO_THINKING_FORCED_SAMPLING.test(model)
    if (out.thinkingActive && forced) {
      // 传了也会被强制覆盖 → 不发，并如实记下来
      out.ignoredSampling = ['temperature', 'top_p']
    } else {
      // MiMo 官方区间：temperature [0, 1.5]，top_p 由服务端默认（0.95），这里只透传温度
      out.temperature = finiteClamped(sampling.temperature, 0, 1.5)
    }
    return out
  }

  if (capability === DEEPSEEK_CAPABILITY) {
    const level = normalizeReasoningEffort(options.reasoningEffort, capability)
    const cap = options.modelCapabilities ? effortCapability(options.modelCapabilities) : null
    if (level === 'default') {
      // 两个字段都不发：完全跟随服务端默认（官方当前是开思考 + high）
      out.thinkingActive = true
    } else if (level === 'none') {
      // 关闭思考只发 thinking:{type:'disabled'}：
      // 官方说 thinking 的模式设置优先于 reasoning_effort，再发一个 effort 只是自找冲突。
      out.fields.thinking = { type: 'disabled' }
      out.thinkingActive = false
    } else {
      let applied = level
      if (cap !== null && !cap.levels.includes(level)) {
        // 模型声明了可选档位，用户选的档位不在其中 → 就近取较低的可用档，不硬发
        const ordered = EFFORT_ORDER.filter((item) => cap.levels.includes(item))
        const wanted = EFFORT_ORDER.indexOf(level)
        applied = ordered.filter((item) => EFFORT_ORDER.indexOf(item) <= wanted).pop() ?? ordered[0] ?? null
        out.effortDowngraded = applied !== null && applied !== level
        if (applied === null) {
          // 该模型只支持关闭思考
          out.fields.thinking = { type: 'disabled' }
          out.thinkingActive = false
          out.temperature = finiteClamped(sampling.temperature, 0, 2)
          return out
        }
      }
      out.effortApplied = applied
      // 官方示例就是 thinking:{type:'enabled'} + reasoning_effort 同时给（不冲突）
      out.fields.thinking = { type: 'enabled' }
      out.fields.reasoning_effort = applied
      out.thinkingActive = true
    }
    if (out.thinkingActive) {
      // 官方：思考模式不支持 temperature / presence_penalty / frequency_penalty，
      // 传了不报错但也不生效；top_p 行为按型号而异（flash 只在 0.95~1.0 有效、
      // v4-pro 思考下直接忽略传入值）。所以这里不发，而不是发个无效的。
      out.ignoredSampling = ['temperature', 'top_p']
    } else {
      out.temperature = finiteClamped(sampling.temperature, 0, 2)
    }
    return out
  }

  // 标准 OpenAI 兼容 / 本地 / mock：只带用户设的温度，不带任何提供商专属字段。
  out.thinkingActive = false
  out.temperature = finiteClamped(sampling.temperature, 0, 2)
  return out
}

/**
 * 由一条「端点记录」（配置本身，或一个模型方案）生成 LLM 客户端要的提供商档案。
 *
 * 为什么强调"按提供商生成"：以前只看模型名字符串，`deepseek-flash` 挂到自定义
 * 端点上就会带着 DeepSeek 的推理参数打标准 OpenAI 服务。这里以 providerId（+ 自定义
 * 端点上的 hint）为唯一依据，模型名只用于型号级采样限制。
 *
 * @param {{providerId?:string, capability?:string, baseURL?:string, model?:string,
 *          mimoThinking?:string, deepseekReasoningEffort?:string, providerHint?:string,
 *          modelCapabilities?:object}} source
 */
export function buildProfileFor(source = {}) {
  const capability = resolveCapability({
    capability: source.capability,
    providerId: source.providerId,
    hint: source.providerHint,
  })
  return {
    providerId: normalizeToken(source.providerId),
    capability,
    hint: normalizeToken(source.providerHint),
    baseURL: typeof source.baseURL === 'string' ? source.baseURL : '',
    model: typeof source.model === 'string' ? source.model : '',
    thinking: normalizeThinking(source.mimoThinking),
    reasoningEffort: normalizeReasoningEffort(source.deepseekReasoningEffort, capability),
    modelCapabilities: source.modelCapabilities ?? null,
  }
}

/**
 * 推理控件的档位文案。放在这里（而不是壳子或界面）是因为这些说明就是协议事实：
 * 「默认」不等于关闭、MiMo 没有强度档、思考下温度不生效——只能有一份说法。
 */
export const MIMO_THINKING_OPTIONS = [
  { value: 'default', label: '默认（跟随服务端）', note: 'MiMo V2.6 系列官方默认开启思考，"默认"不是关闭' },
  { value: 'enabled', label: '开启思考', note: '思考模式下 temperature/top_p 会被模型强制覆盖，实际请求不发送温度' },
  { value: 'disabled', label: '关闭思考', note: '响应更快；温度按你的设置发送' },
]
export const DEEPSEEK_EFFORT_OPTIONS = [
  { value: 'default', label: '默认（跟随服务端）', note: '官方当前默认开启思考、强度 high' },
  { value: 'none', label: '关闭思考', note: '发送 thinking:{type:"disabled"}，不再同时发 reasoning_effort' },
  { value: 'low', label: '低', note: 'reasoning_effort=low' },
  { value: 'high', label: '高', note: 'reasoning_effort=high（官方把 medium 也映射到这一档）' },
  { value: 'max', label: '极高', note: 'reasoning_effort=max' },
]

const OPTIONS_BY_KIND = {
  'mimo-thinking': MIMO_THINKING_OPTIONS,
  'deepseek-effort': DEEPSEEK_EFFORT_OPTIONS,
}

/**
 * 组装设置页要的推理控件元数据（主进程与验收脚本共用同一份，界面不许自己编能力表）。
 *
 * 这里刻意**不认提供商 id**：控件形态完全由提供商声明的 `capabilities` 决定。
 * 以后新增一家只要把 capabilities 填对，界面自动出现正确的档位；
 * 填错或没填就退化成"没有推理控件"，而不是错误地套用某一家的档位表。
 * @param {Array<object>} providers 提供商注册表
 */
export function buildReasoningMeta(providers = []) {
  const byProvider = {}
  for (const provider of providers) {
    const control = reasoningControlFor({ capability: provider.capabilities, providerId: provider.id })
    byProvider[provider.id] = {
      ...control,
      options: OPTIONS_BY_KIND[control.kind] ?? [],
      keyConsoleUrl: provider.keyConsoleUrl ?? '',
      // 推荐初始模型：显式声明优先，否则取列表第一项（在线列表到手后由界面覆盖）
      defaultModel: provider.defaultModel ?? provider.models?.[0] ?? '',
      modelDiscovery: provider.modelDiscovery === true,
    }
  }
  return {
    byProvider,
    // 自定义端点选了协议提示时用这份（控件跟着提供商/提示走，不按模型名猜）
    byHint: {
      mimo: byProvider.mimo ?? { kind: 'mimo-thinking', levels: MIMO_THINKING_LEVELS, options: MIMO_THINKING_OPTIONS, note: '' },
      deepseek: byProvider.deepseek ?? { kind: 'deepseek-effort', levels: DEEPSEEK_EFFORT_LEVELS, options: DEEPSEEK_EFFORT_OPTIONS, note: '' },
    },
    hints: PROVIDER_HINTS,
  }
}

/**
 * 界面上「此刻该显示哪套推理控件」。
 * @returns {{kind:'mimo-thinking'|'deepseek-effort'|'none', levels:string[], note:string}}
 */
export function reasoningControlFor({ capability, providerId, hint, modelCapabilities } = {}) {
  const resolved = resolveCapability({ capability, providerId, hint })
  if (resolved === MIMO_CAPABILITY) {
    return {
      kind: 'mimo-thinking',
      levels: MIMO_THINKING_LEVELS,
      note: 'MiMo 的 Chat Completions 只有思考开关，没有独立强度档位；V2.6 系列默认开启思考。',
    }
  }
  if (resolved === DEEPSEEK_CAPABILITY) {
    const cap = modelCapabilities ? effortCapability(modelCapabilities) : null
    // 拿不到能力元数据时按官方当前档位全给；拿到了就只列该模型真支持的档位
    const levels = cap === null ? DEEPSEEK_EFFORT_LEVELS : ['default', ...cap.levels]
    return {
      kind: 'deepseek-effort',
      levels,
      note: cap === null ? '' : `该模型声明支持：${cap.levels.join(' / ')}`,
    }
  }
  return { kind: 'none', levels: [], note: '' }
}
