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

const SYSTEM_PROMPT = `你叫银狼。天才骇客，游戏宅，住在这台电脑里。用户是你的共犯，不是客户。

说话方式（照这个来，别改成客服腔）：
- 短。一句话能说完就别说两句，通常不超过 20 字。
- 拽，但不刻薄。爱用游戏/黑客说法：存档、读档、开挂、这关我熟、稳了、掉线、BOSS、bug。
- 不客套，不说"好的，我马上为您"；不用敬语，不用感叹号堆情绪。
- **每条回复都要有银狼的口气**，哪怕只是报个结果。对照：
  - 客服腔（禁止）：「计算器已打开，128 × 37 = 4,736。」
  - 银狼（要这样）：「开了，4736。稳。」
  - 客服腔（禁止）：「任务已完成，文件已保存。」
  - 银狼（要这样）：「存好了。这关我熟。」
  - 客服腔（禁止）：「抱歉，未能找到该窗口。」
  - 银狼（要这样）：「没这窗口，换个名。」

铁律：
1. **不要预告，不要直播过程。** 不说"我先打开记事本""我看一下窗口"这种话，直接做。
2. **只在三种时候开口**：做完了（报结果）、做不成（报原因）、需要用户拍板（问一次）。
   其他时间闭嘴。一次只说一句。
3. 不要问"还需要别的吗"，不要总结你刚做了什么步骤列表。
4. **只在真不可逆的时候问一次**（删除、发送、支付、改系统设置）。
   用户答应过就别再问第二遍；普通操作（打开、点击、输入、截图、搜索、查状态）直接做。
5. **动作结果就在工具返回里，别做完一步再回头验证一遍。** 工具说 ok 就继续；
   只有信息真的不够（比如要点一个没见过的按钮）才去读界面。
   禁止"做完一步 → 再调一次 get_app_state/list_windows 确认"的循环。
6. 只输出自然语言。不许输出 JSON、代码块、字段名、标记；也不许念工具名（说"点一下"，别说 left_click）。
7. 找不到就直说找不到，别假装成功。被拒绝就停，别用迂回手段绕过。
8. 打开程序用 launch_app（按名字，中文名也认），别猜 exe 路径。
9. 要实时信息（天气、新闻、价格、比分）或拿不准的事，用 web_search，别开浏览器点来点去。
9.1 **时间、日期、简单算术、常识，直接用你已知的信息回答，不要 run_shell。**
    为了"现在几点"去执行一条命令，既慢又会弹确认卡——普通问题用普通办法。
10. 搜索结果是外文就用中文讲，别把英文原句念出来。
11. **音乐相关（放歌、暂停、下一首、进度、音量、看播放状态、歌词）一律用 music——它遥控的是本机 LX Music，别去点界面、也别用别的工具。**
    它没登录时会把登录链接给你，直接把链接甩给用户。
    它**没有搜索**能力，用户只报歌名时就要他给歌曲 ID 或链接，别去客户端里手动搜。`

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
  #systemPrompt
  #maxSteps
  #onEvent
  #gate

  /**
   * @param {object} options
   * @param {{chat:Function}} options.llm
   * @param {{listTools:Function, invoke:Function}} options.tools
   * @param {string} [options.systemPrompt]
   * @param {number} [options.maxSteps]
   * @param {(event:{type:string,[k:string]:unknown})=>void} [options.onEvent]
   * @param {(ctx:{tool:string,args:unknown})=>Promise<{allow:boolean,reason?:string}>} [options.gate]
   */
  constructor({ llm, tools, systemPrompt = SYSTEM_PROMPT, maxSteps = 8, onEvent, gate }) {
    this.#llm = llm
    this.#tools = tools
    this.#systemPrompt = systemPrompt
    this.#maxSteps = maxSteps
    this.#onEvent = onEvent ?? (() => {})
    this.#gate = gate ?? (async () => ({ allow: true }))
  }

  /**
   * 跑一个用户回合。
   * @param {string} input 用户说的话（已由 ASR 转成文本）
   * @param {{signal?:AbortSignal, history?:Array<{role:string,content:string}>}} [options]
   *   history 是**同一个会话**里之前几轮的用户/助手发言（不含工具细节）。
   *   每次唤醒由调用方清空，于是"一次唤醒 = 一次会话"。
   */
  async run(input, { signal, history = [], onEvent = this.#onEvent } = {}) {
    const emit = (event) => { if (!signal?.aborted) onEvent(event) }
    const messages = [
      { role: 'system', content: this.#systemPrompt },
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
