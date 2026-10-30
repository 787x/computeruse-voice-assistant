/**
 * 播报文本清洗验收。
 *
 * 背景：用户直接在字幕和语音里听到了 {"wait":true,"confidence":0.9,"why":"…"}
 * ——那是给程序看的字段，念出来就是噪音。提示词已经要求只输出自然语言，
 * 但模型偶尔还是会吐，所以必须有一道硬清洗。
 *
 * 用法：node tools/verify-speechtext.mjs
 */
import { cleanLine } from '../packages/agent/text.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

// ---- 正常句子原样保留
check('普通句子', cleanLine('已经帮你打开了记事本。') === '已经帮你打开了记事本。', '')
check('列表符号被去掉', cleanLine('- 第一步：打开设置') === '第一步：打开设置', '')
check('粗体符号被去掉', cleanLine('**好的**，马上') === '好的，马上', '')
check('英文保留', cleanLine('Chrome 已经打开了') === 'Chrome 已经打开了', '')

// ---- 结构化字段必须清掉
const json = '{"wait":true,"confidence":0.9,"why":"在等确认"}'
check('整段 JSON 清掉', cleanLine(json) === '', JSON.stringify(cleanLine(json)))
check('JSON 夹在句中', cleanLine(`好的，${json} 马上做`) === '好的，马上做', JSON.stringify(cleanLine(`好的，${json} 马上做`)))

const fenced = '```json\n{"wait":false}\n```'
check('代码块清掉', cleanLine(fenced) === '', JSON.stringify(cleanLine(fenced)))

check('裸 key:value 清掉', cleanLine('wait: true, confidence: 0.9 然后打开') === '然后打开', JSON.stringify(cleanLine('wait: true, confidence: 0.9 然后打开')))
check('[[继续]] 清掉', cleanLine('好的[[继续]]') === '好的', '')
check('空对象清掉', cleanLine('{}') === '', '')

// ---- 清完只剩标点的行要丢掉，不能念出一串逗号
check('纯标点行丢掉', cleanLine('。。。') === '', '')
check('清完只剩逗号', cleanLine(`，${json}，`) === '', JSON.stringify(cleanLine(`，${json}，`)))

// ---- 不能误伤正常内容
check('不误伤书名号', cleanLine('我看《三体》') === '我看《三体》', '')
check('不误伤英文冒号', cleanLine('地址是 C:\\Users') === '地址是 C:\\Users', JSON.stringify(cleanLine('地址是 C:\\Users')))

console.log('播报文本清洗验收\n' + '='.repeat(64))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(20, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(64))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
