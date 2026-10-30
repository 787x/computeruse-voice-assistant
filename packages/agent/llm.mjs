/**
 * LLM 客户端：只实现 OpenAI 兼容的 /chat/completions。
 *
 * 一套代码适配所有提供商（DeepSeek / 自定义端点 / 本地 llama.cpp），
 * 因为它们的差异只有 baseURL 和 apiKey，协议是一致的。
 *
 * 支持流式（SSE）与非流式；流式下逐块解析 tool_calls 增量。
 */

export class LLMError extends Error {
  constructor(message, details = {}) {
    super(message)
    this.name = 'LLMError'
    this.details = details
  }
}

const DEFAULT_TIMEOUT_MS = 60000

export class LLMClient {
  #baseURL
  #apiKey
  #model
  #temperature
  #timeoutMs

  constructor({ baseURL, apiKey, model, temperature = 0.3, timeoutMs = DEFAULT_TIMEOUT_MS }) {
    if (typeof baseURL !== 'string' || baseURL === '') throw new Error('baseURL 不能为空')
    if (typeof model !== 'string' || model === '') throw new Error('model 不能为空')
    this.#baseURL = baseURL.replace(/\/+$/u, '')
    this.#apiKey = apiKey ?? ''
    this.#model = model
    this.#temperature = temperature
    this.#timeoutMs = timeoutMs
  }

  get model() {
    return this.#model
  }

  /** 运行中换模型（定时切换用），下一条请求即生效 */
  setModel(model) {
    if (typeof model === 'string' && model.trim() !== '') this.#model = model.trim()
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

  /** 当前端点摘要（日志用，不含 Key） */
  get endpoint() {
    return { baseURL: this.#baseURL, model: this.#model, hasKey: this.#apiKey !== '' }
  }

  #headers() {
    const headers = { 'content-type': 'application/json' }
    if (this.#apiKey !== '') headers.authorization = `Bearer ${this.#apiKey}`
    return headers
  }

  /**
   * 发一次对话请求。
   * @param {{messages:object[], tools?:object[], signal?:AbortSignal, onDelta?:(text:string)=>void, stream?:boolean}} options
   * @returns {Promise<{text:string, toolCalls:{id:string,name:string,arguments:string}[], finishReason:string, usage?:object}>}
   */
  async chat({ messages, tools, signal, onDelta, stream = true }) {
    const body = {
      model: this.#model,
      messages,
      temperature: this.#temperature,
      stream,
    }
    if (Array.isArray(tools) && tools.length > 0) {
      body.tools = tools
      body.tool_choice = 'auto'
    }

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
      return {
        text: message.content ?? '',
        toolCalls: normalizeToolCalls(message.tool_calls),
        finishReason: json?.choices?.[0]?.finish_reason ?? 'stop',
        usage: json?.usage,
      }
    }

    return await this.#consumeStream(response, onDelta)
  }

  async #consumeStream(response, onDelta) {
    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let text = ''
    const calls = new Map()
    let finishReason = 'stop'
    let usage

    for (;;) {
      const { value, done } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })

      let boundary
      while ((boundary = buffer.indexOf('\n\n')) >= 0) {
        const rawEvent = buffer.slice(0, boundary)
        buffer = buffer.slice(boundary + 2)
        for (const line of rawEvent.split('\n')) {
          const trimmed = line.trim()
          if (!trimmed.startsWith('data:')) continue
          const payload = trimmed.slice(5).trim()
          if (payload === '[DONE]') continue
          let json
          try {
            json = JSON.parse(payload)
          } catch {
            continue
          }
          const choice = json?.choices?.[0]
          if (json?.usage) usage = json.usage
          if (choice === undefined) continue
          if (choice.finish_reason) finishReason = choice.finish_reason
          const delta = choice.delta ?? {}
          if (typeof delta.content === 'string' && delta.content !== '') {
            text += delta.content
            onDelta?.(delta.content)
          }
          if (Array.isArray(delta.tool_calls)) {
            for (const part of delta.tool_calls) {
              const index = Number(part.index ?? 0)
              const current = calls.get(index) ?? { id: '', name: '', arguments: '' }
              if (part.id) current.id = part.id
              if (part.function?.name) current.name += part.function.name
              if (part.function?.arguments) current.arguments += part.function.arguments
              calls.set(index, current)
            }
          }
        }
      }
    }

    return {
      text,
      toolCalls: [...calls.entries()].sort((a, b) => a[0] - b[0]).map(([, v], i) => ({
        id: v.id || `call_${i}`,
        name: v.name,
        arguments: v.arguments,
      })),
      finishReason,
      usage,
    }
  }

  /** 拉取该端点可用模型列表（配置页的"测试连接"会用到）。 */
  async listModels({ signal } = {}) {
    const timeout = AbortSignal.timeout(Math.min(this.#timeoutMs, 15000))
    const composed = signal === undefined ? timeout : AbortSignal.any([signal, timeout])
    const response = await fetch(`${this.#baseURL}/models`, { headers: this.#headers(), signal: composed })
    if (!response.ok) throw new LLMError(`模型列表返回 ${response.status}`, { status: response.status })
    const json = await response.json()
    return (json?.data ?? []).map((entry) => entry.id).filter(Boolean)
  }
}

function normalizeToolCalls(list) {
  if (!Array.isArray(list)) return []
  return list.map((call, index) => ({
    id: call.id ?? `call_${index}`,
    name: call.function?.name ?? '',
    arguments: call.function?.arguments ?? '{}',
  }))
}

function truncate(text, max) {
  const value = String(text ?? '')
  return value.length <= max ? value : `${value.slice(0, max)}…`
}
