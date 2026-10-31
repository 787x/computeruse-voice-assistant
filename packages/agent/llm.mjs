/**
 * LLM 客户端：只实现 OpenAI 兼容的 /chat/completions。
 *
 * 一套代码适配所有提供商（MiMo / DeepSeek / 自定义端点 / 本地 llama.cpp / mock），
 * 协议的差异由 **提供商档案（profile）** 决定，而不是散落着按模型名猜：
 *  - 推理控制参数（thinking / reasoning_effort / 都不发）见 agent/ll-params.mjs
 *  - 思考内容 `reasoning_content` 与最终回答 `content` **分开返回**：
 *    思考内容只用于协议要求的上下文回传，绝不进字幕/TTS（否则用户会听到模型自言自语，
 *    还会重复播报）。
 *  - 模型发起工具调用时，assistant 消息必须原样带回 reasoning_content + tool_calls，
 *    否则 MiMo / DeepSeek 在思考模式下会返回 HTTP 400。
 *
 * 支持流式（SSE）与非流式；流式下逐块解析 tool_calls 增量（按 index 归并，
 * 函数名和参数都是分片到达的）。
 */

import { buildProfileFor, buildRequestFields, requiresReasoningEcho, resolveCapability } from './ll-params.mjs'
import { parseModelResponse } from '../config/model-catalog.mjs'

/**
 * 由一份配置（或一个模型方案）生成 LLM 客户端的提供商档案。
 * 壳子里三处（初始化、保存设置、定时切换）都用它，保证"生效的推理策略"只有一套算法。
 */
export function profileFromConfig(source = {}) {
  return buildProfileFor(source)
}

export class LLMError extends Error {
  constructor(message, details = {}) {
    super(message)
    this.name = 'LLMError'
    this.details = details
  }
}

const DEFAULT_TIMEOUT_MS = 60000

/** 请求体里 assistant 消息的 reasoning 字段名（两家的 OpenAI 兼容端点同名） */
const REASONING_FIELD = 'reasoning_content'

/**
 * 组装要放回对话历史的 assistant 消息。
 *
 * 为什么单独导出：loop 和 shell 都要用同一份规则。少带一个字段就是 400，
 * 多带一个字段可能把旧提供商的私有字段发给新提供商，所以规则只能有一份。
 *
 * @param {{text:string, reasoning?:string, toolCalls?:Array, capability?:string,
 *          echoReasoning?:boolean}} result
 * @param {{capability?:string}} options 当前端点的能力；
 *   `echoReasoning` 显式覆盖（Agent 用它按回合快照决定要不要带思考内容）
 */
export function assistantMessageFrom(result, options = {}) {
  const capability = options.capability ?? result.capability ?? ''
  // 只有「官方要求回传」的提供商才带 reasoning_content；
  // 标准 OpenAI 兼容端点带上它属于提供商私有字段，可能直接被参数校验拒掉。
  const echoReasoning = options.echoReasoning ?? requiresReasoningEcho(capability)
  const message = { role: 'assistant' }
  const text = typeof result.text === 'string' ? result.text : ''
  const calls = Array.isArray(result.toolCalls) ? result.toolCalls.filter((call) => call?.valid !== false) : []

  if (calls.length > 0) {
    // OpenAI 兼容协议：有 tool_calls 时 content 允许为 null，但**两家都要求字段存在**
    message.content = text === '' ? null : text
    message.tool_calls = calls.map((call) => ({
      id: call.id,
      type: 'function',
      function: { name: call.name, arguments: call.arguments },
    }))
  } else {
    message.content = text
  }

  if (echoReasoning) {
    const reasoning = typeof result.reasoning === 'string' ? result.reasoning : ''
    // 只在真的想过的时候带。空串与缺省在协议上不是一回事：
    // 模型没思考却回一个空 reasoning，部分实现会当成"上一轮的思考丢了"。
    if (reasoning !== '') message[REASONING_FIELD] = reasoning
  }
  return message
}

export class LLMClient {
  #baseURL
  #apiKey
  #model
  #temperature
  #timeoutMs
  /** 提供商档案：capability / thinking / reasoningEffort / 模型能力元数据 */
  #profile

  constructor({ baseURL, apiKey, model, temperature = 0.3, timeoutMs = DEFAULT_TIMEOUT_MS, profile = {} }) {
    if (typeof baseURL !== 'string' || baseURL === '') throw new Error('baseURL 不能为空')
    if (typeof model !== 'string' || model === '') throw new Error('model 不能为空')
    this.#baseURL = baseURL.replace(/\/+$/u, '')
    this.#apiKey = apiKey ?? ''
    this.#model = model
    this.#temperature = temperature
    this.#timeoutMs = timeoutMs
    this.#profile = { ...profile }
  }

  get model() {
    return this.#model
  }

  /** 运行中换模型（定时切换用），下一条请求即生效 */
  setModel(model) {
    if (typeof model === 'string' && model.trim() !== '') this.#model = model.trim()
  }

  /** 运行中改温度（保存设置后下一轮生效，不重建客户端） */
  setTemperature(temperature) {
    const value = Number(temperature)
    if (Number.isFinite(value)) this.#temperature = value
  }

  /** 运行中改超时：思考模式确实更慢，60 秒上限是实测会撞到的 */
  setTimeout(timeoutMs) {
    const value = Number(timeoutMs)
    if (Number.isFinite(value) && value > 0) this.#timeoutMs = value
  }

  /**
   * 运行中换整套端点（定时切换"供应商"用）：地址、Key、模型一起换。
   * 允许只传一部分——比如同一个供应商换模型时只给 model。
   */
  setEndpoint({ baseURL, apiKey, model } = {}) {
    if (typeof baseURL === 'string' && /^https?:\/\//u.test(baseURL)) {
      this.#baseURL = baseURL.replace(/\/+$/u, '')
    }
    if (typeof apiKey === 'string') this.#apiKey = apiKey
    if (typeof model === 'string' && model.trim() !== '') this.#model = model.trim()
  }

  /**
   * 换提供商档案（保存设置或定时切换后调用）。
   * 只认这个入口来改推理参数，避免"界面改了、请求还是旧的"。
   */
  setProfile(profile = {}) {
    this.#profile = { ...profile }
  }

  /**
   * 取一份**回合快照**：地址、Key、模型、推理档位全部冻结在这一刻。
   *
   * 为什么必须有：一次多步任务要来回打好几趟模型。如果中途用户保存了设置或
   * 定时切换改了提供商，后面的请求就会带着上一个提供商的 reasoning_content /
   * 工具历史打到新端点上——轻则模型答非所问，重则直接 400，用户看到的
   * 就是"任务做到一半断了"。回合用快照，改动只对**下一轮**生效。
   */
  snapshot() {
    return new LLMClient({
      baseURL: this.#baseURL,
      apiKey: this.#apiKey,
      model: this.#model,
      temperature: this.#temperature,
      timeoutMs: this.#timeoutMs,
      profile: { ...this.#profile },
    })
  }

  get profile() {
    return { ...this.#profile }
  }

  /** 当前端点摘要（日志用，不含 Key） */
  get endpoint() {
    return {
      baseURL: this.#baseURL,
      model: this.#model,
      hasKey: this.#apiKey !== '',
      providerId: this.#profile.providerId ?? '',
      capability: resolveCapability(this.#profile),
      thinking: this.#profile.thinking ?? 'default',
      reasoningEffort: this.#profile.reasoningEffort ?? 'default',
    }
  }

  #headers(extra = {}) {
    const headers = { 'content-type': 'application/json', ...extra }
    if (this.#apiKey !== '') headers.authorization = `Bearer ${this.#apiKey}`
    return headers
  }

  /**
   * 组装请求体（导出给测试与"测试模型"路径复用，保证界面测的就是实际发的）。
   * @param {{messages:object[], tools?:object[], stream?:boolean}} options
   */
  buildBody({ messages, tools, stream = true, temperatureOverride } = {}) {
    const sampling = {
      temperature: temperatureOverride ?? this.#temperature,
    }
    const built = buildRequestFields({ ...this.#profile, model: this.#model }, sampling)
    const body = {
      model: this.#model,
      messages,
      stream,
    }
    if (built.temperature !== null) body.temperature = built.temperature
    if (Array.isArray(tools) && tools.length > 0) {
      // 空数组**不能**当 tools 发：多家实现会把 `tools: []` 当非法参数（400）。
      // loop 的收尾往返传空数组，意思就是"这一步别调工具"。
      body.tools = tools
      // MiMo 官方：非 auto 的 tool_choice 会被后端剥掉；DeepSeek 思考模式下
      // required/指定工具直接 400。所以只用 auto，两家都安全。
      body.tool_choice = 'auto'
    }
    Object.assign(body, built.fields)
    return { body, built }
  }

  /**
   * 发一次对话请求。
   * @param {{messages:object[], tools?:object[], signal?:AbortSignal,
   *          onDelta?:(text:string)=>void, onReasoning?:(text:string)=>void, stream?:boolean}} options
   * @returns {Promise<{text:string, reasoning:string, toolCalls:Array, finishReason:string,
   *            usage?:object, capability:string, reasoningContent:boolean}>}
   *   `toolCalls` 里参数不完整/无法解析的条目会带 `valid:false`，调用方**不得**当有效调用执行。
   */
  async chat({ messages, tools, signal, onDelta, onReasoning, stream = true, temperatureOverride } = {}) {
    const { body } = this.buildBody({ messages, tools, stream, temperatureOverride })

    const timeout = AbortSignal.timeout(this.#timeoutMs)
    const composed = signal === undefined ? timeout : AbortSignal.any([signal, timeout])

    let response
    try {
      response = await fetch(`${this.#baseURL}/chat/completions`, {
        method: 'POST',
        headers: this.#headers(),
        body: JSON.stringify(body),
        signal: composed,
      })
    } catch (error) {
      const reason = error?.name === 'TimeoutError' ? `请求超时（${this.#timeoutMs}ms）` : error.message
      throw new LLMError(`无法连接模型服务：${reason}`, { baseURL: this.#baseURL })
    }

    if (!response.ok) {
      const text = await response.text().catch(() => '')
      throw new LLMError(`模型服务返回 ${response.status}：${truncate(text, 400)}`, {
        status: response.status,
        baseURL: this.#baseURL,
        model: this.#model,
      })
    }

    if (!stream) {
      const json = await response.json()
      const message = json?.choices?.[0]?.message ?? {}
      return finalize({
        text: textOf(message.content),
        reasoning: textOf(message[REASONING_FIELD]),
        assembler: new ToolCallAssembler().feed(message.tool_calls),
        finishReason: json?.choices?.[0]?.finish_reason ?? 'stop',
        usage: json?.usage,
        profile: this.profile,
        signal,
      })
    }

    return await this.#consumeStream(response, { onDelta, onReasoning, signal })
  }

  async #consumeStream(response, { onDelta, onReasoning, signal }) {
    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    const assembler = new ToolCallAssembler()
    let buffer = ''
    let text = ''
    let reasoning = ''
    let finishReason = 'stop'
    let usage
    let ended = false
    /** 有没有收到过收尾信号：finish_reason 或 [DONE]，一个都没有就是被截断了 */
    let sawFinish = false
    let sawDone = false
    /** 流里出现过 `delta.error` / `error` 事件（服务端异常传输，不能当正常收尾） */
    let sawError = false

    const pump = (chunk) => {
      // 归一化换行：SSE 允许 \r\n，但事件分隔统一按 \n\n 找最省事。
      // 原始 CR 只可能出现在行分隔符上（JSON 里的换行是转义后的字面 \n），所以安全。
      buffer += chunk.replace(/\r\n/gu, '\n').replace(/\r/gu, '\n')
      let boundary
      while ((boundary = buffer.indexOf('\n\n')) >= 0) {
        const rawEvent = buffer.slice(0, boundary)
        buffer = buffer.slice(boundary + 2)
        for (const line of rawEvent.split('\n')) {
          const trimmed = line.trim()
          if (!trimmed.startsWith('data:')) continue
          const payload = trimmed.slice(5).trim()
          if (payload === '') continue
          if (payload === '[DONE]') { sawDone = true; continue }
          // 服务端错误事件（OpenAI 兼容的 `event: error` / `data: {"error": ...}`）
          if (rawEvent.includes('\nevent: error') || (payload.startsWith('{') && /"error"\s*:/u.test(payload))) {
            sawError = true
            continue
          }
          let json
          try {
            json = JSON.parse(payload)
          } catch {
            continue
          }
          if (json?.error) { sawError = true; continue }
          if (json?.usage) usage = json.usage
          // 有些端点在最后一个 chunk 里把 choices 置空，只带 usage
          const choice = json?.choices?.[0]
          if (choice === undefined) continue
          if (choice.finish_reason) { finishReason = choice.finish_reason; sawFinish = true }
          const delta = choice.delta ?? choice.message ?? {}
          // 个别实现把错误放在 delta.error 上（OpenAI 流式错误规范之一）
          if (delta?.error) { sawError = true; continue }
          if (typeof delta.reasoning_content === 'string' && delta.reasoning_content !== '') {
            reasoning += delta.reasoning_content
            // 思考内容**只**走 onReasoning（默认不接），绝不混进字幕/TTS 的 text_delta
            onReasoning?.(delta.reasoning_content)
          }
          if (typeof delta.content === 'string' && delta.content !== '') {
            text += delta.content
            onDelta?.(delta.content)
          }
          if (Array.isArray(delta.tool_calls)) assembler.feed(delta.tool_calls)
        }
      }
    }

    try {
      for (;;) {
        const { value, done } = await reader.read()
        if (done) { ended = true; break }
        if (signal?.aborted === true) break
        pump(decoder.decode(value, { stream: true }))
      }
      if (!ended) buffer += decoder.decode()
      pump('')
    } catch (error) {
      /*
       * 用户打断（dismiss / barge-in）：把手上已解析的东西如实交出去，
       * 上层会按中断处理——这不是错误，不该弹红字。
       */
      if (signal?.aborted === true) {
        try { await reader.cancel() } catch { /* 已经关掉了不算错 */ }
        return finalize({ text, reasoning, assembler, finishReason, usage, profile: this.profile, signal, interrupted: true, sawError })
      }
      // 超时：必须像非流式那样报错，否则用户只看到"话说到一半没了"，不知道是模型没回。
      if (error?.name === 'TimeoutError' || error?.name === 'AbortError') {
        try { await reader.cancel() } catch { /* 已经关掉了不算错 */ }
        throw new LLMError(`模型响应超时（${this.#timeoutMs}ms），已中止`, {
          baseURL: this.#baseURL, model: this.#model, timeoutMs: this.#timeoutMs,
        })
      }
      throw error
    } finally {
      /*
       * 无论正常结束、打断还是超时，都要取消 reader：
       * 连接已经建立后不 cancel，模型侧的连接会一直挂着（思考模式一次生成十几秒，
       * 攒几个就被中断的回合，端点侧会当成你赖着不走）。
       */
      try { await reader.cancel() } catch { /* 已经关掉了不算错 */ }
    }

    return finalize({ text, reasoning, assembler, finishReason, usage, profile: this.profile, signal,
      incomplete: !sawFinish && !sawDone, sawError })
  }

  /** 拉取该端点可用模型列表（返回聊天模型 id + 能力元数据）。 */
  async listModels({ signal } = {}) {
    const timeout = AbortSignal.timeout(Math.min(this.#timeoutMs, 15000))
    const composed = signal === undefined ? timeout : AbortSignal.any([signal, timeout])
    const response = await fetch(`${this.#baseURL}/models`, { headers: this.#headers(), signal: composed })
    if (!response.ok) {
      const detail = response.status === 401 ? '（API Key 无效或未填）'
        : response.status === 429 ? '（请求过于频繁，稍后再试）' : ''
      throw new LLMError(`模型列表返回 ${response.status}${detail}`, { status: response.status })
    }
    let json
    try {
      json = await response.json()
    } catch {
      throw new LLMError('模型列表返回的不是 JSON', { status: response.status })
    }
    return parseModelResponse(json)
  }
}

/**
 * 流式 tool_calls 归并器。
 * OpenAI 协议里 id/name/arguments 是分片到达的（arguments 常拆成几十片），
 * 按 `index` 归并，并**保留到达顺序**。
 */
class ToolCallAssembler {
  #calls = new Map()

  feed(list) {
    if (!Array.isArray(list)) return this
    for (const part of list) {
      if (part === null || typeof part !== 'object') continue
      const index = Number.isFinite(Number(part.index)) ? Number(part.index) : this.#calls.size
      const current = this.#calls.get(index) ?? { id: '', name: '', arguments: '' }
      if (typeof part.id === 'string' && part.id !== '') current.id = part.id
      if (typeof part.function?.name === 'string') current.name += part.function.name
      if (typeof part.function?.arguments === 'string') current.arguments += part.function.arguments
      this.#calls.set(index, current)
    }
    return this
  }

  /**
   * 收尾：把原始分片变成可执行的调用。
   * 参数不是合法 JSON、或函数名缺失的，标记 `valid:false` 而不是硬执行——
   * 半截参数喂给 CUA（点击、键盘、杀进程）比不调用危险得多。
   */
  done() {
    const out = []
    const entries = [...this.#calls.entries()].sort((a, b) => a[0] - b[0])
    for (const [position, [, call]] of entries.entries()) {
      const id = call.id !== '' ? call.id : `call_${position}`
      const name = call.name.trim()
      const raw = call.arguments
      const record = { id, name, arguments: raw, valid: true }
      if (name === '') {
        record.valid = false
        record.invalidReason = '缺少函数名'
      } else if (raw.trim() === '') {
        // 空参数按 {} 处理是各家 SDK 的通行做法（无参工具就是空对象）
        record.arguments = '{}'
      } else {
        try {
          const parsed = JSON.parse(raw)
          if (parsed === null || typeof parsed !== 'object') throw new Error('参数不是对象')
          record.arguments = raw
        } catch (error) {
          record.valid = false
          record.arguments = raw
          record.invalidReason = `参数不是合法 JSON：${error.message}`
        }
      }
      out.push(record)
    }
    return out
  }
}

/**
 * 收口一次请求的结果。
 *
 * 关键规则：模型说「我要调工具」但工具调用本身不完整（被截断、被中断、参数坏掉）时，
 * **不能**当成有效调用返回，否则上层会拿半截参数去执行动作。
 *
 * 完整性判定（LLMClient 与 Agent 必须一致）：
 *  - 只要出现了 `finish_reason`（任何值）或 `[DONE]`，都算**有可靠结束信号**，
 *    不机械要求每家端点都必须发 `[DONE]`（OpenAI 兼容端点普遍只发 finish_reason）。
 *  - 完全没有结束信号就断流（`incomplete`）→ 不确定响应，任何工具调用都不得执行。
 *  - `finish_reason: 'length'`（或其它非 tool_calls 的截断态）→ 即使参数是合法 JSON，
 *    也只代表模型"说到一半被截断"，调用不完整，不得执行。
 *  - 流内出现 `error` 事件 / `delta.error` → 服务端异常，不得执行。
 */
function finalize({ text, reasoning, assembler, finishReason, usage, profile, signal, interrupted = false, incomplete = false, sawError = false }) {
  const all = assembler.done()
  // 参数本身坏掉的（valid=false）
  const invalid = all.filter((call) => call.valid !== true)
  const aborted = interrupted || signal?.aborted === true
  // 端到端完整性：只有"收到过明确结束信号"才算完整完成。注意 interrupted 时 finishReason 会被改成 'interrupted'，
  // 那同样不是可靠完成信号，aborted 一律视为不完整。
  const reliablyComplete = !aborted && !incomplete && !sawError
  const truncated = !reliablyComplete || finishReason === 'length'
  // 完整结束（finish_reason='tool_calls' 或 [DONE]）才算有效工具调用；截断/异常一律按不完整处理
  const valid = reliablyComplete && !truncated ? all.filter((call) => call.valid === true) : []
  // 截断时，即使参数是合法 JSON，也不得执行——归入 broken（带原因），让 Agent 感知并触发"不执行+提示"
  const broken = truncated
    ? [...invalid, ...all.filter((call) => call.valid === true).map((call) => ({
        ...call,
        valid: false,
        invalidReason: call.invalidReason ?? (finishReason === 'length' ? '响应被长度上限截断' : '响应未完整结束（缺少可靠结束信号）'),
      }))]
    : invalid
  return {
    text,
    reasoning,
    toolCalls: valid,
    brokenToolCalls: broken,
    // 截断/异常时如实标注 finishReason，避免被当成正常 stop 收尾
    finishReason: aborted && finishReason === 'stop' ? 'interrupted'
      : (truncated && finishReason === 'stop' ? 'incomplete' : finishReason),
    usage,
    // 回合内的协议判断靠这两个：能力 + 是否真的产生过思考内容
    capability: resolveCapability(profile),
    reasoningContent: reasoning !== '',
    interrupted: aborted,
    incomplete: incomplete && !aborted,
    // Agent 据此决定"该不该把这次调用当作可执行指令"
    toolCallsIncomplete: truncated || (finishReason === 'tool_calls' && valid.length === 0 && broken.length > 0)
      || (aborted && all.length > 0),
  }
}

function textOf(value) {
  return typeof value === 'string' ? value : ''
}

function truncate(text, max) {
  const value = String(text ?? '')
  return value.length <= max ? value : `${value.slice(0, max)}…`
}
