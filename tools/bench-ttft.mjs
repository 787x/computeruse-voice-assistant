/**
 * TTFT 拆解基准：把"首字延迟"分解成可归因的几段。
 *
 * 测四段：
 *   t_conn   —— 从发起到拿到响应头（DNS/TLS/排队/服务端首处理）
 *   t_ttfb   —— 到收到第一个 SSE 字节
 *   t_ttft   —— 到第一个有内容的 delta（真正的"首字"）
 *   t_total  —— 到 [DONE]
 *
 * 三组对照，隔离变量：
 *   A 最小请求（无 tools、一句话 system）
 *   B 加正式 system prompt（无 tools）
 *   C 加 12 个工具 schema（= 线上真实形态）
 */
import { loadConfig, resolveApiKey } from '../packages/config/store.mjs'

const config = await loadConfig()
const apiKey = await resolveApiKey()
const baseURL = config.baseURL.replace(/\/+$/u, '')

const SYSTEM = `你是一个能操作 Windows 电脑的语音助手。用户在语音里说的话可能很短、口语化，也可能有省略。

规则：
1. 先观察再动手：不确定界面状态时，先调用 screenshot 或 get_app_state，不要盲猜。
2. 一次只做一个关键动作，做完用一句话说明结果。
3. 回答要短，像人说话，不要罗列步骤清单，不要复述工具名（说"我正在点发送"，不要说"调用 left_click"）。
4. 找不到目标时，明确说没找到，不要假装成功。
5. 高风险动作（删除、发送、支付、改设置）在执行前先向用户确认。`

// 用真实的 12 个工具 schema，而不是编造的
const { CuaToolkit } = await import('../packages/cua/adapter.mjs')
const toolkit = new CuaToolkit({ frameDir: 'packages/cua/frames' })
const tools = toolkit.listTools()

const CASES = [
  { name: 'A 最小请求（无 tools）', messages: [{ role: 'user', content: '说一句：你好' }], tools: undefined },
  { name: 'B 正式 system（无 tools）', messages: [{ role: 'system', content: SYSTEM }, { role: 'user', content: '说一句：你好' }], tools: undefined },
  { name: 'C 正式 system + 12 工具', messages: [{ role: 'system', content: SYSTEM }, { role: 'user', content: '说一句：你好' }], tools },
  // D：把 12 个工具压成 1 个（action 枚举），payload 应显著变小
  {
    name: 'D 单工具（action 枚举）',
    messages: [{ role: 'system', content: SYSTEM }, { role: 'user', content: '说一句：你好' }],
    tools: [{
      type: 'function',
      function: {
        name: 'computer_use',
        description: 'Observe or control the Windows desktop. Pick an action and pass its parameters.',
        parameters: {
          type: 'object',
          properties: {
            action: {
              type: 'string',
              enum: ['list_windows', 'get_app_state', 'screenshot', 'open_application', 'left_click', 'type', 'key', 'scroll', 'set_value', 'wait', 'read_clipboard'],
              description: 'Which action to run.',
            },
          },
          required: ['action'],
          additionalProperties: true,
        },
      },
    }],
  },
  // E：保留 12 个工具，但把描述压到一句话
  {
    name: 'E 12 工具 + 精简描述',
    messages: [{ role: 'system', content: SYSTEM }, { role: 'user', content: '说一句：你好' }],
    tools: tools.map((tool) => ({
      type: 'function',
      function: {
        name: tool.function.name,
        description: tool.function.description.split(/[.。]/u)[0].slice(0, 60),
        parameters: tool.function.parameters,
      },
    })),
  },
]

async function once({ messages, tools }) {
  const body = { model: config.model, messages, temperature: config.temperature, stream: true }
  if (tools) { body.tools = tools; body.tool_choice = 'auto' }
  const payloadBytes = Buffer.byteLength(JSON.stringify(body), 'utf8')

  const t0 = performance.now()
  const res = await fetch(`${baseURL}/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${apiKey}` },
    body: JSON.stringify(body),
  })
  const tConn = performance.now() - t0

  const reader = res.body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  let tTTFB = null
  let tTTFT = null
  let text = ''

  for (;;) {
    const { value, done } = await reader.read()
    if (done) break
    const now = performance.now()
    if (tTTFB === null) tTTFB = now - t0
    buffer += decoder.decode(value, { stream: true })

    let idx
    while ((idx = buffer.indexOf('\n\n')) >= 0) {
      const raw = buffer.slice(0, idx)
      buffer = buffer.slice(idx + 2)
      for (const line of raw.split('\n')) {
        const trimmed = line.trim()
        if (!trimmed.startsWith('data:')) continue
        const payload = trimmed.slice(5).trim()
        if (payload === '[DONE]') continue
        let json
        try { json = JSON.parse(payload) } catch { continue }
        const delta = json?.choices?.[0]?.delta?.content
        if (typeof delta === 'string' && delta !== '') {
          text += delta
          if (tTTFT === null) tTTFT = now - t0
        }
      }
    }
  }

  return {
    payloadKB: Math.round((payloadBytes / 1024) * 10) / 10,
    t_conn: Math.round(tConn),
    t_ttfb: tTTFB === null ? null : Math.round(tTTFB),
    t_ttft: tTTFT === null ? null : Math.round(tTTFT),
    t_total: Math.round(performance.now() - t0),
    outChars: text.length,
  }
}

const results = []
const ROUNDS = 5
const samples = new Map(CASES.map((c) => [c.name, []]))

try {
  // 交错采样 + 每轮打乱顺序，抵消服务端负载漂移
  for (let round = 0; round < ROUNDS; round += 1) {
    const order = [...CASES].sort(() => Math.random() - 0.5)
    for (const testCase of order) {
      try {
        const r = await once(testCase)
        samples.get(testCase.name).push(r)
      } catch (error) {
        samples.get(testCase.name).push({ error: error.message })
      }
      await new Promise((r) => setTimeout(r, 200))
    }
  }

  for (const testCase of CASES) {
    const all = samples.get(testCase.name)
    const ok = all.filter((r) => !r.error)
    const stat = (key) => {
      const vals = ok.map((r) => r[key]).filter((v) => typeof v === 'number').sort((a, b) => a - b)
      if (vals.length === 0) return { median: null, min: null, max: null }
      return { median: vals[Math.floor(vals.length / 2)], min: vals[0], max: vals[vals.length - 1] }
    }
    results.push({
      case: testCase.name,
      payloadKB: stat('payloadKB').median,
      t_conn: stat('t_conn'),
      t_ttfb: stat('t_ttfb'),
      t_ttft: stat('t_ttft'),
      t_total: stat('t_total'),
      n: ok.length,
      errors: all.filter((r) => r.error).map((r) => r.error),
    })
  }
} finally {
  await toolkit.dispose()
}

console.log(JSON.stringify({ model: config.model, baseURL, rounds: ROUNDS, results }, null, 1))
