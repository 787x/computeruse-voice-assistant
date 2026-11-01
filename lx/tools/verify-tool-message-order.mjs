/**
 * 多工具 / 截图 / 推理字段的**消息顺序**验收。
 *
 * 为什么单独测这一条：OpenAI 兼容协议要求「一个 assistant 的 tool_calls 之后，
 * 必须紧跟它声明的每一个 tool 结果」，中间插任何一条别的消息都会被判成孤立工具结果
 * （MiMo / DeepSeek 直接 400）。而截图附件在协议里只能是 `user` 消息——
 * 以前它是"工具一返回就 push"，于是在**同一批多个工具**时截图会插在结果之间，
 * 整批消息就废了。这里用 Mock 端点把真实发出的每一帧都记下来，逐帧断言顺序。
 *
 * 覆盖：
 *  1. 一批多个工具 + 其中两个返回截图 → 截图必须排在这一批全部 tool 结果之后；
 *  2. 部分失败（一个工具抛错、一个被权限拒绝）时配对仍然完整；
 *  3. 推理字段只在 MiMo/DeepSeek 回传，且与 tool_calls 同一条消息；
 *  4. 下一回合请求里，历史仍是合法序列（没有孤立 tool、没有缺结果的调用）。
 *
 * 用法：node tools/verify-tool-message-order.mjs
 */
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const configDir = await mkdtemp(join(tmpdir(), 'cua-order-'))
process.env.CUA_VA_CONFIG_DIR = configDir
delete process.env.CUA_VA_API_KEY

const { startMockServer } = await import('./mock-openai.mjs')
const { Agent } = await import('../packages/agent/loop.mjs')
const { LLMClient, profileFromConfig, assistantMessageFrom } = await import('../packages/agent/llm.mjs')

const results = []
function check(name, pass, detail = '') {
  results.push({ name, pass: pass === true, detail })
  console.log(`${pass === true ? '✓' : '✗'} ${name}${detail === '' ? '' : `\t${typeof detail === 'string' ? detail : JSON.stringify(detail)}`}`)
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

async function withMock(handler, run) {
  const mock = await startMockServer({ port: 0, handler })
  try {
    return await run(mock)
  } finally {
    await sleep(20)
    mock.server.close()
  }
}

/**
 * 校验一段消息序列是否符合工具调用协议。
 * @returns {string[]} 问题列表（空 = 合法）
 */
function protocolProblems(messages) {
  const problems = []
  for (let i = 0; i < messages.length; i += 1) {
    const message = messages[i]
    if (message.role === 'assistant' && Array.isArray(message.tool_calls)) {
      const ids = message.tool_calls.map((call) => String(call.id))
      // 紧随其后的消息必须先是这批 tool 结果（顺序不强制，但必须都在图片之前）
      let cursor = i + 1
      const seen = new Set()
      while (cursor < messages.length && messages[cursor].role === 'tool') {
        seen.add(String(messages[cursor].tool_call_id))
        cursor += 1
      }
      for (const id of ids) {
        if (!seen.has(id)) problems.push(`tool_calls ${id} 的结果没有紧跟其后`)
      }
      // 图片只能出现在这一批结果之后
      if (cursor < messages.length && messages[cursor].role === 'user' && Array.isArray(messages[cursor].content)) {
        for (const part of messages[cursor].content) {
          if (part?.type === 'image_url') continue
        }
      }
    }
    if (message.role === 'tool') {
      // 必须能找到前面声明它的 assistant
      let found = false
      for (let j = i - 1; j >= 0; j -= 1) {
        const prev = messages[j]
        if (prev.role === 'assistant' && Array.isArray(prev.tool_calls)) {
          if (prev.tool_calls.some((call) => String(call.id) === String(message.tool_call_id))) found = true
          break
        }
        if (prev.role === 'assistant') break
        if (prev.role === 'user' && typeof prev.content === 'string') break
      }
      if (!found) problems.push(`孤立 tool 结果 ${message.tool_call_id}@${i}`)
    }
  }
  return problems
}

/** 找出"截图 user 消息插在 tool 结果之间"的情况 */
function imagesBetweenToolResults(messages) {
  const offenders = []
  for (let i = 0; i < messages.length; i += 1) {
    const message = messages[i]
    if (message.role !== 'user' || !Array.isArray(message.content)) continue
    const isImage = message.content.some((part) => part?.type === 'image_url')
    if (!isImage) continue
    // 前面紧邻的是 tool，后面紧邻的还是 tool → 插在结果之间了
    const before = messages[i - 1]
    const after = messages[i + 1]
    if (before?.role === 'tool' && after?.role === 'tool') offenders.push(i)
  }
  return offenders
}

// ============================================================ 1 一批多工具 + 截图

{
  const bodies = []
  let call = 0
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    call += 1
    if (call === 1) {
      // 一次返回三个工具调用：截图 + 窗口列表 + 截图（两张图）
      return {
        sse: [
          { choices: [{ index: 0, delta: { reasoning_content: '先看一眼屏幕，再列窗口，再看一眼。' } }] },
          { choices: [{ index: 0, delta: { content: '我看一下。' } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 's1', function: { name: 'screenshot', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 1, id: 'w1', function: { name: 'list_windows', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 2, id: 's2', function: { name: 'screenshot', arguments: '{"app":"记事本"}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '看到了记事本。' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'enabled' }),
    })
    const agent = new Agent({
      llm: client,
      tools: {
        listTools: () => [],
        invoke: async (name) => {
          if (name === 'screenshot') {
            return {
              text: '截图已保存',
              image: { mediaType: 'image/png', base64: 'AAAA', width: 1280, height: 720 },
            }
          }
          return { text: '记事本、浏览器' }
        },
      },
      maxSteps: 4,
    })
    const result = await agent.run('看看屏幕')
    const second = bodies[1].messages

    const problems = protocolProblems(second)
    check('M1 一批多工具 + 两张截图：消息序列完全合法', problems.length === 0, problems.join(','))
    check('M2 截图没有插在工具结果之间', imagesBetweenToolResults(second).length === 0,
      imagesBetweenToolResults(second))

    // 顺序断言：3 个 tool 结果连续，然后才是两条图片消息
    const rolesAfterAssistant = []
    const assistantIndex = second.findIndex((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
    for (let i = assistantIndex + 1; i < second.length; i += 1) {
      const message = second[i]
      rolesAfterAssistant.push(message.role === 'user' && Array.isArray(message.content) ? 'image' : message.role)
    }
    check('M3 顺序为 tool,tool,tool,image,image',
      rolesAfterAssistant.slice(0, 5).join(',') === 'tool,tool,tool,image,image',
      rolesAfterAssistant.join(','))
    check('M4 三个工具结果都在（含失败的也回填）',
      second.filter((m) => m.role === 'tool').length === 3,
      second.filter((m) => m.role === 'tool').map((m) => m.tool_call_id))

    // 推理字段：assistant 消息里带着，且与 tool_calls 同一条
    const assistantMsg = second[assistantIndex]
    check('M5 带 tool_calls 的 assistant 同时回传 reasoning_content',
      typeof assistantMsg.reasoning_content === 'string' && assistantMsg.reasoning_content.length > 0,
      Object.keys(assistantMsg).join('|'))
    check('M6 图片以 user 消息承载（协议要求）',
      second.filter((m) => m.role === 'user' && Array.isArray(m.content)).length === 2,
      second.filter((m) => m.role === 'user' && Array.isArray(m.content)).length)

    // 本轮 turn 也要完整（供后续会话历史使用）
    const turn = result.turn
    check('M7 本轮 turn 含完整工具链与图片（供下一回合）',
      turn.filter((m) => m.role === 'tool').length === 3
        && turn.filter((m) => m.role === 'user' && Array.isArray(m.content)).length === 2,
      turn.map((m) => m.role).join(','))
    check('M8 本轮 turn 以 user 开头、以最终回复结尾',
      turn[0].role === 'user' && turn.at(-1).role === 'assistant' && turn.at(-1).content === '看到了记事本。',
      `${turn[0].role} … ${turn.at(-1).role}:${turn.at(-1).content}`)
  })
}

// ============================================================ 2 部分失败 / 权限拒绝

{
  const bodies = []
  let call = 0
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    call += 1
    if (call === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'ok1', function: { name: 'list_windows', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 1, id: 'boom', function: { name: 'screenshot', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 2, id: 'denied', function: { name: 'run_shell', arguments: '{"command":"del x"}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '有两个没做成。' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    const agent = new Agent({
      llm: client,
      tools: {
        listTools: () => [],
        invoke: async (name) => {
          if (name === 'screenshot') throw new Error('截图失败：窗口已关闭')
          return { text: '记事本' }
        },
      },
      maxSteps: 4,
      // 权限门：run_shell 一律拒绝（模拟 PermissionGate 的拒绝判定）
      gate: async ({ tool }) => (tool === 'run_shell' ? { allow: false, reason: '破坏性操作被拒绝' } : { allow: true }),
    })
    await agent.run('试试看')
    const second = bodies[1].messages
    const problems = protocolProblems(second)
    check('M9 部分失败 + 权限拒绝时消息序列仍合法', problems.length === 0, problems.join(','))
    check('M10 每个工具调用都有对应结果（失败/拒绝也回填）',
      second.filter((m) => m.role === 'tool').length === 3,
      second.filter((m) => m.role === 'tool').map((m) => `${m.tool_call_id}:${String(m.content).slice(0, 10)}`))
    check('M11 失败原因回填给模型（不是静默吞掉）',
      second.some((m) => m.role === 'tool' && String(m.content).includes('截图失败'))
        && second.some((m) => m.role === 'tool' && String(m.content).includes('拒绝')),
      second.filter((m) => m.role === 'tool').map((m) => String(m.content).slice(0, 16)))
  })
}

// ============================================================ 3 参数不完整的调用不执行

{
  const bodies = []
  let call = 0
  const executed = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    call += 1
    if (call === 1) {
      return {
        sse: [
          // 合法调用
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'good', function: { name: 'list_windows', arguments: '{}' } }] } }] },
          // 参数不是合法 JSON（截断）
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 1, id: 'bad', function: { name: 'screenshot', arguments: '{"app":' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '有一个参数不完整。' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    const events = []
    const agent = new Agent({
      llm: client,
      tools: {
        listTools: () => [],
        invoke: async (name) => { executed.push(name); return { text: 'ok' } },
      },
      maxSteps: 4,
      onEvent: (event) => events.push(event),
    })
    await agent.run('看屏幕')
    check('M12 参数不完整的调用没有被执行', executed.includes('screenshot') === false, executed)
    check('M13 合法调用照常执行', executed.includes('list_windows') === true, executed)
    const second = bodies[1].messages
    const problems = protocolProblems(second)
    check('M14 残缺调用没有留下孤立 tool 结果（序列仍合法）', problems.length === 0, problems.join(','))
    check('M15 assistant 的 tool_calls 只含合法的那一个',
      second.filter((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
        .every((m) => m.tool_calls.every((c) => c.id !== 'bad')),
      JSON.stringify(second.filter((m) => m.role === 'assistant' && Array.isArray(m.tool_calls)).map((m) => m.tool_calls.map((c) => c.id))))
    // 混合情况（有的能用、有的坏了）：坏的那个不执行，并通过事件如实告知"已丢弃"
    check('M16 残缺调用会通过事件如实报告（不静默丢弃）',
      events.some((e) => e.type === 'tool_end' && e.ok === false && String(e.text ?? '').includes('参数不完整')),
      JSON.stringify(events.filter((e) => e.type === 'tool_end').map((e) => `${e.name}:${e.ok}:${e.text}`)))
  })
}

// ============================================================ 4 流被截断时一个工具都不执行

{
  const executed = []
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'x', function: { name: 'screenshot', arguments: '{}' } }] } }] },
    ],
    closeEarly: true,
  }), async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [], invoke: async (name) => { executed.push(name); return { text: 'ok' } } },
      maxSteps: 3,
    })
    const result = await agent.run('截个图')
    check('M17 流被截断（没有结束信号）时一个工具都不执行', executed.length === 0, executed)
    check('M18 截断回合被标记为失败（不伪装成正常完成）',
      result.failed === true || result.text.includes('异常'), `failed=${result.failed} text=${result.text.slice(0, 20)}`)
  })
}

// ============================================================ 5 步数上限后的收尾进历史

{
  const bodies = []
  let call = 0
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    call += 1
    // 永远要求调工具，逼到步数上限
    if (call <= 2) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: `t${call}`, function: { name: 'scroll', arguments: '{"n":1}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    // 收尾往返（不带 tools）：给一句话
    return { sse: [{ choices: [{ index: 0, delta: { content: '做到第三页了，还差最后一页。' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [], invoke: async () => ({ text: '滚动结果' }) },
      maxSteps: 2,
    })
    const result = await agent.run('一路滚到底')
    const closingBody = bodies.at(-1)
    check('M19 达到步数上限后补了一次不带工具的收尾往返',
      closingBody.tools === undefined && closingBody.messages.some((m) => typeof m.content === 'string' && m.content.includes('已经用满')),
      `tools=${JSON.stringify(closingBody.tools)}`)
    check('M20 收尾回复进入了本轮 turn（下一回合记得自己说过什么）',
      result.turn.some((m) => m.role === 'assistant' && typeof m.content === 'string' && m.content.includes('还差最后一页')),
      result.turn.map((m) => `${m.role}:${String(m.content ?? '').slice(0, 10)}`))
    check('M21 收尾回复不重复追加（只出现一次）',
      result.turn.filter((m) => m.role === 'assistant' && String(m.content ?? '').includes('还差最后一页')).length === 1,
      result.turn.filter((m) => m.role === 'assistant').length)
    check('M22 内部"用满步数"提示没有留在历史里（协议干净）',
      !result.turn.some((m) => typeof m.content === 'string' && m.content.includes('已经用满')),
      result.turn.filter((m) => m.role === 'user').map((m) => String(m.content).slice(0, 16)))
    const problems = protocolProblems(result.turn)
    check('M23 收尾后的历史序列合法', problems.length === 0, problems.join(','))
    check('M24 本轮标记 limited（上层能如实告知）', result.limited === true, result.limited)
  })
}

// ============================================================ 6 assistantMessageFrom 的字段规则

{
  const mimoMsg = assistantMessageFrom(
    { text: '先说一句', reasoning: '思考内容', toolCalls: [{ id: 'c1', name: 'x', arguments: '{}' }] },
    { capability: 'mimo' },
  )
  check('M25 MiMo：reasoning_content 与 tool_calls 同一条消息',
    mimoMsg.reasoning_content === '思考内容' && Array.isArray(mimoMsg.tool_calls), Object.keys(mimoMsg).join('|'))

  const plainMsg = assistantMessageFrom(
    { text: '普通回答', reasoning: '思考内容', toolCalls: [] },
    { capability: 'openai-compatible' },
  )
  check('M26 普通兼容端点：不带 reasoning_content（私有字段会污染请求）',
    plainMsg.reasoning_content === undefined, Object.keys(plainMsg).join('|'))

  const noReasoning = assistantMessageFrom(
    { text: '没思考', reasoning: '', toolCalls: [] },
    { capability: 'deepseek' },
  )
  check('M27 没产生思考时不回传空 reasoning（避免被当成"上一轮思考丢了"）',
    noReasoning.reasoning_content === undefined, Object.keys(noReasoning).join('|'))

  const withBadCall = assistantMessageFrom(
    { text: '', reasoning: 'r', toolCalls: [{ id: 'a', name: 'x', arguments: '{}' }, { id: 'b', name: 'y', arguments: '{', valid: false }] },
    { capability: 'mimo' },
  )
  check('M28 残缺调用不进入 tool_calls（不给它补假结果）',
    withBadCall.tool_calls.length === 1 && withBadCall.tool_calls[0].id === 'a',
    withBadCall.tool_calls.map((c) => c.id))
}

// ============================================================ 5 内部标记绝不进请求体

{
  /*
   * loop 会插"（参数不完整，请重新生成）"这类内部提示，并给消息打 `internal` 标记，
   * 供压缩时区分"用户回合"与"程序提示"。这些标记是本地语义：
   * 一旦随请求发出去，模型会收到它看不懂的字段，严格的端点会直接 400。
   */
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    if (bodies.length === 1) {
      // 参数坏掉 → loop 会插入一条带 internal 标记的提示
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'b1', function: { name: 'scroll', arguments: '{bad' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '好了' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: profileFromConfig({ providerId: 'openai-compatible', model: 'm' }) })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [{ type: 'function', function: { name: 'scroll', parameters: {} } }], invoke: async () => ({ text: 'ok' }) },
      maxSteps: 4,
    })
    const result = await agent.run('滚到底')
    const leaked = bodies.flatMap((body) => body.messages
      .filter((message) => 'internal' in message || '_providerId' in message))
    // 内部提示是在"参数坏了之后"才插进来的，所以它出现在**下一个**请求里
    check('M29 内部提示确实被插入了（这条测试才有意义）',
      bodies.some((body) => body.messages.some((m) => typeof m.content === 'string'
        && (m.content.includes('参数不完整') || m.content.includes('模型响应不完整')))),
      bodies.length)
    check('M30 本地标记（internal / _providerId）绝不进请求体', leaked.length === 0,
      JSON.stringify(leaked.map((m) => m.role)))
    check('M31 回合增量仍然保留 internal 标记（下一轮压缩还认得出）',
      result.turn.some((m) => m.internal === true), result.turn.map((m) => m.internal ?? null).join(','))

    // 下一轮带着历史重放：仍然不得泄漏
    bodies.length = 0
    await agent.run('再来一下', { history: result.turn })
    const leakedAgain = bodies.flatMap((body) => body.messages
      .filter((message) => 'internal' in message || '_providerId' in message))
    check('M32 历史重放时标记同样不泄漏', leakedAgain.length === 0,
      JSON.stringify(leakedAgain.map((m) => m.role)))
  })
}

// ============================================================ 汇总
const failed = results.filter((r) => !r.pass)
console.log('='.repeat(72))
console.log(JSON.stringify({ suite: 'tool message order', cases: results.length, failed: failed.length, failedNames: failed.map((r) => r.name) }))
console.log(failed.length === 0 ? '判定：全部通过' : '判定：存在未通过项')
process.exit(failed.length === 0 ? 0 : 1)
