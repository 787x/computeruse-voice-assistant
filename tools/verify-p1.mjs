/**
 * P1 验收：agent loop 端到端跑通（离线，用 mock 模型服务）。
 *
 * 验证点：
 *   1. 配置层可读写、密钥打码
 *   2. 流式 SSE 解析（含 tool_calls 分片拼接）
 *   3. 工具真实执行（list_windows 打到 cua 后端）
 *   4. 工具结果回填 + 二轮生成
 *   5. 事件流（text_delta / tool_start / tool_end / done）齐全
 *   6. 中断可用
 */
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const configDir = await mkdtemp(join(tmpdir(), 'cua-va-cfg-'))
process.env.CUA_VA_CONFIG_DIR = configDir

const { startMockServer } = await import('./mock-openai.mjs')
const { CuaToolkit, DEFAULT_TOOL_NAMES } = await import('../packages/cua/adapter.mjs')
const { LLMClient } = await import('../packages/agent/llm.mjs')
const { Agent } = await import('../packages/agent/loop.mjs')
const { saveConfig, loadPublicConfig, loadConfig } = await import('../packages/config/store.mjs')

const report = {}
const mock = await startMockServer()

// 1) 配置层
await saveConfig({
  providerId: 'mock',
  baseURL: mock.url,
  model: 'mock-tool-use',
  apiKey: 'sk-test-1234567890abcdef',
  maxSteps: 6,
})
report.config = await loadPublicConfig()
const rawConfig = await loadConfig()
report.configRoundTrip = {
  keyMasked: report.config.apiKey.includes('*'),
  keyStored: rawConfig.apiKey === 'sk-test-1234567890abcdef',
  baseURL: report.config.baseURL,
}

// 2) 组装 agent
const toolkit = new CuaToolkit({ frameDir: join(configDir, 'frames') })
const llm = new LLMClient({
  baseURL: rawConfig.baseURL,
  apiKey: rawConfig.apiKey,
  model: rawConfig.model,
  temperature: rawConfig.temperature,
  timeoutMs: 30000,
})
const events = []
const agent = new Agent({
  llm,
  tools: toolkit,
  maxSteps: rawConfig.maxSteps,
  onEvent: (event) => events.push(event),
})

try {
  await toolkit.warmup()
  const started = Date.now()
  const result = await agent.run('帮我看看现在开了哪些窗口')
  report.run = {
    ms: Date.now() - started,
    text: result.text,
    steps: result.steps.map((s) => ({ step: s.step, tool: s.tool, ok: s.ok })),
  }

  const counts = events.reduce((acc, event) => {
    acc[event.type] = (acc[event.type] ?? 0) + 1
    return acc
  }, {})
  report.events = {
    counts,
    textDeltas: events.filter((e) => e.type === 'text_delta').map((e) => e.text),
    toolCalls: events.filter((e) => e.type === 'tool_start').map((e) => e.name),
  }

  // 3) 中断验证（另起一个回合，立刻 abort）
  const controller = new AbortController()
  controller.abort()
  try {
    await agent.run('这条应该被立刻中断', { signal: controller.signal })
    report.abort = { worked: false, reason: '没有抛错' }
  } catch (error) {
    report.abort = { worked: error.name === 'AbortError', name: error.name }
  }

  report.verdict = {
    toolExecuted: report.run.steps.some((s) => s.tool === 'list_windows' && s.ok),
    gotFinalText: typeof result.text === 'string' && result.text.length > 0,
    eventsOk: counts.tool_start === 1 && counts.tool_end === 1 && counts.done === 1,
    abortOk: report.abort.worked === true,
    keyMasked: report.configRoundTrip.keyMasked,
  }
  report.verdict.pass = Object.values(report.verdict).every((v) => v === true)
} catch (error) {
  report.error = error instanceof Error ? `${error.name}: ${error.message}` : String(error)
  report.verdict = { pass: false }
} finally {
  await toolkit.dispose()
  mock.server.close()
}

console.log(JSON.stringify(report, null, 1))
process.exitCode = report.verdict?.pass === true ? 0 : 1
