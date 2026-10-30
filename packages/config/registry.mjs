/**
 * 模型提供商注册表（R9）。
 *
 * 只描述"有哪些可选"，不负责存储。存储见 store.mjs。
 * 所有提供商都按 OpenAI 兼容协议调用（/chat/completions），
 * 这样 agent loop 只写一套代码，切换提供商不改逻辑。
 */

/** @typedef {{id:string,name:string,baseURL:string,models:string[],apiKeyRequired:boolean,notes:string}} Provider */

export const PROVIDERS = [
  {
    id: 'deepseek',
    name: 'DeepSeek 官方',
    baseURL: 'https://api.deepseek.com/v1',
    models: ['deepseek-flash', 'deepseek-v4-pro'],
    apiKeyRequired: true,
    notes: 'tool calling 稳定，作为默认云端选择',
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

export const DEFAULT_PROVIDER_ID = 'deepseek'

export function getProvider(id) {
  return PROVIDERS.find((provider) => provider.id === id)
}

/** 生成一份默认配置。 */
export function defaultConfig() {
  const provider = getProvider(DEFAULT_PROVIDER_ID)
  return {
    schemaVersion: 1,
    providerId: provider.id,
    baseURL: provider.baseURL,
    model: provider.models[0],
    apiKey: '',
    temperature: 0.3,
    /**
     * 步数上限：一步 = 一次模型往返（可含多个工具调用）。
     * 8 步只够"看一眼+点一下"，开个程序经常用到第 9 步就被截断（实测无畏契约那轮 tools=10 还没点结果）。
     * 16 步留出一倍余量，代价只是失败时多花几秒。
     */
    maxSteps: 16,
    timeoutMs: 60000,
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
    },    /** 网易云音乐：appId + RSA privateKey（ncm-cli 用它们做签名） */
    netease: {
      appId: '',
      privateKey: '',
      // 上次同步给 ncm-cli 的凭据哈希：没变就不重写，避免把登录态顶掉
      syncedHash: '',
    },
    followup: {
      thresholds: { high: 0.7, low: 0.3 },
      windowsMs: { high: 8000, mid: 4000, low: 1500 },
      judgeTimeoutMs: 2500,
    },
  }
}
