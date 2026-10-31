/**
 * 系统提示词：配置层 + Agent 层的语义验收（纯 Node，不需要 Electron）。
 *
 * 主要依据是**捕获发给 LLM 的真实 messages**，不是"配置文件变了"：
 * 只有 system 消息真的变成用户写的文本，功能才算生效。
 *
 * 用法：node tools/verify-system-prompt.mjs
 */
import assert from 'node:assert/strict'
import { mkdtemp, readFile, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const configDir = await mkdtemp(join(tmpdir(), 'cua-va-prompt-'))
process.env.CUA_VA_CONFIG_DIR = configDir

const { saveConfig, loadConfig, loadPublicConfig, CONFIG_FILE } = await import('../packages/config/store.mjs')
const { defaultConfig } = await import('../packages/config/registry.mjs')
const { Agent } = await import('../packages/agent/loop.mjs')
const { DEFAULT_SYSTEM_PROMPT, resolveSystemPrompt, normalizeSystemPromptForStorage } = await import('../packages/agent/system-prompt.mjs')

const results = []
function check(name, fn) {
  try {
    const detail = fn()
    results.push({ name, pass: true, detail })
    console.log(`✓ ${name}${detail ? `  — ${detail}` : ''}`)
  } catch (error) {
    results.push({ name, pass: false, detail: error.message })
    console.log(`✗ ${name}\n  ${error.message}`)
  }
}
async function checkAsync(name, fn) {
  try {
    const detail = await fn()
    results.push({ name, pass: true, detail })
    console.log(`✓ ${name}${detail ? `  — ${detail}` : ''}`)
  } catch (error) {
    results.push({ name, pass: false, detail: error.message })
    console.log(`✗ ${name}\n  ${error.message}`)
  }
}

/** 记录每次 chat 收到的 messages 的假模型：这就是"真的进了模型上下文"的证据 */
function makeLlm(reply = '收到') {
  const calls = []
  return {
    calls,
    /** 每次请求的第一条 system 消息内容 */
    systemOf(index = 0) {
      return calls[index]?.find((m) => m.role === 'system')?.content
    },
    async chat({ messages }) {
      calls.push(JSON.parse(JSON.stringify(messages)))
      return { text: reply, toolCalls: [] }
    },
  }
}
const noTools = { listTools: () => [], invoke: async () => ({ text: '' }) }
const rawConfigFile = async () => JSON.parse(await readFile(CONFIG_FILE, 'utf8'))

/* ---------------- 0. 默认提示词的单一来源 ---------------- */
check('默认提示词非空且含银狼人设原文', () => {
  assert.ok(DEFAULT_SYSTEM_PROMPT.length > 500, `长度 ${DEFAULT_SYSTEM_PROMPT.length}`)
  assert.match(DEFAULT_SYSTEM_PROMPT, /你叫银狼/)
  return `${DEFAULT_SYSTEM_PROMPT.length} 字`
})

check('resolveSystemPrompt：null/未设置 → 默认，自定义 → 原样', () => {
  assert.equal(resolveSystemPrompt(null), DEFAULT_SYSTEM_PROMPT)
  assert.equal(resolveSystemPrompt(undefined), DEFAULT_SYSTEM_PROMPT)
  assert.equal(resolveSystemPrompt(''), DEFAULT_SYSTEM_PROMPT)
  assert.equal(resolveSystemPrompt('   \n '), DEFAULT_SYSTEM_PROMPT)
  assert.equal(resolveSystemPrompt('A'), 'A')
  // 原样：不 trim、不改写、不追加任何人设
  assert.equal(resolveSystemPrompt('  我是自定义\n第二行  '), '  我是自定义\n第二行  ')
  assert.ok(!resolveSystemPrompt('只有这一句').includes('银狼'), '自定义后不得偷偷追加旧人设')
  return 'null/空/空白→默认；文本原样'
})

check('normalizeSystemPromptForStorage：拒绝非字符串与空白', () => {
  for (const bad of [42, true, {}, [], null]) {
    assert.throws(() => normalizeSystemPromptForStorage(bad), /文本|null/u, `应拒绝 ${JSON.stringify(bad) ?? String(bad)}`)
  }
  for (const blank of ['', '   ', '\n\t ']) {
    assert.throws(() => normalizeSystemPromptForStorage(blank), /空白/u, `应拒绝空白 ${JSON.stringify(blank)}`)
  }
  assert.equal(normalizeSystemPromptForStorage('第一行\n  第二行'), '第一行\n  第二行')
  return '非法值抛错，合法值原样'
})

/* ---------------- 1. 配置层语义 ---------------- */
await checkAsync('默认配置里 systemPrompt=null（未自定义）', async () => {
  assert.equal(defaultConfig().systemPrompt, null)
  const fresh = await loadConfig()
  assert.equal(fresh.systemPrompt, null)
  return 'null'
})

await checkAsync('旧配置（没有该字段）升级后沿用内置默认', async () => {
  const legacy = { schemaVersion: 1, providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash', apiKey: 'sk-old-1234567890', temperature: 0.3, maxSteps: 16 }
  await writeFile(CONFIG_FILE, JSON.stringify(legacy, null, 2), 'utf8')
  const loaded = await loadConfig()
  assert.ok(!('systemPrompt' in legacy), '旧配置样本确实没这字段')
  assert.equal(loaded.systemPrompt, null, '补齐后必须是 null → 用默认提示词')
  // 旧配置的其它字段照旧读出来
  assert.equal(loaded.apiKey, 'sk-old-1234567890')
  assert.equal(await (async () => (await loadPublicConfig()).systemPrompt)(), null)
  return '旧配置读出来 systemPrompt=null，密钥等原字段不动'
})

const PROMPT_A = '你是 A 人设。\n第二行带缩进：  ok'
await checkAsync('保存自定义 A 后原样落盘（换行/缩进不丢）', async () => {
  const saved = await saveConfig({ systemPrompt: PROMPT_A })
  assert.equal(saved.systemPrompt, PROMPT_A, '返回给界面的值必须原样')
  assert.equal((await loadConfig()).systemPrompt, PROMPT_A)
  assert.equal((await rawConfigFile()).systemPrompt, PROMPT_A, '文件里必须是原文，不是摘要或长度')
  return `${PROMPT_A.length} 字`
})

await checkAsync('保存其他设置不会清空或覆盖自定义提示词', async () => {
  await saveConfig({ temperature: 0.7, ui: { ttsEnabled: false } })
  assert.equal((await loadConfig()).systemPrompt, PROMPT_A, '不带该字段的保存必须保留原值')
  await saveConfig({ systemPrompt: undefined })
  assert.equal((await loadConfig()).systemPrompt, PROMPT_A, '显式 undefined 也不能冲掉（IPC 会带 undefined）')
  assert.equal((await rawConfigFile()).systemPrompt, PROMPT_A)
  // 无关设置真的写进去了
  assert.equal((await loadConfig()).temperature, 0.7)
  assert.equal((await loadConfig()).ui.ttsEnabled, false)
  return '未提供/undefined 都不修改'
})

await checkAsync('非法提示词保存失败且不破坏现有有效配置', async () => {
  for (const bad of ['', '   \n ', 42, true, { text: 'x' }, ['a']]) {
    await assert.rejects(saveConfig({ systemPrompt: bad, temperature: 0.9 }), /systemPrompt|提示词/u, `应拒绝 ${JSON.stringify(bad)}`)
  }
  const after = await loadConfig()
  assert.equal(after.systemPrompt, PROMPT_A, '失败后旧提示词还在')
  assert.equal(after.temperature, 0.7, '同一次保存里的其它字段也不得半写进去')
  return '6 类非法值全部拒绝，文件保持原样'
})

await checkAsync('恢复默认：null 有明确语义，之后不再用旧自定义文本', async () => {
  const saved = await saveConfig({ systemPrompt: null })
  assert.equal(saved.systemPrompt, null)
  assert.equal((await loadConfig()).systemPrompt, null)
  assert.equal((await rawConfigFile()).systemPrompt, null, '落盘必须是 null，不是旧文本')
  const agent = new Agent({ llm: makeLlm(), tools: noTools })
  return 'null → Agent 用默认（见运行时用例）'
})

await checkAsync('已有 API Key / 模型方案 / 权限的存储语义不变', async () => {
  await saveConfig({
    providerId: 'deepseek',
    baseURL: 'https://api.deepseek.com/v1',
    model: 'deepseek-v4-pro',
    apiKey: 'sk-secret-abcdefghijklmnop',
    systemPrompt: '先设一个自定义',
    permissions: { mode: 'standard', overrides: { 'fs.write': 'ask' } },
    modelProfiles: [{ id: 'p1', name: '白天', providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash', apiKey: 'sk-profile-key-123456' }],
  })
  const before = await loadConfig()
  // 掩码回显（界面把掩码原样提交）不得覆盖真密钥
  const publicConfig = await loadPublicConfig()
  assert.match(publicConfig.apiKey, /\*/u, '公开配置里的 key 必须打码')
  assert.equal(publicConfig.systemPrompt, '先设一个自定义', '提示词不是密钥，必须原样给界面（要能看全文）')
  await saveConfig({ apiKey: publicConfig.apiKey, systemPrompt: '改成第二个' })
  const after = await loadConfig()
  assert.equal(after.apiKey, before.apiKey, '掩码值不得覆盖 API Key')
  assert.equal(after.modelProfiles[0].apiKey, before.modelProfiles[0].apiKey, '方案 key 语义不变')
  assert.equal(after.permissions.mode, 'standard')
  assert.equal(after.systemPrompt, '改成第二个')
  return '密钥/方案/权限与改动前一致'
})

/* ---------------- 2. 运行时：发给 LLM 的真实 messages ---------------- */
await checkAsync('默认模式：第一条 system 消息与原版完整提示词逐字相同', async () => {
  const llm = makeLlm()
  const agent = new Agent({ llm, tools: noTools })
  await agent.run('帮我看看窗口')
  assert.equal(llm.calls.length, 1)
  assert.equal(llm.systemOf(0), DEFAULT_SYSTEM_PROMPT)
  assert.equal(llm.calls[0][0].role, 'system', 'system 必须是第一条')
  return `${llm.systemOf(0).length} 字`
})

await checkAsync('自定义 A：下一轮 system 消息就是 A', async () => {
  await saveConfig({ systemPrompt: PROMPT_A })
  let current = (await loadConfig()).systemPrompt
  // Agent 是缓存的：注入函数读当前配置，改设置不重建 Agent
  const llm = makeLlm('A说完了')
  const agent = new Agent({ llm, tools: noTools, systemPrompt: () => current })
  await agent.run('第一句')
  assert.equal(llm.systemOf(0), PROMPT_A)
  assert.ok(!llm.calls[0][0].content.includes('银狼'), '不得混入旧人设')
  // A → B：第三轮起用 B，不再用 A
  const PROMPT_B = '你是 B 人设，只回一个字。'
  await saveConfig({ systemPrompt: PROMPT_B })
  current = (await loadConfig()).systemPrompt
  await agent.run('第二句')
  assert.equal(llm.systemOf(1), PROMPT_B)
  assert.notEqual(llm.systemOf(1), PROMPT_A)
  await agent.run('第三句')
  assert.equal(llm.systemOf(2), PROMPT_B)
  return 'A → B 换掉，已缓存 Agent 直接跟上'
})

await checkAsync('回合内快照：多步工具调用中途改提示词不混入', async () => {
  const PROMPT_LONG = '长回合用的 A：保持到底。'
  const PROMPT_MID = '中途改成的 B：不该出现在这一回合。'
  let current = PROMPT_LONG
  const seen = []
  let step = 0
  const llm = {
    async chat({ messages }) {
      seen.push(messages.find((m) => m.role === 'system').content)
      step += 1
      if (step === 1) {
        // 第一步：模型要调工具；工具执行期间用户保存成 B
        return { text: '先看一眼', toolCalls: [{ id: 'c1', name: 'screenshot', arguments: '{}' }] }
      }
      if (step === 2) return { text: '办好了', toolCalls: [] }
      return { text: '', toolCalls: [] }
    },
  }
  const tools = {
    listTools: () => [],
    invoke: async () => {
      current = PROMPT_MID // 模拟"工具跑到一半时保存成功"
      return { text: '截图完成' }
    },
  }
  const agent = new Agent({ llm, tools, systemPrompt: () => current })
  await agent.run('做个要两步的事')
  assert.equal(seen.length, 2, `应有两次 LLM 往返，实际 ${seen.length}`)
  assert.deepEqual(seen, [PROMPT_LONG, PROMPT_LONG], '同一回合内所有请求必须用开始时的快照')
  // 下一回合才是 B
  current = PROMPT_MID
  await agent.run('下一件事')
  assert.equal(seen[2], PROMPT_MID)
  return '回合1两步=A，回合2=B'
})

await checkAsync('恢复默认后下一轮用完整内置提示词', async () => {
  const saved = await saveConfig({ systemPrompt: null })
  assert.equal(saved.systemPrompt, null)
  const llm = makeLlm()
  const agent = new Agent({ llm, tools: noTools, systemPrompt: () => saved.systemPrompt })
  await agent.run('现在呢')
  assert.equal(llm.systemOf(0), DEFAULT_SYSTEM_PROMPT)
  assert.ok(!llm.systemOf(0).includes('B 人设'), '旧自定义文本必须不再出现')
  return '回到内置原文'
})

await checkAsync('重启（新进程读配置）后自定义状态与内容保持', async () => {
  await saveConfig({ systemPrompt: '重启后还在的人设\n第二行' })
  const reloaded = await loadConfig()
  assert.equal(reloaded.systemPrompt, '重启后还在的人设\n第二行')
  const llm = makeLlm()
  // 新"进程"里的 Agent 只看这份配置（这里用 loadPublicConfig 模拟界面侧读到同一份）
  const publicConfig = await loadPublicConfig()
  const agent = new Agent({ llm, tools: noTools, systemPrompt: publicConfig.systemPrompt })
  await agent.run('第一句')
  assert.equal(llm.systemOf(0), reloaded.systemPrompt)
  return '落盘→读回→进模型 一致'
})

await checkAsync('不传 systemPrompt 的既有调用行为完全不变', async () => {
  const llm = makeLlm()
  const agent = new Agent({ llm, tools: noTools })
  await agent.run('老调用')
  assert.equal(llm.systemOf(0), DEFAULT_SYSTEM_PROMPT)
  const roles = llm.calls[0].map((m) => m.role).join(',')
  assert.equal(roles, 'system,user', '消息结构不变')
  assert.equal(agent.systemPrompt, DEFAULT_SYSTEM_PROMPT, 'getter 给出当前生效值')
  return '默认值 = 内置原文'
})

await checkAsync('run() 覆盖参数可用于一次性提示词', async () => {
  const llm = makeLlm()
  const agent = new Agent({ llm, tools: noTools, systemPrompt: '默认来源' })
  await agent.run('这轮特殊', { systemPrompt: '这轮专用' })
  assert.equal(llm.systemOf(0), '这轮专用')
  await agent.run('回到常规')
  assert.equal(llm.systemOf(1), '默认来源')
  return 'per-turn 覆盖不粘性'
})

console.log('='.repeat(62))
const failed = results.filter((r) => !r.pass)
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
