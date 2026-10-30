/**
 * 会话记忆验收（纯逻辑，不需要 Electron）。
 *
 * 要钉死两件事：
 *   1. 同一个会话里，后一轮能看到前一轮说的话（否则"第二个呢"这种追问没法接）
 *   2. 记忆必须被裁剪，且**以 user 开头**——从 assistant 开始会让部分模型答非所问
 *
 * 用法：node tools/verify-session.mjs
 */
import { Agent } from '../packages/agent/loop.mjs'

/** 假模型：把收到的 messages 原样记下来，回一句固定的答案 */
function makeLlm(reply = '好的') {
  const calls = []
  return {
    calls,
    async chat({ messages }) {
      calls.push(messages.map((m) => ({ role: m.role, content: m.content })))
      return { text: reply, toolCalls: [] }
    },
  }
}

const tools = { listTools: () => [], invoke: async () => ({ text: '' }) }
const results = []

function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

// 1. 第一轮只有 system + user
{
  const llm = makeLlm('现在十点')
  const agent = new Agent({ llm, tools })
  await agent.run('现在几点')
  const messages = llm.calls[0]
  check('首轮：system + user', messages.length === 2 && messages[1].role === 'user', `${messages.length} 条`)
}

// 2. 第二轮带上第一轮的 user/assistant
{
  const llm = makeLlm('第二个是预算表')
  const agent = new Agent({ llm, tools })
  const history = [
    { role: 'user', content: '桌面上有什么' },
    { role: 'assistant', content: '有两个文件：报告和预算表' },
  ]
  await agent.run('第二个是什么', { history })
  const messages = llm.calls[0]
  const roles = messages.map((m) => m.role).join(',')
  check('二轮：带上历史', roles === 'system,user,assistant,user'
    && messages[2].content.includes('预算表'), roles)
}

// 3. 历史被裁剪：超过 10 条只留最近的
{
  const llm = makeLlm('ok')
  const agent = new Agent({ llm, tools })
  const history = []
  for (let i = 0; i < 20; i += 1) {
    history.push({ role: 'user', content: `问题${i}` })
    history.push({ role: 'assistant', content: `答案${i}` })
  }
  await agent.run('最后一问', { history })
  const messages = llm.calls[0]
  const historyPart = messages.slice(1, -1)
  check('裁剪：最多 10 条历史', historyPart.length <= 10, `${historyPart.length} 条`)
  check('裁剪：保留最近的', historyPart.at(-1).content.includes('答案19'), historyPart.at(-1).content.slice(0, 12))
}

// 4. 历史以 assistant 开头时要被丢掉（模型对消息顺序很敏感）
{
  const llm = makeLlm('ok')
  const agent = new Agent({ llm, tools })
  await agent.run('接着上面', {
    history: [
      { role: 'assistant', content: '我修好了' },
      { role: 'user', content: '谢谢' },
      { role: 'assistant', content: '不客气' },
    ],
  })
  const messages = llm.calls[0]
  check('历史必须以 user 开头', messages[1].role === 'user', messages.map((m) => m.role).join(','))
}

// 5. 工具细节不入历史（截图的 base64 会立刻撑爆上下文）
{
  const llm = makeLlm('ok')
  const agent = new Agent({ llm, tools })
  await agent.run('第二句', {
    history: [
      { role: 'user', content: '看看屏幕' },
      { role: 'assistant', content: '屏幕上是一个编辑器' },
      { role: 'tool', content: 'data:image/jpeg;base64,AAAA…' },
    ],
  })
  const messages = llm.calls[0]
  check('丢弃 tool 消息', messages.every((m) => m.role !== 'tool'), messages.map((m) => m.role).join(','))
}

// 6. 不传 history（新会话）时和以前完全一样
{
  const llm = makeLlm('ok')
  const agent = new Agent({ llm, tools })
  await agent.run('新会话的第一句')
  const messages = llm.calls[0]
  check('新会话不带任何历史', messages.length === 2 && messages[0].role === 'system', `${messages.length} 条`)
}

console.log('会话记忆验收\n' + '='.repeat(64))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(28, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((r) => !r.pass)
console.log('='.repeat(64))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
