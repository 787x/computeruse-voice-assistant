/**
 * 离线兜底验收。
 *
 * 断网时最危险的失败不是"办不到"，而是"以为听懂了，办错了"。
 * 所以这里重点验：哪些话会被认出、哪些话必须老实说办不了、
 * 以及"网络错误 vs 配置错误"有没有被混为一谈（后者走兜底会掩盖真实问题）。
 *
 * 用法：node tools/verify-offline.mjs
 */
import {
  parseOfflineIntent,
  offlineUnsupported,
  formatTime,
  formatDate,
  isOfflineError,
} from '../packages/agent/offline.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

// ---- 能认出来的
const positives = [
  ['现在几点', 'time'],
  ['几点了', 'time'],
  ['现在是什么时间', 'time'],
  ['今天几号', 'date'],
  ['今天星期几', 'date'],
  ['停', 'stop'],
  ['别说了', 'stop'],
  ['再说一遍', 'repeat'],
  ['打开记事本', 'launch'],
  ['帮我打开无畏契约', 'launch'],
  ['启动微信', 'launch'],
  ['运行计算器', 'launch'],
]
for (const [text, intent] of positives) {
  const parsed = parseOfflineIntent(text)
  check(`认出「${text}」`, parsed?.intent === intent, parsed?.intent ?? '未识别')
}

// ---- 参数抽取
check('抽出程序名', parseOfflineIntent('帮我打开无畏契约')?.arg === '无畏契约', JSON.stringify(parseOfflineIntent('帮我打开无畏契约')?.arg))
check('去掉句末标点', parseOfflineIntent('打开记事本。')?.arg === '记事本', JSON.stringify(parseOfflineIntent('打开记事本。')?.arg))

// ---- 认不出来的一律别硬猜
const negatives = ['帮我看看屏幕上有什么', '上海今天天气怎么样', '把那个文件删了', '你好', '', '打开']
for (const text of negatives) {
  check(`不硬猜「${text || '(空)'}」`, parseOfflineIntent(text) === null, JSON.stringify(parseOfflineIntent(text)))
}

// ---- 说辞
const message = offlineUnsupported('帮我订机票')
check('办不到时说的话要诚实', message.includes('连不上网') && message.includes('简单的事还能办'), message.slice(0, 30))

// ---- 时间/日期格式
const sample = new Date(2026, 9, 28, 21, 5)
check('时间格式', formatTime(sample) === '晚上9点5分', formatTime(sample))
check('日期格式', formatDate(sample) === '10月28日，星期三', formatDate(sample))
check('整点格式', formatTime(new Date(2026, 9, 28, 8, 0)) === '上午8点整', formatTime(new Date(2026, 9, 28, 8, 0)))

// ---- 错误分类：网络错误才兜底
const networkErrors = [
  new Error('fetch failed'),
  new Error('getaddrinfo ENOTFOUND api.deepseek.com'),
  new Error('connect ECONNREFUSED 127.0.0.1:443'),
  Object.assign(new Error('The operation was aborted'), { name: 'AbortError' }),
]
const configErrors = [
  new Error('401 Unauthorized'),
  new Error('还没配置模型 API Key'),
  new Error('model 不能为空'),
]
check('网络错误 → 走兜底', networkErrors.every((error) => isOfflineError(error)), `${networkErrors.length} 例`)
check('配置错误 → 不兜底（要暴露）', configErrors.every((error) => !isOfflineError(error)), `${configErrors.length} 例`)

// ---- 顺序：不能把"打开"当成"停下"
check('「打开记事本」不是 stop', parseOfflineIntent('打开记事本')?.intent === 'launch', '')

console.log('离线兜底验收\n' + '='.repeat(64))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(26, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(64))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
