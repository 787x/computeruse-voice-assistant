/**
 * 模型响应失败 / 工具结果 / 截图上限的行为验收。
 *
 * 为什么单独一份：这三件事以前分别错在三个地方，而且都"看起来正常"——
 *  1. **失败被当成成功**：LLM 流到一半报错/断流/被长度截断时，Agent 已经标记了
 *     `failed`，但主程序从来不看，用户听到半句话之后界面就"好了"，
 *     下一轮还会基于一段半截回答去猜用户意图。
 *  2. **工具结果固定截到 1,200 字**：CUA / 文件 / 命令输出的 1,200 字经常只是表头，
 *     模型于是"看着半截结果瞎猜"，而上下文窗口其实有 1M 空着。
 *  3. **截图超过 16 张就静默丢**：模型以为屏幕上没变化，继续在错误的位置点击。
 *
 * 这里用可编程的 Mock 端点 + 假工具，逐条断言**实际发给模型的字节**与本轮最终状态，
 * 不联网、不用真实 Key。
 *
 * 用法：node tools/verify-turn-failure.mjs
 */
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createServer } from 'node:http'

const configDir = await mkdtemp(join(tmpdir(), 'cua-failure-'))
process.env.CUA_VA_CONFIG_DIR = configDir
delete process.env.CUA_VA_API_KEY

const { startMockServer } = await import('./mock-openai.mjs')
const { Agent } = await import('../packages/agent/loop.mjs')
const { LLMClient, profileFromConfig } = await import('../packages/agent/llm.mjs')

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

const noTools = { listTools: () => [], invoke: async () => ({ text: '' }) }
const plainProfile = { providerId: 'openai-compatible' }

// ============================================================ 1 失败状态一路传到 Agent 结果

{
  // A) 说到一半服务端报错（HTTP 200 + 流内 error）
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { content: '我看看这个' } }] },
      { error: { message: '上游模型炸了' } },
    ],
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const events = []
    const agent = new Agent({ llm: client, tools: noTools, maxSteps: 2, onEvent: (e) => events.push(e) })
    const result = await agent.run('问一下')
    check('F1 流内报错 → failed=true（不当成功回答）', result.failed === true, `failed=${result.failed}`)
    check('F2 已经流出的文字保留（不重播、不吞掉）',
      result.text === '我看看这个', JSON.stringify(result.text))
    check('F3 发出 stream_error 事件（上层能立刻知道）',
      events.some((e) => e.type === 'stream_error'), events.map((e) => e.type).join(','))
    check('F4 done 事件里也带 failed（收尾判定看得见）',
      events.some((e) => e.type === 'done' && e.failed === true), '')
  })

  // B) 断流（不发 [DONE]/finish_reason 就结束）
  await withMock(() => ({
    sse: [{ choices: [{ index: 0, delta: { content: '说到一半' } }] }],
    closeEarly: true,
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const agent = new Agent({ llm: client, tools: noTools, maxSteps: 2 })
    const result = await agent.run('问一下')
    check('F5 断流 → failed=true（不伪装成正常收尾）', result.failed === true, `failed=${result.failed}`)
    check('F6 断流保留已到文字', result.text === '说到一半', JSON.stringify(result.text))
  })

  // C) 输出被长度上限截断（finish_reason=length）
  await withMock(() => ({
    sse: [{ choices: [{ index: 0, delta: { content: '太长被砍' }, finish_reason: 'length' }] }],
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const agent = new Agent({ llm: client, tools: noTools, maxSteps: 2 })
    const result = await agent.run('问一下')
    check('F7 finish_reason=length → failed=true', result.failed === true, `failed=${result.failed}`)
  })

  // D) 正常完成 → 不误报失败（连续对话/追问判定照旧）
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { content: '好的，都办完了。' } }] },
      { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] },
    ],
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const events = []
    const agent = new Agent({ llm: client, tools: noTools, maxSteps: 2, onEvent: (e) => events.push(e) })
    const result = await agent.run('问一下')
    check('F8 正常完成不报失败', result.failed === false && !events.some((e) => e.type === 'stream_error'),
      `failed=${result.failed}`)
    check('F9 正常完成仍然走 assistant 事件（追问判定照旧）',
      events.some((e) => e.type === 'assistant' && e.text === '好的，都办完了。'), events.map((e) => e.type).join(','))
  })
}

// ============================================================ 2 用户主动打断 ≠ 模型故障

{
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { content: '我先' } }] },
      { choices: [{ index: 0, delta: { content: '看一下' } }] },
    ],
    delayMs: 400,
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const agent = new Agent({ llm: client, tools: noTools, maxSteps: 2 })
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 250)
    let thrown = null
    try {
      await agent.run('问一下', { signal: controller.signal })
    } catch (error) {
      thrown = error
    }
    check('F10 用户打断走正常取消流程（抛 AbortError，不报模型故障）',
      thrown !== null && thrown.name === 'AbortError', thrown?.name ?? '没抛')
  })
}

// ============================================================ 3 失败后不自动重试已执行的 CUA 动作

{
  const executed = []
  const tools = {
    listTools: () => [{ type: 'function', function: { name: 'left_click', parameters: {} } }],
    invoke: async (name) => { executed.push(name); return { text: '点了一下' } },
  }
  await withMock(({ body }) => {
    const hasToolResult = body.messages.some((m) => m.role === 'tool')
    if (hasToolResult) {
      // 工具已经执行完，但这一轮在收尾流里被切断（没有结束信号）
      return {
        sse: [{ choices: [{ index: 0, delta: { content: '点好了，我' } }] }],
        closeEarly: true,
      }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'c1', function: { name: 'left_click', arguments: '{}' } }] } }] },
        { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
      ],
    }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const agent = new Agent({ llm: client, tools, maxSteps: 3 })
    const result = await agent.run('点一下')
    check('F11 失败后不重放已执行的工具动作（不重复点击）',
      executed.length === 1 && executed[0] === 'left_click', executed)
    check('F11b 失败回合也标记 failed（上层不会当成功）', result.failed === true, `failed=${result.failed}`)
    check('F12 失败回合也保留工具执行记录（用户知道发生过什么）',
      result.steps.some((step) => step.tool === 'left_click'), JSON.stringify(result.steps.map((s) => s.tool)))
  })
}

// ============================================================ 4 CRLF 跨分片

{
  /*
   * SSE 允许 CRLF 作为行分隔符。**`\r` 与 `\n` 落在两个读取分片里**时，
   * 如果按分片各自做换行归一化，就会把 `\r\n` 变成 `\n\n`——凭空多出一个事件边界，
   * 于是后半截事件被拆坏（丢数据或整段丢失）。
   */
  const full = [
    'data: {"choices":[{"index":0,"delta":{"content":"第一段"}}]}\r\n\r\n',
    'data: {"choices":[{"index":0,"delta":{"content":"第二段"},"finish_reason":"stop"}]}\r\n\r\n',
    'data: [DONE]\r\n\r\n',
  ].join('')
  const bytes = Buffer.from(full, 'utf8')

  async function chatWithSplits(splits) {
    const server = createServer((req, res) => {
      res.writeHead(200, { 'content-type': 'text/event-stream' })
      const parts = []
      let cursor = 0
      for (const at of splits) { parts.push(bytes.subarray(cursor, at)); cursor = at }
      parts.push(bytes.subarray(cursor))
      let index = 0
      const tick = () => {
        if (index >= parts.length) return res.end()
        res.write(parts[index])
        index += 1
        setTimeout(tick, 6)
      }
      tick()
    })
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
    const port = server.address().port
    const client = new LLMClient({ baseURL: `http://127.0.0.1:${port}/v1`, apiKey: 'k', model: 'm' })
    const got = await client.chat({ messages: [{ role: 'user', content: 'x' }] })
    server.close()
    return got
  }

  // 找出所有 \r\n 的中间位置（\r 之后、\n 之前）以及 \n\n 之间
  const boundarySplits = []
  for (let i = 1; i < bytes.length; i += 1) {
    const prev = String.fromCharCode(bytes[i - 1])
    const cur = String.fromCharCode(bytes[i])
    if ((prev === '\r' && cur === '\n') || (prev === '\n' && cur === '\n')) boundarySplits.push(i)
  }
  let broken = []
  for (const at of boundarySplits) {
    const got = await chatWithSplits([at])
    if (got.text !== '第一段第二段' || got.incomplete === true || got.finishReason !== 'stop') {
      broken.push({ at, text: got.text, incomplete: got.incomplete })
    }
  }
  check('F13 CRLF 恰好在分片边界被切开时事件不丢不拆',
    broken.length === 0, `${boundarySplits.length} 个边界，问题 ${broken.length} 个 ${JSON.stringify(broken.slice(0, 3))}`)

  // 整段一次性到达也要正确
  const atOnce = await chatWithSplits([])
  check('F14 整段 CRLF 一次性到达同样解析正确',
    atOnce.text === '第一段第二段' && atOnce.finishReason === 'stop', JSON.stringify(atOnce.text))
}

// ============================================================ 5 工具结果不再被固定 1,200 字截断

{
  const bigText = `表头行\n${'数据行：这一行是任务真正需要的内容。\n'.repeat(400)}`
  const tools = {
    listTools: () => [{ type: 'function', function: { name: 'run_shell', parameters: {} } }],
    invoke: async () => ({ text: bigText }),
  }
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    if (!body.messages.some((m) => m.role === 'tool')) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'r1', function: { name: 'run_shell', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '看完了' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: plainProfile })
    const agent = new Agent({ llm: client, tools, maxSteps: 3 })
    const result = await agent.run('跑个命令')
    const toolMessage = bodies.at(-1).messages.find((m) => m.role === 'tool')
    check('T1 长工具结果不再被砍到 1,200 字',
      typeof toolMessage?.content === 'string' && toolMessage.content.length > bigText.length * 0.9,
      `${toolMessage?.content.length} / ${bigText.length}`)
    check('T2 结果末尾内容真的在（不是只剩表头）',
      String(toolMessage?.content ?? '').includes('这一行是任务真正需要的内容'), '')
    check('T3 工具结果与调用配对完整', result.steps.length === 1 && result.steps[0].ok === true, JSON.stringify(result.steps))
  })

  // 极端超长时截断，但必须让模型知道自己看的是截断版
  const huge = 'X'.repeat(120000)
  const hugeTools = {
    listTools: () => [{ type: 'function', function: { name: 'run_shell', parameters: {} } }],
    invoke: async () => ({ text: huge }),
  }
  const hugeBodies = []
  await withMock(({ body }) => {
    hugeBodies.push(JSON.parse(JSON.stringify(body)))
    if (!body.messages.some((m) => m.role === 'tool')) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'r2', function: { name: 'run_shell', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '好' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: plainProfile })
    const agent = new Agent({ llm: client, tools: hugeTools, maxSteps: 3 })
    await agent.run('跑个超长命令')
    const sent = hugeBodies.at(-1).messages.find((m) => m.role === 'tool')
    const text = String(sent?.content ?? '')
    check('T4 极端超长结果被截断（不把 100K 字符硬塞）', text.length < huge.length, `${text.length}`)
    check('T5 截断时模型能看出这是截断内容（原长 + 提示）',
      text.includes('结果过长已截断') && text.includes(String(huge.length)), text.slice(-60))
  })
}

// ============================================================ 6 截图不再因 16 张上限静默缺失

{
  /*
   * 连续 20 次截图：旧实现从第 17 张开始**静默不给图**，模型以为屏幕没变化。
   * 现在：前 N 张照常给，超上限的那几张必须留下一条明确的文字说明，
   * 并且这条说明不能插在未完成的工具结果之间。
   */
  const shot = { mediaType: 'image/png', base64: 'AAAA', width: 1280, height: 720 }
  let callCount = 0
  const tools = {
    listTools: () => [{ type: 'function', function: { name: 'screenshot', parameters: {} } }],
    invoke: async () => { callCount += 1; return { text: '截图已保存', image: shot } },
  }
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    const toolCount = body.messages.filter((m) => m.role === 'tool').length
    if (toolCount >= 20) {
      return { sse: [{ choices: [{ index: 0, delta: { content: '看完 20 张了' }, finish_reason: 'stop' }] }] }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: `s${toolCount}`, function: { name: 'screenshot', arguments: '{}' } }] } }] },
        { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
      ],
    }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: plainProfile })
    const agent = new Agent({ llm: client, tools, maxSteps: 24 })
    await agent.run('一直截图直到你确定')
    const finalMessages = bodies.at(-1).messages
    const images = finalMessages.filter((m) => Array.isArray(m.content)
      && m.content.some((part) => part?.type === 'image_url'))
    check('S1 20 次截图里有相当数量真的传给了模型（不是卡在 16）',
      images.length >= 16, `${images.length} 张`)
    const notices = finalMessages.filter((m) => typeof m.content === 'string' && m.content.includes('达到单轮上限'))
    const overflowed = callCount > images.length
    check('S2 超上限的截图有明确说明（不静默缺失）',
      overflowed ? notices.length > 0 : true, `截图 ${callCount} 次，附图 ${images.length} 张，说明 ${notices.length} 条`)
    // 协议完整性：每个 assistant.tool_calls 之后必须紧跟它自己的 tool 结果
    const problems = []
    for (let i = 0; i < finalMessages.length; i += 1) {
      const message = finalMessages[i]
      if (message.role === 'assistant' && Array.isArray(message.tool_calls)) {
        const ids = new Set(message.tool_calls.map((call) => String(call.id)))
        let cursor = i + 1
        while (cursor < finalMessages.length && finalMessages[cursor].role === 'tool') {
          ids.delete(String(finalMessages[cursor].tool_call_id))
          cursor += 1
        }
        if (ids.size > 0) problems.push(`缺结果:${[...ids].join(',')}`)
      }
    }
    check('S3 20 张截图后消息序列仍合法（无孤立工具结果/缺结果调用）',
      problems.length === 0, problems.slice(0, 3))
    const imageBetween = []
    for (let i = 0; i < finalMessages.length; i += 1) {
      const message = finalMessages[i]
      if (message.role === 'assistant' && Array.isArray(message.tool_calls)) {
        let cursor = i + 1
        const sawTool = new Set()
        while (cursor < finalMessages.length && finalMessages[cursor].role === 'tool') {
          sawTool.add(String(finalMessages[cursor].tool_call_id))
          cursor += 1
        }
        // 之后才允许出现图片/说明
        const after = finalMessages[cursor]
        if (after !== undefined && sawTool.size > 0 && Array.isArray(after.content)
          && after.content.some((part) => part?.type === 'image_url')) {
          // 正常：图片排在全部 tool 结果之后
        } else if (after !== undefined && after.role === 'tool') {
          imageBetween.push(i)
        }
      }
    }
    check('S4 没有在未完成的工具结果之间插入 user 图片消息',
      imageBetween.length === 0, imageBetween.slice(0, 3))
  })

  /*
   * 故意超过**当前上限**再截几次：模型必须收到一句"这张图没附上"，而不是凭空少一张。
   * 上限值从 loop 的导出常量读不到，这里用"比分片测试更多轮"的方式压到边界之外。
   */
  let overCalls = 0
  const overTools = {
    listTools: () => [{ type: 'function', function: { name: 'screenshot', parameters: {} } }],
    invoke: async () => { overCalls += 1; return { text: '截图已保存', image: shot } },
  }
  const overBodies = []
  const OVER_RUNS = 26
  await withMock(({ body }) => {
    overBodies.push(JSON.parse(JSON.stringify(body)))
    const toolCount = body.messages.filter((m) => m.role === 'tool').length
    if (toolCount >= OVER_RUNS) {
      return { sse: [{ choices: [{ index: 0, delta: { content: '截完了' }, finish_reason: 'stop' }] }] }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: `o${toolCount}`, function: { name: 'screenshot', arguments: '{}' } }] } }] },
        { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
      ],
    }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: plainProfile })
    const agent = new Agent({ llm: client, tools: overTools, maxSteps: 30 })
    await agent.run('一直截图')
    const finalMessages = overBodies.at(-1).messages
    const images = finalMessages.filter((m) => Array.isArray(m.content)
      && m.content.some((part) => part?.type === 'image_url'))
    const notices = finalMessages.filter((m) => typeof m.content === 'string' && m.content.includes('达到单轮上限'))
    check('S5 超过单轮上限后不再附新图（上限确实生效）',
      images.length < overCalls, `截图 ${overCalls} 次，附图 ${images.length} 张`)
    check('S6 每张被挡掉的图都有明确说明（模型不会以为屏幕没变）',
      notices.length === overCalls - images.length && notices.length > 0,
      `缺 ${overCalls - images.length} 张，说明 ${notices.length} 条`)
  })
}

// ============================================================ 7 装不下时不盲目发请求

{
  /*
   * system 提示词本身就超过窗口容量：压也压不掉（压缩动不了 system），
   * 这时**一个请求都不能发**，并且要明确失败——不能静默截断用户任务后假装正常完成。
   */
  let calls = 0
  const client = {
    profile: plainProfile,
    chat: async () => { calls += 1; return { text: 'x', toolCalls: [], finishReason: 'stop' } },
  }
  const policy = {
    contextWindow: 5_000, compressPercent: 90, compressAtTokens: 4_500,
    hardLimitTokens: 4_000, targetTokens: 3_000, outputReserve: 1_000,
  }
  const events = []
  const agent = new Agent({
    llm: client, tools: noTools, maxSteps: 2, contextPolicy: policy,
    systemPrompt: 'S'.repeat(30_000), onEvent: (e) => events.push(e),
  })
  const result = await agent.run('新问题')
  check('X1 装不下时完全不发请求', calls === 0, `calls=${calls}`)
  check('X2 装不下时标记 failed（不当正常完成）', result.failed === true, `failed=${result.failed}`)
  check('X3 失败原因可区分（context_overflow）', result.failureReason === 'context_overflow', result.failureReason)
  check('X4 说明里给出估算值与硬上限（用户知道为什么）',
    typeof result.text === 'string' && result.text.includes('上限'), result.text.slice(0, 60))
  check('X5 上层收到 stream_error 事件', events.some((e) => e.type === 'stream_error'), events.map((e) => e.type).join(','))
}

// ============================================================ 汇总

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(72))
console.log(JSON.stringify({ suite: 'turn failure + tool payload limits', cases: results.length, failed: failed.length, failedNames: failed.map((r) => r.name) }))
console.log(failed.length === 0 ? '判定：全部通过' : '判定：存在未通过项')
process.exit(failed.length === 0 ? 0 : 1)
