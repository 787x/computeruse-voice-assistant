/**
 * Agent loop（P1 核心）。
 *
 * 职责边界：只负责"想和调用工具"，不碰音频、不碰 UI、不碰权限实现细节。
 * 权限通过注入的 `gate(decision)` 回调外部化，P3 再把 PermissionGate 接进来。
 *
 * 关键设计：
 *  - 工具结果是文本信封（上游 format.js 生成），模型只读文本，不直接吃图片
 *  - 上下文只保留最近 2 条完整工具结果，更早的压成一行摘要（截图会撑爆上下文）
 *  - 每一步都可中断：所有 await 前都有 signal 检查
 */

/** 默认系统提示词的唯一来源见 system-prompt.mjs（设置页显示的是同一份文本）。 */
import { DEFAULT_SYSTEM_PROMPT, turnSystemPrompt } from './system-prompt.mjs'

const TOOL_RESULT_KEEP = 2
const TOOL_RESULT_MAX_CHARS = 1200
/** 一轮里最多把几张图塞给模型（截图多的任务很容易把上下文挤爆） */
const MAX_IMAGES_PER_RUN = 4
const COMPRESS_AFTER_MESSAGES = 12
/** 会话记忆：最多带几轮、多少字（多了既贵又容易跑偏） */
const HISTORY_MAX_MESSAGES = 10
const HISTORY_MAX_CHARS = 3000

/**
 * 整理会话记忆：只保留最近若干条，且必须以 user 开头。
 *
 * 为什么必须以 user 开头：消息序列若从 assistant 开始，部分模型会认为
 * "上一条是我的发言，用户还没说话"，进而答非所问（不同厂商实现差异很大）。
 */
function trimHistory(history) {
  if (!Array.isArray(history) || history.length === 0) return []
  const usable = history.filter((m) => (m.role === 'user' || m.role === 'assistant') && typeof m.content === 'string' && m.content !== '')
  const recent = usable.slice(-HISTORY_MAX_MESSAGES)
  // 从头去掉 assistant，直到第一条是 user
  while (recent.length > 0 && recent[0].role !== 'user') recent.shift()

  // 再从最老的开始丢，直到总字数达标
  let total = recent.reduce((sum, m) => sum + m.content.length, 0)
  while (recent.length > 1 && total > HISTORY_MAX_CHARS) {
    total -= recent[0].content.length
    recent.shift()
    while (recent.length > 0 && recent[0].role !== 'user') {
      total -= recent[0].content.length
      recent.shift()
    }
  }
  return recent
}

export class Agent {
  #llm
  #tools
  /** 提示词来源：文本，或返回配置的函数（缓存的 Agent 靠它拿到最新设置） */
  #systemPrompt
  #maxSteps
  #onEvent
  #gate

  /**
   * @param {object} options
   * @param {{chat:Function}} options.llm
   * @param {{listTools:Function, invoke:Function}} options.tools
   * @param {string|(()=>unknown)} [options.systemPrompt] 文本，或返回文本/null 的函数；
   *   null/undefined → 默认提示词。传函数时每轮开始时求值一次（改设置不用重建 Agent）。
   * @param {number} [options.maxSteps]
   * @param {(event:{type:string,[k:string]:unknown})=>void} [options.onEvent]
   * @param {(ctx:{tool:string,args:unknown})=>Promise<{allow:boolean,reason?:string}>} [options.gate]
   */
  constructor({ llm, tools, systemPrompt = DEFAULT_SYSTEM_PROMPT, maxSteps = 8, onEvent, gate }) {
    this.#llm = llm
    this.#tools = tools
    this.#systemPrompt = systemPrompt
    this.#maxSteps = maxSteps
    this.#onEvent = onEvent ?? (() => {})
    this.#gate = gate ?? (async () => ({ allow: true }))
  }

  /** 此刻会生效的提示词（诊断/验收用；不代表正在跑的回合，回合用开始时的快照） */
  get systemPrompt() {
    return turnSystemPrompt(this.#systemPrompt)
  }

  /**
   * 跑一个用户回合。
   * @param {string} input 用户说的话（已由 ASR 转成文本）
   * @param {{signal?:AbortSignal, history?:Array<{role:string,content:string}>, systemPrompt?:string|(()=>unknown)}} [options]
   *   history 是**同一个会话**里之前几轮的用户/助手发言（不含工具细节）。
   *   每次唤醒由调用方清空，于是"一次唤醒 = 一次会话"。
   *   systemPrompt 覆盖构造函数的来源（**本轮快照**）。
   */
  async run(input, { signal, history = [], onEvent = this.#onEvent, systemPrompt = this.#systemPrompt } = {}) {
    const emit = (event) => { if (!signal?.aborted) onEvent(event) }
    /*
     * 提示词在回合开始时求值一次，整个回合（含后续多步工具调用和收尾往返）都用这一份。
     * 中途改设置不能把人设换掉——用户会看到"前后两半不像同一个人"，
     * 而且工具调用序列里混进新的 system 消息很容易让模型重新解释任务。
     */
    const turnPrompt = turnSystemPrompt(systemPrompt)
    const messages = [
      { role: 'system', content: turnPrompt },
      ...trimHistory(history),
      { role: 'user', content: input },
    ]
    const steps = []
    const availableTools = this.#tools.listTools()
    let finalText = ''
    let hitStepLimit = false
    /** 本轮已经塞给模型多少张图 */
    let imagesAttached = 0

    for (let step = 1; step <= this.#maxSteps; step += 1) {
      throwIfAborted(signal)

      const result = await this.#llm.chat({
        messages,
        tools: availableTools,
        signal,
        stream: true, // 必须流式：首字和"先说话后动手"的白描都依赖它
        onDelta: (text) => emit({ type: 'text_delta', text }),
      })
      throwIfAborted(signal)

      if (result.text) finalText += result.text

      if (result.toolCalls.length === 0) {
        emit({ type: 'assistant', text: result.text, step })
        break
      }

      messages.push({
        role: 'assistant',
        content: result.text || null,
        tool_calls: result.toolCalls.map((call) => ({
          id: call.id,
          type: 'function',
          function: { name: call.name, arguments: call.arguments },
        })),
      })

      for (const call of result.toolCalls) {
        throwIfAborted(signal)
        const args = safeParseArgs(call.arguments)

        emit({ type: 'tool_start', name: call.name, args, step })

        throwIfAborted(signal)
        const verdict = await this.#gate({ tool: call.name, args, signal })
        throwIfAborted(signal)
        if (verdict?.allow !== true) {
          const denied = `已拒绝：${verdict?.reason ?? '权限不足'}`
          messages.push({ role: 'tool', tool_call_id: call.id, content: denied })
          steps.push({ step, tool: call.name, ok: false, text: denied })
          emit({ type: 'tool_end', name: call.name, ok: false, text: denied, step })
          continue
        }

        const started = Date.now()
        try {
          const invoked = await this.#tools.invoke(call.name, args, { signal })
          throwIfAborted(signal)
          const text = clamp(invoked.text ?? '', TOOL_RESULT_MAX_CHARS)
          messages.push({ role: 'tool', tool_call_id: call.id, content: text })
          steps.push({ step, tool: call.name, ok: true, text, ms: Date.now() - started })
          emit({ type: 'tool_end', name: call.name, ok: true, text, step, ms: Date.now() - started })

          /*
           * 截图这类工具会带回一张图。**必须把图塞进对话**，否则模型只看到
           * "图存在某个路径"的文字——它对屏幕就是瞎的（这正是"任务做一半"的根因）。
           * OpenAI 兼容协议里图片只能放在 user 消息的 content 数组上，所以单独补一条。
           * 一轮最多带 4 张：再多既贵又容易把上下文挤爆。
           */
          if (invoked?.image !== undefined && invoked.image !== null && imagesAttached < MAX_IMAGES_PER_RUN) {
            imagesAttached += 1
            messages.push({
              role: 'user',
              content: [
                { type: 'text', text: `（这是 ${call.name} 刚返回的图，${invoked.image.width ?? '?'}x${invoked.image.height ?? '?'}）` },
                { type: 'image_url', image_url: { url: `data:${invoked.image.mediaType ?? 'image/jpeg'};base64,${invoked.image.base64}` } },
              ],
            })
          }
        } catch (error) {
          throwIfAborted(signal)
          const message = `工具执行失败：${error instanceof Error ? error.message : String(error)}`
          messages.push({ role: 'tool', tool_call_id: call.id, content: message })
          steps.push({ step, tool: call.name, ok: false, text: message })
          emit({ type: 'tool_end', name: call.name, ok: false, text: message, step })
        }
      }

      compress(messages)
      // 这是最后一步，且这一步还在调工具 → 走完就撞上限，需要补一次交代
      if (step === this.#maxSteps) hitStepLimit = true
    }

    /*
     * 步数用完时**必须补一次"交代"往返**。
     *
     * 之前的行为是：最后一步工具执行完，往上下文塞一句"达到上限，先停下汇报"，
     * 然后循环直接结束——模型根本没机会说话，用户听到的就是"话说到一半停了"。
     * 现在多给一次不带工具的往返，让它用一句话说清楚做到哪、还差什么。
     */
    throwIfAborted(signal)
    if (hitStepLimit && steps.length > 0) {
      try {
        const closing = await this.#llm.chat({
          messages: [
            ...messages,
            {
              role: 'user',
              content: `（已经用满 ${this.#maxSteps} 步，不能再调工具了。用一句话告诉用户：你做到哪一步、还差什么、需要他怎么配合。）`,
            },
          ],
          tools: [],
          signal,
          stream: true,
          onDelta: (text) => emit({ type: 'text_delta', text }),
        })
        throwIfAborted(signal)
        if (closing.text) {
          finalText += closing.text
          emit({ type: 'assistant', text: closing.text, step: this.#maxSteps + 1, limited: true })
        }
      } catch (error) {
        throwIfAborted(signal)
        emit({ type: 'assistant', text: `（步数用完了，没能收尾：${error.message}）`, step: this.#maxSteps + 1, limited: true })
      }
    }

    emit({ type: 'done', text: finalText, steps: steps.length, limited: hitStepLimit })
    return { text: finalText, steps, limited: hitStepLimit }
  }
}

function safeParseArgs(raw) {
  if (typeof raw !== 'string' || raw.trim() === '') return {}
  try {
    return JSON.parse(raw)
  } catch {
    return { __raw: raw }
  }
}

function clamp(text, max) {
  const value = String(text ?? '')
  return value.length <= max ? value : `${value.slice(0, max)}…（截断）`
}

/**
 * 上下文压缩：只保留最近 N 条工具结果，更早的替换成一行摘要。
 * 截图结果单条可达数 KB，不压缩的话多步任务必然超长。
 */
function compress(messages) {
  if (messages.length <= COMPRESS_AFTER_MESSAGES) return
  const toolIndexes = []
  for (let i = 0; i < messages.length; i += 1) {
    if (messages[i].role === 'tool') toolIndexes.push(i)
  }
  const keepFrom = toolIndexes.length - TOOL_RESULT_KEEP
  for (let i = 0; i < toolIndexes.length; i += 1) {
    if (i >= keepFrom) continue
    const target = messages[toolIndexes[i]]
    if (target.content.startsWith('（已压缩）')) continue
    const firstLine = String(target.content).split('\n')[0].slice(0, 80)
    target.content = `（已压缩）${firstLine}`
  }
}

function throwIfAborted(signal) {
  if (signal?.aborted === true) {
    const error = new Error('已中断')
    error.name = 'AbortError'
    throw error
  }
}
