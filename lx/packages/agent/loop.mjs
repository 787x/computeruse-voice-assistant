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
import { resolveCapability, requiresReasoningEcho } from './ll-params.mjs'

const TOOL_RESULT_MAX_CHARS = 1200
/** 一轮里最多把几张图塞给模型（截图多的任务很容易把上下文挤爆） */
const MAX_IMAGES_PER_RUN = 4
const COMPRESS_AFTER_MESSAGES = 12
/**
 * 会话记忆：最多带几轮、多少字。
 *
 * 注意这里不是"把历史压短"，而是**按完整交互单元裁剪**：一旦裁剪掉某个早期用户回合，
 * 连同它的 assistant 回答、reasoning_content、工具调用和 tool 结果一起丢弃，
 * 绝不对保留的 `reasoning_content` 做任何截断/改写/占位。
 */
const HISTORY_MAX_MESSAGES = 10
const HISTORY_MAX_CHARS = 3000
/** 单回合上下文总字符预算（超出时同样按完整交互单元丢弃最老的单元，不截断思考内容） */
const CONTEXT_BUDGET_CHARS = 40000

/**
 * 底部“裁剪历史交给模型”时，按**完整交互单元**保留最近若干条 user/assistant/tool。
 *
 * 与旧实现的关键差异：
 *  - 不再把 messages 剥成只有 role/content 两个字段——保留工具调用和配对 tool 结果，
 *    也保留 `reasoning_content`（完整、不截断、不改写）。
 *  - 只保留“完整单元”：user 开头，其后的 assistant/tool 全部配对（tool 有对应 tool_calls，
 *    有 tool_calls 的 assistant 有对应 tool 结果）。残缺单元整组剔除，绝不留孤立 tool 或
 *    缺失工具调用的 assistant。
 *  - `providerCapability` 用于跨提供商：旧提供商（MiMo/DeepSeek）的 reasoning_content 是
 *    新提供商不认识的私有字段，发过去会污染请求；此时剥掉该字段但保留普通 content。
 *
 * 返回的数组可以直接塞进请求体（不重新按 role/content 重建）。
 */
function trimHistory(history, { providerCapability = '', providerId = '' } = {}) {
  if (!Array.isArray(history) || history.length === 0) return []
  const messages = history.map((m) => ({ ...m }))

  // 1) 切分成完整交互单元：每个单元 = [user, assistant?, tool*]（assistant 与 tool 配对）
  const units = []
  let current = null
  /** 当前单元的 assistant 声明的 tool_call ids */
  let pendingIds = new Set()
  for (const msg of messages) {
    if (msg.role === 'user') {
      if (current !== null) { units.push(current); current = null }
      current = { head: msg, calls: [] }
      pendingIds = new Set()
      continue
    }
    if (current === null) {
      // 遇到 user 之前出现 assistant/tool：残缺开头，跳过（不以这一条开始）
      continue
    }
    if (msg.role === 'assistant') {
      current.calls.push(msg)
      if (Array.isArray(msg.tool_calls)) {
        for (const call of msg.tool_calls) {
          if (call?.id !== undefined) pendingIds.add(String(call.id))
        }
      }
      continue
    }
    if (msg.role === 'tool') {
      if (msg.tool_call_id !== undefined && pendingIds.has(String(msg.tool_call_id))) {
        current.calls.push(msg)
        // 匹配到结果后可继续，但不需要从 pendingIds 删除：一个 call 允许多个 tool 结果（不太规范但安全）
      } else {
        // 孤立 tool 结果：没有对应的 assistant tool_calls，整组不保留
        current.calls.length = 0
        current = null
        pendingIds = new Set()
      }
      continue
    }
    // 其它角色（system 等）不属于交互单元，忽略
  }
  if (current !== null) units.push(current)

  // 剔除缺失工具结果的 assistant 单元：assistant 声明了 tool_calls 但单元内没有对应 tool
  const complete = units.filter((unit) => {
    const ids = new Set()
    for (const m of unit.calls) {
      if (m.role === 'assistant' && Array.isArray(m.tool_calls)) {
        for (const call of m.tool_calls) {
          if (call?.id !== undefined) ids.add(String(call.id))
        }
      }
    }
    const results = new Set(unit.calls.filter((m) => m.role === 'tool').map((m) => String(m.tool_call_id)))
    // 每个声明的 id 都必须有结果（允许结果多于声明，那是冗余但无害）
    for (const id of ids) {
      if (!results.has(id)) return false
    }
    return true
  })

  // 2) 从最老开始丢弃单元，直到不超过消息数与字符预算
  let kept = complete.slice(-HISTORY_MAX_MESSAGES)
  const totalChars = () => kept.reduce((sum, u) => sum + unitChars(u), 0)
  while (kept.length > 1 && totalChars() > HISTORY_MAX_CHARS) {
    kept = kept.slice(1)
  }
  // 单回合上下文预算：保留最近的完整单元（不截断任何一条消息）
  if (kept.length > 1 && totalChars() > CONTEXT_BUDGET_CHARS) {
    kept = kept.slice(-1)
  }

  // 3) 重组为平铺消息
  const flat = []
  for (const unit of kept) {
    flat.push(unit.head)
    for (const m of unit.calls) flat.push(m)
  }

  // 4) 跨提供商时不把旧提供商的 reasoning_content 发给新端点
  //    判断依据：消息上若带 `_providerId`（rememberTurn 从回合快照带上来的），
  //    与当前 providerId 不同 → 该 reasoning 属于旧家，剥离；
  //    没有标记的（老会话/外部构造的历史），按当前提供商是否需要回传闻决定：
  //    需要回传（MiMo/DeepSeek）默认保留，否则剥离。
  for (const m of flat) {
    if (m.role !== 'assistant' || typeof m.reasoning_content !== 'string') continue
    const fromOldProvider = m._providerId !== undefined
      && providerId !== '' && String(m._providerId) !== String(providerId)
    if (fromOldProvider || !requiresReasoningEcho(providerCapability)) {
      delete m.reasoning_content
    }
    // `_providerId` 只是内部追踪标记，绝不能发到模型请求里
    delete m._providerId
  }
  return flat
}

/** 一个交互单元的字符数（content 可能是字符串或数组） */
function unitChars(unit) {
  let total = typeof unit.head.content === 'string' ? unit.head.content.length : JSON.stringify(unit.head.content ?? '').length
  for (const m of unit.calls) {
    total += typeof m.content === 'string' ? m.content.length : JSON.stringify(m.content ?? '').length
  }
  return total
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
   * @param {{signal?:AbortSignal, history?:Array<object>, systemPrompt?:string|(()=>unknown)}} [options]
   *   history 是**同一个会话**里之前几轮的消息（可含 reasoning_content / tool_calls / tool 结果）。
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
    // 当前回合能力的唯一依据：LLM 客户端档案（注册表优先，不会被旧 providerHint 覆盖）
    const capability = typeof llm.profile !== 'undefined'
      ? resolveCapability(llm.profile)
      : (typeof llm.capability === 'string' ? llm.capability : '')
    const providerId = typeof llm.profile?.providerId === 'string' ? llm.profile.providerId : ''
    const messages = [
      { role: 'system', content: turnPrompt },
      ...trimHistory(history, { providerCapability: capability, providerId }),
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

      /*
       * 响应完整性硬闸：LLMClient 只要判定这次响应不完整（截断 / length / 无结束信号 /
       * 服务端错误事件 / 打断），`toolCallsIncomplete` 就会为 true —— 此时**一个工具都不执行**，
       * 即使个别调用参数已经是合法 JSON（"模型说到一半"并不是可安全执行的指令）。
       * 已流出的普通文字仍然保留；不重复执行已完成的工具（本轮没有执行任何工具）。
       */
      if (result.toolCallsIncomplete === true) {
        if (toolCalls.length > 0 || broken.length > 0) {
          messages.push(assistantMessageFrom(result))
          messages.push({
            role: 'user',
            content: `（上一步的模型响应不完整${result.finishReason === 'length' ? '（输出被长度上限截断）' : ''}，`
              + '未执行任何工具调用。请重新组织你的回答；如果需要工具，请完整地重新发起调用。）',
          })
          for (const call of [...toolCalls, ...broken]) {
            steps.push({ step, tool: call.name || '(未知工具)', ok: false, text: '响应不完整，未执行' })
            emit({ type: 'tool_end', name: call.name || '(未知工具)', ok: false, text: '响应不完整，未执行', step })
          }
          compress(messages)
          if (step === maxSteps) hitStepLimit = true
          continue
        }
      }

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
    /*
     * 同时返回本回合完整会话（含 reasoning_content / tool_calls / tool 结果），
     * 供主进程 `rememberTurn` 做跨用户回合记忆——同一唤醒会话的下一轮提问需要这些字段
     * 才能满足 MiMo/DeepSeek 思考模式的回传要求。这里只做浅拷贝 + 深拷贝 content，
     * 不把 system 消息带出去（主进程记忆不需要人设）。
     */
    const conversation = messages
      .filter((m) => m.role !== 'system')
      .map((m) => ({
        ...m,
        // `_providerId` 是内部追踪标记：跨回合记忆时用它判断历史推理属于哪个提供商，
        // 供 trimHistory 跨提供商剥离。它绝不进请求体（trimHistory 会删）。
        _providerId: providerId,
        content: typeof m.content === 'string' ? m.content : (m.content === undefined || m.content === null ? null : JSON.parse(JSON.stringify(m.content))),
        ...(m.tool_calls !== undefined ? { tool_calls: JSON.parse(JSON.stringify(m.tool_calls)) } : {}),
        ...(m.reasoning_content !== undefined ? { reasoning_content: m.reasoning_content } : {}),
        ...(m.tool_call_id !== undefined ? { tool_call_id: m.tool_call_id } : {}),
      }))
    return { text: finalText, steps, limited: hitStepLimit, reasoningChars, llm, conversation, providerId }
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
 * 回合内上下文压缩：只按**完整交互单元**移除最老的 user 轮。
 *
 * 旧实现会把较早的工具结果压成“（已压缩）首行”，并把 reasoning_content 截成 120 字——
 * 这不符合 MiMo/DeepSeek 回传要求：保留在上下文里的推理内容必须与原始响应一致。
 *
 * 新行为：
 *  - 不再截断/改写任何 `reasoning_content`。
 *  - 上下文超长时，从最老的 user 单元开始整体删除（user + assistant + tool 结果一起），
 *    不留下孤立 tool、不留下缺结果的 tool_calls。
 */
function compress(messages) {
  if (messages.length <= COMPRESS_AFTER_MESSAGES) return
  // 仍给一个宽松上限：大多数正常回合不会触发；超长时丢弃最老的完整单元。
  const budget = CONTEXT_BUDGET_CHARS
  if (messageLength(messages) <= budget) return

  const units = splitCompleteUnits(messages)
  // 从最老开始丢弃完整单元，直到不超过预算
  let kept = units
  while (kept.length > 1 && messageLength(flattenUnits(kept)) > budget) {
    kept = kept.slice(1)
  }
  const flat = flattenUnits(kept)
  // 原地替换 messages（保留对调用方数组的引用）
  messages.splice(0, messages.length, ...flat)
}

/** 回合内完整交互单元拆分：user 开头，assistant/tool 配对，残缺单元整体剔除 */
function splitCompleteUnits(messages) {
  const units = []
  let current = null
  let pendingIds = new Set()
  const flush = (unit) => { if (unit !== null && unitComplete(unit)) units.push(unit) }
  for (const msg of messages) {
    if (msg.role === 'user') {
      flush(current)
      current = { head: msg, calls: [] }
      pendingIds = new Set()
      continue
    }
    if (current === null) continue
    if (msg.role === 'assistant') {
      current.calls.push(msg)
      if (Array.isArray(msg.tool_calls)) {
        for (const c of msg.tool_calls) {
          if (c?.id !== undefined) pendingIds.add(String(c.id))
        }
      }
      continue
    }
    if (msg.role === 'tool') {
      if (msg.tool_call_id !== undefined && pendingIds.has(String(msg.tool_call_id))) {
        current.calls.push(msg)
      } else {
        current = null
        pendingIds = new Set()
      }
      continue
    }
  }
  flush(current)
  const kept = units.filter((u) => u.calls.length > 0)
  if (kept.length === 0 && messages.some((m) => m.role === 'user')) {
    // 至少保住当前回合的 user（避免裁空）
    const lastUser = [...messages].reverse().find((m) => m.role === 'user')
    return [{ head: lastUser, calls: [] }]
  }
  return kept
}

function unitComplete(unit) {
  const ids = new Set()
  for (const m of unit.calls) {
    if (m.role === 'assistant' && Array.isArray(m.tool_calls)) {
      for (const c of m.tool_calls) if (c?.id !== undefined) ids.add(String(c.id))
    }
  }
  const results = new Set(unit.calls.filter((m) => m.role === 'tool').map((m) => String(m.tool_call_id)))
  return [...ids].every((id) => results.has(id))
}

function flattenUnits(units) {
  const out = []
  for (const u of units) {
    out.push(u.head)
    for (const m of u.calls) out.push(m)
  }
  return out
}

function messageLength(list) {
  return list.reduce((sum, m) => {
    const c = m.content
    return sum + (typeof c === 'string' ? c.length : JSON.stringify(c ?? '').length)
  }, 0)
}

function throwIfAborted(signal) {
  if (signal?.aborted === true) {
    const error = new Error('已中断')
    error.name = 'AbortError'
    throw error
  }
}
