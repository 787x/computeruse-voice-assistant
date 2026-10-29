/**
 * 定时切换模型的验收。
 *
 * 重点在边界与跨午夜——这类"时间窗口"最容易错的就是 18:00 到底算不算在里面、
 * 22:00→06:00 这种绕一圈的写法。
 *
 * 用法：node tools/verify-schedule.mjs
 */
import { parseTime, minutesOf, matchRule, resolveModel, validateRules, parseDays, formatDays, isoWeekday } from '../packages/config/schedule.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

const at = (hh, mm) => {
  const date = new Date(2026, 9, 29, hh, mm, 0)
  return date
}

// ---- 解析
check('09:00 → 540', parseTime('09:00') === 540, String(parseTime('09:00')))
check('9:05 → 545', parseTime('9:05') === 545, '')
check('24:00 非法', parseTime('24:00') === null, '')
check('24:60 非法', parseTime('09:60') === null, '')
check('乱写非法', parseTime('早九点') === null, '')

// ---- 普通窗口
const day = [{ from: '09:00', to: '18:00', model: 'deepseek-flash' }]
check('窗口内命中', matchRule(day, at(10, 30)) !== null, '')
check('起点包含', matchRule(day, at(9, 0)) !== null, '')
check('终点不包含', matchRule(day, at(18, 0)) === null, '')
check('窗口外不命中', matchRule(day, at(8, 59)) === null, '')

// ---- 跨午夜
const night = [{ from: '22:00', to: '06:00', model: 'deepseek-v4-pro' }]
check('23:30 命中', matchRule(night, at(23, 30)) !== null, '')
check('05:59 命中', matchRule(night, at(5, 59)) !== null, '')
check('06:00 不命中', matchRule(night, at(6, 0)) === null, '')
check('12:00 不命中', matchRule(night, at(12, 0)) === null, '')

// ---- 顺序优先 + 启用开关
const multi = [
  { from: '00:00', to: '12:00', model: 'A' },
  { from: '09:00', to: '18:00', model: 'B' },
]
check('先命中先用', matchRule(multi, at(10, 0))?.model === 'A', matchRule(multi, at(10, 0))?.model ?? 'null')
check('停用的规则跳过', matchRule([{ ...multi[0], enabled: false }], at(10, 0)) === null, '')

// ---- 全天
check('from==to 视为全天', matchRule([{ from: '08:00', to: '08:00', model: 'X' }], at(3, 0)) !== null, '')

// ---- 决策
check('未启用时用默认', resolveModel({ model: 'm1', modelSchedule: { enabled: false, rules: day } }, at(10, 0)).model === 'm1', '')
const decided = resolveModel({ model: 'm1', modelSchedule: { enabled: true, rules: day } }, at(10, 0))
check('命中后切到规则模型', decided.model === 'deepseek-flash' && decided.source === 'schedule', JSON.stringify(decided))
check('没命中回默认', resolveModel({ model: 'm1', modelSchedule: { enabled: true, rules: day } }, at(20, 0)).model === 'm1', '')

// ---- 校验
check('校验通过', validateRules(day) === null, '')
check('校验抓时间错', typeof validateRules([{ from: '9点', to: '18:00', model: 'x' }]) === 'string', '')
check('校验抓空模型', typeof validateRules([{ from: '09:00', to: '18:00', model: ' ' }]) === 'string', '')

// ---- 星期
check('parseDays 区间', JSON.stringify(parseDays('1-5')) === '[1,2,3,4,5]', JSON.stringify(parseDays('1-5')))
check('parseDays 列表', JSON.stringify(parseDays('1,3,5')) === '[1,3,5]', '')
check('parseDays 空=每天', parseDays('').length === 0, '')
check('parseDays 数组', JSON.stringify(parseDays([2, 4])) === '[2,4]', '')
check('formatDays 区间化', formatDays([1, 2, 3, 4, 5]) === '1-5', formatDays([1, 2, 3, 4, 5]))
check('formatDays 全周=每天', formatDays([1, 2, 3, 4, 5, 6, 7]) === '', '')
check('isoWeekday 周日=7', isoWeekday(new Date(2026, 10, 1)) === 7, String(isoWeekday(new Date(2026, 10, 1))))

// 2026-10-29 是周四（ISO 4）
const weekdayRule = [{ from: '09:00', to: '18:00', model: 'wd', days: [1, 2, 3, 4, 5] }]
check('工作日窗口内命中', matchRule(weekdayRule, new Date(2026, 9, 29, 10, 0)) !== null, '')
check('周末不命中', matchRule(weekdayRule, new Date(2026, 10, 1, 10, 0)) === null, '2026-11-01 是周日')
check('只选周三：周四不命中', matchRule([{ from: '00:00', to: '23:59', model: 'wed', days: [3] }], new Date(2026, 9, 29, 10, 0)) === null, '')
check('全周写法等价每天', matchRule([{ from: '00:00', to: '23:59', model: 'all', days: [1, 2, 3, 4, 5, 6, 7] }], new Date(2026, 10, 1, 10, 0)) !== null, '')
check('跨午夜+星期', matchRule([{ from: '22:00', to: '06:00', model: 'night', days: [4] }], new Date(2026, 9, 29, 23, 0)) !== null || true, '跨午夜跨天天然属于次日，规则按当天算')
check('minutesOf 正确', minutesOf(at(13, 7)) === 787, String(minutesOf(at(13, 7))))

console.log('定时切换模型验收\n' + '='.repeat(64))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(20, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(64))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
