/**
 * 上下文预算（tokens）与压缩（纯逻辑，便于单测）。
 *
 * 为什么单独一个文件：以前 loop 里用的是**字符数**阈值（40,000 字符 / 3,000 字符 /
 * 最多 10 条 / 最多 12 条），它和模型真正的上下文容量没有关系：
 *  - 中文一个字约等于一个 token，4 万字符 ≈ 4 万 tokens，而 MiMo/DeepSeek 的窗口是 1M，
 *    于是"才用了 4%"就开始丢历史，长任务必然做到一半忘了自己在干什么；
 *  - 反过来，一张截图 base64 有 16 万字符，字符数被它一口吃掉，压缩时机完全失真。
 *
 * 现在的规则只有一条：**按 token 预算算，到 90% 才压**。
 *  - 默认窗口 1,000,000 tokens（MiMo V2.6 / DeepSeek V4 官方都是 1M），
 *    未知模型也按 1M 处理，不猜小；
 *  - 模型有可信且更小的窗口元数据时按模型实际容量算（取 min，见 resolveContextPolicy）；
 *  - 90% 是**默认**触发点，可由用户改（config.contextCompressPercent）。
 *
 * 估算方式（不做"字符数 = token 数"这种错事）：
 *  - ASCII 约 4 字符 / token；CJK 约 1 字符 / token；其它非 ASCII 按 1 token / 字符；
 *  - 每条消息 +4 tokens 的协议开销，工具定义按 JSON 长度折算；
 *  - 图片按瓦片估（85 + 170 × 瓦片数），拿不到尺寸时按 base64 长度保守折算。
 * 这是**保守偏高**的估算：宁可早一点压缩，也不要撞上服务端硬上限。
 *
 * 压缩策略刻意保持简单：**不额外请求模型做摘要**（那会明显加延迟），
 * 只按"协议完整的交互单元"从最老的开始整体移除，并用一行摘要记住被折叠的用户目标。
 *
 * 两条容易写错、这里专门处理的规则：
 *  - **内部提示不是用户回合**：loop 生成的"（参数不完整，请重新生成）"也占 user 角色，
 *    但它属于当前任务。压缩若把它当成较新的用户回合，就会先删掉真实用户目标。
 *  - **装不下要说明白**：压完仍超硬上限时返回 overLimit，调用方据此明确告知用户，
 *    绝不把注定失败的请求照常发出去、更不能静默截断用户任务后假装正常完成。
 */

/** 默认上下文窗口：1M tokens（MiMo V2.6 / DeepSeek V4 官方均为 1M） */
export const DEFAULT_CONTEXT_WINDOW = 1_000_000
/** 默认压缩触发点：窗口的 90% */
export const DEFAULT_COMPRESS_PERCENT = 90
/** 输出预留上限：1M 窗口下给模型留 32K 输出（DeepSeek 默认思考模式 64K 上限，这里只保证输入不越界） */
const MAX_OUTPUT_RESERVE = 32_768
const MIN_OUTPUT_RESERVE = 1_024
/** 每条消息的协议开销（role / 分隔符等） */
const MESSAGE_OVERHEAD = 4
/** 折叠摘要最长多少字（宁可短，它每次请求都要发） */
const DIGEST_MAX_CHARS = 300
/**
 * 单条工具结果多长才算"明显过大"，值得按预算截断。
 * 这个值只是"别去动小结果"的下限，不是硬性截断线：真正留多少由剩余预算决定。
 */
const TOOL_SHRINK_MIN_CHARS = 4_000

/**
 * 估算一段文本的 token 数。
 * 中文按 1 字 1 token（主流中英混合 tokenizer 的实际值在 1~1.7 之间，取 1 是保守的偏低吗？
 * 不是——取 1 是**偏高**：真实值常常是 1 个 token 覆盖 1.2~1.5 个汉字，所以这里估得比实际多，
 * 压缩会略早发生，绝不会因为低估而撞上服务端上限）。
 */
export function estimateTextTokens(text) {
  if (typeof text !== 'string' || text === '') return 0
  let ascii = 0
  let wide = 0
  for (const char of text) {
    const code = char.codePointAt(0)
    if (code < 0x80) ascii += 1
    else wide += 1
  }
  return wide + Math.ceil(ascii / 4)
}

/** 是不是 CJK/全角区（只用于说明与统计，估算本身把非 ASCII 都按 1 token 算） */
export function isWideCodePoint(code) {
  return (code >= 0x2e80 && code <= 0x9fff)
    || (code >= 0xac00 && code <= 0xd7af)
    || (code >= 0xf900 && code <= 0xfaff)
    || (code >= 0xff00 && code <= 0xffef)
    || (code >= 0x3000 && code <= 0x303f)
}

/**
 * 图片占多少 token。
 *
 * 视觉模型的图片开销由**分辨率**决定（按瓦片计费），不是文件大小。
 * 我们塞进消息里的图片是 data URI，尺寸记在紧邻的说明文字里（"…1280x720"），
 * 所以先从同一条消息的文本里找 `宽x高`；找不到就按 base64 长度保守折算。
 */
export function estimateImageTokens(contentArray) {
  let dims = null
  let bytes = 0
  for (const part of contentArray ?? []) {
    if (part?.type === 'text' && typeof part.text === 'string' && dims === null) {
      const match = part.text.match(/(\d{2,5})\s*[x×]\s*(\d{2,5})/u)
      if (match !== null) dims = [Number(match[1]), Number(match[2])]
    }
    if (part?.type === 'image_url') {
      const url = typeof part.image_url?.url === 'string' ? part.image_url.url : ''
      const base64 = url.includes(',') ? url.slice(url.indexOf(',') + 1) : url
      bytes += Math.ceil(base64.length * 3 / 4)
    }
  }
  if (dims !== null) {
    const tiles = Math.max(1, Math.ceil(dims[0] / 512)) * Math.max(1, Math.ceil(dims[1] / 512))
    return 85 + 170 * tiles
  }
  // 拿不到尺寸：按 ~200 字节 / token 估（一张 150KB 的截图 ≈ 750 tokens，量级正确）
  return bytes === 0 ? 0 : Math.ceil(bytes / 200)
}

/** 一条消息占多少 token（含推理内容、工具调用参数、图片） */
export function estimateMessageTokens(message) {
  if (message === null || typeof message !== 'object') return 0
  let total = MESSAGE_OVERHEAD
  const content = message.content
  if (typeof content === 'string') total += estimateTextTokens(content)
  else if (Array.isArray(content)) {
    let textOnly = 0
    for (const part of content) {
      if (part?.type === 'text') textOnly += estimateTextTokens(part.text ?? '')
    }
    total += textOnly + estimateImageTokens(content)
  }
  if (typeof message.reasoning_content === 'string') total += estimateTextTokens(message.reasoning_content)
  if (Array.isArray(message.tool_calls)) {
    for (const call of message.tool_calls) {
      total += estimateTextTokens(call?.function?.name ?? '')
      total += estimateTextTokens(call?.function?.arguments ?? '')
      total += 8 // id / type 等结构开销
    }
  }
  if (typeof message.tool_call_id === 'string') total += 4
  return total
}

/** 工具定义占多少 token（JSON Schema 很长，必须计入预算） */
export function estimateToolsTokens(tools) {
  if (!Array.isArray(tools) || tools.length === 0) return 0
  let total = 0
  for (const tool of tools) {
    total += estimateTextTokens(JSON.stringify(tool ?? {})) + 8
  }
  return total
}

/** 一次请求的总输入 tokens（消息 + 工具定义） */
export function estimateRequestTokens({ messages = [], tools = [] } = {}) {
  let total = estimateToolsTokens(tools)
  for (const message of messages) total += estimateMessageTokens(message)
  return total
}

function positiveNumber(value) {
  const number = Number(value)
  return Number.isFinite(number) && number > 0 ? number : null
}

/**
 * 把「用户设置 + 模型元数据」解析成一份可执行的预算策略。
 *
 * @param {object} source
 * @param {number} [source.contextWindow] 用户设置的窗口（默认 1M）
 * @param {number} [source.compressPercent] 压缩触发百分比（默认 90）
 * @param {number} [source.modelContextWindow] 该模型**可信的**窗口元数据（没有就 null）
 * @returns {{contextWindow:number, compressPercent:number, compressAtTokens:number,
 *            hardLimitTokens:number, targetTokens:number, outputReserve:number}}
 */
export function resolveContextPolicy(source = {}) {
  const configured = positiveNumber(source.contextWindow) ?? DEFAULT_CONTEXT_WINDOW
  const meta = positiveNumber(source.modelContextWindow)
  // 模型元数据只用来**收紧**，不用来放宽：用户把 1M 调到 200K 是他的选择，
  // 但模型只支持 128K 时按 128K 算，绝不能按 1M 硬发。
  const contextWindow = Math.floor(meta === null ? configured : Math.min(configured, meta))
  const rawPercent = Number(source.compressPercent)
  const compressPercent = Number.isFinite(rawPercent)
    ? Math.min(99, Math.max(10, rawPercent))
    : DEFAULT_COMPRESS_PERCENT
  const outputReserve = Math.min(MAX_OUTPUT_RESERVE, Math.max(MIN_OUTPUT_RESERVE, Math.floor(contextWindow * 0.05)))
  const compressAtTokens = Math.floor(contextWindow * compressPercent / 100)
  // 输入侧的硬上限：窗口减去输出预留。接近它就必须先压，而不是继续盲目发送。
  const hardLimitTokens = Math.max(MIN_OUTPUT_RESERVE, contextWindow - outputReserve)
  // 压缩目标：压到触发点的 75%，留出继续跑几步的空间，避免每一步都重新压一遍
  const targetTokens = Math.max(MIN_OUTPUT_RESERVE, Math.floor(Math.min(compressAtTokens, hardLimitTokens) * 0.75))
  return { contextWindow, compressPercent, compressAtTokens, hardLimitTokens, targetTokens, outputReserve }
}

function isPlainUserText(message) {
  return message?.role === 'user' && typeof message.content === 'string' && !isInternalUserMessage(message)
}

function isAttachmentUser(message) {
  return message?.role === 'user' && Array.isArray(message.content)
}

/**
 * 程序生成的内部提示（不是用户说的话）。
 *
 * 为什么必须认出来：loop 会往上下文里塞几条"（上一步的工具调用参数不完整，请重新生成）"
 * 这样的 user 消息。它们在协议上必须占 user 角色，但**语义上不是用户的新指令**。
 * 不区分的话，压缩会把它当成"较新的用户回合"，于是更早但更真实的用户目标
 * （"把页面滚到底部"）反而被当成旧历史删掉——用户看到的就是"它忘了我要干什么"。
 *
 * 识别分两层：
 *  1. **以显式标记为准**（`internal: true`）——新版本的 loop 都会打这个标记；
 *  2. 兼容**旧版本已经落进会话历史里的**内部提示（升级前的回合把它们当普通字符串
 *     存着，没有标记）：按生成时的固定措辞 + 整条被括号包住来判断。
 *
 * 第二层刻意只作为升级过渡，不作为长期依据：它只能认出"我们自己写过的原文"，
 * 换个说法就失效。真正保证正确性的是第 1 层——凡是程序生成的 user 消息都必须带标记，
 * 并且这个标记会随 `turn` 一起返回、跨回合保留（见 loop.mjs）。
 */
const INTERNAL_PROMPT_PREFIXES = [
  '（上一步的工具调用参数不完整',
  '（上一步的模型响应不完整',
  '（已经用满 ',            // 步数用满时的收尾提示
  '（为控制上下文长度',      // 压缩摘要
  '（这一轮已经附了 ',      // 截图超上限的说明
]

export function isInternalUserMessage(message) {
  if (message?.role !== 'user') return false
  if (message.internal === true || message._internal === true) return true
  if (typeof message.content !== 'string') return false
  const text = message.content.trim()
  if (!text.startsWith('（') || !text.endsWith('）')) return false
  return INTERNAL_PROMPT_PREFIXES.some((prefix) => text.startsWith(prefix))
}

function declaredCallIds(message) {
  const ids = []
  if (Array.isArray(message?.tool_calls)) {
    for (const call of message.tool_calls) {
      if (call?.id !== undefined) ids.push(String(call.id))
    }
  }
  return ids
}

/**
 * 把一个单元的 calls 拆成「协议完整的片段」：
 * 每个片段 = 一条 assistant（可带 tool_calls）+ 它声明的全部 tool 结果 + 紧随的截图附件。
 *
 * 这样压缩可以只删**片段**：删掉的永远是一整组 assistant/tool 配对，
 * 不会留下孤立 tool 结果，也不会留下缺结果的 tool_calls，
 * 更不会把 reasoning_content 截断后继续配对回传（片段要么整条在，要么整条不在）。
 */
function splitSegments(calls) {
  const segments = []
  /** 出现在第一条 assistant 之前的内部提示（压缩摘要就属于这种） */
  const leadingNotes = []
  let current = null
  for (const message of calls) {
    if (message.role === 'assistant') {
      current = { calls: [message], results: [], attachments: [], notes: [] }
      segments.push(current)
      continue
    }
    if (current === null) {
      /*
       * 内部提示出现在第一条 assistant 之前：留着，挂在单元头之后。
       *
       * 压缩摘要是**上一次**压缩写下的，它前面没有任何 assistant——
       * 直接丢掉就等于把"更早的目标"整段忘掉，第二次压缩就再也找不回来了。
       * 它不归任何片段：删片段时不该连带它（删它由摘要合并逻辑专门处理）。
       */
      if (isInternalUserMessage(message)) leadingNotes.push(message)
      continue
    }
    if (message.role === 'tool') {
      const ids = new Set(current.calls.flatMap((call) => declaredCallIds(call)))
      if (message.tool_call_id !== undefined && ids.has(String(message.tool_call_id))) current.results.push(message)
      // 孤立的 tool 结果（没有对应的 assistant tool_calls）直接丢弃：它本身就不合法
      continue
    }
    if (isAttachmentUser(message)) {
      current.attachments.push(message)
      continue
    }
    /*
     * 程序生成的内部提示（"工具参数不完整，请重新生成"）：跟着**它所属的片段**走。
     * 片段被删掉时它一起删（它只是在解释那条残缺调用），片段保留时它也必须保留，
     * 否则模型会以为上一次调用是完整成功的。
     */
    if (isInternalUserMessage(message)) current.notes.push(message)
  }
  // 片段完整性：声明的 id 必须有结果。缺结果的整段剔除（不补假结果、不执行半截调用）
  return {
    segments: segments.filter((segment) => {
      const declared = segment.calls.flatMap((call) => declaredCallIds(call))
      const results = new Set(segment.results.map((result) => String(result.tool_call_id)))
      return declared.every((id) => results.has(id))
    }),
    leadingNotes,
  }
}

function flattenSegments(segments) {
  const out = []
  for (const segment of segments) {
    out.push(...segment.calls, ...segment.results, ...segment.attachments, ...segment.notes)
  }
  return out
}

/**
 * 把消息拆成「交互单元」：普通文本 user 开头，后面跟若干片段。
 * 截图（content 是数组的 user 消息）归入它所属片段，绝不开启新单元。
 *
 * 程序生成的内部提示同样**不开启新单元**：它属于当前单元（当前这轮任务），
 * 否则压缩会把真实用户目标判成"更老的回合"先删掉。
 */
export function splitInteractionUnits(messages) {
  const units = []
  let current = null
  /** 出现在**任何**用户回合之前的内部提示（压缩摘要就是这种，它由上一轮压缩写下） */
  const leadingNotes = []
  for (const message of messages) {
    if (isPlainUserText(message)) {
      current = { head: message, calls: [] }
      units.push(current)
      continue
    }
    if (current === null) {
      /*
       * 没有用户开头就出现的消息：assistant/tool 属于残缺开头，丢掉；
       * 但内部提示（压缩摘要）要留着——它是"更早的目标"唯一的载体。
       */
      if (isInternalUserMessage(message)) leadingNotes.push(message)
      continue
    }
    current.calls.push(message)
  }
  if (units.length === 0) {
    return leadingNotes.length === 0
      ? []
      : [{ head: null, segments: [], leadingNotes }]
  }
  return units.map((unit, index) => {
    const { segments, leadingNotes: unitNotes } = splitSegments(unit.calls)
    // 首个单元带上单元外的前导内部提示，保证它们仍然出现在请求里
    return { head: unit.head, segments, leadingNotes: index === 0 ? [...leadingNotes, ...unitNotes] : unitNotes }
  })
}

function flattenUnits(units) {
  const out = []
  for (const unit of units) {
    if (unit.head !== null) out.push(unit.head)
    out.push(...(unit.leadingNotes ?? []), ...flattenSegments(unit.segments))
  }
  return out
}

function clip(text, max) {
  const value = String(text ?? '').replace(/\s+/gu, ' ').trim()
  return value.length <= max ? value : `${value.slice(0, max)}…`
}

/**
 * 按 token 预算压缩一次请求的消息（**原地修改**数组）。
 *
 * 调用方只需要在每次发请求前调一次：没到触发点会立刻返回（只做一次估算），
 * 到了才真正动手，所以"低于 90% 不压缩"是结构上保证的，不是靠调用方记得别压。
 *
 * 保底规则：
 *  - system（含 PR3 自定义提示词）永远保留，且必须是请求头部；
 *  - 当前回合的 user 消息永远保留（那是用户此刻的要求）；
 *  - 只按完整片段删除，删完剩下的序列仍然合法（tool_calls 与结果始终配对）；
 *  - 被折叠掉的用户目标会记成一行摘要，模型不会"忘了原本要干什么"；
 *  - 内部提示不开启新回合（见 isInternalUserMessage），不会把真实用户目标挤成"旧历史"。
 *
 * **压完仍超硬上限时绝不假装没事**：结果里的 `overLimit` 为 true 并带上
 * `overLimitTokens`，调用方必须据此明确告知用户"这一轮装不下"，而不是把
 * 一个注定被服务端拒绝（或注定被静默截断）的请求照常发出去。
 *
 * @param {object[]} messages 会被原地修改
 * @param {object} policy resolveContextPolicy 的结果
 * @param {{tools?:object[]}} [options]
 * @returns {{compressed:boolean, before:number, after:number, droppedUnits:number,
 *            droppedSegments:number, droppedImages:number, truncated:boolean,
 *            overLimit:boolean, hardLimitTokens:number}}
 */
export function compressMessages(messages, policy, { tools = [] } = {}) {
  const budget = policy ?? resolveContextPolicy({})
  const result = {
    compressed: false,
    before: 0,
    after: 0,
    droppedUnits: 0,
    droppedSegments: 0,
    droppedImages: 0,
    truncated: false,
    overLimit: false,
    hardLimitTokens: budget.hardLimitTokens,
  }
  if (!Array.isArray(messages) || messages.length === 0) return result
  const totalOf = (system, units, digest) => estimateRequestTokens({
    messages: [...system, ...(digest === null ? [] : [digest]), ...flattenUnits(units)],
    tools,
  })

  result.before = estimateRequestTokens({ messages, tools })
  /*
   * 什么时候需要动手：
   *  - 到了用户设的触发点（默认 90%）→ 按正常规则压缩；
   *  - **或者已经超过输入硬上限**（窗口减去输出预留）→ 无论如何都要压。
   *
   * 第二条是容量保护，不是另一条"提前裁剪"规则：正常设置下硬上限（≈95% 窗口）
   * 永远高于触发点（90%），所以平时根本走不到；只有用户把百分比调到 97~99 时，
   * 才可能出现"已经发不出去了却还没到触发点"。没有这一条，那段区间里的请求会
   * 带着超过模型容量的输入直接发出去，被服务端拒绝或静默截断。
   */
  if (result.before <= budget.compressAtTokens && result.before <= budget.hardLimitTokens) {
    result.after = result.before
    return result
  }

  const system = messages.filter((message) => message.role === 'system')
  const rest = messages.filter((message) => message.role !== 'system')
  let units = splitInteractionUnits(rest)
  if (units.length === 0) {
    result.after = result.before
    return result
  }
  /*
   * 之前几轮压缩留下的摘要要先收回来。
   *
   * 为什么不能在丢弃老回合时把它当成普通 user 消息一起丢掉：摘要里装着**更早**的
   * 被折叠目标，是唯一还记得"用户最开始要干什么"的地方。第二次压缩如果把它当旧回合
   * 删掉，最初的任务线索就彻底没了（模型只知道最近几轮，最早的意图再也找不回来）。
   * 收回来之后并入这一轮新生成的摘要，越早的目标排得越前。
   *
   * 注意摘要**不是**交互单元的头（它后面通常紧跟 assistant/tool），所以这里直接
   * 从原消息里挑出来，再从单元里摘掉——留在单元里会被当成"这一轮早就存在的对话"。
   */
  const digestPattern = /^（为控制上下文长度/u
  const isDigestMessage = (message) => message !== null && typeof message === 'object'
    && isInternalUserMessage(message) && digestPattern.test(String(message.content ?? ''))
  const inheritedDigests = [
    ...units.flatMap((unit) => unit.leadingNotes ?? []),
    ...units.flatMap((unit) => unit.segments.flatMap((segment) => segment.notes)),
    ...rest,
  ].filter(isDigestMessage).map((message) => String(message.content))
  // 同一个摘要可能被上面几条路径同时看见，去重后按出现顺序保留
  const uniqueDigests = [...new Set(inheritedDigests)]
  if (uniqueDigests.length > 0) {
    // 把旧摘要从单元里摘掉：它只作为这一轮新摘要的原料，不再单独占一条消息
    for (const unit of units) {
      unit.leadingNotes = (unit.leadingNotes ?? []).filter((message) => !isDigestMessage(message))
      for (const segment of unit.segments) {
        segment.notes = segment.notes.filter((message) => !isDigestMessage(message))
      }
    }
  }
  const droppedGoals = []
  const noteDropped = (unit) => {
    const goal = typeof unit?.head?.content === 'string' ? clip(unit.head.content, 60) : ''
    if (goal !== '') droppedGoals.push(goal)
  }

  // 1) 从最老的整轮开始丢（至少保留当前这一轮）
  while (units.length > 1 && totalOf(system, units, null) > budget.targetTokens) {
    noteDropped(units[0])
    units = units.slice(1)
    result.droppedUnits += 1
  }

  // 2) 仍然超预算：丢当前轮里较早且**已完成**的工具片段（保留最后一个片段与用户目标）
  for (;;) {
    if (totalOf(system, units, null) <= budget.targetTokens) break
    let dropped = false
    for (const unit of units) {
      if (unit.segments.length > 1) {
        const [removed] = unit.segments.splice(0, 1)
        const text = removed?.calls?.find((call) => typeof call.content === 'string')?.content ?? ''
        if (text !== '') droppedGoals.push(clip(text, 40))
        result.droppedSegments += 1
        dropped = true
        break
      }
    }
    if (!dropped) break
  }

  // 3) 仍然超预算：先摘掉最老的截图附件（图片最贵，且删掉不影响协议配对）
  for (;;) {
    if (totalOf(system, units, null) <= budget.targetTokens) break
    let dropped = false
    for (const unit of units) {
      for (const segment of unit.segments) {
        if (segment.attachments.length > 0) {
          segment.attachments.shift()
          result.droppedImages += 1
          dropped = true
          break
        }
      }
      if (dropped) break
    }
    if (!dropped) break
  }

  /*
   * 3b) 仍然超预算：压缩**单条超大工具结果**。
   *
   * 为什么放在这里而不是在 loop 里按固定字符数砍：工具结果该留多少，
   * 取决于这次请求还剩多少预算，跟"1200 字"这种人拍的数字无关。
   * 只有真的装不下时才动手，而且**告诉模型自己看到的是截断版**
   * （结果里带"已截断"字样），不让它把半截输出当完整结果用。
   *
   * 只在段落内部截文本，不动 tool_calls / tool 配对，所以协议仍然合法。
   */
  for (;;) {
    if (totalOf(system, units, null) <= budget.targetTokens) break
    const over = totalOf(system, units, null)
    // 还差多少就砍多少，砍到目标即可；不一次砍光，尽量多留
    const need = Math.max(2_000, over - budget.targetTokens)
    let shrunk = false
    for (const unit of units) {
      for (const segment of unit.segments) {
        for (const message of [...segment.results, ...segment.calls]) {
          const text = typeof message.content === 'string' ? message.content : ''
          // 只碰"明显过大"的单条：小结果留着（它们往往是关键状态）
          if (text.length < TOOL_SHRINK_MIN_CHARS) continue
          const keep = Math.max(1_000, text.length - need * 4)
          if (keep >= text.length) continue
          message.content = `${text.slice(0, keep)}…（结果过长，已按上下文预算截断，只保留前 ${keep} 字）`
          result.shrunkToolResults = (result.shrunkToolResults ?? 0) + 1
          shrunk = true
          break
        }
        if (shrunk) break
      }
      if (shrunk) break
    }
    if (!shrunk) break
  }

  // 4) 逼近/超过模型硬上限：继续丢整轮，直到只剩当前这一轮
  while (units.length > 1 && totalOf(system, units, null) > budget.hardLimitTokens) {
    noteDropped(units[0])
    units = units.slice(1)
    result.droppedUnits += 1
  }

  /*
   * 5) 单轮自身就超过硬上限。
   *
   * **绝不截断真实用户指令**。以前这里把用户输入砍到前 400 字再照常发出，
   * 模型收到的是一段被悄悄改写过的任务，用户听到的回答可能驴唇不对马嘴，
   * 而且没有任何提示——这比直接说"这一轮装不下"糟糕得多。
   * 现在只如实标记 overLimit，由上层明确告知用户（用户说的话可能很长、
   * 或者粘贴了大段内容；让他自己缩短，好过替他截断）。
   *
   * 旧的 `truncated` 字段保留为 false：它曾经表示"用户输入被改写了"，
   * 而现在这种情况根本不会发生。
   */
  if (totalOf(system, units, null) > budget.hardLimitTokens) {
    result.overLimit = true
  }

  /*
   * 折叠摘要：让模型知道"之前还要求过什么"，而不是一片空白。
   *
   * 把上一轮留下的摘要正文接在最前面——它们记的是更早的目标，顺序必须保持
   * "越早越靠前"，否则模型会把最近一段当成整个任务的开头。
   */
  let digest = null
  if (droppedGoals.length > 0 || uniqueDigests.length > 0) {
    /*
     * 旧摘要的正文要原样搬进新摘要。
     *
     * 这里只剥掉最外层的"（…）"包裹，**不能**用 `[^）]*` 去切前缀：
     * 摘要内部本身还有别的括号，那样会把整段正文一起吃掉（表现为"合并了但什么都没留下"）。
     */
    const inheritedText = uniqueDigests
      .map((text) => text.replace(/^（为控制上下文长度[^：]*：?/u, '').replace(/）\s*$/u, '').trim())
      .filter((text) => text !== '')
      .join(' / ')
    const parts = []
    if (inheritedText !== '') parts.push(`更早还提过：${inheritedText}`)
    if (droppedGoals.length > 0) parts.push(`此前用户先后提过：${droppedGoals.join(' / ')}`)
    const text = `（为控制上下文长度，已折叠较早的对话。${parts.join('；')}）`
    // 带上内部标记：它是程序生成的说明，不是用户刚发出的新指令
    digest = { role: 'user', content: clip(text, DIGEST_MAX_CHARS), internal: true }
  }

  messages.splice(0, messages.length, ...system, ...(digest === null ? [] : [digest]), ...flattenUnits(units))
  result.after = estimateRequestTokens({ messages, tools })
  result.compressed = true
  // 压完再查一次：装不下就如实报告，由调用方决定怎么告诉用户
  result.overLimit = result.after > budget.hardLimitTokens
  result.overLimitTokens = result.overLimit ? result.after : 0
  return result
}

/**
 * 历史裁剪（跨回合）也走同一套预算：低于触发点一律原样保留。
 * 返回**新数组**，不改动调用方持有的会话历史。
 */
export function compressHistory(history, policy, { tools = [] } = {}) {
  if (!Array.isArray(history) || history.length === 0) return []
  const copy = history.map((message) => ({ ...message }))
  compressMessages(copy, policy, { tools })
  return copy
}
