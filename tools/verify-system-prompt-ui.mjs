/**
 * 系统提示词：设置界面与主进程接线的静态验收（纯 Node，不开窗口）。
 *
 * 为什么单独一个静态检查：这个功能最容易出的错不在逻辑，而在**接线**——
 * 界面显示一份文案、模型收到另一份；或者自定义文本被 .trim() 改了；
 * 或者「恢复默认」只是把输入框清空（那样保存时就是"不修改"，等于没恢复）。
 * 这些用配置读写测试看不出来，所以直接盯源码里的关键写法。
 * 界面真正点击保存的端到端路径由 apps/shell/main.mjs 的 --settings-test 覆盖。
 *
 * 本脚本读的是**自己这套树**的文件：根目录跑验证 v0.1.0，lx 跑验证 v0.2.0。
 *
 * 用法：node tools/verify-system-prompt-ui.mjs
 */
import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'

const tree = import.meta.url.includes('/lx/') ? 'lx' : 'root'
const read = (relative) => {
  const url = new URL(relative, import.meta.url)
  assert.ok(existsSync(url), `找不到 ${relative}`)
  return readFileSync(url, 'utf8').replace(/\r\n/g, '\n')
}

const html = read('../apps/shell/renderer/settings.html')
const renderer = read('../apps/shell/renderer/settings.js')
const main = read('../apps/shell/main.mjs')
const systemPromptModule = read('../packages/agent/system-prompt.mjs')
const loop = read('../packages/agent/loop.mjs')

const results = []
function check(name, fn) {
  try {
    record(name, true, fn())
  } catch (error) {
    record(name, false, error.message)
  }
}
function record(name, pass, detail) {
  results.push({ name, pass, detail })
  console.log(`${pass ? '✓' : '✗'} [${tree}] ${name}${detail ? ` — ${detail}` : ''}`)
}

/* 1. 界面控件齐不齐 */
const ids = ['promptModeDefault', 'promptModeCustom', 'systemPrompt', 'promptCustomArea', 'defaultPromptDetails',
  'restoreDefaultPrompt', 'copyDefaultPrompt', 'copyCustomPrompt', 'defaultPrompt']
check('设置页有「系统提示词」区域和全部控件', () => {
  for (const id of ids) assert.ok(html.includes(`id="${id}"`), `缺 id=${id}`)
  assert.match(html, /<textarea[^>]*id="systemPrompt"/u, '自定义区必须是可编辑的多行文本框')
  assert.match(html, /<h2>系统提示词<\/h2>/u, '要有独立分区标题')
  assert.match(html, /使用默认提示词/u)
  assert.match(html, /使用自定义提示词/u)
  assert.match(html, /恢复默认/u)
  // 默认全文常驻可查（切到自定义时也看得到、能复制）
  assert.match(html, /<details id="defaultPromptDetails"/u)
  return `${ids.length} 个控件`
})

/* 2. 默认文案不许有两份 */
check('默认提示词只有单一来源（前端不抄文案）', () => {
  assert.match(systemPromptModule, /export const DEFAULT_SYSTEM_PROMPT/u)
  // Agent 与主进程都从同一个模块取
  assert.match(loop, /import \{[^}]*DEFAULT_SYSTEM_PROMPT[^}]*\} from '\.\/system-prompt\.mjs'/u)
  assert.ok(!/const SYSTEM_PROMPT = `/u.test(loop), 'loop.mjs 里不能再留第二份提示词字面量')
  assert.match(main, /import \{ DEFAULT_SYSTEM_PROMPT \} from '\.\.\/\.\.\/packages\/agent\/system-prompt\.mjs'/u)
  assert.match(main, /defaultSystemPrompt: DEFAULT_SYSTEM_PROMPT/u, 'settings:load 必须把默认文本给界面')
  // 渲染层：只接收，不内置
  assert.match(renderer, /data\.defaultSystemPrompt/u)
  for (const marker of ['你叫银狼', '天才骇客', '银狼的口气']) {
    assert.ok(!renderer.includes(marker), `settings.js 里不得硬编码「${marker}」`)
    assert.ok(!html.includes(marker), `settings.html 里不得硬编码「${marker}」`)
  }
  // 显示用 textContent（不能用 innerHTML，否则提示词里的符号会被当标签解析）
  assert.match(renderer, /\$\('defaultPrompt'\)\.textContent = defaultPrompt/u)
  return 'DEFAULT_SYSTEM_PROMPT 只在 system-prompt.mjs 一份'
})

/* 3. 自定义文本原样交给主进程 */
check('自定义文本原样提交（不 trim、不加工）', () => {
  assert.match(renderer, /payload\.systemPrompt = \$\('systemPrompt'\)\.value/u)
  assert.ok(!/systemPrompt'\)\.value\.trim\(\)/u.test(renderer), '提交前不得 trim 用户文本')
  assert.ok(!/systemPrompt'\)\.value\.replace/u.test(renderer), '提交前不得改写用户文本')
  return '直接交 textarea 原值'
})

/* 4. 恢复默认的语义 */
check('恢复默认 = 显式 systemPrompt:null（不是清空/省略）', () => {
  assert.match(renderer, /payload\.systemPrompt = null/u)
  // 只在"当前确实是自定义"时才发 null；本来就是默认就不发这个字段
  assert.match(renderer, /else if \(typeof state\.config\.systemPrompt === 'string'/u)
  assert.match(renderer, /\$\('promptModeDefault'\)\.checked = true/u, '「恢复默认」按钮要切回默认模式')
  return 'null=恢复默认，缺省=不修改'
})

/* 5. 切自定义时以完整默认提示词为编辑起点 */
check('首次切自定义用完整默认提示词当起点', () => {
  assert.match(renderer, /\$\('systemPrompt'\)\.value = defaultPrompt/u)
  // 且只在框里没东西时填，别把用户已经写的盖掉
  assert.match(renderer, /\$\('systemPrompt'\)\.value === '' && defaultPrompt !== ''/u)
  return '默认文本作为编辑起点'
})

/* 6. 不在自定义后追加旧人设 */
check('不做"偷偷追加旧人设"的拼接', () => {
  assert.ok(!/systemPrompt[\s\S]{0,120}?\+[\s\S]{0,80}?(DEFAULT_SYSTEM_PROMPT|银狼)/u.test(renderer), '渲染层不得拼接默认人设')
  assert.match(systemPromptModule, /return typeof value === 'string' && value\.trim\(\) !== '' \? value : DEFAULT_SYSTEM_PROMPT/u,
    '解析必须是二选一，而不是拼接')
  assert.ok(!/DEFAULT_SYSTEM_PROMPT\s*\+/u.test(systemPromptModule), '不得把默认文本拼在自定义后面')
  assert.ok(!/\+\s*DEFAULT_SYSTEM_PROMPT/u.test(systemPromptModule), '不得把默认文本拼在自定义前面')
  return '二选一，无拼接'
})

/* 7. 主进程侧热生效接线 */
check('主进程：注入函数 + 保存后同步 + 启动时恢复', () => {
  assert.match(main, /systemPrompt: \(\) => agentSystemPrompt/u, 'Agent 必须拿到"读当前配置"的函数')
  assert.match(main, /function syncAgentSystemPrompt\(config\)/u)
  const syncCount = main.match(/syncAgentSystemPrompt\(/g)?.length ?? 0
  assert.equal(syncCount, 3, `syncAgentSystemPrompt 应出现 3 次（定义/启动恢复/保存后），实际 ${syncCount}`)
  assert.match(main, /if \(payload\?\.systemPrompt !== undefined\) \{\n\s*syncAgentSystemPrompt\(saved\)/u,
    '保存成功后必须立刻同步内存值')
  assert.match(loop, /const turnPrompt = turnSystemPrompt\(systemPrompt\)/u, 'Agent 回合开始时要取快照（loop.mjs）')
  assert.match(loop, /\{ role: 'system', content: turnPrompt \}/u, '回合内所有请求复用同一份快照')
  return 'definition + 2 call sites'
})

/* 8. 权限与语音相关代码没被这个功能改动 */
check('权限门与语音确认仍由原有逻辑决定（提示词不参与授权）', () => {
  // 提示词不参与 gate：取 gate 回调本体，里面不该出现 systemPrompt
  const gateStart = main.indexOf('gate: async ({ tool, args, signal }) => {')
  assert.ok(gateStart >= 0, '找不到 gate 回调')
  const gateBody = main.slice(gateStart, main.indexOf('\n      },', gateStart))
  assert.ok(gateBody.length > 200, `gate 回调本体异常：${gateBody.length}`)
  assert.ok(!gateBody.includes('systemPrompt'), 'gate 里不得读提示词（授权仍由 PermissionGate 独立决定）')
  assert.match(gateBody, /gateInstance\.check\(\{ tool, args, utterance: lastUtterance, signal \}\)/u)
  // 保存 handler 里除了提示词同步，原有的语音/权限接线还在
  assert.match(main, /if \(typeof payload\?\.ui\?\.voiceConfirmEnabled === 'boolean'\) setVoiceConfirmEnabled/u)
  assert.match(main, /gateInstance\.update\(\{[\s\S]{0,200}?overrides: payload\.permissions\.overrides/u)
  // 保存后的唤醒静默期：根版本一直有，lx 版本本来就没有（本次改动不动它）
  if (tree === 'root') assert.match(main, /wakeMutedUntil = Date\.now\(\) \+ 1500/u, '保存后的唤醒静默期不变')
  return 'gate/语音确认 接线不变'
})

/* 9. 保存反馈仍然复用现有「保存」按钮 */
check('提示词保存复用现有保存操作与成功/失败反馈', () => {
  assert.match(renderer, /\$\('save'\)\.addEventListener\('click'/u)
  assert.match(renderer, /setStatus\('已保存', 'ok'\)/u)
  assert.match(renderer, /setStatus\(result\.error \?\? '保存失败', 'bad'\)/u)
  // 校验在主进程：渲染层不做"必须包含某某"的强约束
  assert.ok(!/payload\.systemPrompt[\s\S]{0,200}?(必须|一定要|不能为空)/u.test(renderer), '渲染层不得加人设约束')
  return 'ok/bad 反馈照旧'
})

console.log('='.repeat(62))
const failed = results.filter((r) => !r.pass)
console.log(JSON.stringify({ tree, cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
