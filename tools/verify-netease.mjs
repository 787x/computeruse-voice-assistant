/**
 * 网易云音乐工具验收。
 *
 * 验三件事：
 *   1. 参数翻译成正确的 CLI 命令（ID 类型判断、音量夹紧、队列子操作）
 *   2. 参数不全时给出模型能照做的提示，而不是让它瞎猜
 *   3. 真实 CLI 能跑通 state / login --check（不依赖登录态）
 *
 * 用法：node tools/verify-netease.mjs
 */
import { buildCommand, runNetease, loginStatus, resolveCliEntry, NETEASE_TOOL } from '../packages/tools/netease.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

const argvOf = (args) => {
  try { return buildCommand(args).argv } catch (error) { return { error: error.message } }
}

// ---- 命令翻译
check('state', argvOf({ action: 'state' }).join(' ') === 'state', '')
check('pause', argvOf({ action: 'pause' })[0] === 'pause', '')
check('音量夹紧到 0-100', argvOf({ action: 'volume', level: 150 }).join(' ') === 'volume 100', '')
check('跳转取整', argvOf({ action: 'seek', seconds: 12.7 }).join(' ') === 'seek 13', '')

// 单曲必须两种 ID 一起给（官方要求）
const both = argvOf({ action: 'play', id: 'a'.repeat(32), original_id: '186016' })
check('双 ID 都传', both.join(' ').includes('--encrypted-id aaaa') && both.join(' ').includes('--original-id 186016'), both.join(' ').slice(0, 56))
const single = argvOf({ action: 'play', id: 'a'.repeat(32) })
check('缺 originalId 给提示', typeof single.error === 'string', single.error?.slice(0, 22) ?? '')
check('URL 直接播', argvOf({ action: 'play', url: 'https://x/y.mp3' }).join(' ').startsWith('play https://x/y.mp3'), '')
check('歌单模式带 --playlist', argvOf({ action: 'play', kind: 'playlist', id: '123' }).join(' ').includes('--playlist'), '')

// 搜索 / 推荐 / 歌词 / 用户
check('search 命令构造', argvOf({ action: 'search', keyword: '晴天', limit: 3 }).join(' ') === 'search song --keyword 晴天 --limit 3', '')
check('search 缺 keyword 报错', typeof argvOf({ action: 'search' }).error === 'string', '')
check('recommend 默认 daily', argvOf({ action: 'recommend' }).join(' ').startsWith('recommend daily'), '')
check('lyric 需要 id', typeof argvOf({ action: 'lyric' }).error === 'string', '')
check('user info', argvOf({ action: 'user' }).join(' ') === 'user info', '')
check('playlist 需要 name', typeof argvOf({ action: 'playlist', op: 'create' }).error === 'string', '')

// ---- 参数不全时的引导
const missing = argvOf({ action: 'play' })
check('play 缺参给提示', typeof missing.error === 'string' && /id 或 url/u.test(missing.error), missing.error?.slice(0, 26) ?? '')
const badSeek = argvOf({ action: 'seek', seconds: -5 })
check('seek 负数报错', typeof badSeek.error === 'string', '')
check('未知 action 报错', typeof argvOf({ action: 'rm-rf' }).error === 'string', '')

// ---- 队列
check('队列默认 list', argvOf({ action: 'queue' })[0] === 'queue', '')
check('队列 clear', argvOf({ action: 'queue', op: 'clear' }).join(' ') === 'queue clear', '')

// ---- 真实 CLI（不依赖登录）
check('找到 ncm-cli 入口', resolveCliEntry() !== null, resolveCliEntry() ?? '未安装')
const state = await runNetease({ action: 'state' })
check('真实 state 可读', /状态|stopped|playing|paused/u.test(state.text), state.text.split('\n')[0].slice(0, 40))
const login = loginStatus()
check('登录状态可读', typeof login.loggedIn === 'boolean', login.loggedIn ? '已登录' : '未登录（正常）')

// ---- 工具描述
check('工具名与必填', NETEASE_TOOL.function.name === 'netease_music'
  && Array.isArray(NETEASE_TOOL.function.parameters.required)
  && NETEASE_TOOL.function.parameters.required.includes('action'), '')

console.log('网易云音乐工具验收\n' + '='.repeat(66))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(24, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(66))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
