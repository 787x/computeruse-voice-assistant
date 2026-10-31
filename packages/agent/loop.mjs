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
 *  - **思考内容（reasoning_content）与最终回答分开**：思考内容只放回上下文（MiMo /
 *    DeepSeek 在思考模式下做工具调用时不回传会 HTTP 400），绝不进字幕/TTS；
 *    裁剪历史时按"完整消息组"下手，不留孤立的工具结果或缺 thinking 字段的工具调用
 *  - 参数不完整/解析失败的工具调用**不执行**，只把失败原因回给模型
 */

/** 默认系统提示词的唯一来源见 system-prompt.mjs（设置页显示的是同一份文本）。 */
import { DEFAULT_SYSTEM_PROMPT, turnSystemPrompt } from './system-prompt.mjs'
import { assistantMessageFrom } from './llm.mjs'

const TOOL_RESULT_KEEP = 2
const TOOL_RESULT_MAX_CHARS = 1200
/** 一轮里最多把几张图塞给模型（截图多的任务很容易把上下文挤爆） */
const MAX_IMAGES_PER_RUN = 4
const COMPRESS_AFTER_MESSAGES = 12
/** 会话记忆：最多带几轮、多少字（多了既贵又容易跑偏） */
const HISTORY_MAX_MESSAGES = 10
const HISTORY_MAX_CHARS = 3000
/**
 * 被压掉的老思考内容留多少字。
 * 字段本身**必须保留**（MiMo/DeepSeek 的回传要求），但十几步任务的思维链能长到几万字，
 * 不压就一定撑爆上下文。留个开头既保住协议要求的字段，也把token花在今天真正要做的事上。
 */
const REASONING_KEPT_CHARS = 120

/**
 * 整理会话记忆：只保留最近若干条，且必须以 user 开头。
 *
 * 为什么必须以 user 开头：消息序列若从 assistant 开始，部分模型会认为
 * "上一条是我的发言，用户还没说话"，进而答非所问（不同厂商实现差异很大）。
 *
 * 只留纯文本的 user/assistant：工具调用链属于**某一个回合**，跨回合带过去会留下
 * "有 tool_calls 却没有对应 tool 结果"的残缺链（协议上就是 400 的来源），
 * 而思考内容是提供商私有字段，更不该跟着跨提供商跑。
 */
function trimHistory(history) {
  if (!Array.isArray(history) || history.length === 0) return []
  const usable = history
    .filter((m) => (m.role === 'user' || m.role === 'assistant') && typeof m.content === 'string' && m.content !== '')
    .map((m) => ({ role: m.role, content: m.content }))
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

  /** 此刻会生效的步数上限 */
  get maxSteps() {
    return this.#maxSteps
  }

  /**
   * 运行中改步数上限（保存设置后下一轮生效，不重建 Agent）。
   * 正在跑的回合已经按旧上限在走循环，不受影响——上限只在循环开始时比较。
   */
  setMaxSteps(maxSteps) {
    const value = Number(maxSteps)
    if (Number.isFinite(value) && value >= 1) this.#maxSteps = Math.min(20, Math.max(1, Math.round(value)))
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
     * 端点快照：整回合用同一套「地址 + Key + 模型 + 推理档位」。
     * 中途保存设置或定时切换都不能把正在跑的工具调用链换到另一个提供商上——
     * 旧提供商的 reasoning_content / 工具历史发给新提供商就是 400。
     * 没有 snapshot()（mock、测试注入的假 llm）就直接用它本身。
     */
    const llm = typeof this.#llm.snapshot === 'function' ? this.#llm.snapshot() : this.#llm
    /*
     * 步数上限同样是回合快照：中途保存把它调小，不该把正在跑的多步任务从中间掐断
     * （那等于"话说到一半停了"，而且残留在上下文里的工具调用链不完整）。
     */
    const maxSteps = this.#maxSteps
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
    /** 本轮累计的思考内容字数（只记长度，不落盘、不播报） */
    let reasoningChars = 0

    for (let step = 1; step <= maxSteps; step += 1) {
      throwIfAborted(signal)

      const result = await llm.chat({
        messages,
        tools: availableTools,
        signal,
        stream: true, // 必须流式：首字和"先说话后动手"的白描都依赖它
        onDelta: (text) => emit({ type: 'text_delta', text }),
      })
      throwIfAborted(signal)

      if (result.text) finalText += result.text
      if (result.reasoning) {
        reasoningChars += result.reasoning.length
        // 只带长度，不带内容：思考过程不播报、不入库
        emit({ type: 'reasoning', step, chars: result.reasoning.length })
      }
      const toolCalls = result.toolCalls ?? []
      const broken = result.brokenToolCalls ?? []

      if (toolCalls.length === 0 && broken.length === 0) {
        emit({ type: 'assistant', text: result.text, step })
        break
      }

      /*
       * 模型这一轮想调工具，但调用本身残缺（流被截断、参数不是合法 JSON、缺函数名）。
       * 绝不能拿半截参数去执行——点击/键盘/杀进程这类动作，参数错一位就是事故。
       *
       * 处理方式：assistant 消息里**只写成功的调用**（残缺的那些不进 tool_calls，
       * 也就不给它们补 tool 结果——孤立的 tool 消息同样是协议错误），
       * 思考内容与文本照常回传（协议要求），再用一条 user 消息如实说明没执行。
       */
      if (toolCalls.length === 0) {
        messages.push(assistantMessageFrom(result))
        messages.push({
          role: 'user',
          content: `（上一步的工具调用参数不完整，没有执行${broken.map((call) => call.name ? `（${call.name}）` : '').join('')}。`
            + '请重新给出完整参数，或直接用一句话告诉用户做不到以及原因。）',
        })
        for (const call of broken) {
          steps.push({ step, tool: call.name || '(未知工具)', ok: false, text: '参数不完整，未执行' })
          emit({ type: 'tool_end', name: call.name || '(未知工具)', ok: false, text: '参数不完整，未执行', step })
        }
        compress(messages)
        if (step === maxSteps) hitStepLimit = true
        continue
      }

      // 混合情况（有的能解析、有的坏了）：assistant 只带能用的那些，坏的直接不认
      if (broken.length > 0) {
        emit({ type: 'tool_end', name: broken.map((call) => call.name || '(未知工具)').join(', '), ok: false, text: '参数不完整，已丢弃', step })
      }
      messages.push(assistantMessageFrom(result))

      for (const call of toolCalls) {
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
      if (step === maxSteps) hitStepLimit = true
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
        const closing = await llm.chat({
          messages: [
            ...messages,
            {
              role: 'user',
              content: `（已经用满 ${maxSteps} 步，不能再调工具了。用一句话告诉用户：你做到哪一步、还差什么、需要他怎么配合。）`,
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
          emit({ type: 'assistant', text: closing.text, step: maxSteps + 1, limited: true })
        }
      } catch (error) {
        throwIfAborted(signal)
        emit({ type: 'assistant', text: `（步数用完了，没能收尾：${error.message}）`, step: maxSteps + 1, limited: true })
      }
    }

    emit({ type: 'done', text: finalText, steps: steps.length, limited: hitStepLimit, reasoningChars })
    /*
     * 把这一回合用的端点快照一起返回：回合之后的旁路请求（比如"还要不要继续听"的判定）
     * 必须用**同一套**提供商与推理参数，否则中途改了设置会让那次判定换个模型、
     * 甚至换个提供商，判出来的东西和刚说的这段话不是一套。
     */
    return { text: finalText, steps, limited: hitStepLimit, reasoningChars, llm }
  }
}

function safeParseArgs(raw) {
  if (typeof raw !== 'string' || raw.trim() === '') return {}
  try {
    const parsed = JSON.parse(raw)
    return parsed !== null && typeof parsed === 'object' ? parsed : { __raw: raw }
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
 *
 * 边界（很重要）：这里**只改内容、不删消息**。
 * 删掉一条 tool 结果，或者抹掉带 tool_calls 的 assistant 的 reasoning_content，
 * 都会让 MiMo / DeepSeek 在思考模式下直接 400，所以只能就地缩短。
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

  /*
   * 思考内容同理：保留最近若干条完整的，更早的只留开头。
   * 字段一定留着（协议要求回传），值变短（省 token）。
   */
  const reasoningIndexes = []
  for (let i = 0; i < messages.length; i += 1) {
    if (messages[i].role === 'assistant' && typeof messages[i].reasoning_content === 'string') reasoningIndexes.push(i)
  }
  const keepReasoningFrom = reasoningIndexes.length - TOOL_RESULT_KEEP
  for (let i = 0; i < reasoningIndexes.length; i += 1) {
    if (i >= keepReasoningFrom) continue
    const target = messages[reasoningIndexes[i]]
    const value = target.reasoning_content
    if (value.length <= REASONING_KEPT_CHARS) continue
    target.reasoning_content = `${value.slice(0, REASONING_KEPT_CHARS)}…（较早的思考已折叠）`
  }
}

function throwIfAborted(signal) {
  if (signal?.aborted === true) {
    const error = new Error('已中断')
    error.name = 'AbortError'
    throw error
  }
}
