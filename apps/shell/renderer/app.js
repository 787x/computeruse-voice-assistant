/**
 * HUD 渲染层：只负责"把状态画出来"，不做任何决策。
 * 状态由主进程通过 preload 的 window.hud.onState 推入。
 */
const $ = (id) => document.getElementById(id)

// 渲染层出错时必须能被主进程看到，否则只有"界面不动了"这一个症状
window.addEventListener('error', (event) => {
  console.error(`HUDERR ${event.message}\n${event.error?.stack ?? ''}`)
})

const els = {
  hud: $('hud'), dot: $('dot'), title: $('title'), subs: $('subs'), think: $('think'),
  thinkText: $('thinkText'), wave: $('wave'), confirm: $('confirm'), confirmWhat: $('confirmWhat'),
  sub: $('sub'),
}

let highlightTimer = null
let lastReportedHeight = 0

/**
 * 实时字幕状态：partial 是"正在说的这半句"（会被不断改写），
 * finals 是已经定稿的句子（点端点后不再变）。分开存的原因：
 * partial 改字是常态，如果混进同一份数组，历史句子会被反复重排导致闪烁。
 */
const live = { partial: '', finals: [] }

function para(text, className) {
  const p = document.createElement('p')
  if (className) p.className = className
  p.textContent = text
  return p
}

/** 把"听到的话"画出来：历史句子 + 当前这半句 */
function renderHeard() {
  els.subs.replaceChildren()
  for (let i = 0; i < live.finals.length; i += 1) {
    // 没有正在说的半句时，最后一句就是"当前句"，给它加重点色
    const isLast = live.partial === '' && i === live.finals.length - 1
    els.subs.append(para(live.finals[i], isLast ? 'current' : undefined))
  }
  if (live.partial !== '') els.subs.append(para(live.partial, 'current'))
}

function clearAll() {
  els.subs.replaceChildren()
  els.think.style.display = 'none'
  els.wave.style.display = 'none'
  els.confirm.style.display = 'none'
  els.dot.className = 'dot'
  clearInterval(highlightTimer)
  highlightTimer = null
}

/** 句内逐字高亮：真实实现按音频时长驱动，这里按字数插值 */
function typeSentence(text, durationMs) {
  const p = document.createElement('p')
  p.className = 'current'
  const spoken = document.createElement('span')
  spoken.className = 'spoken'
  const pending = document.createElement('span')
  pending.className = 'pending'
  pending.textContent = text
  p.append(spoken, pending)
  els.subs.append(p)

  const chars = [...text]
  const per = durationMs / chars.length
  let i = 0
  highlightTimer = setInterval(() => {
    i += 1
    if (i > chars.length) {
      clearInterval(highlightTimer)
      highlightTimer = null
      reportHeight()
      return
    }
    spoken.textContent = chars.slice(0, i).join('')
    pending.textContent = chars.slice(i).join('')
  }, per)
}

/** 内容高度上报：主进程据此 setContentSize 并重新贴底 */
function reportHeight() {
  const height = Math.ceil(els.hud.getBoundingClientRect().height)
  if (Math.abs(height - lastReportedHeight) <= 1) return
  lastReportedHeight = height
  window.hud.setHeight(height)
}

const RENDER = {
  hidden() { clearAll() },

  listening() {
    clearAll()
    els.dot.classList.add('listen')
    els.title.textContent = '聆听中'
    els.sub.textContent = live.finals.length === 0 && live.partial === ''
      ? '我在听，请说'
      : '可以继续说，说完我会接'
    renderHeard()
  },

  thinking() {
    clearAll()
    els.dot.classList.add('listen')
    els.title.textContent = '思考中'
    els.sub.textContent = '正在理解你的意思'
    if (live.finals.length > 0 || live.partial !== '') renderHeard()
    else els.subs.append(para('帮我把下午三点的会议改到四点。', 'current'))
    els.think.style.display = 'flex'
    els.thinkText.textContent = '我看一下日程'
  },

  speaking() {
    clearAll()
    els.dot.classList.add('speak')
    els.title.textContent = '正在说'
    els.sub.textContent = '可以随时打断我'
    els.wave.style.display = 'flex'
    if (live.finals.length > 0 || live.partial !== '') {
      renderHeard()
      return
    }
    // 没有真实内容时（托盘"演示状态"菜单），用假数据占位，方便看视觉
    els.subs.append(
      para('好的，我看一下你的日程。'),
      para('下午三点是项目同步会，改到四点会撞上评审。'),
    )
    typeSentence('要我把项目同步会挪到四点十五吗？', 4200)
  },

  confirm() {
    clearAll()
    els.dot.classList.add('speak')
    els.title.textContent = '需要你确认'
    els.sub.textContent = '可以说「确认」或「取消」，也可以点击按钮'
    if (confirmData !== null) {
      els.subs.append(para(confirmData.text, 'current'))
      els.confirmWhat.textContent = confirmData.detail
    } else {
      els.subs.append(para('这三个文件要一起删掉吗？', 'current'))
      els.confirmWhat.textContent = '删除 3 个文件：报告.docx、预算.xlsx、草稿.md'
    }
    els.confirm.style.display = 'flex'
  },

  /**
   * 说完了：留一个明确的"结束"状态，1.5 秒后收起。
   *
   * 为什么需要它：播报结束后如果还挂着「正在说」，用户看到的是
   * "它自己念完了却还说在说" —— 分不清是卡住还是在等。
   */
  done() {
    clearAll()
    els.dot.classList.add('listen')
    els.title.textContent = '好了'
    els.sub.textContent = '需要我做什么，喊「你好狼宝」'
    if (live.finals.length > 0 || live.partial !== '') renderHeard()
  },

  error() {
    clearAll()
    els.dot.classList.add('error')
    els.title.textContent = '出错了'
    els.sub.textContent = '5 秒后自动收起'
    if (live.finals.length > 0 || live.partial !== '') renderHeard()
    else els.subs.append(para('找不到「日程」应用，可能没有安装。', 'current'))
  },
}

let currentState = 'hidden'

function apply(state) {
  try {
    currentState = state
    const render = RENDER[state]
    if (typeof render !== 'function') {
      console.error(`HUDRENDER 未知状态 ${JSON.stringify(state)}，类型 ${typeof render}`)
      RENDER.hidden()
    } else {
      render()
    }
    // 隐藏窗口里 requestAnimationFrame 不会触发（实测），
    // 而首帧高度上报正是"可以显示"的信号，所以用宏任务兜底。
    setTimeout(reportHeight, 0)
    setTimeout(reportHeight, 60)
  } catch (error) {
    console.error(`HUDRENDER apply(${JSON.stringify(state)}) 失败: ${error?.message}\n${error?.stack ?? ''}`)
  }
}

/**
 * 播报：主进程合成好 WAV 交给这里出声。
 * 用 <audio> 而不是 Web Audio API —— 只需要播整段文件，
 * 而且 <audio> 自带 ended 事件，正好当作"这句读完了"的回执。
 */
let audioEl = null

window.hud.onPlay(({ id, url, text, showText }) => {
  if (audioEl !== null) {
    audioEl.pause()
    audioEl = null
  }
  /*
   * 字幕跟着这一句的开口走：不是"生成时上屏"，而是"播到它时才上屏"。
   * 之前字幕在合成前就出现，卡片总是比声音快半句，用户看到的就是"错位"。
   */
  if (showText === true && typeof text === 'string' && text !== '') {
    live.finals.push(text)
    live.partial = ''
    renderHeard()
    setTimeout(reportHeight, 0)
  }
  const el = new Audio(url)
  audioEl = el
  const done = () => {
    if (audioEl === el) audioEl = null
    window.hud.spoken(id)
  }
  el.addEventListener('ended', done)
  el.addEventListener('error', done)
  el.play().catch(done)
})

window.hud.onStopAudio(() => {
  if (audioEl !== null) {
    audioEl.pause()
    audioEl.currentTime = 0
    audioEl = null
  }
})

window.hud.onState(apply)

/** 确认卡片的内容由主进程给，渲染层不猜 */
let confirmData = null

window.hud.onConfirm((payload) => {
  confirmData = payload
  if (currentState === 'confirm') RENDER.confirm()
})

window.hud.onConfirmDone(() => {
  confirmData = null
})

/**
 * 字幕流。注意 partial 是高频推送（每 100ms 可能一次），
 * 所以这里只改文本、不重建 DOM 结构，避免长句时肉眼可见的重排闪烁。
 */
window.hud.onSubtitle((payload) => {
  if (payload.reset === true) {
    live.partial = ''
    live.finals = []
  }
  if (typeof payload.hint === 'string' && payload.hint !== '') {
    els.sub.textContent = payload.hint
  }
  if (typeof payload.partial === 'string') live.partial = payload.partial
  if (typeof payload.final === 'string' && payload.final !== '') {
    live.finals.push(payload.final)
    live.partial = ''
  }
  renderHeard()
  setTimeout(reportHeight, 0)
})

$('close').addEventListener('click', () => window.hud.dismiss())

window.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') window.hud.dismiss()
})

apply('listening')
