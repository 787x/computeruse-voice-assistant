/**
 * Agent loop（P1 核心）。
 *
 * 职责边界：只负责"想和调用工具"，不碰音频、不碰 UI、不碰权限实现细节。
 * 权限通过注入的 `gate(decision)` 回调外部化，P3 再把 PermissionGate 接进来。
 *
 * 关键设计：
 *  - 工具结果是文本信封（上游 format.js 生成），模型只读文本，不直接吃图片
 *  - **上下文按 tokens 预算管理**（见 agent/context.mjs）：默认 1M 窗口、到 90% 才压缩。
 *    以前那套"最多 10 条消息 / 3,000 字符 / 40,000 字符 / 12 条就开始压"的阈值全部删掉了——
 *    它们与模型的真实容量无关，会在只用了 4% 窗口时就开始丢历史。
 *  - 每一步都可中断：所有 await 前都有 signal 检查
 *  - **思考内容（reasoning_content）与最终回答分开**：思考内容只放回上下文（MiMo /
 *    DeepSeek 在思考模式下做工具调用时不回传会 HTTP 400），绝不进字幕/TTS；
 *    压缩时按"完整消息片段"下手，不留孤立的工具结果或缺 thinking 字段的工具调用，
 *    也绝不把 reasoning_content 截断后继续配对回传。
 *  - 参数不完整/解析失败的工具调用**不执行**，只把失败原因回给模型
 */

/** 默认系统提示词的唯一来源见 system-prompt.mjs（设置页显示的是同一份文本）。 */
import { DEFAULT_SYSTEM_PROMPT, turnSystemPrompt } from './system-prompt.mjs'
import { assistantMessageFrom } from './llm.mjs'
import { resolveCapability, requiresReasoningEcho } from './ll-params.mjs'
import {
  compressMessages,
  estimateRequestTokens,
  resolveContextPolicy,
  splitInteractionUnits,
} from './context.mjs'

const TOOL_RESULT_MAX_CHARS = 1200
/**
 * 一轮里最多把几张图塞给模型。
 *
 * 以前是 4——那是"字符预算"时代的产物（一张截图 16 万字符，四张就"撑爆"了）。
 * 现在图片按 token 计价（见 context.mjs 的 estimateImageTokens），预算自己会管住用量，
 * 所以这里只留一个防跑飞的硬上限，不再把长 CUA 任务的截图提前砍掉。
 */
const MAX_IMAGES_PER_RUN = 16

/**
 * 会话历史整理：只做**协议安全**的整理，不做预算裁剪。
 *
 * 预算裁剪统一交给 compressMessages（到 90% 才动手），所以这里不再有
 * "最多 10 条 / 3,000 字符"这种与模型容量无关的丢历史逻辑。
 *
 * 仍然必须做的事：
 *  - 只保留"完整片段"：user 开头，其后的 assistant/tool 全部配对（tool 有对应 tool_calls，
 *    有 tool_calls 的 assistant 有对应 tool 结果）。残缺片段整组剔除，绝不留孤立 tool 或
 *    缺失工具调用的 assistant。
 *  - `providerCapability` / `providerId` 用于跨提供商：旧提供商（MiMo/DeepSeek）的
 *    reasoning_content 与新端点的工具链是私有协议上下文，发给别家会污染请求；
 *    此时剥掉这些字段，但保留普通对话记忆。
 */
function trimHistory(history, { providerCapability = '', providerId = '' } = {}) {
  if (!Array.isArray(history) || history.length === 0) return []
  const units = splitInteractionUnits(history.map((message) => ({ ...message })))

  const providersDiffer = providerId !== ''
    ? (unit) => {
        const all = [unit.head, ...unit.segments.flatMap((segment) => [
          ...segment.calls, ...segment.results, ...segment.attachments,
        ])]
        return all.some((message) => message._providerId !== undefined && String(message._providerId) !== String(providerId))
      }
    : () => false

  const needEcho = requiresReasoningEcho(providerCapability)
  const flat = []
  for (const unit of units) {
    const foreign = providersDiffer(unit)
    if (foreign) {
      /*
       * 旧提供商：只保留「用户问题 + 纯文本回复」，工具链与截图整段去除。
       * reasoning_content 是旧端点的私有字段（新端点可能直接参数校验报错），
       * 所以这里**必须一起剥掉**——保留纯文本记忆就够了。
       */
      const plain = unit.segments
        .flatMap((segment) => segment.calls)
        .filter((message) => message.role === 'assistant' && !Array.isArray(message.tool_calls))
      if (plain.length === 0) continue
      flat.push({ ...unit.head })
      for (const message of plain) {
        const copy = { ...message }
        delete copy.reasoning_content
        flat.push(copy)
      }
      continue
    }
    flat.push({ ...unit.head })
    for (const segment of unit.segments) {
      for (const message of [...segment.calls, ...segment.results, ...segment.attachments]) {
        const copy = { ...message }
        if (copy.role === 'assistant' && typeof copy.reasoning_content === 'string' && !needEcho) {
          delete copy.reasoning_content
        }
        flat.push(copy)
      }
    }
  }
  // 内部标记绝不发往模型
  for (const message of flat) delete message._providerId
  return flat
}

export class Agent {
  #llm
  #tools
  /** 提示词来源：文本，或返回配置的函数（缓存的 Agent 靠它拿到最新设置） */
  #systemPrompt
  #maxSteps
  #onEvent
  #gate
  #contextPolicy

  /**
   * @param {object} options
   * @param {{chat:Function}} options.llm
   * @param {{listTools:Function, invoke:Function}} options.tools
   * @param {string|(()=>unknown)} [options.systemPrompt] 文本，或返回文本/null 的函数；
   *   null/undefined → 默认提示词。传函数时每轮开始时求值一次（改设置不用重建 Agent）。
   * @param {number} [options.maxSteps]
   * @param {(event:{type:string,[k:string]:unknown})=>void} [options.onEvent]
   * @param {(ctx:{tool:string,args:unknown})=>Promise<{allow:boolean,reason?:string}>} [options.gate]
   * @param {object} [options.contextPolicy] resolveContextPolicy 的结果（窗口 / 压缩触发点）
   */
  constructor({ llm, tools, systemPrompt = DEFAULT_SYSTEM_PROMPT, maxSteps = 8, onEvent, gate, contextPolicy }) {
    this.#llm = llm
    this.#tools = tools
    this.#systemPrompt = systemPrompt
    this.#maxSteps = maxSteps
    this.#onEvent = onEvent ?? (() => {})
    this.#gate = gate ?? (async () => ({ allow: true }))
    this.#contextPolicy = contextPolicy ?? resolveContextPolicy({})
  }

  /** 此刻会生效的提示词（诊断/验收用；不代表正在跑的回合，回合用开始时的快照） */
  get systemPrompt() {
    return turnSystemPrompt(this.#systemPrompt)
  }

  /** 此刻会生效的步数上限 */
  get maxSteps() {
    return this.#maxSteps
  }

  /** 此刻会生效的上下文预算（诊断/验收用） */
  get contextPolicy() {
    return { ...this.#contextPolicy }
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
   * 运行中改上下文预算（保存设置后下一轮生效）。
   * 只影响下一轮开始时的快照，正在跑的回合仍用它开始时的预算。
   */
  setContextPolicy(policy) {
    if (policy !== null && typeof policy === 'object') this.#contextPolicy = policy
  }

  /**
   * 跑一个用户回合。
   * @param {string} input 用户说的话（已由 ASR 转成文本）
   * @param {{signal?:AbortSignal, history?:Array<object>, systemPrompt?:string|(()=>unknown),
   *          contextPolicy?:object}} [options]
   *   history 是**同一个会话**里之前几轮的消息（可含 reasoning_content / tool_calls / tool 结果）。
   *   每次唤醒由调用方清空，于是"一次唤醒 = 一次会话"。
   *   systemPrompt 覆盖构造函数的来源（**本轮快照**）。
   */
  async run(input, {
    signal,
    history = [],
    onEvent = this.#onEvent,
    systemPrompt = this.#systemPrompt,
    contextPolicy = this.#contextPolicy,
  } = {}) {
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
     * 上下文预算也是回合快照：中途改窗口/触发点，只对**下一轮**生效，
     * 免得一次任务跑到一半突然换了一套压缩尺度。
     */
    const policy = contextPolicy ?? resolveContextPolicy({})
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
    const availableTools = this.#tools.listTools()
    const messages = [
      { role: 'system', content: turnPrompt },
      ...trimHistory(history, { providerCapability: capability, providerId }),
    ]
    /*
     * 本轮增量边界：用**对象引用**而不是数字索引。
     *
     * `compress(messages)` 会整体重建数组（splice 把旧单元从头部移除），
     * 用数字索引 `turnStart` 会在压缩后失效——可能把本轮 user / 工具结果 / 回复漏掉。
     * 压缩总是保留“当前回合的 user”（见 context.mjs 的 splitInteractionUnits），
     * 因此拿到本轮第一条 user 消息的对象引用，处理后按 `indexOf` 定位即可稳定切出本轮增量。
     */
    const turnUserMessage = { role: 'user', content: input }
    messages.push(turnUserMessage)
    /**
     * 每次发请求前压一次。**没到 90% 会立刻返回**（只花一次估算），
     * 到了才按完整片段整体删除——所以"低于阈值不压缩"是结构上保证的。
     */
    const compress = () => compressMessages(messages, policy, { tools: availableTools })
    /*
     * 发第一枪之前先看一眼总预算：历史本身就可能已经很大（长时间 CUA 会话）。
     * 没到触发点这一步什么都不做。
     */
    compress()

    const steps = []
    let finalText = ''
    let hitStepLimit = false
    /** 本轮因流式异常/错误而未正常完成（上层不应把 finalText 当作成功回答） */
    let turnFailed = false
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
       *
       * 即使没有工具调用（纯文本错误流），也不能当成正常成功回答结束：
       * 服务端错误/异常断流必须让上层明确感知失败。
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
          compress()
          if (step === maxSteps) hitStepLimit = true
          continue
        }
      }
      // 纯文本回复但也可能是错误流（finishReason 非正常 / sawError / incomplete）
      if (toolCalls.length === 0 && broken.length === 0) {
        const streamFailed = result.toolCallsIncomplete === true
          || result.finishReason === 'incomplete'
          || result.finishReason === 'length'
          || result.finishReason === 'content_filter'
          || result.finishReason === 'repetition_truncation'
        if (streamFailed) {
          // 保留已流出的文字，但明确标记本轮失败（不当作成功回答结束）
          finalText = finalText || '（模型响应异常，未能完成回答）'
          if (result.text) messages.push(assistantMessageFrom(result))
          emit({ type: 'stream_error', text: result.text ?? '', finishReason: result.finishReason })
          turnFailed = true
          break
        }
        // 纯文本回复也要进消息历史：后续同一会话的提问需要带着这条回复，
        // 且如果是思考模式下，reasoning_content 也要如实回传。
        messages.push(assistantMessageFrom(result))
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
        compress()
        if (step === maxSteps) hitStepLimit = true
        continue
      }

      // 混合情况（有的能解析、有的坏了）：assistant 只带能用的那些，坏的直接不认
      if (broken.length > 0) {
        emit({ type: 'tool_end', name: broken.map((call) => call.name || '(未知工具)').join(', '), ok: false, text: '参数不完整，已丢弃', step })
      }
      messages.push(assistantMessageFrom(result))

      /*
       * 同一批工具调用的结果必须**连续**出现：协议要求 tool 消息紧跟声明它们的 assistant，
       * 中间插一条 user（哪怕只是图片附件）就会被判成孤立工具结果。
       * 所以截图先攒在 `pendingImages`，等这一批工具**全部**返回后再一次性补进消息。
       */
      const pendingImages = []
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
           * OpenAI 兼容协议里图片只能放在 user 消息的 content 数组上，所以单独补一条；
           * 但补的时机是**这一批工具全部返回之后**（见上面的 pendingImages 说明）。
           */
          if (invoked?.image !== undefined && invoked.image !== null && imagesAttached < MAX_IMAGES_PER_RUN) {
            imagesAttached += 1
            pendingImages.push({
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
      // 这一批工具结果已经齐了，截图附件现在才能进来（不会插在结果之间）
      messages.push(...pendingImages)

      compress()
      // 这是最后一步，且这一步还在调工具 → 走完就撞上限，需要补一次交代
      if (step === maxSteps) hitStepLimit = true
    }

    /*
     * 步数用完时**必须补一次"交代"往返**。
     *
     * 之前的行为是：最后一步工具执行完，往上下文塞一句"达到上限，先停下汇报"，
     * 然后循环直接结束——模型根本没机会说话，用户听到的就是"话说到一半停了"。
     * 现在多给一次不带工具的往返，让它用一句话说清楚做到哪、还差什么。
     *
     * 这条收尾回复**必须进消息历史**：用户听到的是这一轮的回答，
     * 下一轮追问"那接下来呢"时模型得记得自己刚才说过什么。
     */
    throwIfAborted(signal)
    if (hitStepLimit && steps.length > 0) {
      const noteIndex = messages.length
      messages.push({
        role: 'user',
        content: `（已经用满 ${maxSteps} 步，不能再调工具了。用一句话告诉用户：你做到哪一步、还差什么、需要他怎么配合。）`,
      })
      try {
        const closing = await llm.chat({
          messages,
          tools: [],
          signal,
          stream: true,
          onDelta: (text) => emit({ type: 'text_delta', text }),
        })
        throwIfAborted(signal)
        // 这条"用满步数"的内部提示不属于对话内容，拿到回复后就撤掉（保持历史干净且协议合法）
        messages.splice(noteIndex, 1)
        if (closing.text) {
          finalText += closing.text
          messages.push(assistantMessageFrom(closing))
          emit({ type: 'assistant', text: closing.text, step: maxSteps + 1, limited: true })
        }
        if (closing.toolCallsIncomplete === true && closing.text === '') turnFailed = true
      } catch (error) {
        throwIfAborted(signal)
        messages.splice(noteIndex, 1)
        const fallback = `（步数用完了，没能收尾：${error.message}）`
        messages.push({ role: 'assistant', content: fallback })
        emit({ type: 'assistant', text: fallback, step: maxSteps + 1, limited: true })
        turnFailed = true
      }
    }

    emit({ type: 'done', text: finalText, steps: steps.length, limited: hitStepLimit, reasoningChars, failed: turnFailed })
    /*
     * 把这一回合用的端点快照一起返回：回合之后的旁路请求（比如"还要不要继续听"的判定）
     * 必须用**同一套**提供商与推理参数，否则中途改了设置会让那次判定换个模型、
     * 甚至换个提供商，判出来的东西和刚说的这段话不是一套。
     */
    /*
     * 返回**本轮增量** `turn`：本轮 user + 全部 assistant
     * （含 reasoning_content / tool_calls） + tool 结果 + 截图消息。
     *
     * 用本轮 user 消息的对象引用定位边界（compress 重建数组、删除旧单元后，
     * 数字索引会失效；对象引用不受影响，且压缩保证不会删掉当前回合 user）。
     * **不包含**传入 history 里的旧消息——旧历史由保存方自行维护，不能由 Agent 反复带出造成重复。
     *
     * 每条消息打 `_providerId` 标记供跨提供商剥离，但该字段绝不会进请求体（trimHistory 会删）。
     */
    const turnIndex = messages.indexOf(turnUserMessage)
    const from = turnIndex === -1 ? messages.length : turnIndex
    const turn = messages.slice(from).map((m) => ({
      ...m,
      _providerId: providerId,
      content: typeof m.content === 'string' ? m.content : (m.content === undefined || m.content === null ? null : JSON.parse(JSON.stringify(m.content))),
      ...(m.tool_calls !== undefined ? { tool_calls: JSON.parse(JSON.stringify(m.tool_calls)) } : {}),
      ...(m.reasoning_content !== undefined ? { reasoning_content: m.reasoning_content } : {}),
      ...(m.tool_call_id !== undefined ? { tool_call_id: m.tool_call_id } : {}),
    }))
    return {
      text: finalText,
      steps,
      limited: hitStepLimit,
      reasoningChars,
      llm,
      turn,
      providerId,
      failed: turnFailed,
      contextTokens: estimateRequestTokens({ messages, tools: availableTools }),
      contextWindow: policy.contextWindow,
    }
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

function throwIfAborted(signal) {
  if (signal?.aborted === true) {
    const error = new Error('已中断')
    error.name = 'AbortError'
    throw error
  }
}
