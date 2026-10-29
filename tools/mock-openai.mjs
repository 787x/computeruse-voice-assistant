/**
 * 本地 mock 的 OpenAI 兼容服务（仅用于离线验证 agent loop）。
 *
 * 行为：
 *   第 1 次调用 → 返回一个 tool_call（list_windows），模拟"模型决定先观察"
 *   第 2 次调用（已带工具结果）→ 返回最终回答
 *
 * 这样可以在不花 token、不联网的情况下验证：流式解析、tool_calls 增量拼接、
 * 工具执行、结果回填、上下文压缩这条完整链路。
 */
import { createServer } from 'node:http'
import { pathToFileURL } from 'node:url'

export function startMockServer(port = Number(process.env.MOCK_PORT ?? 18080)) {
  const server = createServer(async (req, res) => {
    if (req.method === 'GET' && req.url?.endsWith('/models')) {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ data: [{ id: 'mock-tool-use' }] }))
      return
    }

    if (req.method !== 'POST' || !req.url?.endsWith('/chat/completions')) {
      res.writeHead(404, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: { message: 'not found' } }))
      return
    }

    const body = await readJson(req)
    const messages = body?.messages ?? []
    const hasToolResult = messages.some((m) => m.role === 'tool')
    const toolCount = messages.filter((m) => m.role === 'tool').length

    const stream = res.writeHead === undefined ? false : body?.stream !== false
    if (stream) {
      res.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      })
    }

    const send = (obj) => {
      if (stream) res.write(`data: ${JSON.stringify(obj)}\n\n`)
    }

    if (!hasToolResult) {
      // 第一轮：要求调用 list_windows
      const callDelta = {
        id: 'call_mock_1',
        index: 0,
        type: 'function',
        function: { name: 'list_windows', arguments: '' },
      }
      if (stream) {
        send({ choices: [{ index: 0, delta: { role: 'assistant', content: '我先看一下当前有哪些窗口。' } }] })
        send({ choices: [{ index: 0, delta: { tool_calls: [callDelta] } }] })
        // arguments 分两段发，验证增量拼接（拼起来必须是合法 JSON：{}）
        send({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '{' } }] } }] })
        send({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '}' } }] } }] })
        send({ choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] })
        res.write('data: [DONE]\n\n')
        res.end()
      } else {
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(JSON.stringify({
          choices: [{
            index: 0,
            message: {
              role: 'assistant',
              content: '我先看一下当前有哪些窗口。',
              tool_calls: [{ id: 'call_mock_1', type: 'function', function: { name: 'list_windows', arguments: '{}' } }],
            },
            finish_reason: 'tool_calls',
          }],
        }))
      }
      return
    }

    // 第二轮：总结
    const answer = `我看过了，刚才读了 ${toolCount} 次窗口列表。`
    if (stream) {
      for (const piece of [answer.slice(0, 4), answer.slice(4)]) {
        send({ choices: [{ index: 0, delta: { content: piece } }] })
      }
      send({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })
      res.write('data: [DONE]\n\n')
      res.end()
    } else {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify({
        choices: [{ index: 0, message: { role: 'assistant', content: answer }, finish_reason: 'stop' }],
      }))
    }
  })

  return new Promise((resolve) => {
    server.listen(port, '127.0.0.1', () => resolve({ server, url: `http://127.0.0.1:${port}/v1` }))
  })
}

function readJson(req) {
  return new Promise((resolve, reject) => {
    let raw = ''
    req.on('data', (chunk) => { raw += chunk })
    req.on('end', () => {
      try { resolve(JSON.parse(raw || '{}')) } catch (error) { reject(error) }
    })
    req.on('error', reject)
  })
}

// 允许直接运行：node tools/mock-openai.mjs
if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { url } = await startMockServer()
  console.log(`mock OpenAI server: ${url}`)
}
