/**
 * 本地 mock 的 OpenAI 兼容服务（仅用于离线验证 agent loop）。
 *
 * 两种用法：
 *  1. **固定剧本**（不带 handler 时，verify-p1 用它，行为和以前一致）：
 *     第 1 次调用 → 返回一个 tool_call（list_windows），模拟"模型决定先观察"
 *     第 2 次调用（已带工具结果）→ 返回最终回答
 *  2. **可编程**：传 `handler(ctx)` 返回响应描述，验收脚本自己决定回什么 SSE，
 *     并且**收到的每一个请求体都被记下来**（核对推理参数、reasoning_content 回传
 *     这两件事只能靠看真实发出去的字节）。
 *
 * 不花 token、不联网就能验证：流式解析、tool_calls 增量拼接、工具执行、
 * 结果回填、上下文压缩这条完整链路。
 */
import { createServer } from 'node:http'
import { pathToFileURL } from 'node:url'

/**
 * @param {object|number} [options] 数字视为端口（兼容旧调用）
 * @param {number} [options.port]
 * @param {(ctx:{url:string,method:string,body:any,attempt:number})=>any|Promise<any>} [options.handler]
 *   返回 undefined/null → 404；否则：
 *    - `{status: 4xx|5xx, json}` → 回错误
 *    - `{json}` → 回 JSON
 *    - `{sse: [...]}` → 逐条写 SSE（数组项可以是对象=一条 data:，或字符串=原样字节）
 *    - `{closeEarly: true}` → 写完不发 [DONE] 就断连（模拟截断/中断）
 *    - `{delayMs}` → 先等一会儿（测超时）
 */
export function startMockServer(options = {}) {
  const port = typeof options === 'number' ? options : (options.port ?? Number(process.env.MOCK_PORT ?? 18080))
  const handler = typeof options === 'object' && options !== null ? options.handler : undefined
  /** 收到的所有请求（含 headers 与解析后的 body）；验收脚本按它核对实际发出的请求体 */
  const requests = []

  const server = createServer(async (req, res) => {
    const raw = await readRaw(req)
    let body = null
    try {
      body = raw === '' ? null : JSON.parse(raw)
    } catch {
      body = { __unparseable: raw }
    }
    const attempt = requests.push({
      url: req.url ?? '',
      method: req.method ?? '',
      headers: { ...req.headers },
      body,
      raw,
    })
    const ctx = { url: req.url ?? '', method: req.method ?? '', body, attempt, headers: req.headers }

    if (typeof handler === 'function') {
      applyPlan(res, await handler(ctx))
      return
    }
    if (req.method === 'POST' && (req.url ?? '').endsWith('/chat/completions')) {
      applyPlan(res, scriptedPlan(body))
      return
    }
    // 默认只认 /models
    applyPlan(res, { json: { data: [{ id: 'mock-tool-use' }] } })
  })

  return new Promise((resolve) => {
    server.listen(port, '127.0.0.1', () => {
      // port:0 时由系统分配，URL 必须用**实际端口**，不能用传进来的 0
      const actual = server.address()?.port ?? port
      resolve({ server, url: `http://127.0.0.1:${actual}/v1`, requests, port: actual })
    })
  })
}

/** 把一个 plan 落到 HTTP 响应上 */
function applyPlan(res, plan) {
  if (res.writableEnded || res.destroyed) return
  const write = () => {
    if (plan === undefined || plan === null) {
      res.writeHead(404, { 'content-type': 'application/json' })
      return res.end(JSON.stringify({ error: { message: 'not found' } }))
    }
    if (typeof plan.status === 'number' && plan.status !== 200) {
      res.writeHead(plan.status, { 'content-type': 'application/json' })
      return res.end(typeof plan.json === 'string' ? plan.json : JSON.stringify(plan.json ?? { error: { message: 'mock error' } }))
    }
    if (plan.json !== undefined) {
      res.writeHead(200, { 'content-type': 'application/json' })
      return res.end(typeof plan.json === 'string' ? plan.json : JSON.stringify(plan.json))
    }
    const chunks = plan.sse ?? []
    res.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    })
    for (const item of chunks) {
      res.write(typeof item === 'string' ? item : `data: ${JSON.stringify(item)}\n\n`)
    }
    if (plan.closeEarly === true) return res.end()
    res.write('data: [DONE]\n\n')
    res.end()
  }
  if (typeof plan?.delayMs === 'number' && plan.delayMs > 0) {
    const timer = setTimeout(write, plan.delayMs)
    res.on('close', () => clearTimeout(timer))
    return
  }
  write()
}

/** 固定剧本：第一轮调工具，第二轮总结（arguments 分片到达，验证拼接） */
function scriptedPlan(body) {
  const messages = body?.messages ?? []
  const hasToolResult = messages.some((m) => m.role === 'tool')
  const toolCount = messages.filter((m) => m.role === 'tool').length
  const stream = body?.stream !== false
  const wrap = (obj) => (stream ? { choices: [{ index: 0, ...obj }] } : obj)

  if (!hasToolResult) {
    if (!stream) {
      return {
        json: {
          choices: [{
            index: 0,
            message: {
              role: 'assistant',
              content: '我先看一下当前有哪些窗口。',
              tool_calls: [{ id: 'call_mock_1', type: 'function', function: { name: 'list_windows', arguments: '{}' } }],
            },
            finish_reason: 'tool_calls',
          }],
        },
      }
    }
    return {
      sse: [
        wrap({ delta: { role: 'assistant', content: '我先看一下当前有哪些窗口。' } }),
        wrap({ delta: { tool_calls: [{ id: 'call_mock_1', index: 0, type: 'function', function: { name: 'list_windows', arguments: '' } }] } }),
        // arguments 分两段发，验证增量拼接（拼起来必须是合法 JSON：{}）
        wrap({ delta: { tool_calls: [{ index: 0, function: { arguments: '{' } }] } }),
        wrap({ delta: { tool_calls: [{ index: 0, function: { arguments: '}' } }] } }),
        wrap({ delta: {}, finish_reason: 'tool_calls' }),
      ],
    }
  }

  const answer = `我看过了，刚才读了 ${toolCount} 次窗口列表。`
  if (!stream) {
    return { json: { choices: [{ index: 0, message: { role: 'assistant', content: answer }, finish_reason: 'stop' }] } }
  }
  return {
    sse: [
      wrap({ delta: { content: answer.slice(0, 4) } }),
      wrap({ delta: { content: answer.slice(4) } }),
      wrap({ delta: {}, finish_reason: 'stop' }),
    ],
  }
}

function readRaw(req) {
  return new Promise((resolve) => {
    let raw = ''
    req.on('data', (chunk) => { raw += chunk })
    req.on('end', () => resolve(raw))
    req.on('error', () => resolve(raw))
  })
}

// 允许直接运行：node tools/mock-openai.mjs
if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { url } = await startMockServer()
  console.log(`mock OpenAI server: ${url}`)
}
