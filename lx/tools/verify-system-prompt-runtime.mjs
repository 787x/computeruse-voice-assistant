/**
 * 系统提示词：主进程（壳）侧的热生效验收（纯 Node，不需要 Electron）。
 *
 * 钉死的是"接线"，而不是配置文件变了没有：
 *   设置保存 → 内存状态 → **已缓存的 Agent** → 下一轮发给 LLM 的真实 messages。
 * 用的是 lx/apps/shell/main.mjs 里真正的 ensureAgent / settings:save handler，
 * 模型与 CUA 桥接是可控替身（不发网络、不动桌面），配置走临时目录。
 *
 * 用法：node lx/tools/verify-system-prompt-runtime.mjs
 */
import assert from 'node:assert/strict'
import { mkdtemp, readFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import vm from 'node:vm'

import { AgentInitialization } from '../packages/agent/initialization.mjs'
import { Agent as RealAgent } from '../packages/agent/loop.mjs'
import { DEFAULT_SYSTEM_PROMPT } from '../packages/agent/system-prompt.mjs'
import { FOLLOWUP_DEFAULTS } from '../packages/agent/followup.mjs'

process.env.CUA_VA_CONFIG_DIR = await mkdtemp(join(tmpdir(), 'cua-va-lx-prompt-shell-'))
const store = await import('../packages/config/store.mjs')
await store.saveConfig({
  providerId: 'mock',
  baseURL: 'http://127.0.0.1:1/v1',
  model: 'mock-tool-use',
  apiKey: 'sk-shell-test-abcdefghijkl',
  maxSteps: 16,
})

const source = (await readFile(new URL('../apps/shell/main.mjs', import.meta.url), 'utf8')).replace(/\r\n/g, '\n')
const shellFunction = (marker) => {
  const start = source.indexOf(marker)
  assert.ok(start >= 0, `main.mjs 里找不到 ${marker}`)
  return source.slice(start, source.indexOf('\n}', start) + 2)
}
/** settings:save 的 handler 文本：body 里没有 2 空格缩进的 `})`，这个结束标记是安全的 */
const saveMarker = "ipcMain.handle('settings:save', async (_event, payload) => {"
const saveStart = source.indexOf(saveMarker)
assert.ok(saveStart >= 0, '找不到 settings:save 的 handler 头部')
const saveEnd = source.indexOf('\n  })', saveStart)
assert.ok(saveEnd > saveStart, 'settings:save 的结束标记没找到')
const saveHandler = `${source.slice(saveStart, saveEnd).replace(saveMarker, 'async function settingsSave(_event, payload) {')
}
}`

const results = []
function record(name, pass, detail) {
  results.push({ name, pass, detail })
  console.log(`${pass ? '✓' : '✗'} ${name}${detail ? ` — ${detail}` : ''}`)
}
function check(name, fn) {
  try {
    record(name, true, fn())
  } catch (error) {
    record(name, false, error.message)
  }
}

const deferred = () => {
  let resolve
  const promise = new Promise((r) => { resolve = r })
  return { promise, resolve }
}

function fixture({ gateVerdicts = [] } = {}) {
  const bridge = { created: 0, disposed: 0 }
  const llmCalls = []
  const gateCalls = []
  const logs = []
  let initStage = null
  let initPause = null
  let toolHook = null
  let replies = []

  /** 在指定的初始化阶段挂起，用来造出"Agent 正在初始化"的确定时机 */
  const stageWait = async (name) => {
    if (initStage === name && initPause !== null) await initPause.promise
  }
  const storeShim = {
    loadConfig: async () => { await stageWait('config'); return store.loadConfig() },
    loadPublicConfig: async () => store.loadPublicConfig(),
    resolveApiKey: async () => store.resolveApiKey(),
    resolveSearchKey: async () => store.resolveSearchKey(),
    saveConfig: async (...args) => store.saveConfig(...args),
    listProviders: () => store.listProviders(),
    CONFIG_FILE: store.CONFIG_FILE,
  }

  class FakeToolkit {
    constructor() { bridge.created += 1; this.closed = false }
    async warmup() {
      await stageWait('warmup')
    }
    listTools() { return [{ type: 'function', function: { name: 'list_windows' } }] }
    async invoke(name, args) {
      if (toolHook !== null) { const hook = toolHook; toolHook = null; await hook() }
      return { text: `${name} ok`, data: {} }
    }
    async dispose() { bridge.disposed += 1; this.closed = true }
  }
  class FakeLLMClient {
    constructor(options) { this.options = options }
    setModel() {}
    setEndpoint() {}
    setProfile() {}
    setTemperature() {}
    setTimeout() {}
    /** 回合快照：本用例关心的是提示词与调用顺序，返回自身即可 */
    snapshot() { return this }
    async chat({ messages }) {
      llmCalls.push(messages.map((m) => ({
        role: m.role,
        content: typeof m.content === 'string' ? m.content : '[multipart]',
      })))
      const next = replies.shift()
      if (next === undefined) return { text: '办好了', toolCalls: [], finishReason: 'stop' }
      return next
    }
  }
  const modules = {
    '../../packages/config/store.mjs': storeShim,
    // 壳子会给 LLMClient 传提供商档案（决定推理参数），桩件必须提供同一个函数
    '../../packages/agent/llm.mjs': { LLMClient: FakeLLMClient, profileFromConfig: (source) => ({ providerId: source?.providerId ?? '', capability: source?.providerId ?? '' }) },
    '../../packages/agent/loop.mjs': { Agent: RealAgent },
    '../../packages/cua/adapter.mjs': { CuaToolkit: FakeToolkit },
    '../../packages/tools/web-search.mjs': { WebSearch: class { enabled = false }, WEB_SEARCH_TOOL: {} },
  }

  const ctx = vm.createContext({
    DOMException, Date, console, process, setTimeout, AbortController, JSON, Math,
    join: (...parts) => parts.join('/'), DATA_ROOT: 'test',
    agentInitialization: null, agentRuntime: null,
    shuttingDown: false, lastUtterance: '测试指令', deniedInTurn: {},
    agentSystemPrompt: null,
    ttsEnabled: true, continuousMode: true, voiceConfirmEnabled: true, subtitlesEnabled: true,
    autoStartEnabled: false, releaseOnFullscreen: true, wakeMutedUntil: 0, ttsEmotion: '中立', ttsSpeed: 1,
    wakeOnLaunch: true, permissionMode: 'standard', followUpConfig: null, resourceConfig: null,
    FOLLOWUP_DEFAULTS, isAutoStartRegistered: () => false,
    permissionGate: null,
    applyAutoStart() {}, setVoiceConfirmEnabled() {}, stopSpeaking() {}, applyModelSchedule() {},
    log: (m) => logs.push(m), handleAgentEvent() {},
    ensurePermissions: async () => ({
      async check({ tool }) {
        gateCalls.push(tool)
        return gateVerdicts.shift() ?? { allow: true }
      },
      update() {},
      permissions: { mode: 'standard' },
    }),
    ensureLauncher: async () => ({ launch: async () => ({ text: '已启动' }) }),
    loadModule: async (path) => modules[path],
    LAUNCH_APP_TOOL: {}, SHELL_TOOL: {}, FILES_TOOL: {}, PROC_TOOL: {}, KILL_TOOL: {},
    SYSINFO_TOOL: {}, LX_MUSIC_TOOL: { function: { name: 'music' } },
    runShell: async () => ({ text: 'ok' }), runFiles: async () => ({ text: 'ok' }),
    runAppControl: async () => ({ text: 'ok' }), runKillApp: async () => ({ text: 'ok' }),
    runSystemInfo: async () => ({ text: 'ok' }),
    runLx: async () => ({ text: 'ok', data: {} }),
    normalizeCuaTarget: async (_toolkit, _name, args) => args,
    invokeCuaWithRecovery: async (toolkit, name, args) => toolkit.invoke(name, args),
    attachScreenshotImage: async () => null,
  })
  const owner = new AgentInitialization({
    publish: (runtime) => { ctx.agentRuntime = runtime },
    diagnostic: () => {},
  })
  ctx.agentInitialization = owner

  vm.runInContext(shellFunction('function syncAgentSystemPrompt(config'), ctx)
  vm.runInContext(shellFunction('async function loadUiPrefs()').replaceAll('import(', 'loadModule('), ctx)
  vm.runInContext(shellFunction('async function ensureAgent(signal)').replaceAll('import(', 'loadModule('), ctx)
  vm.runInContext(saveHandler.replaceAll('import(', 'loadModule('), ctx)

  return {
    ctx,
    get runtime() { return ctx.agentRuntime },
    bridge, llmCalls, gateCalls, logs,
    /** 第 index 次请求里的 system 消息内容 */
    systemOf: (index) => llmCalls[index]?.find((m) => m.role === 'system')?.content,
    lastSystem: () => llmCalls.at(-1)?.find((m) => m.role === 'system')?.content,
    setInitStage(stage, pause = null) { initStage = stage; initPause = pause },
    setToolHook(fn) { toolHook = fn },
    setReplies(list) { replies = list },
    ensureAgent: () => ctx.ensureAgent(),
    saveSettings: (payload) => ctx.settingsSave(null, payload),
    /** 模拟"进程启动"：从磁盘把提示词读回内存（真实 loadUiPrefs） */
    loadUiPrefs: () => ctx.loadUiPrefs(),
    runTurn: async (input, history = []) => {
      const { agent } = await ctx.ensureAgent()
      return agent.run(input, { history })
    },
    close: () => owner.close(),
  }
}

const toolCallReply = (name = 'list_windows') => ({
  text: '先看一眼',
  toolCalls: [{ id: `c-${Math.random().toString(36).slice(2, 6)}`, name, arguments: '{}' }],
  finishReason: 'tool_calls',
})
const plainReply = (text) => ({ text, toolCalls: [], finishReason: 'stop' })
const userTextOf = (call) => call.find((m) => m.role === 'user')?.content

/** 每个场景都从干净状态开始：内存清掉、磁盘回到"未自定义" */
async function freshFixture(options) {
  await store.saveConfig({ systemPrompt: null })
  return fixture(options)
}

/* 1) Agent 尚未初始化时保存 */
{
  const f = await freshFixture()
  try {
    const saved = await f.saveSettings({ systemPrompt: '未初始化时保存的人设' })
    check('未初始化时保存：IPC 返回 ok，配置已落盘', () => {
      assert.equal(saved.ok, true, JSON.stringify(saved.error))
      assert.equal(saved.config.systemPrompt, '未初始化时保存的人设')
      return 'ok'
    })
    f.setReplies([plainReply('好')])
    await f.runTurn('第一句')
    check('未初始化时保存：第一轮（新建 Agent 后的第一个请求）system 即新提示词', () => {
      assert.equal(f.llmCalls.length, 1)
      assert.equal(f.systemOf(0), '未初始化时保存的人设')
      assert.equal(userTextOf(f.llmCalls[0]), '第一句')
      return '不重启也生效'
    })
  } finally { await f.close() }
}

/* 2) 已缓存 Agent 的热更新：A→B→默认，且不重建资源 */
{
  const f = await freshFixture()
  try {
    f.setReplies([plainReply('第一轮的答复')])
    await f.runTurn('第一句')
    check('默认模式：已缓存 Agent 的第一个 system 与原版提示词逐字相同', () => {
      assert.equal(f.systemOf(0), DEFAULT_SYSTEM_PROMPT)
      return `${f.systemOf(0).length} 字`
    })

    await f.saveSettings({ systemPrompt: '人设 A：只说 A' })
    f.setReplies([plainReply('A 的答复')])
    await f.runTurn('第二句')
    check('保存 A 后下一轮 system=A（缓存的 Agent 不重建）', () => {
      assert.equal(f.systemOf(1), '人设 A：只说 A')
      assert.equal(f.bridge.created, 1, `CUA 桥接应只建一次，实际 ${f.bridge.created}`)
      assert.equal(f.bridge.disposed, 0, '热更新不得销毁桥接')
      return 'bridge 未重建'
    })

    await f.saveSettings({ systemPrompt: '人设 B：只说 B' })
    f.setReplies([plainReply('B 的答复')])
    await f.runTurn('第三句')
    check('A 改成 B 后下一轮用 B，不再用 A', () => {
      assert.equal(f.systemOf(2), '人设 B：只说 B')
      assert.notEqual(f.systemOf(2), '人设 A：只说 A')
      assert.equal(f.bridge.created, 1)
      return 'ok'
    })

    const restored = await f.saveSettings({ systemPrompt: null })
    f.setReplies([plainReply('默认的答复')])
    await f.runTurn('第四句')
    check('恢复默认：下一轮 system 回到完整内置提示词，旧自定义不再出现', () => {
      assert.equal(restored.ok, true)
      assert.equal(restored.config.systemPrompt, null)
      assert.equal(f.systemOf(3), DEFAULT_SYSTEM_PROMPT)
      assert.ok(!f.systemOf(3).includes('人设 B'))
      return `${f.systemOf(3).length} 字`
    })
  } finally { await f.close() }
}

/* 3) 某轮正在执行工具调用时保存：本回合快照、下一回合新提示词 */
{
  const f = await freshFixture()
  try {
    await f.saveSettings({ systemPrompt: '长回合用的 A' })
    let toolSaveResult = null
    // 工具执行到一半时保存成 B（真实保存，走同一条 IPC 路径）
    f.setToolHook(async () => { toolSaveResult = await f.saveSettings({ systemPrompt: '中途改成的 B' }) })
    f.setReplies([toolCallReply('list_windows'), plainReply('这回合收尾')])
    await f.runTurn('要两步才能做完的事')
    check('回合中途 A→B：本回合所有请求（含工具之后的那次）都用 A', () => {
      assert.ok(f.llmCalls.length >= 2, `应至少两次往返，实际 ${f.llmCalls.length}`)
      assert.equal(toolSaveResult?.ok, true, '回合中途的保存确实成功了')
      assert.equal(f.systemOf(0), '长回合用的 A')
      assert.equal(f.systemOf(1), '长回合用的 A', '多步工具调用中途不得混入新提示词')
      return `${f.llmCalls.length} 次往返都是 A`
    })
    f.setReplies([plainReply('新一轮')])
    await f.runTurn('下一件事')
    check('回合中途 A→B：下一回合开始用 B', () => {
      assert.equal(f.lastSystem(), '中途改成的 B')
      return 'B 生效'
    })
  } finally { await f.close() }
}

/* 4) Agent 正在初始化时保存 */
{
  const f = await freshFixture()
  try {
    const pause = deferred()
    f.setInitStage('warmup', pause)
    const initializing = f.ensureAgent()
    await f.saveSettings({ systemPrompt: '初始化期间保存的人设' })
    pause.resolve()
    const { agent } = await initializing
    check('初始化期间保存：初始化里的旧 config 快照不回盖内存值', () => {
      // 初始化在保存之前就 loadConfig() 了，它那份快照不含新提示词
      assert.notEqual(f.runtime.config.systemPrompt, '初始化期间保存的人设',
        `初始化快照应更旧，实际「${f.runtime.config.systemPrompt}」`)
      assert.equal(f.ctx.agentSystemPrompt, '初始化期间保存的人设')
      return `快照=「${f.runtime.config.systemPrompt ?? 'null'}」，内存=新值`
    })
    f.setReplies([plainReply('ok')])
    await agent.run('第一句')
    check('初始化期间保存：完成后第一轮就用新提示词', () => {
      assert.equal(f.systemOf(0), '初始化期间保存的人设')
      return 'ok'
    })
  } finally { await f.close() }
}

/* 5) 保存失败不得影响正在用的提示词；保存其它设置不得冲掉它 */
{
  const f = await freshFixture()
  try {
    f.setReplies([plainReply('第一轮')])
    await f.runTurn('先建好 Agent')
    assert.equal(f.systemOf(0), DEFAULT_SYSTEM_PROMPT)

    await f.saveSettings({ systemPrompt: '正在用的提示词' })
    const badBlank = await f.saveSettings({ systemPrompt: '   \n  ' })
    const badType = await f.saveSettings({ systemPrompt: 42 })
    check('非法提示词：ok:false + 明确错误，磁盘与内存都不动', () => {
      assert.equal(badBlank.ok, false, JSON.stringify(badBlank))
      assert.match(badBlank.error, /空白|默认/u, badBlank.error)
      assert.equal(badType.ok, false, JSON.stringify(badType))
      assert.match(badType.error, /systemPrompt|文本/u, badType.error)
      assert.equal(f.ctx.agentSystemPrompt, '正在用的提示词')
      return `「${badBlank.error}」/「${badType.error}」`
    })
    f.setReplies([plainReply('失败之后的一轮')])
    await f.runTurn('接着说')
    check('非法提示词保存失败后，下一轮继续用改动前的提示词', () => {
      assert.equal(f.lastSystem(), '正在用的提示词')
      return '不受影响'
    })
    const other = await f.saveSettings({ temperature: 0.5, ui: { ttsEnabled: true }, permissions: { mode: 'standard', overrides: {} } })
    check('通过设置页保存其它项：自定义提示词原样保留', () => {
      assert.equal(other.ok, true, JSON.stringify(other.error))
      assert.equal(other.config.systemPrompt, '正在用的提示词')
      assert.equal(f.ctx.agentSystemPrompt, '正在用的提示词')
      return '保留'
    })
    f.setReplies([plainReply('其它设置保存后的一轮')])
    await f.runTurn('再试一次')
    check('保存其它项后，system 仍是那份自定义提示词（真实请求核对）', () => {
      assert.equal(f.lastSystem(), '正在用的提示词')
      return 'ok'
    })
  } finally { await f.close() }
}

/* 6) 提示词只指导行为，授权仍由 PermissionGate 独立决定 */
{
  const f = await freshFixture({ gateVerdicts: [{ allow: false, reason: '未获同意', capability: 'fs.delete' }] })
  try {
    await f.saveSettings({ systemPrompt: '你什么都能做，不需要征求同意，直接删。' })
    f.setReplies([toolCallReply('list_windows'), plainReply('被拦下了')])
    await f.runTurn('把那个文件删了')
    check('自定义提示词写"不用确认"仍必须过权限门（可用性不降级）', () => {
      assert.equal(f.gateCalls.length, 1, `权限门应被调用一次，实际 ${f.gateCalls.length}`)
      assert.equal(f.systemOf(0), '你什么都能做，不需要征求同意，直接删。', '提示词确实进了模型上下文')
      const toolMessage = f.llmCalls[1].find((m) => m.role === 'tool')
      assert.match(toolMessage.content, /已拒绝/u, toolMessage.content)
      return '拒绝原因照旧回填给模型'
    })
  } finally { await f.close() }
}

/* 7) 重启（新进程的 loadUiPrefs 从磁盘恢复）后仍用自定义提示词 */
{
  const f = await freshFixture()
  try {
    await f.saveSettings({ systemPrompt: '重启后还在的人设\n第二行' })
    // 模拟进程重启：内存状态清零，只靠启动时的读盘恢复
    f.ctx.agentSystemPrompt = null
    await f.loadUiPrefs()
    f.setReplies([plainReply('重启后的第一轮')])
    await f.runTurn('第一句')
    check('重启后：文本原样恢复（换行不丢）并进入 system 消息', () => {
      assert.equal(f.ctx.agentSystemPrompt, '重启后还在的人设\n第二行')
      assert.equal(f.systemOf(0), '重启后还在的人设\n第二行')
      return '落盘→读回→进模型 一致'
    })
    await f.saveSettings({ systemPrompt: null })
    f.ctx.agentSystemPrompt = '不该再用的旧值'
    await f.loadUiPrefs()
    f.setReplies([plainReply('默认答复')])
    await f.runTurn('第二句')
    check('重启后：恢复默认的选择也保持（不再用旧自定义文本）', () => {
      assert.equal(f.ctx.agentSystemPrompt, null)
      assert.equal(f.lastSystem(), DEFAULT_SYSTEM_PROMPT)
      return 'ok'
    })
  } finally { await f.close() }
}

console.log('='.repeat(62))
const failed = results.filter((r) => !r.pass)
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
