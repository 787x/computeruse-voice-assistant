/**
 * 权限引擎（R7 / P3）。
 *
 * 职责：**所有工具调用必须先过这里**。它是唯一能说"能不能做"的地方，
 * 也是审计日志的唯一入口——散落在各处的 break-glass 检查一定会有漏网的。
 *
 * 三个概念分清楚，别混：
 *   能力（capability）  按"能造成什么后果"划分：看屏幕 / 点鼠标 / 敲键盘 / 读剪贴板…
 *   级别（level）       allow / ask / deny / allow_once，回答"这个能力现在允许到什么程度"
 *   作用域（scope）     "对谁允许"：按应用、按目录、按时间。这一版只做时间，
 *                       应用/目录作用域等 P5 有了 app 识别再落。
 *
 * 默认取向（§6.3）：低风险放行保证流畅，不可逆动作一律询问，破坏性系统操作默认拒绝。
 */
import { appendFile, mkdir } from 'node:fs/promises'
import { dirname } from 'node:path'

/** 工具名 → 能力。没有映射的工具一律按 deny 处理（宁可挡住，不要漏过）。 */
export const TOOL_CAPABILITY = {
  screenshot: 'screen.capture',
  list_windows: 'window.read',
  get_app_state: 'window.read',
  left_click: 'input.pointer',
  scroll: 'input.pointer',
  type: 'input.keyboard',
  key: 'input.keyboard',
  set_value: 'input.text',
  open_application: 'app.launch',
  read_clipboard: 'clipboard.read',
  // 搜索会把查询词发给第三方，所以留痕（默认放行：问"今天天气"不该每次弹卡片）
  web_search: 'web.search',
  launch_app: 'app.launch',
  run_shell: 'system.shell',
  system_info: 'system.info',
  // files / app_control 按 action 再细分，见 classify 里的分支
  files: 'file.read',
  app_control: 'window.read',
  kill_app: 'app.close',
  music: 'music.control',  wait: 'none',
  request_access: 'none',
}

/**
 * 能力的默认级别（标准档）。
 * 加 ask 的门槛是"这一步之后不可撤销或会把内容发出去"。
 */
export const CAPABILITIES = {
  'screen.capture': { label: '看屏幕', level: 'allow', audit: false },
  'window.read': { label: '读窗口列表', level: 'allow', audit: false },
  'input.pointer': { label: '移动/点击鼠标', level: 'allow', audit: true },
  'input.keyboard': { label: '敲键盘', level: 'allow', audit: true },
  'input.text': { label: '填写输入框', level: 'allow', audit: true },
  'app.launch': { label: '启动应用', level: 'allow', audit: true },
  'app.close': { label: '关闭窗口', level: 'ask', audit: true },
  'web.search': { label: '联网搜索', level: 'allow', audit: true },
  'file.read': { label: '读取文件', level: 'allow', audit: true },
  'file.write': { label: '写入/移动文件', level: 'ask', audit: true },
  'system.shell': { label: '执行命令', level: 'ask', audit: true },
  'system.info': { label: '查看系统状态', level: 'allow', audit: false },
  'music.control': { label: '音乐播放控制', level: 'allow', audit: true },
  'clipboard.read': { label: '读剪贴板', level: 'ask', audit: true },
  'clipboard.write': { label: '写剪贴板', level: 'ask', audit: true },
  'file.delete': { label: '删除文件', level: 'ask', audit: true },
  'file.write': { label: '修改文件', level: 'ask', audit: true },
  'comm.send': { label: '发送消息', level: 'ask', audit: true },
  'system.settings': { label: '修改系统设置', level: 'ask', audit: true },
  'system.power': { label: '关机/重启/安装', level: 'deny', audit: true },
  'pay.transfer': { label: '支付转账', level: 'deny', audit: true },
  'none': { label: '无副作用', level: 'allow', audit: false },
}

/** 三档预设：保守 / 标准 / 宽松。改动只影响"默认级别"，用户覆盖优先。 */
const MODES = {
  conservative: { promote: ['input.pointer', 'input.keyboard', 'input.text', 'app.launch'] },
  standard: { promote: [] },
  permissive: { demote: ['clipboard.read', 'clipboard.write', 'file.write', 'file.delete', 'comm.send', 'system.settings'] },
  /**
   * 免确认（全放行）：ask 和 deny 都变成 allow。
   *
   * 这是给"我就想让它自己跑完、别弹卡片"的场景准备的，代价必须说清楚：
   * 删除、发消息、执行命令都不会再问你。所以
   *   - 审计照记（channel 会标成 unattended），出事还能查
   *   - 全局暂停仍然优先——"我现在别动"比模式更高
   *   - 单项覆盖仍然可以把它掰回 ask/deny（留了一个刹车）
   *   - 白名单之外的工具照旧拒绝（不认识的工具不该因为模式放开）
   */
  unattended: { allowAll: true },
}

/**
 * 按键组合的风险升级：这些键本身不"删东西"，但能打开任意命令执行面板、
 * 关掉用户正在改的文档、锁屏。机械层面看不出语义风险，只能按具体组合挡。
 */
const RISKY_KEYS = [
  { pattern: /win\s*\+\s*r/i, capability: 'system.settings', why: '打开"运行"对话框，等于可以执行任意命令' },
  { pattern: /win\s*\+\s*x/i, capability: 'system.settings', why: '打开系统快捷菜单' },
  { pattern: /win\s*\+\s*i/i, capability: 'system.settings', why: '打开系统设置' },
  { pattern: /win\s*\+\s*l/i, capability: 'system.settings', why: '锁屏' },
  { pattern: /alt\s*\+\s*f4/i, capability: 'app.close', why: '关闭当前窗口，可能丢失未保存的改动' },
  { pattern: /ctrl\s*\+\s*shift\s*\+\s*(esc|delete)/i, capability: 'system.settings', why: '打开任务管理器' },
]

/** 文本里的危险命令迹象：模型把这句打进终端/运行框就真会执行。 */
const RISKY_TEXT = [
  { pattern: /\b(rm\s+-rf|del\s+\/[sq]|rd\s+\/s|format\s+[a-z]:)/i, capability: 'file.delete', why: '文本里包含递归删除命令' },
  { pattern: /\b(shutdown|restart-computer|stop-computer)\b/i, capability: 'system.power', why: '文本里包含关机/重启命令' },
  { pattern: /(git\s+push\s+(-f|--force))/i, capability: 'system.settings', why: '强制推送会覆盖远端历史' },
]

/**
 * 判断一次调用属于哪个能力、默认什么级别。
 * 注意：这里只做"机械风险"判断，语义风险（"这个按钮点了会付款吗"）归模型的白描和用户确认。
 */
export function classify(tool, args = {}) {
  const capability = TOOL_CAPABILITY[tool]
  if (capability === undefined) {
    return { capability: 'unknown', level: 'deny', reason: `未知工具：${tool}`, why: '不在白名单里的工具一律拒绝' }
  }

  if (tool === 'key') {
    const keys = String(args.keys ?? args.key ?? args.combo ?? '')
    for (const rule of RISKY_KEYS) {
      if (rule.pattern.test(keys)) {
        return { capability: rule.capability, level: 'ask', reason: `${keys}：${rule.why}`, escalated: true }
      }
    }
  }

  if (tool === 'type' || tool === 'set_value') {
    const text = String(args.text ?? args.value ?? '')
    for (const rule of RISKY_TEXT) {
      if (rule.pattern.test(text)) {
        return { capability: rule.capability, level: 'ask', reason: rule.why, escalated: true }
      }
    }
  }

  // 带 action 的工具按动作细分能力：同一个 "files"，列表和删除的风险差着量级
  if (tool === 'files') {
    const action = String(args.action ?? 'list')
    if (action === 'delete') return { capability: 'file.delete', level: null, reason: '', escalated: false }
    if (action === 'write' || action === 'copy' || action === 'move') {
      return { capability: 'file.write', level: null, reason: '', escalated: false }
    }
    return { capability: 'file.read', level: null, reason: '', escalated: false }
  }
  if (tool === 'app_control') {
    // 只有 list / find 了，都是只读
    return { capability: 'window.read', level: null, reason: '', escalated: false }
  }

  return { capability, level: null, reason: '', escalated: false }
}

/** 合并"档位默认 + 用户覆盖 + 过期临时授权"得出最终级别。 */
export function resolveLevel(capability, permissions = {}) {
  const mode = MODES[permissions.mode] ?? MODES.standard
  const base = CAPABILITIES[capability]
  let level = base?.level ?? 'ask'

  if (mode.allowAll === true) {
    // 免确认模式：连 deny 也放开（否则"完全权限"名不副实）
    level = 'allow'
  } else {
    if (mode.promote?.includes(capability) && level === 'allow') level = 'ask'
    if (mode.demote?.includes(capability) && level === 'ask') level = 'allow'
  }

  const override = permissions.overrides?.[capability]
  if (typeof override === 'string' && ['allow', 'ask', 'deny'].includes(override)) level = override

  const until = permissions.allowedOnce?.[capability]
  if (typeof until === 'number' && until > Date.now() && level === 'ask') level = 'allow_once'

  return level
}

/**
 * 权限门。所有工具调用从这里过一道，顺带写审计。
 *
 * `confirm` 是外部注入的"问用户"能力（HUD 卡片 + 语音），
 * 权限引擎自己不碰 UI/音频——这样它能在任何环境里单测。
 */
export class PermissionGate {
  #permissions
  #confirm
  #auditPath
  #now
  #log

  constructor({ permissions = {}, confirm, auditPath, now = () => Date.now(), log = () => {} } = {}) {
    this.#permissions = permissions
    this.#confirm = confirm
    this.#auditPath = auditPath
    this.#now = now
    this.#log = log
  }

  get permissions() { return this.#permissions }

  /** 运行期更新（设置页 / 暂停 10 分钟 / 本次会话允许） */
  update(patch) {
    this.#permissions = { ...this.#permissions, ...patch }
  }

  /**
   * 决策。
   * @param {{tool:string, args?:object, utterance?:string, result?:{ok:boolean,ms:number}}} request
   * @returns {Promise<{allow:boolean, level:string, capability:string, reason?:string, channel?:string, needsConfirm?:boolean}>}
   */
  async check({ tool, args = {}, utterance = '', signal }) {
    const started = this.#now()

    // 全局暂停优先于一切：用户说"暂停 10 分钟"就是想立刻静音
    const pausedUntil = this.#permissions.pausedUntil ?? 0
    if (pausedUntil > started) {
      const minutes = Math.ceil((pausedUntil - started) / 60000)
      return this.#finish({
        allow: false, level: 'deny', capability: 'all', channel: 'pause',
        reason: `已暂停 ${minutes} 分钟，期间不执行任何操作`, tool, args, utterance, started,
      })
    }

    const verdict = classify(tool, args)
    if (verdict.level === 'deny') {
      return this.#finish({
        allow: false, level: 'deny', capability: verdict.capability, channel: 'policy',
        reason: verdict.reason, tool, args, utterance, started,
      })
    }

    const allowAll = MODES[this.#permissions.mode ?? 'standard']?.allowAll === true
    let level = resolveLevel(verdict.capability, this.#permissions)
    // 风险升级（Win+R、rm -rf 之类）默认强制 ask；免确认模式下由模式决定
    if (verdict.escalated === true && !allowAll) level = 'ask'

    if (level === 'allow' || level === 'allow_once') {
      return this.#finish({
        allow: true, level, capability: verdict.capability,
        channel: allowAll ? 'unattended' : 'policy',
        reason: verdict.reason, tool, args, utterance, started,
      })
    }

    if (level === 'deny') {
      return this.#finish({
        allow: false, level: 'deny', capability: verdict.capability, channel: 'policy',
        reason: `「${CAPABILITIES[verdict.capability]?.label ?? verdict.capability}」已被设为拒绝`,
        tool, args, utterance, started,
      })
    }

    // level === 'ask'：交给确认通道
    if (typeof this.#confirm !== 'function') {
      return this.#finish({
        allow: false, level: 'deny', capability: verdict.capability, channel: 'no-channel',
        reason: '需要确认，但当前没有可用的确认通道',
        tool, args, utterance, started,
      })
    }

    const answer = await this.#confirm({
      tool,
      args,
      signal,
      capability: verdict.capability,
      label: CAPABILITIES[verdict.capability]?.label ?? verdict.capability,
      why: verdict.reason,
    })

    return this.#finish({
      allow: answer?.allow === true && ['click', 'voice'].includes(answer?.channel),
      level: 'ask',
      capability: verdict.capability,
      channel: answer?.channel ?? 'unknown',
      reason: answer?.allow === true && ['click', 'voice'].includes(answer?.channel)
        ? ''
        : (answer?.reason ?? '用户没有确认'),
      tool, args, utterance, started,
    })
  }

  /** 记一条"本次会话/15 分钟内允许" */
  grantOnce(capability, minutes = 15) {
    const allowedOnce = { ...(this.#permissions.allowedOnce ?? {}) }
    allowedOnce[capability] = this.#now() + minutes * 60000
    this.update({ allowedOnce })
    return allowedOnce[capability]
  }

  /** 暂停全部操作 */
  pause(minutes = 10) {
    const pausedUntil = this.#now() + minutes * 60000
    this.update({ pausedUntil })
    return pausedUntil
  }

  /** 审计落盘：只在需要留痕的决策上写 */
  async #finish(info) {
    const { allow, level, capability, channel, reason, tool, args, utterance, started } = info
    const record = {
      ts: new Date(started).toISOString(),
      tool,
      capability,
      level,
      decision: allow ? 'allow' : 'deny',
      channel,
      utterance: String(utterance ?? '').slice(0, 120),
      args: summarizeArgs(args),
      reason: reason ?? '',
      duration_ms: this.#now() - started,
    }

    const base = CAPABILITIES[capability]
    const worthLogging = level !== 'allow' || base?.audit === true || capability === 'unknown' || capability === 'all'
    if (worthLogging) this.#log(`perm: ${record.decision} ${record.capability} via ${record.channel}${reason ? ` (${reason})` : ''}`)
    if (worthLogging && this.#auditPath !== undefined && this.#auditPath !== '') {
      try {
        await mkdir(dirname(this.#auditPath), { recursive: true })
        await appendFile(this.#auditPath, `${JSON.stringify(record)}\n`, 'utf8')
      } catch (error) {
        this.#log(`audit write failed: ${error.message}`)
      }
    }

    return { allow, level, capability, channel, reason }
  }
}

/** 只接受完整的确认/拒绝表达；疑问、引用和歧义保持等待。 */
export function parseConfirmText(raw) {
  // Whole utterances only; never search for affirmative substrings.
  const text = String(raw ?? '').normalize('NFKC').replace(/\s/gu, '').replace(/[。.!！,，、]+$/u, '')
  // Complete commands only: questions, quotations and negated cancellation stay undecided.
  if (/^(?:取消(?:吧)?|不用了|不行|不要(?:了|执行(?:了)?|确认这个东西)?|别执行(?:了)?|不可以|不同意|不确认|不允许|不执行|拒绝|算了(?:吧)?|停止(?:操作)?)$/u.test(text)) {
    return { allow: false, channel: 'voice', reason: `用户说：${text}` }
  }
  if (/^(?:(?:可以|确认|确定|同意|允许|好的|好|是的|对|执行|继续)(?:吧|了)?|可以[，,]?执行吧)$/u.test(text)) {
    return { allow: true, channel: 'voice' }
  }
  return null
}

/** 参数摘要：审计里不能塞整段截图 base64，但也不能只剩个工具名 */export function summarizeArgs(args) {
  if (args === null || typeof args !== 'object') return {}
  const out = {}
  for (const [key, value] of Object.entries(args)) {
    if (typeof value === 'string') out[key] = value.length > 80 ? `${value.slice(0, 80)}…` : value
    else if (typeof value === 'number' || typeof value === 'boolean' || value === null) out[key] = value
    else if (Array.isArray(value)) out[key] = `[${value.length}]`
    else out[key] = '[object]'
  }
  return out
}
