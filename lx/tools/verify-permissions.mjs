/**
 * 权限引擎验收（纯逻辑，不需要 Electron / 麦克风）。
 *
 * 这一层的错都是"安静地错"：多放行一次不会报错，只会在某天误删东西时才被发现。
 * 所以每个分支都要有明确期望，尤其是 deny 与 ask 的边界。
 *
 * 用法：node tools/verify-permissions.mjs
 */
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { PermissionGate, classify, resolveLevel, CAPABILITIES, parseConfirmText } from '../packages/agent/permissions.mjs'

const dir = await mkdtemp(join(tmpdir(), 'perm-'))
const auditPath = join(dir, 'audit.jsonl')

let confirmAnswer = { allow: true, channel: 'voice' }
let confirmCalls = 0
const gate = new PermissionGate({
  permissions: { mode: 'standard' },
  auditPath,
  confirm: async () => { confirmCalls += 1; return confirmAnswer },
})

const results = []
async function check(name, expect, request, mutate) {
  if (typeof mutate === 'function') mutate(gate)
  confirmCalls = 0
  const verdict = await gate.check(request)
  const pass = expect(verdict)
  results.push({ name, pass, verdict: `${verdict.decision ?? (verdict.allow ? 'allow' : 'deny')}/${verdict.level}/${verdict.capability}` })
}

// 1. 感知类：直接放行，不打扰用户
await check('截图 → allow', (v) => v.allow && v.level === 'allow' && v.channel === 'policy', { tool: 'screenshot', args: {} })

// 2. 输入类：放行，但要留痕
await check('左键点击 → allow', (v) => v.allow && v.level === 'allow', { tool: 'left_click', args: { x: 100, y: 200 } })

// 3. 剪贴板：必须先问
await check('读剪贴板 → ask → 用户确认', (v) => v.allow && v.level === 'ask' && v.channel === 'voice', { tool: 'read_clipboard' })

// 4. 用户拒绝
confirmAnswer = { allow: false, channel: 'voice' }
await check('读剪贴板 → ask → 用户拒绝', (v) => !v.allow && v.channel === 'voice', { tool: 'read_clipboard' })
confirmAnswer = { allow: true, channel: 'voice' }

// 5. 危险按键升级：Win+R 能打开任意命令执行
await check('Win+R → 升级为 ask', (v) => v.level === 'ask' && v.capability === 'system.settings', { tool: 'key', args: { keys: 'WIN+R' } })

// 6. 危险文本：模型把删除命令打进去
await check('输入 rm -rf → 升级为 ask', (v) => v.level === 'ask' && v.capability === 'file.delete', { tool: 'type', args: { text: 'rm -rf /tmp/x' } })

// 7. 普通输入不升级
await check('普通文本 → allow', (v) => v.allow && v.level === 'allow', { tool: 'type', args: { text: '你好，请帮我写个备忘' } })

// 8. 未知工具一律拒绝（白名单之外没有商量余地）
await check('未知工具 → deny', (v) => !v.allow && v.capability === 'unknown', { tool: 'os_shutdown', args: {} })

// 9. 定时授权：grantOnce 之后同样的操作不再问
await check('grantOnce 后读剪贴板 → allow_once', (v) => v.allow && (v.level === 'allow' || v.level === 'allow_once') && confirmCalls === 0, { tool: 'read_clipboard' }, (g) => g.grantOnce('clipboard.read', 15))

// 10. 暂停：一切操作拒绝，且要说清还有多久
await check('暂停 10 分钟 → deny', (v) => !v.allow && v.channel === 'pause' && /暂停/.test(v.reason ?? ''), { tool: 'left_click' }, (g) => g.pause(10))

// 11. 暂停到期后恢复
await check('暂停到期 → 恢复', (v) => v.allow, { tool: 'left_click' }, (g) => g.update({ pausedUntil: Date.now() - 1000, allowedOnce: {} }))

// 12. 保守档：连点击也要问
await check('保守档：点击 → ask', (v) => v.level === 'ask', { tool: 'left_click' }, (g) => g.update({ mode: 'conservative' }))

// 13. 宽松档：剪贴板不再问
await check('宽松档：读剪贴板 → allow', (v) => v.allow && v.level === 'allow', { tool: 'read_clipboard' }, (g) => g.update({ mode: 'permissive' }))

// 14. 用户显式拒绝某项能力
await check('覆盖为 deny：键盘 → deny', (v) => !v.allow && v.level === 'deny', { tool: 'type', args: { text: 'hi' } }, (g) => g.update({ mode: 'standard', overrides: { 'input.keyboard': 'deny' } }))

// 15. deny 的系统能力：关机/IPC 里没有对应工具，但策略层要能表达
await check('支付转账 → deny', (v) => !v.allow, { tool: 'pay_transfer', args: {} }, (g) => g.update({ overrides: {} }))

// 16. 暂停期不写审计里的"耗时"之外的字段要能序列化
const auditRaw = await readFile(auditPath, 'utf8').catch(() => '')
const auditLines = auditRaw.split('\n').filter(Boolean).map((line) => JSON.parse(line))
const auditOk = auditLines.length > 0
  && auditLines.every((row) => typeof row.ts === 'string' && typeof row.tool === 'string' && typeof row.decision === 'string')
  && auditLines.some((row) => row.level === 'ask' && row.decision === 'deny')
  && auditLines.every((row) => JSON.stringify(row.args ?? {}).length < 400)

results.push({ name: '审计日志：可解析/含拒绝记录/参数已截断', pass: auditOk, verdict: `${auditLines.length} 行` })

// 17. classify 纯函数：普通点击不升级
const plain = classify('left_click', { x: 1, y: 2 })
results.push({ name: 'classify：普通点击不升级', pass: plain.capability === 'input.pointer' && plain.escalated === false, verdict: plain.capability })

// 18. resolveLevel：能力表覆盖完整（避免 capability 打错字悄悄变成 ask）
const missing = Object.keys(CAPABILITIES).length === 0 ? ['(空)'] : []
results.push({ name: '能力表非空且每项有级别', pass: missing.length === 0 && Object.values(CAPABILITIES).every((c) => ['allow', 'ask', 'deny'].includes(c.level)), verdict: `${Object.keys(CAPABILITIES).length} 项` })

// 20. 免确认模式：ask 与 deny 都放行，但白名单/暂停/单项覆盖仍然是刹车
{
  let asked = 0
  const yolo = new PermissionGate({
    permissions: { mode: 'unattended' },
    confirm: async () => { asked += 1; return { allow: true, channel: 'click' } },
  })
  const clipboard = await yolo.check({ tool: 'read_clipboard', args: {} })
  const shell = await yolo.check({ tool: 'run_shell', args: { command: 'dir' } })
  const del = await yolo.check({ tool: 'files', args: { action: 'delete', path: 'C:\\x' } })
  const unknown = await yolo.check({ tool: 'rm_rf', args: {} })
  results.push({
    name: '免确认模式：不再弹卡片',
    pass: asked === 0 && clipboard.channel === 'unattended' && shell.allow === true && del.allow === true,
    verdict: `asked=${asked} shell=${shell.channel} delete=${del.channel}`,
  })
  results.push({
    name: '免确认模式：白名单外仍拒绝',
    pass: unknown.allow === false && unknown.capability === 'unknown',
    verdict: `${unknown.capability}/${unknown.channel}`,
  })

  const pausedYolo = new PermissionGate({
    permissions: { mode: 'unattended', pausedUntil: Date.now() + 60000 },
    confirm: async () => ({ allow: true }),
  })
  const paused = await pausedYolo.check({ tool: 'left_click', args: {} })
  results.push({
    name: '免确认模式：暂停优先',
    pass: paused.allow === false && paused.channel === 'pause',
    verdict: paused.channel,
  })

  const clamped = new PermissionGate({
    permissions: { mode: 'unattended', overrides: { 'file.delete': 'deny' } },
    confirm: async () => ({ allow: true }),
  })
  const denied = await clamped.check({ tool: 'files', args: { action: 'delete', path: 'C:\\x' } })
  results.push({
    name: '免确认模式：单项覆盖仍是刹车',
    pass: denied.allow === false && denied.capability === 'file.delete',
    verdict: `${denied.capability}/${denied.channel}`,
  })
}

// 19. 语音确认的读法：否定优先，模糊话不表态
const speech = [
  { text: '确认', allow: true },
  { text: '可以，执行吧', allow: true },
  { text: '不用了', allow: false },
  { text: '不要确认这个东西', allow: false },
  { text: '取消', allow: false },
  { text: '嗯…', allow: null },
  { text: '', allow: null },
]
const speechOk = speech.every((row) => {
  const parsed = parseConfirmText(row.text)
  const actual = parsed === null ? null : parsed.allow
  return actual === row.allow
})
results.push({
  name: '语音确认读法（否定优先/模糊不表态）',
  pass: speechOk,
  verdict: speech.map((row) => `${row.text}→${row.allow}`).join(' '),
})

// Exact decisions, including former negative-substring false positives.
const utterances = [
 ...['可以','确认','确定','同意','允许','好的','好','是的','对','执行','继续','我同意','我确认','可以的','好啊','可以，执行吧',' 可以 。'].map(text=>[text,true]),
 ...['不可以','不同意','不确认','不允许','不执行','不要执行','取消','不行','不对','错了','停','不用','别','不要','别执行','不用了','不要了','别执行了','不要执行了','取消吧','算了吧','停止操作'].map(text=>[text,false]),
 ...['可以吗？','可不可以？','不是不可以','不要取消','我说的是可以','我说的是不可以','如果安全就确认','“确认”','我同意吗','为什么要取消','嗯…','','   '].map(text=>[text,null]),
]
for (const [text,expected] of utterances) {
 const parsed=parseConfirmText(text)
 results.push({name:`完整语音 ${text}`,pass:(parsed===null?null:parsed.allow)===expected,verdict:String(expected)})
}

console.log('权限引擎验收\n' + '='.repeat(64))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(36, ' ')} ${row.verdict}`)
}
const failed = results.filter((r) => !r.pass)
console.log('='.repeat(64))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))

await rm(dir, { recursive: true, force: true })
if (failed.length > 0) process.exitCode = 1
