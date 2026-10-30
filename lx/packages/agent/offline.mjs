/**
 * 离线兜底：网络不通时，哪些话仍然能听懂并办到。
 *
 * 整条链路里只有 LLM 在云端，其余都是本地的：
 *   唤醒（KWS）· 识别（ASR）· 合成（GPT-SoVITS）· 操作电脑（CUA）—— 全在本机。
 * 所以断网不该表现成"这软件废了"，而应该是"复杂的不会，简单的照做"。
 *
 * 这里只做**规则匹配**，故意不接本地小模型：
 * 离线时最怕的是"以为听懂了其实办错"，规则匹配宁可少认几样，也不会乱来。
 */

/** 去掉客套话，拿到真正的动作 */
function stripPoliteness(text) {
  return String(text ?? '')
    .trim()
    .replace(/^(那个|嗯|呃|你好|喂)[，,\s]*/u, '')
    .replace(/^(请|麻烦|帮我|帮忙|给我)[一下]?/u, '')
    .trim()
}

/**
 * 解析一句离线能处理的指令。
 * @param {string} raw ASR 文本
 * @returns {{intent:string, arg?:string, reply?:string}|null}
 */
export function parseOfflineIntent(raw) {
  const text = stripPoliteness(raw)
  if (text === '') return null

  // 时间 / 日期：本地时钟，永远可用
  if (/(现在)?(几点|什么时间|什么时刻|几点了)/u.test(text)) return { intent: 'time' }
  if (/(今天|现在|当前).{0,4}(几号|几月几号|日期|星期几|周几)/u.test(text)) return { intent: 'date' }

  // 停止：正在播报时喊停
  if (/^(停|停下|停一下|别说了|安静|闭嘴|不用了)/u.test(text)) return { intent: 'stop' }

  // 重说一遍
  if (/^(再说一遍|重复一遍|你刚才说什么|没听清)/u.test(text)) return { intent: 'repeat' }

  // 打开 / 启动 / 运行 X
  const launch = text.match(/^(打开|启动|运行|开启|开)(一下)?\s*(.+?)[。！!]?$/u)
  if (launch !== null) {
    const target = launch[3].trim()
    if (target !== '') return { intent: 'launch', arg: target }
  }

  return null
}

/** 这句话离线办不到时，怎么跟用户说（要诚实，不要假装在努力） */
export function offlineUnsupported(text) {
  return `现在连不上网，复杂的指令我处理不了。${text === '' ? '' : `「${text}」`}`
    + '简单的事还能办：打开程序、报时间、报日期、让我停下。'
}

/** 把"几点"说成人话 */
export function formatTime(date = new Date()) {
  const hour = date.getHours()
  const minute = date.getMinutes()
  const period = hour < 6 ? '凌晨' : hour < 12 ? '上午' : hour < 13 ? '中午' : hour < 18 ? '下午' : '晚上'
  const display = hour === 0 ? 12 : hour > 12 ? hour - 12 : hour
  return `${period}${display}点${minute === 0 ? '整' : `${minute}分`}`
}

/** 把日期说成人话 */
export function formatDate(date = new Date()) {
  const weekdays = ['日', '一', '二', '三', '四', '五', '六']
  return `${date.getMonth() + 1}月${date.getDate()}日，星期${weekdays[date.getDay()]}`
}

/**
 * 判断一个错误是不是"网络不通"这一类。
 * 只有这类错误才走离线兜底；参数写错、密钥错这类不该被兜底掩盖。
 */
export function isOfflineError(error) {
  const message = String(error?.message ?? error ?? '')
  return /fetch failed|ENOTFOUND|ECONNREFUSED|ETIMEDOUT|EAI_AGAIN|network|socket|timeout|aborted|连不上|网络/u
    .test(message)
}
