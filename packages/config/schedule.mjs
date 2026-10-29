/**
 * 按时间段切换模型（纯逻辑，便于单测）。
 *
 * 规则形态：
 *   { from: "09:00", to: "18:00", model: "deepseek-flash" }
 * 支持跨午夜（22:00 → 06:00），也支持"整天"（from == to 视为全天）。
 *
 * 为什么按分钟比较而不是 Date 对象：用户填的是"墙上的时间"，
 * 用分钟数比较能天然处理"18:00 包含、18:01 不包含"这种边界，也便于跨天判断。
 */

/** "HH:mm" → 当天第几分钟；格式不对返回 null */
export function parseTime(text) {
  const match = String(text ?? '').trim().match(/^(\d{1,2}):(\d{2})$/u)
  if (match === null) return null
  const hour = Number(match[1])
  const minute = Number(match[2])
  if (hour < 0 || hour > 23 || minute < 0 || minute > 59) return null
  return hour * 60 + minute
}

/**
 * 星期：用 ISO 记法（1=周一 … 7=周日），跟界面上"一二三四五六日"的顺序一致。
 * JS 的 getDay() 是 0=周日，差一位，这里统一转换掉，别让两边各记一套。
 */
export function isoWeekday(now = new Date()) {
  const day = now.getDay()
  return day === 0 ? 7 : day
}

/** "1-5" / "1,3,5" / [1,5] / 空 → 数字数组（空 = 每天） */
export function parseDays(value) {
  if (value === undefined || value === null) return []
  if (Array.isArray(value)) return value.map(Number).filter((day) => Number.isInteger(day) && day >= 1 && day <= 7)
  const text = String(value).trim()
  if (text === '') return []
  const days = new Set()
  for (const piece of text.split(/[,，\s]+/u)) {
    if (piece === '') continue
    const range = piece.match(/^(\d)\s*[-~至]\s*(\d)$/u)
    if (range !== null) {
      const from = Number(range[1])
      const to = Number(range[2])
      for (let day = Math.min(from, to); day <= Math.max(from, to); day += 1) {
        if (day >= 1 && day <= 7) days.add(day)
      }
      continue
    }
    const single = Number(piece)
    if (Number.isInteger(single) && single >= 1 && single <= 7) days.add(single)
  }
  return [...days].sort((a, b) => a - b)
}

/** 数组 → "1-5" 这种紧凑写法（设置页回显用） */
export function formatDays(days) {
  const list = parseDays(days)
  if (list.length === 0 || list.length === 7) return ''
  const parts = []
  let start = list[0]
  let prev = list[0]
  for (const day of list.slice(1)) {
    if (day === prev + 1) { prev = day; continue }
    parts.push(start === prev ? String(start) : `${start}-${prev}`)
    start = day
    prev = day
  }
  parts.push(start === prev ? String(start) : `${start}-${prev}`)
  return parts.join(',')
}

/** 当前时间 → 当天第几分钟 */
export function minutesOf(now = new Date()) {
  return now.getHours() * 60 + now.getMinutes()
}

/**
 * 命中哪条规则（按数组顺序，先命中先用）。
 * @param {Array<{from:string,to:string,model:string,enabled?:boolean}>} rules
 */
export function matchRule(rules, now = new Date()) {
  if (!Array.isArray(rules)) return null
  const current = minutesOf(now)
  for (const rule of rules) {
    if (rule === null || typeof rule !== 'object') continue
    if (rule.enabled === false) continue
    const from = parseTime(rule.from)
    const to = parseTime(rule.to)
    if (from === null || to === null) continue
    if (typeof rule.model !== 'string' || rule.model.trim() === '') continue
    // days 为空 = 每天；填了就必须包含今天（ISO：1=周一 … 7=周日）
    const days = parseDays(rule.days)
    if (days.length > 0 && days.length < 7 && !days.includes(isoWeekday(now))) continue
    // from == to 视为"全天"
    if (from === to) return rule
    if (from < to) {
      if (current >= from && current < to) return rule
    } else {
      // 跨午夜：22:00 → 06:00
      if (current >= from || current < to) return rule
    }
  }
  return null
}

/**
 * 算出"此刻应该用哪个模型"。
 * 没启用定时、或没有规则命中时，用配置里的默认模型。
 */
export function resolveModel(config, now = new Date()) {
  const schedule = config?.modelSchedule ?? {}
  const fallback = typeof config?.model === 'string' ? config.model : ''
  if (schedule.enabled !== true) return { model: fallback, rule: null, source: 'default' }
  const rule = matchRule(schedule.rules, now)
  if (rule === null) return { model: fallback, rule: null, source: 'default' }
  return { model: rule.model.trim(), rule, source: 'schedule' }
}

/** 校验规则数组（设置页保存前用；返回错误字符串或 null） */
export function validateRules(rules) {
  if (!Array.isArray(rules)) return '规则必须是数组'
  for (const [index, rule] of rules.entries()) {
    const where = `第 ${index + 1} 条`
    if (parseTime(rule?.from) === null) return `${where}的开始时间格式不对（要 HH:mm）`
    if (parseTime(rule?.to) === null) return `${where}的结束时间格式不对（要 HH:mm）`
    if (typeof rule?.model !== 'string' || rule.model.trim() === '') return `${where}没填模型名`
  }
  return null
}
