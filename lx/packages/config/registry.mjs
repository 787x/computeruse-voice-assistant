/**
 * 模型提供商注册表（R9）。
 *
 * 只描述"有哪些可选"，不负责存储。存储见 store.mjs。
 * 所有提供商都按 OpenAI 兼容协议调用（/chat/completions），
 * 这样 agent loop 只写一套代码，切换提供商不改逻辑。
 *
 * `capabilities` 决定这个提供商能接受哪些推理参数（见 agent/ll-params.mjs）：
 *  - `mimo`：只有 `thinking:{type:enabled|disabled}`，没有强度档位
 *  - `deepseek`：`thinking` 开关 + `reasoning_effort` 强度
 *  - 其它：标准 OpenAI 兼容，**不发送任何提供商专属字段**
 * `modelDiscovery` 表示这个提供商的 `/v1/models` 是官方在线目录，设置页会自动拉取。
 */

/** @typedef {{id:string,name:string,baseURL:string,models:string[],apiKeyRequired:boolean,notes:string,capabilities?:string,modelDiscovery?:boolean,keyConsoleUrl?:string,defaultModel?:string}} Provider */

export const PROVIDERS = [
  {
    id: 'mimo',
    name: 'Xiaomi MiMo',
    // 按量付费的 OpenAI 兼容端点（Token Plan 是另一个地址 + tp-/ttp- 开头的 Key，不能混用）
    baseURL: 'https://api.xiaomimimo.com/v1',
    models: ['mimo-v2.6-flash', 'mimo-v2.6-pro', 'mimo-v2.6-pro-ultraspeed'],
    defaultModel: 'mimo-v2.6-flash',
    apiKeyRequired: true,
    capabilities: 'mimo',
    modelDiscovery: true,
    keyConsoleUrl: 'https://platform.xiaomimimo.com/#/console/api-keys',
    notes: '按量付费地址。在线模型列表会自动获取；V2.6 系列默认开启思考。ASR/TTS 模型已从选择器里排除。',
  },
  {
    id: 'deepseek',
    name: 'DeepSeek 官方',
    baseURL: 'https://api.deepseek.com/v1',
    models: ['deepseek-flash', 'deepseek-v4-pro'],
    defaultModel: 'deepseek-flash',
    apiKeyRequired: true,
    capabilities: 'deepseek',
    modelDiscovery: true,
    keyConsoleUrl: 'https://platform.deepseek.com/api_keys',
    notes: 'tool calling 稳定；可单独设置推理等级（none/low/high/max）',
  },
  {
    id: 'openai-compatible',
    name: 'OpenAI 兼容端点（自定义）',
    baseURL: 'https://api.openai.com/v1',
    models: [],
    apiKeyRequired: true,
    notes: '任何实现 /chat/completions 的服务：OpenAI、Kimi、Qwen、OneAPI、vLLM…',
  },
  {
    id: 'local',
    name: '本地 llama.cpp',
    baseURL: 'http://127.0.0.1:8080/v1',
    models: ['local-model'],
    apiKeyRequired: false,
    notes: 'llama-server 默认端口；模型名以 /v1/models 返回为准',
  },
  {
    id: 'mock',
    name: '本地模拟（开发用）',
    baseURL: 'http://127.0.0.1:18080/v1',
    models: ['mock-tool-use'],
    apiKeyRequired: false,
    notes: '仅用于离线验证 agent loop，不产生真实推理',
  },
]

/**
 * 新装默认提供商：MiMo。
 * 注意这只影响**全新配置**（磁盘上还没有 providerId）。
 * 已有 DeepSeek / 自定义端点的配置升级后保持原样，见 store.mjs 的迁移说明。
 */
export const DEFAULT_PROVIDER_ID = 'mimo'

export function getProvider(id) {
  return PROVIDERS.find((provider) => provider.id === id)
}

/** 该提供商的推荐初始模型（新装/切提供商时用；在线列表优先于它） */
export function defaultModelFor(provider) {
  if (typeof provider?.defaultModel === 'string' && provider.defaultModel !== '') return provider.defaultModel
  return provider?.models?.[0] ?? ''
}

/** 生成一份默认配置。 */
export function defaultConfig() {
  const provider = getProvider(DEFAULT_PROVIDER_ID)
  return {
    schemaVersion: 1,
    providerId: provider.id,
    baseURL: provider.baseURL,
    model: defaultModelFor(provider),
    apiKey: '',
    /*
     * 每个提供商自己的 Key（`{deepseek: 'sk-…', mimo: 'sk-…'}`）。
     * `apiKey` 是「当前提供商正在用的那一份」，由本表推导出来，不单独存意义。
     * 这样切提供商不会把上一家的 Key 发到新端点，切回来又不用重填。
     */
    providerKeys: {},
    temperature: 0.3,
    /*
     * 推理控制（按提供商生效，互不冲突）：
     *  - mimoThinking: default（跟随服务端；V2.6 官方默认开）| enabled | disabled
     *  - deepseekReasoningEffort: default | none | low | high | max
     * 存"档位"而不是存布尔：换提供商时另一个字段不会被误当成"用户表过态"。
     * 未识别的取值一律回 default（详见 agent/ll-params.mjs）。
     */
    mimoThinking: 'default',
    deepseekReasoningEffort: 'default',
    /**
     * 自定义端点上的协议提示（`openai-compatible` 这类没有内置能力的提供商用）：
     *  - `auto`：什么都不发（默认，最安全）
     *  - `deepseek` / `mimo`：按该协议的推理参数发
     * 内置提供商会直接用注册表里的 capabilities，这个字段只对自定义端点起作用。
     */
    providerHint: 'auto',
    /**
     * 步数上限：一步 = 一次模型往返（可含多个工具调用）。
     * 8 步只够"看一眼+点一下"，开个程序经常用到第 9 步就被截断（实测无畏契约那轮 tools=10 还没点结果）。
     * 16 步留出一倍余量，代价只是失败时多花几秒。
     */
    maxSteps: 16,
    timeoutMs: 60000,
    /**
     * 系统提示词（人设与行为指引）。
     *  - null / 未设置：用 packages/agent/system-prompt.mjs 里的内置默认提示词
     *  - 非空字符串：用户自定义，**原样**发给模型（不追加、不改写）
     * 语义见 store.saveConfig：字段缺省=不修改，null=显式恢复默认，空白/非文本=保存失败。
     */
    systemPrompt: null,
    /**
     * 权限（R7）。默认"标准档"：低风险放行、不可逆询问、破坏性系统操作拒绝。
     * pausedUntil / allowedOnce 是运行期状态，落盘是为了重启后不丢"暂停 10 分钟"的承诺。
     */
    permissions: {
      mode: 'standard',
      overrides: {},
      pausedUntil: 0,
      allowedOnce: {},
    },
    /** 联网搜索（默认 Tavily）。没填 key 时工具不会出现，模型也就不会去调它。 */
    search: {
      provider: 'tavily',
      apiKey: '',
    },
    /** 语音合成（GPT-SoVITS / GSV-TTS-Lite）。情绪对应音色包里的参考音频标签。 */
    tts: {
      emotion: '中立',
      speed: 1,
    },
    /** 界面开关（以前这些只活在内存里，重启就丢，用户会发现"我明明关过播报"） */
    ui: {
      ttsEnabled: true,
      continuousMode: true,
      voiceConfirmEnabled: true,
      subtitles: true,
      /** 开机自启（打包后生效；开发态只写配置不注册） */
      autoStart: false,
      /** 语音唤醒。默认开——这是个唤醒词助手，开箱不该是"麦没开" */
      wakeEnabled: true,
      /** 全屏（打游戏）时让出显存：关掉 TTS worker，唤醒与识别照旧 */
      releaseOnFullscreen: true,
    },
    /** R14 资源策略 */
    resources: {
      enabled: true,
      /** 前台窗口占工作区多少比例算"全屏遮挡" */
      fullscreenRatio: 0.98,
      /** 可用内存低于多少 MB 时卸载 ASR（保留唤醒用的 KWS） */
      memoryFloorMb: 1200,
      /** 多久探测一次 */
      pollMs: 20000,
    },
    /**
     * 连续对话的判定参数。
     * 不做 true/false 硬开关，而是把"用户还会继续说"的置信度映射成等待时长——
     * 拿不准就多等一会儿，判错也不至于把用户的话吞掉。
     */
    /**
     * 定时切换模型：不同时间段用不同模型（比如白天用便宜的、夜里用强的）。
     * 规则按数组顺序匹配，先命中先用；from == to 视为全天。
     */
    /**
     * 模型方案：一套"供应商 + 地址 + Key + 模型"的组合，供定时切换直接引用。
     * 不写方案也能用（规则里只填模型名），但那样换不了供应商。
     */
    modelProfiles: [],    modelSchedule: {
      enabled: false,
      rules: [],
    },
    followup: {
      thresholds: { high: 0.7, low: 0.3 },
      windowsMs: { high: 8000, mid: 4000, low: 1500 },
      judgeTimeoutMs: 2500,
    },
  }
}
