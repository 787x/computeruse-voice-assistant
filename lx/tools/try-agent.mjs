/**
 * 真实模型回合，带逐轮 LLM 计时（首字/结束分开记）。
 *
 * 目的：把"一次任务 3 秒"拆成每一轮往返的 t_ttft 与 t_decode，
 * 避免把"整段生成"误当成"首字延迟"。
 *
 * 用法：node tools/try-agent.mjs "帮我看看现在开了哪些窗口"
 */
import { loadConfig, resolveApiKey } from '../packages/config/store.mjs'
import { LLMClient } from '../packages/agent/llm.mjs'
import { Agent } from '../packages/agent/loop.mjs'
import { CuaToolkit } from '../packages/cua/adapter.mjs'

const input = process.argv.slice(2).join(' ') || '帮我看看现在开了哪些窗口'
const config = await loadConfig()
const apiKey = await resolveApiKey()

const base = new LLMClient({
  baseURL: config.baseURL,
  apiKey,
  model: config.model,
  temperature: config.temperature,
  timeoutMs: config.timeoutMs,
})

// 逐轮计时包装：对每次 chat 调用记录 ttft / decode / total
const rounds = []
const llm = {
  async chat(options) {
    const started = performance.now()
    let ttft = null
    const wrapped = {
      ...options,
      onDelta: (text) => {
        if (ttft === null) ttft = performance.now() - started
        options.onDelta?.(text)
      },
    }
    const result = await base.chat(wrapped)
    const total = performance.now() - started
    rounds.push({
      ttft: ttft === null ? null : Math.round(ttft),
      decode: Math.round(total - (ttft ?? 0)),
      total: Math.round(total),
      toolCalls: result.toolCalls.map((c) => c.name),
      chars: result.text.length,
    })
    return result
  },
}

const toolkit = new CuaToolkit({ frameDir: 'packages/cua/frames' })
const agent = new Agent({
  llm,
  tools: toolkit,
  maxSteps: config.maxSteps,
  onEvent: (event) => {
    if (event.type === 'tool_start') console.log(`[→ ${event.name}]`)
    if (event.type === 'tool_end') console.log(`[← ${event.name} ${event.ok ? 'ok' : 'fail'} ${event.ms ?? ''}ms]`)
  },
})

try {
  await toolkit.warmup()
  const started = Date.now()
  const result = await agent.run(input)
  const wall = Date.now() - started

  console.log('\n=== 用户 ===\n' + input)
  console.log('\n=== 助手 ===\n' + result.text)
  console.log('\n=== 计时 ===')
  console.log(JSON.stringify({
    model: config.model,
    wallMs: wall,
    llmRounds: rounds,
    firstSpeakableMs: rounds[0]?.ttft ?? null,
    toolMs: result.steps.reduce((sum, s) => sum + (s.ms ?? 0), 0),
  }, null, 1))
} catch (error) {
  console.error('失败：', error.name, error.message)
  process.exitCode = 1
} finally {
  await toolkit.dispose()
}
