/**
 * 资源策略（R14）：什么时候该把模型"让"出来。
 *
 * 背景：TTS worker 常驻 1.5GB 显存、ASR 常驻 240MB 内存。
 * 用户打游戏（全屏）时这两块就是纯占用——游戏掉帧的第一嫌疑。
 *
 * 两档动作，都只做"可逆的让路"，不牺牲唤醒能力：
 *   release-voice  全屏应用占满屏幕 → 关掉 TTS worker（释放显存），唤醒与识别照旧
 *   release-asr    可用内存低于阈值 → 卸掉 ASR（240MB），但**保留 KWS**（3.3MB），
 *                  所以唤醒词仍然有效，只是唤醒了要多等 6 秒加载识别模型
 *
 * 为什么保留 KWS：唤醒是"这个软件还在"的唯一证据。为了省 3MB 把唤醒也关了，
 * 用户只会觉得软件死了。
 */

export const RESOURCE_DEFAULTS = {
  /** 前台窗口覆盖工作区超过这个比例就算"全屏遮挡"（边框全屏游戏常见 1.0，最大化约 1.0 也扣掉任务栏） */
  fullscreenRatio: 0.98,
  /** 可用内存低于多少 MB 时卸 ASR */
  memoryFloorMb: 1200,
  /** 全屏判定额外要求：窗口高度接近屏幕高度（防止横向铺满但只占一半的情况） */
  minHeightRatio: 0.95,
  pollMs: 20000,
}

/**
 * 覆盖比例：用**整块显示器**（rcMonitor）而不是工作区来算。
 *
 * 为什么：最大化窗口正好等于工作区（1.0），如果按工作区算，用户平常"浏览器最大化"
 * 就会被当成全屏游戏，语音一直被关掉。真正的全屏游戏会盖住任务栏、等于整块显示器。
 * 数据里没有 monW/monH 时退回工作区（老版本探测脚本的兼容）。
 */
export function coverageRatio(view) {
  if (view === null || typeof view !== 'object') return 0
  const baseX = view.monLeft ?? view.screenLeft
  const baseY = view.monTop ?? view.screenTop
  const baseW = view.monW ?? view.screenW
  const baseH = view.monH ?? view.screenH
  if (!(baseW > 0) || !(baseH > 0)) return 0
  const width = Math.max(0, Math.min(view.right, baseX + baseW) - Math.max(view.left, baseX))
  const height = Math.max(0, Math.min(view.bottom, baseY + baseH) - Math.max(view.top, baseY))
  return (width * height) / (baseW * baseH)
}

/** 是否处于"全屏遮挡"状态 */
export function isFullscreen(info, options = {}) {
  const cfg = { ...RESOURCE_DEFAULTS, ...options }
  if (info === null || typeof info !== 'object') return false
  if (typeof info.title !== 'string' || info.title === '') return false
  // 任务栏/桌面这类不算游戏
  const exempt = new Set(['explorer', 'ShellExperienceHost', 'SearchHost', 'StartMenuExperienceHost'])
  if (exempt.has(String(info.process ?? ''))) return false
  const ratio = coverageRatio(info)
  const baseY = info.monTop ?? info.screenTop
  const baseH = info.monH ?? info.screenH
  const heightRatio = (info.bottom - info.top) / Math.max(1, baseH)
  // 高度也要占满：横向铺满但只占半屏的情况（分屏）不算
  return ratio >= cfg.fullscreenRatio && heightRatio >= cfg.minHeightRatio && baseY !== undefined
}

/**
 * 决策。
 * @param {{fullscreen:boolean, freeMemMb:number, voiceLoaded:boolean, asrLoaded:boolean, busy:boolean}} state
 *   busy = 正在对话/播报/等确认 —— 这时候一律不动
 * @returns {{releaseVoice:boolean, releaseAsr:boolean, reason:string}}
 */
export function decide(state, options = {}) {
  const cfg = { ...RESOURCE_DEFAULTS, ...options }
  const { fullscreen = false, freeMemMb = Infinity, voiceLoaded = false, asrLoaded = false, busy = false } = state ?? {}

  if (busy) return { releaseVoice: false, releaseAsr: false, reason: 'busy' }

  const needsVoiceRelease = fullscreen && voiceLoaded
  const needsAsrRelease = freeMemMb < cfg.memoryFloorMb && asrLoaded
  if (!needsVoiceRelease && !needsAsrRelease) {
    return { releaseVoice: false, releaseAsr: false, reason: 'ok' }
  }
  const parts = []
  if (needsVoiceRelease) parts.push('全屏应用占满屏幕 → 释放 TTS 显存')
  if (needsAsrRelease) parts.push(`可用内存 ${Math.round(freeMemMb)}MB < ${cfg.memoryFloorMb}MB → 卸载 ASR（保留唤醒）`)
  return { releaseVoice: needsVoiceRelease, releaseAsr: needsAsrRelease, reason: parts.join('；') }
}
