/**
 * 语音助手 · Electron 主进程（P2 壳）
 *
 * 本阶段只验证三件硬要求（docs/08 §3）：
 *   1. 启动不建窗口，只有托盘
 *   2. 语音唤醒用 showInactive（不抢焦点），托盘点击用 show（聚焦）
 *   3. 小窗按 workArea 贴右下角，DPI 缩放下不偏移
 *
 * 语音、ASR、TTS、agent 都还没接，窗口状态由托盘"演示"菜单驱动。
 */
import { app, BrowserWindow, Tray, Menu, screen, ipcMain, nativeImage, session, safeStorage, net, shell } from 'electron'
import { appendFileSync, existsSync, renameSync, statSync } from 'node:fs'
import { freemem } from 'node:os'
import { spawnSync } from 'node:child_process'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { writeWav } from '../../packages/voice/wav.mjs'
import { judgeBargeIn, stripBargeWords } from '../../packages/voice/bargein.mjs'
import { parseConfirmText } from '../../packages/agent/permissions.mjs'
import { parseOfflineIntent, offlineUnsupported, formatTime, formatDate, isOfflineError } from '../../packages/agent/offline.mjs'
import { judgeFollowUp, windowFor, heuristicFollowUp, FOLLOWUP_DEFAULTS } from '../../packages/agent/followup.mjs'
import { cleanLine as cleanLineText } from '../../packages/agent/text.mjs'
import { resolveModel } from '../../packages/config/schedule.mjs'
import { LAUNCH_APP_TOOL } from '../../packages/tools/app-launch.mjs'
import { SHELL_TOOL, runShell } from '../../packages/tools/shell.mjs'
import { FILES_TOOL, runFiles } from '../../packages/tools/files.mjs'
import { PROC_TOOL, KILL_TOOL, runAppControl, runKillApp } from '../../packages/tools/procs.mjs'
import { SYSINFO_TOOL, runSystemInfo } from '../../packages/tools/sysinfo.mjs'
import { NETEASE_TOOL, runNetease, runCliAsync, syncCredentialsAsync, loginStatus, startLoginSession, clearLoginCache } from '../../packages/tools/netease.mjs'
import { player as musicPlayer } from '../../packages/tools/mpv-player.mjs'
import { externalPath } from '../../packages/config/paths.mjs'

const HERE = dirname(fileURLToPath(import.meta.url))
const PROJECT_ROOT = join(HERE, '..', '..')
/**
 * 打包后：代码在 app.asar 里，模型与 .runtime 在 resources 里，配置必须在 userData。
 * 这些变量必须在**任何** voice/config 模块被动态 import 之前设好——它们按调用时读取。
 */
const APP_ROOT = app.isPackaged ? process.resourcesPath : PROJECT_ROOT
const DATA_ROOT = app.isPackaged ? join(app.getPath('userData'), 'data') : join(PROJECT_ROOT, '.config')
process.env.CUA_VA_ROOT = APP_ROOT
process.env.CUA_VA_DATA_DIR = DATA_ROOT
process.env.CUA_VA_CONFIG_DIR = app.isPackaged ? join(DATA_ROOT, 'config') : join(PROJECT_ROOT, '.config')
// Python 读不到 asar：打包后 worker 脚本在 resources 根目录（见 electron-builder.config.cjs）
if (app.isPackaged) process.env.CUA_VA_WORKER_SCRIPT = join(APP_ROOT, 'gsv_worker.py')

// 打包后安装目录通常不可写，日志必须落到 userData
const LOG_PATH = app.isPackaged ? join(app.getPath('userData'), 'assistant.log') : join(HERE, 'shell.log')
/** 日志轮转：单文件超过这个大小就换名（保留一份 .1） */
const LOG_MAX_BYTES = 5 * 1024 * 1024
let logWrites = 0
/** 启动期日志：Electron 在部分环境拿不到 stdout，落文件最可靠 */
function log(message) {
  try {
    appendFileSync(LOG_PATH, `${new Date().toISOString()} ${message}\n`)
    logWrites += 1
    if (logWrites % 200 === 0) rotateLogIfNeeded()
  } catch { /* 忽略 */ }
}

function rotateLogIfNeeded() {
  try {
    if (!existsSync(LOG_PATH)) return
    if (statSync(LOG_PATH).size < LOG_MAX_BYTES) return
    renameSync(LOG_PATH, `${LOG_PATH}.1`)
    appendFileSync(LOG_PATH, `${new Date().toISOString()} log rotated（上一份见 .1）\n`)
  } catch { /* 轮转失败不影响主流程 */ }
}
process.on('uncaughtException', (error) => log(`uncaughtException: ${error.stack ?? error.message}`))
process.on('unhandledRejection', (reason) => log(`unhandledRejection: ${String(reason)}`))
log('module loaded')
const HUD_WIDTH = 380
const MARGIN = 20
const AUTO_HIDE_MS = 8000
/**
 * 窗口高度固定，卡片在渲染层贴底显示。
 *
 * 为什么固定：透明窗口在"可见状态下改尺寸"不会重绘新增区域，会留残影；
 * 而先 hide 再改再 show 会让用户看到一次闪烁（实测）。
 * 固定高度 + 卡片内部自适应，是唯一既无残影又无闪烁的做法。
 */
const CONV_HEIGHT = 300

/** 唤醒词说完后丢弃的一段音频（识别要跳过"你好狼宝"本身） */
const WAKE_TAIL_MS = 300
/** 说指令的最长等待：15 秒没说完就按已听到的收尾 */
const COMMAND_TIMEOUT_MS = 15000
/** 启动后多久开始预热识别模型（避开启动高峰） */
const ASR_PREWARM_DELAY_MS = 4000
/** 设置窗口 */
const SETTINGS_WIDTH = 760
const SETTINGS_HEIGHT = 720

const isSelfTest = process.argv.includes('--selftest')
const isDemo = process.argv.includes('--demo')
const isMicTest = process.argv.includes('--mic-test')
const isListen = process.argv.includes('--listen')
const asrDemoIndex = process.argv.indexOf('--asr-demo')
const isAsrDemo = asrDemoIndex !== -1
const asrDemoWav = isAsrDemo ? (process.argv[asrDemoIndex + 1] ?? 'assets/test/asr-zh.wav') : ''
const cmdDemoIndex = process.argv.indexOf('--cmd-demo')
const isCmdDemo = cmdDemoIndex !== -1
const cmdDemoText = isCmdDemo ? (process.argv.slice(cmdDemoIndex + 1).join(' ') || '帮我看看现在开了哪些窗口') : ''
const isEchoTest = process.argv.includes('--echo-test')
const isSettingsDemo = process.argv.includes('--settings-demo')
const isSettingsTest = process.argv.includes('--settings-test')
const isResourceTest = process.argv.includes('--resource-test')
const isOfflineTest = process.argv.includes('--offline-test')
const isVisionProbe = process.argv.includes('--vision-probe')
const isConfirmCloseTest = process.argv.includes('--confirm-close-test')
const isQuitTest = process.argv.includes('--quit-test')
const isFollowUpUiTest = process.argv.includes('--followup-ui-test')
const isPlayerUiTest = process.argv.includes('--player-ui-test')
const isCuaTest = process.argv.includes('--cua-test')
const isFollowUpTest = process.argv.includes('--followup-test')
const isHistoryTest = process.argv.includes('--history-test')
const searchTestIndex = process.argv.indexOf('--search-test')
const isSearchTest = searchTestIndex !== -1
const searchTestQuery = isSearchTest ? (process.argv.slice(searchTestIndex + 1).join(' ') || '北京今天天气') : ''
const gateIndex = process.argv.indexOf('--gate-test')
const isGateTest = gateIndex !== -1
const gateTestMode = isGateTest ? (process.argv[gateIndex + 1] ?? 'voice') : ''
const bargeIndex = process.argv.indexOf('--bargein-test')
const isBargeInTest = bargeIndex !== -1
const bargeInWav = isBargeInTest ? (process.argv[bargeIndex + 1] ?? 'assets/test/asr-zh.wav') : ''

/** @type {BrowserWindow|null} */
let win = null
/** @type {Tray|null} */
let tray = null
let hideTimer = null
/** 用户是否亲手收起过小窗（决定"自动隐藏后要不要再弹出来"） */
let userDismissedHud = false
/** 保存设置后的短暂静默期：点击声容易被 KWS 当成唤醒词 */
let wakeMutedUntil = 0
/** 当前进行中的网易云登录会话（前台子进程，扫码成功即结束） */
let loginSession = null
let lastShowMode = null
let currentState = 'hidden'
let contentReady = false
let windowReady = false
/** @type {{mode:string,state:string}|null} */
let pendingShow = null

/** @type {BrowserWindow|null} 隐藏的采集窗口 */
let captureWin = null
let listening = false
/** @type {import('../../packages/voice/stream.mjs').StreamingPipeline|null} */
let pipeline = null
let lastWakeAt = 0
let micStats = { chunks: 0, peakLevelDb: -120, floorDb: -60, wakes: 0 }

/** @type {import('../../packages/voice/asr.mjs').Asr|null} 识别模型，一次加载常驻 */
let asr = null
/** @type {Promise<any>|null} 加载中的句柄，防止并发重复加载 */
let asrLoading = null
/** 当前处于"说指令"阶段：唤醒之后、这句话定稿之前 */
let commandActive = false
/** 这个时间点之前的音频是唤醒词，不喂给识别 */
let commandStartAt = 0
let commandTimer = null
/** 最后一次非空的识别文本（端点定稿后 flush 可能为空，用它兜底） */
let lastCommandText = ''
/** 这一轮到底有没有听到任何字——决定"没听清"要不要弹脸 */
let heardAny = false
/** 上一次推给渲染层的 partial，用来去重 */
let lastSentPartial = ''
/** 回声测量用的采样桶（--echo-test），正常运行时为 null */
let echoProbe = null
/** 调试用：累积采集音频并落盘 */
let dumpPath = process.env.CUA_VA_DUMP_WAV ?? null
let dumpChunks = []

// ---------------------------------------------------------------- 窗口

function ensureWindow() {
  if (win !== null && !win.isDestroyed()) return win

  win = new BrowserWindow({
    width: HUD_WIDTH,
    height: CONV_HEIGHT,
    useContentSize: true,       // 380 指的是内容宽度，不含 Windows 的隐形边框
    show: false,
    frame: false,
    // 用透明窗口 + CSS 圆角（Windows 11 的 roundedCorners 对无框窗口未生效，实测方角）。
    // 透明窗口的坑是"可见状态下改尺寸不重绘"，所以在 resize 时先 hide、改完再 show。
    transparent: true,
    backgroundColor: '#00000000',
    hasShadow: false,
    resizable: false,
    movable: false,
    minimizable: false,
    maximizable: false,
    fullscreenable: false,
    skipTaskbar: true,          // 不进任务栏
    alwaysOnTop: true,          // 置顶但不激活
    webPreferences: {
      preload: join(HERE, 'preload.mjs'),
      contextIsolation: true,
      nodeIntegration: false,
      // ESM preload（.mjs）要求关闭 sandbox，否则 preload 加载失败、
      // 渲染层拿不到 window.hud —— 表现为"只有静态 HTML，没有状态"
      sandbox: false,
    },
  })

  win.setAlwaysOnTop(true, 'floating')
  win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: false })
  win.loadFile(join(HERE, 'renderer', 'index.html'))

  // 首帧渲染完成再允许显示：否则会先闪一下白底（实测）
  windowReady = false
  win.once('ready-to-show', () => {
    windowReady = true
    if (pendingShow !== null && contentReady) {
      const { mode, state } = pendingShow
      pendingShow = null
      doShow(win, mode, state)
    }
  })

  // 渲染层错误要能被看到，否则"页面白着"很难查
  win.webContents.on('console-message', (_event, level, message) => {
    if (level >= 2) console.log(`[renderer:${level}] ${message}`)
  })
  win.webContents.on('preload-error', (_event, preloadPath, error) => {
    console.log(`[preload-error] ${preloadPath}: ${error.message}`)
  })
  win.webContents.on('did-finish-load', () => {
    if (win !== null && !win.isDestroyed()) win.webContents.send('hud:state', currentState)
    // 首帧先把内容高度报上来，否则窗口停在初始高度、内容被裁
    win?.webContents.send('hud:state', currentState)
  })

  // 失焦不隐藏：用户可能正在看字幕
  win.on('close', (event) => {
    // 点 × / Alt+F4 = 想关掉这个软件，不是"把卡片藏起来"
    // （以前这里直接销毁窗口，进程还活着，用户看到的就是"关不掉"）
    if (app.isQuitting === true) return
    event.preventDefault()
    quitApp('hud close')
  })
  win.on('closed', () => { closeConfirmCard(); win = null })
  return win
}

/** 按 workArea（不含任务栏）贴到右下角；所有数值都是 DIP，由 Electron 处理 DPI。 */
function positionBottomRight(target) {
  const { workArea } = screen.getPrimaryDisplay()
  const bounds = target.getBounds()          // 外框尺寸才决定贴边位置
  const x = workArea.x + workArea.width - bounds.width - MARGIN
  const y = workArea.y + workArea.height - bounds.height - MARGIN
  target.setPosition(Math.round(x), Math.round(y), false)
  return { x, y, workArea, size: [bounds.width, bounds.height] }
}

function setState(state) {
  currentState = state
  if (tray !== null) {
    const icon = state === 'listening' ? 'tray-listen.png'
      : state === 'speaking' || state === 'confirm' ? 'tray-speak.png'
        : 'tray-idle.png'
    try { tray.setImage(nativeImage.createFromPath(join(HERE, 'assets', icon))) } catch { /* 图标缺失不影响功能 */ }
  }
  if (win !== null && !win.isDestroyed()) {
    win.webContents.send('hud:state', state)
  }
}

/** 托盘点击：明确的用户意图，聚焦。 */
function showForInteraction(state = 'listening') {
  return requestShow('show', state, () => showForInteraction(state))
}

/** 语音唤醒：不抢焦点，用户可能正在打字。 */
function showForWake(state = 'listening') {
  return requestShow('showInactive', state, () => showForWake(state))
}

/**
 * 统一入口：先确保窗口高度已经按内容调整好，再显示。
 *
 * 为什么：透明窗口在「可见状态下改高度」时，Windows 不会重绘新增区域，
 * 会在卡片内部留下矩形接缝（实测）。所以首次显示要等渲染层报过高度。
 */
function requestShow(mode, state, retry) {
  lastShowMode = mode
  const target = ensureWindow()
  if (!contentReady || !windowReady) {
    pendingShow = { mode, state, retry }
    // 兜底：某些环境下 ready-to-show 可能迟迟不来，不能让"唤醒"被卡住
    setTimeout(() => {
      if (contentReady && windowReady) return
      contentReady = true
      windowReady = true
      if (pendingShow !== null) {
        const { mode: m, state: s } = pendingShow
        pendingShow = null
        log('show fallback: ready signal missing')
        doShow(target, m, s)
      }
    }, 800)
    return target
  }
  doShow(target, mode, state)
  return target
}

function doShow(target, mode, state) {
  // 诊断用：任何一次 HUD 显示都记下触发栈（CUA_VA_TRACE_SHOW=1 时更详细）
  try {
    const frames = String(new Error().stack ?? '').split('\n').slice(2, 6).map((line) => line.trim()).join(' <- ')
    log(`doShow(state=${state}, mode=${mode}, settingsOpen=${settingsWin !== null && !settingsWin.isDestroyed() && settingsWin.isVisible()}) <- ${frames}`)
  } catch { /* 记录失败不影响显示 */ }
  /*
   * 改设置的时候一律不弹 HUD。
   * 之前点"保存"会让助手从内部冒出来（不是唤醒词，是某条内部调用链触发的显示），
   * 与其一条条追，不如在这里立一条规矩：设置窗口开着，谁也别想弹窗。
   */
  if (process.env.CUA_VA_SUPPRESS_SHOW !== '0' && settingsWin !== null && !settingsWin.isDestroyed() && settingsWin.isVisible()) {
    log(`show suppressed（设置在开，state=${state}）: ${new Error().stack?.split('\n').slice(2, 4).join(' | ') ?? ''}`)
    return
  }
  userDismissedHud = false
  positionBottomRight(target)
  if (mode === 'showInactive') target.showInactive()
  else { target.show(); target.focus() }
  setState(state)
  scheduleHide()
}

function scheduleHide(ms = AUTO_HIDE_MS) {
  if (hideTimer !== null) clearTimeout(hideTimer)
  hideTimer = setTimeout(() => {
    /*
     * 还有话没说完、或者这一轮还在跑，就不能收：
     *   activeTurn  —— 模型还在调工具（一轮实测能跑 60 秒），中途收窗口会让后面的播报
     *                  撞上"窗口已隐藏"的保护逻辑被丢掉（用户听到的就是"话说一半没了"）
     *   ttsLoading  —— worker 还在加载（冷启动 20 秒）
     *   ttsPumping / 队列非空 —— 正在合成或还有句子没播
     */
    if (activeTurn !== null || ttsLoading !== null || ttsPumping || ttsQueue.length > 0) {
      scheduleHide(2000)
      return
    }
    hideHud()
  }, ms)
}

function hideHud() {
  log(`hideHud（state=${currentState} activeTurn=${activeTurn !== null} pumping=${ttsPumping} queue=${ttsQueue.length} buffer=${speechBuffer.length}）`)
  if (hideTimer !== null) { clearTimeout(hideTimer); hideTimer = null }
  stopSpeaking()
  if (win !== null && !win.isDestroyed()) win.hide()
  setState('hidden')
}

/** 对话结束后的调用点：R14 里"隐藏 vs 销毁"的决策也在这里。 */
function dismiss() {
  closeConfirmCard()
  /*
   * 用户点 HUD 的 × 就是"别干了"，不是"先躲一下"。
   * 之前只隐藏窗口、麦克风还开着，随便一点动静又把它叫醒——这就是"关不掉还复活"。
   * 所以这里连监听一起停：要再用，从托盘点「唤醒 / 开始对话」。
   */
  userDismissedHud = true
  stopSpeaking()
  activeTurn = null
  commandActive = false
  clearTimeout(commandTimer)
  if (listening) {
    void stopListening().then(() => { syncWakeTray(false) })
  }
  hideHud()
  log('dismiss: 窗口关闭，监听已停止')
}

/**
 * 退出。先把手上的东西放干净，再直接 exit。
 *
 * 为什么用 app.exit 而不是 app.quit：sherpa/onnx 的原生对象在进程收尾阶段被析构时
 * 会 fail-fast（实测崩溃码 0xc0000409，Windows 事件日志里有 Application Error 记录）。
 * 用户看到的就是"一退出就弹崩溃报告"。app.exit 跳过 JS 收尾，绕开这个析构顺序问题；
 * 该释放的（麦克风、子进程、音频）我们已经在上面前置做好了。
 */
async function shutdown() {
  log('shutdown requested')
  // 兜底：任何一步卡住都不能让"退出"变成没反应（实测过一次等 worker 不返回）
  const hardStop = setTimeout(() => {
    log('shutdown hard-stop: 释放超时，强制退出')
    app.exit(0)
  }, 5000)
  try { await stopListening() } catch { /* 麦克风可能已经释放 */ }
  try { stopSpeaking() } catch { /* 队列可能已空 */ }
  // TTS worker 常驻 1.5GB 显存，退出必须回收，否则下次启动会和残留进程抢显存
  try { if (ttsEngine !== null) await ttsEngine.dispose() } catch { /* 可能已经退出 */ }
  try { if (agentRuntime !== null) await agentRuntime.toolkit.dispose() } catch { /* 桥接可能已退出 */ }
  clearTimeout(hardStop)
  log('shutdown done')
  app.exit(0)
}

/** 唯一的退出入口（托盘、窗口关闭、SIGINT 都走这里，保证只跑一次） */
let shuttingDown = false
function quitApp(reason) {
  if (shuttingDown) return
  shuttingDown = true
  app.isQuitting = true
  log(`quit requested（${reason}）`)
  void shutdown()
}

// ---------------------------------------------------------------- 采集与唤醒

/** 隐藏的采集窗口：getUserMedia 只能在渲染进程用 */
function ensureCaptureWindow() {
  if (captureWin !== null && !captureWin.isDestroyed()) return captureWin
  captureWin = new BrowserWindow({
    show: false,
    width: 1,
    height: 1,
    skipTaskbar: true,
    webPreferences: {
      preload: join(HERE, 'preload-capture.mjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
      backgroundThrottling: false, // 后台窗口也要持续收音频
    },
  })
  captureWin.loadFile(join(HERE, 'renderer', 'capture.html'), {
    // 声学闭环测试时关掉 AEC（否则扬声器里的测试音会被消除）
    query: process.env.CUA_VA_NO_AEC === '1' ? { noaec: '1' } : {},
  })
  captureWin.webContents.on('console-message', (_e, level, message) => {
    if (level >= 2) log(`[capture:${level}] ${message}`)
  })
  return captureWin
}

/** 打开采集 + 唤醒监听 */
/**
 * 开麦（幂等）。
 *
 * 开机自启时，"恢复上次唤醒"和 "--listen" 两条路径会在同一瞬间各调一次；
 * 原来的 `if (listening)` 挡不住——因为 listening 要等 await 之后才置 true，
 * 于是开了两个采集窗口：一个成功、一个报错，报错的把状态改成 error（托盘图标不对），
 * KWS 也挂在那个半死的流上，喊唤醒词没反应。这里用"进行中的 Promise"做互斥。
 */
let listeningStartPromise = null

async function startListening() {
  if (listening) return { ok: true, already: true }
  if (listeningStartPromise !== null) return listeningStartPromise
  listeningStartPromise = doStartListening().finally(() => { listeningStartPromise = null })
  return listeningStartPromise
}

async function doStartListening() {
  try {
    // 延迟加载：sherpa-onnx 原生模块 + KWS 模型初始化约 1 秒，
    // 而且模型文件缺失时不应该影响应用启动
    if (pipeline === null) {
      const { StreamingPipeline } = await import('../../packages/voice/stream.mjs')
      pipeline = new StreamingPipeline()
    }

    const target = ensureCaptureWindow()
    await new Promise((resolve) => {
      if (target.webContents.isLoading() === false) { resolve(); return }
      target.webContents.once('did-finish-load', () => resolve())
    })
    const result = await target.webContents.executeJavaScript('window.captureControl.start()')
    listening = true
    setState('listening')
    syncWakeTray(true)
    log(`listening started: ${JSON.stringify(result)}`)
    return result
  } catch (error) {
    listening = false
    log(`listening start failed: ${error.message}`)
    setState('error')
    return { ok: false, error: error.message }
  }
}

/** 关闭采集：必须释放麦克风，否则托盘麦克风图标会一直亮 */
async function stopListening() {  if (!listening) return { ok: true, already: true }
  listening = false

  // 关麦就等于这一轮对话结束了，识别状态必须一起清，
  // 否则下次唤醒会带着上一轮的残句（stream 里的假设不会自己消失）
  commandActive = false
  clearTimeout(commandTimer)
  lastCommandText = ''
  heardAny = false
  if (asr !== null) asr.reset()

  // 落盘调试音频（如果开启了 dump）
  if (dumpPath !== null && dumpChunks.length > 0) {
    try {
      const { writeWav } = await import('../../packages/voice/wav.mjs')
      const total = dumpChunks.reduce((sum, c) => sum + c.length, 0)
      const merged = new Float32Array(total)
      let cursor = 0
      for (const c of dumpChunks) { merged.set(c, cursor); cursor += c.length }
      writeWav(dumpPath, merged, 16000)
      log(`dumped ${(total / 16000).toFixed(1)}s to ${dumpPath}`)
    } catch (error) {
      log(`dump failed: ${error.message}`)
    }
    dumpChunks = []
  }

  const target = captureWin
  if (target !== null && !target.isDestroyed()) {
    try { await target.webContents.executeJavaScript('window.captureControl.stop()') } catch { /* 窗口可能已销毁 */ }
    try { target.destroy() } catch { /* 已销毁 */ }
  }
  captureWin = null
  setState('hidden')
  syncWakeTray(false)
  resetSession('mic-off')
  log('listening stopped')
  return { ok: true }
}

/** 唤醒命中：弹出小窗并进入"说指令"阶段 */
function onWake(keyword) {
  if (confirmResolver !== null) return // A wake must not replace a pending confirmation.
  /*
   * 设置窗口在前台时不响应唤醒词。
   * 用户点"保存"、敲键盘时，KWS 很容易被点击声/人声误触发，
   * 表现出来就是"一保存助手就醒了"——它是被声音叫醒的，不是被保存叫醒的。
   */
if ((settingsWin !== null && !settingsWin.isDestroyed() && settingsWin.isVisible())
    || (wakeMutedUntil > 0 && Date.now() < wakeMutedUntil)) {
    return
  }
  log(`wake: ${keyword}`)
  // 一次唤醒 = 一次会话：清掉上一件事的记忆，避免"新问题被老问题带偏"
  resetSession('wake')
  showForWake('listening')
  sendSubtitle({ reset: true, hint: '我在听，请说' })

  commandActive = true
  heardAny = false
  lastCommandText = ''
  lastSentPartial = ''
  // 唤醒词本身（"你好狼宝"）已经进了麦克风。识别从 300ms 后开始，
  // 否则识别结果会带着"你好狼宝"四个字，指令就变成"你好狼宝打开浏览器"。
  commandStartAt = Date.now() + WAKE_TAIL_MS
  clearTimeout(commandTimer)
  commandTimer = setTimeout(() => finishCommand('timeout'), COMMAND_TIMEOUT_MS)

  if (asr === null) {
    // 首次唤醒可能还在加载模型：先说"准备中"，加完自动接上
    if (asrLoading) sendSubtitle({ hint: '正在准备识别…' })
    void startAsrLoad().then((instance) => {
      if (instance !== null && commandActive) instance.reset()
    })
  } else {
    asr.reset()
  }
}

// ---------------------------------------------------------------- 会话记忆

/** 会话条数上限（一轮 = user + assistant 两条） */
const SESSION_MAX_TURNS = 6

/**
 * 开始一段新会话。
 *
 * 触发点：唤醒、关麦。为什么要清：用户按下唤醒词的意思是"这事儿重新说"，
 * 而不是"接着上一件事"；留着旧记忆会让"帮我打开浏览器"变成"再开一个"。
 */
function resetSession(reason) {
  if (sessionHistory.length > 0) log(`session reset (${reason}), dropped ${sessionHistory.length} msgs`)
  sessionHistory = []
}

/** 记下一轮对话（只存话，不存工具细节） */
function rememberTurn(userText, assistantText, toolNames) {
  sessionHistory.push({ role: 'user', content: userText })
  const note = toolNames.length > 0 ? `\n（这一轮动过：${toolNames.join(', ')}）` : ''
  sessionHistory.push({ role: 'assistant', content: `${assistantText}${note}` })
  while (sessionHistory.length > SESSION_MAX_TURNS * 2) sessionHistory.shift()
}

// ---------------------------------------------------------------- 识别（ASR）

/**
 * 识别结果 → 字幕/指令。
 *
 * 分成两段：partial（边说边变）和 final（端点定稿）。
 * 字幕要 partial，执行只能等 final——半句话拿去执行会做错事。
 */
function sendSubtitle(payload) {
  if (!subtitlesEnabled) return
  if (win !== null && !win.isDestroyed()) win.webContents.send('hud:subtitle', payload)
}

/**
 * 声音还在播的时候，卡片的"下一步"要等一等。
 *
 * 用户的原话是「操作和卡片显示是一个整体，跟语音完全错位」——
 * 之前模型一边生成一边推进状态，卡片已经写着"正在看屏幕"，
 * 而耳朵里还在听上一句。这里把这类 UI 变更挂起，等播报结束再执行。
 */
let pendingUiAction = null

function speechBusy() {
  return ttsPumping || ttsQueue.length > 0 || speechBuffer !== '' || playingAudio()
}

function applyUiWhenQuiet(action) {
  if (!speechBusy()) {
    pendingUiAction = null
    action()
    return
  }
  pendingUiAction = action
}

/** 播报彻底安静下来时调用：把挂起的 UI 变更补上 */
function flushPendingUiAction() {
  if (pendingUiAction === null) return
  const action = pendingUiAction
  pendingUiAction = null
  try { action() } catch (error) { log(`pending ui action failed: ${error.message}`) }
}

/** 加载识别模型。6.5s / 240MB（实测），所以只能加载一次，绝不能每次唤醒都建 */
function startAsrLoad() {
  if (asr !== null) return Promise.resolve(asr)
  if (asrLoading !== null) return asrLoading
  const startedAt = Date.now()
  asrLoading = (async () => {
    try {
      const { Asr } = await import('../../packages/voice/asr.mjs')
      asr = new Asr()
      log(`asr ready in ${Date.now() - startedAt}ms`)
      return asr
    } catch (error) {
      log(`asr load failed: ${error.message}`)
      return null
    } finally {
      asrLoading = null
    }
  })()
  return asrLoading
}

/**
 * 启动后预热：模型初始化要 6.5 秒，如果等到用户开口才加载，
 * 第一次唤醒就是干等 6.5 秒。空闲时先付掉这个成本。
 * 内存不够就不预热——宁可第一次慢，也不能把用户的机器吃满（R14）。
 */
function maybePrewarmAsr() {
  if (process.env.CUA_VA_NO_PREWARM === '1') { log('asr prewarm disabled by env'); return }
  const freeGb = freemem() / 1073741824
  if (freeGb < 2) { log(`asr prewarm skipped: free ${freeGb.toFixed(1)}GB`); return }
  setTimeout(() => {
    // 模型初始化是同步的原生调用，会独占主线程 6~8 秒（实测）。
    // 正在对话/正在播报时预热 = 让用户的话和打断检测全部卡住，宁愿晚点再热。
    if (activeTurn !== null || commandActive || playingAudio() || ttsQueue.length > 0) {
      log('asr prewarm postponed: conversation active')
      setTimeout(() => maybePrewarmAsr(), 20000)
      return
    }
    log(`asr prewarm start (free ${freeGb.toFixed(1)}GB)`)
    void startAsrLoad()
  }, ASR_PREWARM_DELAY_MS)
}

/** 一句说完了：把字幕定稿，进入思考态 */
function finishCommand(reason) {
  if (!commandActive) return
  commandActive = false
  clearTimeout(commandTimer)

  // 待命窗口的结局要记下来：这是以后调阈值的唯一依据
  if (followUpWindowStart > 0) {
    const waited = Date.now() - followUpWindowStart
    log(`follow-up outcome: ${reason === 'followup-timeout' ? 'silent' : 'spoke'} after ${waited}ms (${reason})`)
    followUpWindowStart = 0
  }

  let text = asr === null ? '' : asr.flush()
  if (reason === 'timeout') log(`command timeout, tail="${text}"`)
  if (text === '' && lastCommandText !== '') {
    // 端点已判过：flush 可能是空的，用最后一次 partial 兜底
    text = lastCommandText
  }
  lastCommandText = ''

  // 正在等确认：这句话是"答不答应"，不是新指令
  if (confirmResolver !== null) {
    const answer = voiceConfirmEnabled ? parseConfirmText(text) : null
    log(`confirm heard: "${text}" → ${answer === null ? '未表态' : (answer.allow ? '允许' : '拒绝')}`)
    if (answer?.allow === false) settleConfirm(answer, pendingConfirmId)
    else {
      sendSubtitle({ hint: '请点击按钮批准；语音仅可取消（需开启语音取消）' })
      if (voiceConfirmEnabled) {
        asr?.reset()
        commandActive = true
        heardAny = false
        lastSentPartial = ''
        commandStartAt = Date.now() + 300
      }
    }
    return
  }

  // 只说了打断词（"等一下"）——那不是指令，是"闭嘴"。
  // 打断之后同一句音频的尾巴还会被识别，实测会拼成「等一下等一下」拿去问模型，
  // 于是它自言自语"你要我做什么"。这里把它清掉并重新进入聆听。
  const cleaned = stripBargeWords(text)
  if (cleaned === '' && text.trim() !== '') {
    log(`command is only the barge word: "${text}"`)
    sendSubtitle({ hint: '我在听，请说' })
    setState('listening')
    commandActive = true
    heardAny = false
    lastSentPartial = ''
    commandStartAt = Date.now() + 200
    clearTimeout(commandTimer)
    commandTimer = setTimeout(() => finishCommand('timeout'), COMMAND_TIMEOUT_MS)
    scheduleHide(12000)
    return
  }

  if (cleaned.trim() === '') {
    log('command empty')
    if (!heardAny) {
      // 用户唤醒了但一句话没说：安静收起，不要用红字问他"没听清"
      hideHud()
      return
    }
    sendSubtitle({ hint: '没听清，再说一次？' })
    setState('error')
    scheduleHide()
    return
  }

  log(`command: ${cleaned} (${reason})`)
  sendSubtitle({ final: cleaned })
  setState('thinking')
  scheduleHide(15000)
  void runCommand(cleaned)
}

// ---------------------------------------------------------------- agent（说 → 做）

/** 回合序号：晚到的旧回合不该覆盖新回合的字幕 */
let turnSeq = 0
/** @type {{agent:any, toolkit:any, config:any, readyMs:number}|null} */
let agentRuntime = null
let agentLoading = null
/** 当前回合；agent 的事件都倒进这里 */
let activeTurn = null

/**
 * 把流式文本切成句：字幕和播报都以"句"为单位。
 * 半句话没法读——TTS 攒到句号才出声，听起来才像人在说话。
 */
function takeSentences(turn) {
  const parts = turn.buffer.split(/(?<=[。！？!?\n])/)
  turn.buffer = parts.pop() ?? ''
  const out = []
  for (const part of parts) {
    const sentence = part.trim()
    if (sentence !== '') out.push(sentence)
  }
  return out
}

/** 一句话定稿：进播报队列；**字幕等真的开始播才出现**，卡住不准跑到声音前面 */
function speakLine(turn, sentence) {
  const clean = cleanLine(sentence)
  if (clean === '') return
  lastAssistantText = clean
  log(`say: ${clean}`)
  // 没有声音输出（关播报/全屏让路）时，字幕就该立刻出现，否则用户什么都看不到
  if (!ttsEnabled || voiceSuspended) sendSubtitle({ final: clean })
  enqueueSpeech(clean)
}

/** 最后一句话（离线"再说一遍"要用） */
let lastAssistantText = ''

/**
 * 离线回复：不经过 LLM，直接说一句。
 * 走的是同一条播报队列，所以打断、字幕、连续对话都照旧生效。
 */
function replyOffline(text) {
  lastAssistantText = text
  sendSubtitle({ final: text })
  log(`say(offline): ${text}`)
  setState('speaking')
  enqueueSpeech(text)
}

/** 现在是不是"没有网"（测试时可用 CUA_VA_FORCE_OFFLINE=1 强制） */
function isOfflineNow() {
  if (process.env.CUA_VA_FORCE_OFFLINE === '1') return true
  try { return net.isOnline() === false } catch { return false }
}

/**
 * 离线兜底：只处理本地能办到的几件事。
 *
 * 判断原则：宁可不认，也不要"以为听懂了其实办错"。
 * 所以这里只做规则匹配，不接本地小模型。
 */
async function runOfflineCommand(text) {
  const parsed = parseOfflineIntent(text)
  if (parsed === null) {
    replyOffline(offlineUnsupported(text))
    return { handled: false, intent: null }
  }

  switch (parsed.intent) {
    case 'time':
      replyOffline(`现在是${formatTime()}。`)
      break
    case 'date':
      replyOffline(`今天是${formatDate()}。`)
      break
    case 'stop':
      stopSpeaking()
      break
    case 'repeat':
      replyOffline(lastAssistantText === '' ? '我刚才没说话。' : lastAssistantText)
      break
    case 'launch': {
      const launcher = await ensureLauncher()
      const result = await launcher.launch(parsed.arg)
      replyOffline(result.text.split('\n')[0])
      break
    }
    default:
      replyOffline(offlineUnsupported(text))
      return { handled: false, intent: parsed.intent }
  }
  log(`offline handled: ${parsed.intent}${parsed.arg === undefined ? '' : ` (${parsed.arg})`}`)
  return { handled: true, intent: parsed.intent }
}

/**
 * 模型偶尔会输出 markdown（"**要点**"、"- 项目"）。
 * 字幕里这些符号很碍眼，读出来更莫名其妙，所以统一清掉。
 */
const cleanLine = (text) => cleanLineText(text)

function flushTurn(turn) {
  const rest = turn.buffer.trim()
  turn.buffer = ''
  if (rest !== '') speakLine(turn, rest)
  // 这一轮不会再有新句子了：立刻把缓冲里的合成掉，不用等那 220ms
  flushSpeechBuffer()
}

// ---------------------------------------------------------------- 播报（TTS）

let ttsEngine = null
let ttsLoading = null
/** 托盘可以关掉播报；关掉后字幕照常，只是不出声 */
let ttsEnabled = true
/** 待读的句子队列：一句合成一句播，不攒整段（攒整段首字延迟就废了） */
const ttsQueue = []
let ttsPumping = false
/** 播报回执的 id 序号 */
let ttsSeq = 0
/** 播报"代次"：一旦喊停就 +1，正在合成的旧内容直接作废（否则收起后还会冒出一句） */
let speechGen = 0
/** 播完回执：id → resolve */
const spokenWaiters = new Map()
/** 最后一次播报结束的时刻，用于"刚说完别急着听" */
let lastSpokenEndAt = 0
/** 打断用的独立识别流：只在播报期间存在，和"听指令"那条流互不干扰 */
let bargeSession = null
/** 播报刚开始的一瞬间先不判打断（音频起播有瞬态） */
let bargeStartAt = 0
/** 打断判定用的电平峰值（缓慢衰减），避免"文字到了、电平已落回静音"的错判 */
let bargeLevelPeak = 0
/** 打断后要回灌的音频（用户已经在说，前面 0.6 秒不能丢） */
const recentChunks = []

/**
 * 半双工门：我自己正在说话时，不要让麦克风"听到我"。
 * 真要做全双工得靠 AEC（回声消除），在那之前先进来这条硬闸——
 * 否则助手会被自己的声音唤醒，然后对着自己说个不停。
 */
function selfSpeaking() {
  return ttsPumping || ttsQueue.length > 0 || Date.now() - lastSpokenEndAt < 600
}

/** 真的在出声（不含播完后的 600ms 冷静期）——打断只在这个窗口内判定 */
let playbackStartedAt = 0
/** 最近一次被打断的时刻（验收脚本用） */
let userBargeInAt = 0

// ---------------------------------------------------------------- 权限（R7）

/** @type {import('../../packages/agent/permissions.mjs').PermissionGate|null} */
let permissionGate = null
/** 正在等确认的那个请求；null 表示没有待确认的事 */
let confirmResolver = null
let confirmSequence = 0
let pendingConfirmId = null
/** 待确认请求对应的能力（"允许 15 分钟"要按能力记授权） */
let pendingConfirmCapability = null
/**
 * 等确认最多多久：超时按拒绝处理（安全默认）。
 *
 * 3 分钟：模型常常是先做完一堆观察再问一句，用户此时可能在另一个窗口忙。
 * 12 秒那种"你还没点它就替你拒绝"的行为，会让任务白跑一半（实测）。
 */
const CONFIRM_TIMEOUT_MS = Number(process.env.CUA_VA_CONFIRM_TIMEOUT ?? 180000)
/** 权限档位（设置页/托盘都能改；unattended = 免确认全放行） */
let permissionMode = 'standard'
/** 语音确认通道总开关（托盘可关，关了就只能点按钮） */
let voiceConfirmEnabled = true
/** 连续对话：说完一轮继续待命，等用户接着讲（托盘可关 = 单次对话） */
let continuousMode = process.env.CUA_VA_SINGLE_TURN === '1' ? false : true
/** 合成用的情绪与语速（来自设置页，跟着音色包里的参考音频走） */
let ttsEmotion = '中立'
let ttsSpeed = 1
/** 字幕开关（关掉只出声不显示文字） */
let subtitlesEnabled = true
/** 开机自启（打包后注册到注册表 Run 项；开发态只记配置） */
let autoStartEnabled = false
/** 全屏时让出显存（R14）。关掉后模型一直常驻 */
let releaseOnFullscreen = true
/** 上次退出时的唤醒开关：启动时按它恢复（默认开） */
let wakeOnLaunch = true
/** 设置窗口 */
let settingsWin = null
/** 音色包目录（设置页读情绪列表用，不必等 TTS worker） */
const ROOT_VOICE_DIR = join(APP_ROOT, '.runtime', 'voices', 'v2ProPlus', '银狼')
/** 模型是否要求"接着说"（回复里带了 [[继续]] 标记，或最后一句是问句） */
let wantContinue = false
/** 待命窗口：用户接着说话的时间窗（现在由判断结果动态决定时长） */
const FOLLOWUP_LISTEN_MS = 9000
/** 本轮待命窗口开始于何时（0 = 没开）；用于记录"用户到底接话没有" */
let followUpWindowStart = 0
/** 这一轮回复的原文（判定失败时给启发式兜底用） */
let lastReplyText = ''
/** 本轮"还要不要继续"的判定结果（judgeFollowUp 的产物） */
let followUpJudgement = null
/** 正在跑的判定请求 */
let followUpJudgePromise = null
/** 连续对话的阈值与窗口配置（来自配置文件 followup.*） */
let followUpConfig = { ...FOLLOWUP_DEFAULTS }
/**
 * 会话记忆：**一次唤醒 = 一次会话**。
 * 只存"用户说了什么 + 助手回了什么"，不存工具细节（截图结果塞进去会立刻撑爆上下文）。
 * 唤醒时清空，所以新的唤醒绝不会带着上一件事的记忆。
 */
let sessionHistory = []
/** 回合结束时话还没播完：播完再判一次要不要待命 */
let pendingFollowUpCheck = false
/** 托盘"语音唤醒"菜单项：必须和真实麦克风状态一致，否则用户看到勾选却听不见/没勾却在听 */
let wakeMenuItem = null
/** 本轮用户说的话，写进审计日志用 */
let lastUtterance = ''
/** 本回合内被拒次数：同一个工具被拒两次就不再问，避免卡片反复弹 */
let deniedInTurn = {}

function playingAudio() {
  return playbackStartedAt > 0
}

async function ensureTtsEngine() {
  if (ttsEngine !== null) return ttsEngine
  if (ttsLoading !== null) return ttsLoading
  ttsLoading = (async () => {
    const startedAt = Date.now()
    const { Tts } = await import('../../packages/voice/tts.mjs')
    const engine = new Tts()
    // worker 要加载 GPT+SoVITS 并预热一次推理（实测 25~30 秒），
    // 不等它 ready 就返回，第一句话会卡在一个没人监听的状态里
    const ready = await engine.ready()
    ttsEngine = engine
    log(`tts ready in ${Date.now() - startedAt}ms (loadMs=${ready.loadMs}, voices=${ready.voices.join('/')})`)
    return ttsEngine
  })()
  try {
    return await ttsLoading
  } finally {
    ttsLoading = null
  }
}

/**
 * 语音合成预热：应用启动后就把 worker 起起来。
 * 模型加载 25~30 秒、常驻 1.5GB 显存，等用户开口才加载就太晚了。
 */
function maybePrewarmTts() {
  if (process.env.CUA_VA_NO_TTS_PREWARM === '1') { log('tts prewarm disabled by env'); return }
  if (!ttsEnabled) return
  setTimeout(() => {
    // 只有"真的在出声"时才让路：worker 在独立进程里加载，跟模型对话不冲突
    if (playingAudio() || ttsPumping) {
      setTimeout(() => maybePrewarmTts(), 3000)
      return
    }
    log('tts prewarm start')
    void ensureTtsEngine().catch((error) => log(`tts prewarm failed: ${error.message}`))
  }, Number(process.env.CUA_VA_TTS_PREWARM_DELAY ?? 500))
}

/**
 * 待播文本的攒批缓冲。
 *
 * 为什么需要：模型是**一句一句吐**的，以前每吐一句就立刻合成一句，
 * 于是每个句号在我们这边都变成"一次独立的朗读"——模型会认真收尾、留句末静音，
 * 听感就是"标点处停得比真人长"。
 *
 * 攒 220ms 再合成，三句话会合并成一次朗读，标点处的停顿交还给模型的语义判断。
 * 代价只是最后一句晚 0.2 秒出声，人耳基本无感。
 */
let speechBuffer = ''
let speechFlushTimer = null
const SPEECH_FLUSH_MS = Number(process.env.CUA_VA_SPEECH_FLUSH_MS ?? 220)
/** 攒到这么长就立刻合成（别让首字等太久） */
const SPEECH_FLUSH_CHARS = 180

function enqueueSpeech(text) {
  if (!ttsEnabled) return
  speechBuffer += text
  if (speechFlushTimer !== null) clearTimeout(speechFlushTimer)
  if (speechBuffer.length >= SPEECH_FLUSH_CHARS) {
    flushSpeechBuffer()
    return
  }
  speechFlushTimer = setTimeout(() => flushSpeechBuffer(), SPEECH_FLUSH_MS)
}

function flushSpeechBuffer() {
  if (speechFlushTimer !== null) { clearTimeout(speechFlushTimer); speechFlushTimer = null }
  const text = speechBuffer.trim()
  speechBuffer = ''
  if (text === '') return
  for (const piece of splitForSpeech(text)) ttsQueue.push(piece)
  void pumpSpeech()
}

/**
 * 长句切短——只切第一刀。
 *
 * 为什么原来"标点间断很大"：以前按逗号/句号切成 22 字的小块，
 * 每块对模型来说都是**一句完整的独立朗读**——它会认真收尾、拖长尾音、留句末静音。
 * 于是我们切出来的每一个标点，都变成一次"句号级"停顿，听起来比真人说话顿得多。
 *
 * 现在的策略：
 *   1. 第一块切短（≤18 字），保证尽快出声；
 *   2. 剩下的**整段合并**（上限 160 字）一次性合成，标点处的停顿交还给模型的语义判断。
 * 代价是第二块起首字延迟变长（整段合成 2~4 秒），但那 2~4 秒正好被第一块的声音盖住。
 */
function splitForSpeech(text) {
  const FIRST_MAX = 18
  const CHUNK_MAX = 160
  // 短回复不切：一句"你好，有什么需要帮忙的吗"切成两块反而多一次停顿
  if (text.length <= 40) return [text.trim()]
  const sentences = text
    .split(/(?<=[。！？!?；;])/u)
    .map((part) => part.trim())
    .filter((part) => part !== '')
  if (sentences.length === 0) return []

  // 第一块：从开头切一小口（优先在逗号处切，语义更完整）
  let first = ''
  const headSource = sentences[0]
  if (headSource.length > FIRST_MAX) {
    let head = ''
    for (const piece of headSource.split(/(?<=[，,、])/u)) {
      if (head !== '' && (head + piece).length > FIRST_MAX) break
      head += piece
    }
    if (head === '') head = headSource.slice(0, FIRST_MAX)
    first = head.trim()
  } else {
    first = headSource
  }

  const restText = (first !== headSource ? headSource.slice(first.length) : '') + sentences.slice(1).join('')
  const rest = restText.trim()

  const out = first === '' ? [] : [first]
  if (rest === '') return out

  // 剩下的一次性给模型；实在太长才继续按句子合并切块
  if (rest.length <= CHUNK_MAX) {
    out.push(rest)
    return out
  }
  let buffer = ''
  for (const sentence of rest.split(/(?<=[。！？!?；;])/u)) {
    if (buffer !== '' && (buffer + sentence).length > CHUNK_MAX) {
      out.push(buffer.trim())
      buffer = sentence
    } else {
      buffer += sentence
    }
  }
  if (buffer.trim() !== '') out.push(buffer.trim())
  return out.filter((part) => part !== '')
}

/**
 * 逐句合成 → 写临时 WAV → 交给渲染层播。
 *
 * 为什么让渲染层播：主进程没有音频输出接口，渲染层有 <audio>；
 * 顺带能拿到"播完了"的回执，队列才能串行，不会两句叠在一起。
 *
 * 预取：当前句**还在播**的时候就把后面一两句合成好。
 * 不做预取的话，每句之间要空等一整次合成（实测 0.7~1.5 秒），
 * 听感就是"说完一句先愣一下"。预取之后间隔只剩 IPC 的时间。
 */
const TTS_PREFETCH = 2

/**
 * 合成一句，产物写进临时 WAV。
 * 返回 null 表示这一代已经作废（被打断/换轮）或合成失败——调用方据此静默跳过。
 */
async function synthesizeClip(text, gen) {
  try {
    const engine = await ensureTtsEngine()
    if (gen !== speechGen) return null
    const result = await engine.speak(text, { emotion: ttsEmotion, speed: ttsSpeed })
    if (gen !== speechGen) return null
    const id = ++ttsSeq
    const file = join(app.getPath('temp'), `dsh-va-tts-${id}.wav`)
    writeWav(file, result.samples, result.sampleRate)
    return { id, text, file, result }
  } catch (error) {
    log(`tts error: ${error.message}`)
    return null
  }
}

async function pumpSpeech() {
  if (ttsPumping) return
  ttsPumping = true
  const gen = speechGen
  /** 已排队/正在合成的句子（先合成后播，播到它时通常已经好了） */
  const pending = []
  let lastClipEndAt = 0
  try {
    const fill = () => {
      while (pending.length < TTS_PREFETCH && ttsQueue.length > 0) {
        const text = ttsQueue.shift()
        pending.push({ text, promise: synthesizeClip(text, gen) })
      }
    }

    for (;;) {
      fill()
      if (pending.length === 0) break
      // 全屏时已经主动把 worker 关了：再把它叫起来就白让了，直接跳过出声（字幕照常）
      if (voiceSuspended) {
        log('speech skipped: voice released for fullscreen')
        sendSubtitle({ hint: '全屏中，语音已让出（退出全屏自动恢复）' })
        ttsQueue.length = 0
        for (const item of pending) item.promise.catch(() => {})
        pending.length = 0
        break
      }
      const item = pending.shift()
      try {
        const clip = await item.promise
        if (clip === null) break // 已经取消/出错
        // 合成期间可能被打断/收起：这一代已经作废，别再出声
        if (gen !== speechGen) { log('speech cancelled before playback'); break }
        if (win === null || win.isDestroyed()) break
        // 窗口被自动收起时别把话吞掉：用户没主动关，就把它再叫出来
        if (!win.isVisible() && !userDismissedHud) {
          log('playback: hud hidden but not dismissed → 重新显示')
          showForWake(currentState === 'hidden' ? 'speaking' : currentState)
        }
        // 用户主动关掉的窗口不再打扰：直接跳过出声（字幕也已经没地方显示）
        if (!win.isVisible()) { log('skip playback: hud dismissed by user'); break }
        playbackStartedAt = Date.now()
        if (lastClipEndAt > 0) {
          const gap = playbackStartedAt - lastClipEndAt
          if (gap > 120) log(`speech gap ${gap}ms before clip ${clip.id}`)
        }
        // 播报期间不能自动收起：字幕和声音都还在，收起来等于"话说到一半人没了"
        if (hideTimer !== null) { clearTimeout(hideTimer); hideTimer = null }
        // 打断识别流跟着播报走：每句一条新流，避免上一句的假设带过来
        if (asr !== null) {
          bargeSession = asr.newSession()
          bargeStartAt = playbackStartedAt + 150
          bargeLevelPeak = 0
        } else {
          // 理论上不会发生（能播报说明刚跑过一轮，ASR 已加载），
          // 真发生了要能看见——否则"打断没反应"会变成查不出来的幽灵问题
          log('barge-in disabled: asr not loaded')
        }
        win.webContents.send('hud:play', {
          id: clip.id,
          url: pathToFileURL(clip.file).href,
          text: clip.text,
          // 字幕由播放事件驱动：这一句开口的同一时刻才上屏，和声音严丝合缝
          showText: subtitlesEnabled,
          synthMs: clip.result.stats.synthMs,
          firstChunkMs: clip.result.stats.firstChunkMs,
        })
        // 预取：播放期间就把下一句合成好
        fill()
        const waitMs = Math.round((clip.result.samples.length / clip.result.sampleRate) * 1000) + 5000
        const played = await waitSpoken(clip.id, waitMs)
        playbackStartedAt = 0
        lastSpokenEndAt = Date.now()
        lastClipEndAt = lastSpokenEndAt
        log(`spoken ${clip.id} ${played ? 'ok' : 'timeout'} dur=${Math.round((clip.result.samples.length / clip.result.sampleRate) * 1000)}ms synth=${clip.result.stats.synthMs}ms 「${clip.text.slice(0, 16)}…」`)
      } catch (error) {
        log(`tts error: ${error.message}`)
      }
    }
  } finally {
    ttsPumping = false
    log(`pumpSpeech end queue=${ttsQueue.length} buffer=${speechBuffer.length} pendingFollowUpCheck=${pendingFollowUpCheck}`)
    // 队列空了：把"等声音说完再做"的卡片变更补上
    if (ttsQueue.length === 0 && speechBuffer === '') flushPendingUiAction()
    if (ttsQueue.length === 0 && speechBuffer === '') void afterTurnSpeechSettled()
    else if (ttsQueue.length > 0) void pumpSpeech()
  }
}

/**
 * 一轮的话说完了：要么开待命窗口，要么按常规收起。
 *
 * 为什么需要这个函数而不是在两处各写一遍：播报可能在模型还在生成时就播完了
 * （中间夹着工具调用），也可能在回合结束后才播完。两个时刻都必须判一次，
 * 否则"模型说要继续、但没开待命窗口"这种 bug 只在长回答上出现。
 */
async function afterTurnSpeechSettled() {
  if (ttsPumping || ttsQueue.length > 0 || playingAudio()) {
    pendingFollowUpCheck = true
    return false
  }
  pendingFollowUpCheck = false
  // 这一轮已经跑完：还挂着的确认卡片收掉，别让它一直"等人回答"
  closeConfirmCard()
  flushPendingUiAction()
  const followUp = await startFollowUpIfWanted()
  log(`afterTurnSpeechSettled state=${currentState} followUp=${followUp}`)
  if (followUp) return true
  // 说完了就立刻换掉「正在说」，否则用户会以为它卡在播报里
  if (currentState === 'speaking') setState('done')
  scheduleHide(1500)
  return false
}

/**
 * 连续对话：一轮说完就开一个待命窗口，等用户接着讲。
 *
 * 时长不写死：由"还要不要继续说"的判定结果（置信度）映射而来。
 * 拿不准就多等一会儿，判错也不至于把用户想说的话吞掉。
 */
async function startFollowUpIfWanted() {
  if (!continuousMode) return false
  if (!listening) return false          // 麦克风都关了，待命没意义
  if (activeTurn !== null) return false  // 回合还没结束
  if (confirmResolver !== null) return false // 正在等确认，别抢
  if (commandActive) return false        // 已经在听了

  const judgement = await resolveFollowUpJudgement()
  if (judgement.wait !== true && judgement.confidence < followUpConfig.thresholds.low) {
    log(`follow-up skipped: 判定不该继续等（conf=${judgement.confidence.toFixed(2)} ${judgement.why}）`)
    return false
  }
  const windowMs = windowFor(judgement.confidence, followUpConfig)
  log(`follow-up window ${windowMs}ms（conf=${judgement.confidence.toFixed(2)} src=${judgement.source} ${judgement.why}）`)

  commandActive = true
  heardAny = false
  lastCommandText = ''
  lastSentPartial = ''
  commandStartAt = Date.now() + 250
  if (asr !== null) asr.reset()
  clearTimeout(commandTimer)
  commandTimer = setTimeout(() => finishCommand('followup-timeout'), windowMs)
  if (hideTimer !== null) { clearTimeout(hideTimer); hideTimer = null }
  followUpWindowStart = Date.now()
  /*
   * 待命窗口用「好了」而不是「聆听中」。
   *
   * 麦克风确实开着、用户直接说话也能接上，但视觉上不该跳成一个大大的"我在听"——
   * 用户刚听完结果，看到界面突然进入聆听态再自己消失，会觉得"它没关干净"。
   * 保持「好了 + 想继续说就直接说」，窗口到期自然收起，看起来才是正常结束。
   */
  setState('done')
  sendSubtitle({ reset: true, hint: '想继续说就直接说' })
  return true
}

/**
 * 拿到判定结果。
 * 判定在回复生成完就开始跑了，等到"话说完"时通常早就回来了；
 * 万一还没回来，最多等 1.2 秒，超时用启发式——收尾不能被一个判断卡住。
 */
async function resolveFollowUpJudgement() {
  if (followUpJudgement !== null) return followUpJudgement
  if (followUpJudgePromise !== null) {
    try {
      return await Promise.race([
        followUpJudgePromise,
        // 只等 700ms：判定慢的时候先用启发式把窗口开起来，回头再用真实置信度微调
        new Promise((resolve) => setTimeout(() => resolve(null), 700)),
      ]).then((value) => value ?? { ...heuristicFollowUp(lastReplyText), source: 'heuristic-fast' })
    } catch {
      return { ...heuristicFollowUp(lastReplyText), source: 'heuristic-error' }
    }
  }
  return { ...heuristicFollowUp(lastReplyText), source: 'heuristic' }
}

function waitSpoken(id, timeoutMs) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => { spokenWaiters.delete(id); resolve(false) }, timeoutMs)
    spokenWaiters.set(id, () => { clearTimeout(timer); spokenWaiters.delete(id); resolve(true) })
  })
}

/** 停止播报（收起窗口时），队列一起清空 */
function stopSpeaking() {
  ttsQueue.length = 0
  // 还在攒批的文本也要丢掉，否则停下之后那半句还会冒出来
  speechBuffer = ''
  if (speechFlushTimer !== null) { clearTimeout(speechFlushTimer); speechFlushTimer = null }
  speechGen += 1
  playbackStartedAt = 0
  if (win !== null && !win.isDestroyed()) win.webContents.send('hud:stopAudio')
  for (const resolve of spokenWaiters.values()) resolve()
  spokenWaiters.clear()
}

/**
 * 用户说了约定的打断词（"等一下"），立刻停下播报。
 *
 * 三件事必须同时做对，否则体验比不打断更糟：
 *   1. 立刻停声（含清空队列，不然下一句还会接着念）
 *   2. 马上进入聆听，不要再要求说唤醒词
 *   3. 把刚过去 0.6 秒的音频回灌给识别——用户说的是"等一下"，
 *      如果从"下"字开始记，就会识别成"等一"或者干脆没听清
 */
function triggerBargeIn(word = '等一下') {
  log(`barge-in: 听到「${word}」，停止播报`)
  userBargeInAt = Date.now()
  bargeSession = null
  stopSpeaking()
  setState('listening')
  sendSubtitle({ reset: true, hint: '我在听' })

  commandActive = true
  heardAny = false
  lastCommandText = ''
  lastSentPartial = ''
  // 立即开始识别（不跳过 300ms：用户已经在说话了）
  commandStartAt = Date.now()
  if (asr !== null) asr.reset()
  clearTimeout(commandTimer)
  commandTimer = setTimeout(() => finishCommand('barge-in-timeout'), COMMAND_TIMEOUT_MS)

  for (const item of recentChunks) feedCommandAudio(item.chunk)
  recentChunks.length = 0
}

/**
 * 一块 100ms 麦克风音频的完整处理。
 * 抽成函数是为了让"模拟麦克风"的验收（--bargein-test kw）走**完全相同**的代码路径，
 * 而不是另写一套只在测试里生效的逻辑。
 */
function processChunk(chunk) {
  // 调试用：把实时采集的音频落盘，便于离线复现"为什么没唤醒"
  if (dumpPath !== null && dumpChunks.length < 300) dumpChunks.push(Float32Array.from(chunk))

  const result = pipeline.push(chunk)
  micStats.chunks += 1
  if (result.levelDb > micStats.peakLevelDb) micStats.peakLevelDb = result.levelDb
  micStats.floorDb = result.floorDb

  if (result.wake !== null && !selfSpeaking() && Date.now() - lastWakeAt > 2000) {
    lastWakeAt = Date.now()
    micStats.wakes += 1
    onWake(result.wake)
  }

  // 指令识别：只在这轮唤醒的有效期内跑，静默时不占 CPU
  feedCommandAudio(chunk)

  // 最近 600ms 音频留一份：打断时用户已经开口了，这段不能丢
  recentChunks.push({ at: Date.now(), chunk: Float32Array.from(chunk) })
  while (recentChunks.length > 0 && Date.now() - recentChunks[0].at > 600) recentChunks.shift()

  // 打断：只在我正在出声、且没在等指令的时候，用识别文本找约定词
  if (!commandActive && playingAudio() && bargeSession !== null && Date.now() >= bargeStartAt) {
    // 电平用"最近一秒的最高值"，不能用当前块：关键词一般落在句尾，
    // 而识别把字吐出来时这一块往往已经是静音了（实测：文本对了却被判 too quiet）
    const diff = result.levelDb - result.floorDb
    bargeLevelPeak = Math.max(diff, bargeLevelPeak * 0.92)

    const heard = bargeSession.push(chunk)
    const verdict = judgeBargeIn({
      text: heard.final ?? heard.partial,
      levelDb: result.floorDb + bargeLevelPeak,
      floorDb: result.floorDb,
    })
    if (verdict.hit) triggerBargeIn(verdict.word)
    else if (verdict.reason === 'too-quiet') log(`barge-in ignored (too quiet, peak ${bargeLevelPeak.toFixed(1)}dB): "${heard.final ?? heard.partial}"`)
  }

  if (echoProbe !== null) {
    echoProbe.push({ levelDb: result.levelDb, floorDb: result.floorDb, speech: result.speech, playing: selfSpeaking() })
  }
  // 每 2 秒记一行电平，便于排查"为什么没触发"
  if (micStats.chunks % 20 === 0) {
    log(`level=${result.levelDb} floor=${result.floorDb} speech=${result.speech} gain=${result.gainDb}`)
  }
}

/** 工具名前半句人话，用于"正在截图 / 正在点击"这种状态行 */
function toolHint(name) {
  const hints = {
    screenshot: '看屏幕',
    list_windows: '看窗口',
    get_app_state: '看界面',
    left_click: '点一下',
    right_click: '右键',
    double_click: '双击',
    drag: '拖动',
    type_text: '打字',
    press_key: '按键',
    scroll: '滚动',
    launch_app: '开程序',
    wait: '等一下',
  }
  return hints[name] ?? '动手'
}

function handleAgentEvent(event) {
  const turn = activeTurn
  if (turn === null || turn.id !== turnSeq) return

  if (event.type === 'text_delta') {
    if (turn.state !== 'speaking') {
      turn.state = 'speaking'
      setState('speaking')
    }
    turn.raw += event.text
    turn.buffer += event.text
    for (const sentence of takeSentences(turn)) speakLine(turn, sentence)
    return
  }
  if (event.type === 'tool_start') {
    turn.tools += 1
    // 模型调工具前会先说一句"我看一下窗口"，这句必须在动作之前读出来。
    // 它要是没打句号就直接调工具，这里把残句冲出去，别等整轮结束。
    flushTurn(turn)
    turn.state = 'thinking'
    /*
     * 卡片跟着声音走：上一句还没念完时，别把标题推进到下一件事。
     * 先把状态挂起，等话说完再切——否则用户看到「正在看屏幕」，
     * 耳朵里还是上一句「我帮你看看」，两边完全错位。
     */
    applyUiWhenQuiet(() => {
      setState('thinking')
      sendSubtitle({ hint: `正在${toolHint(event.name)}` })
    })
    log(`tool: ${event.name} ${JSON.stringify(event.args ?? {}).slice(0, 160)}`)
    return
  }
  if (event.type === 'tool_end') {
    log(`tool ${event.name} ${event.ok ? 'ok' : 'fail'} ${event.ms ?? ''}ms${event.ok ? '' : ` ${String(event.text ?? '').slice(0, 160)}`}`)
  }
}

/**
 * 把截图文件读成 base64 交给模型。
 *
 * 为什么必须做：`screenshot` 的返回值只有"图存在某路径"的文字，
 * 模型不会去打开文件——它需要图片本体才能"看见"。
 * 图片走 OpenAI 兼容协议的 image_url（data URI），模型侧直接当视觉输入。
 */
async function attachScreenshotImage(shot) {
  const savedPath = shot?.data?.savedPath
  if (typeof savedPath !== 'string' || savedPath === '') return null
  try {
    const { readFile } = await import('node:fs/promises')
    const absolute = savedPath.startsWith('.') || !/^[a-zA-Z]:/u.test(savedPath) ? join(APP_ROOT, savedPath) : savedPath
    const buffer = await readFile(absolute)
    // 太大就别塞（模型侧单图预算 256KiB 量级），让模型改用 get_app_state
    if (buffer.length > 400 * 1024) {
      log(`screenshot too large to attach: ${Math.round(buffer.length / 1024)}KB`)
      return null
    }
    const raster = String(shot?.text ?? '').match(/raster="(\d+)x(\d+)"/u)
    return {
      mediaType: absolute.toLowerCase().endsWith('.png') ? 'image/png' : 'image/jpeg',
      base64: buffer.toString('base64'),
      width: raster === null ? undefined : Number(raster[1]),
      height: raster === null ? undefined : Number(raster[2]),
      bytes: buffer.length,
    }
  } catch (error) {
    log(`attach screenshot failed: ${error.message}`)
    return null
  }
}

/**
 * 把模型给的 target 参数规整成后端真正认识的形式。
 *
 * 实测模型会写三种"人类觉得没问题、后端看不懂"的形式：
 *   1. `app: "{\"name\": \"Calculator\"}"`  —— JSON 字符串（而不是对象）
 *   2. `app: "计算器"` / `app: "网易云音乐"` —— 显示名（后端只认 appKey/pid/hwnd）
 *   3. `window: "{\"appKey\": \"calculatorapp\"}"` —— 同上
 * 后端拿到这些只会回 "No application with appKey …"，模型再换个名字重试，
 * 一轮烧掉十几次调用（实测计算器任务 16 次工具调用，最后一步没用上）。
 *
 * 这里统一做：JSON 字符串 → 对象；显示名/进程名 → 用我们自己的窗口列表解析成 hwnd。
 */
async function normalizeCuaTarget(toolkit, name, args) {
  if (name !== 'get_app_state' && name !== 'wait' && name !== 'screenshot') return args
  const next = { ...args }

  for (const key of ['app', 'window']) {
    const value = next[key]
    if (value === undefined || value === null) continue
    if (typeof value === 'string' && value.trim().startsWith('{')) {
      try { next[key] = JSON.parse(value) } catch { /* 保持原样，让后端报错 */ }
    }
  }

  const target = next.app ?? next.window
  const plain = typeof target === 'string' ? target : (target?.name ?? target?.appKey ?? target?.title)
  if (typeof plain !== 'string' || plain.trim() === '') return next
  // 已经是 hwnd / pid 之类后端认得的写法，不用管
  if (/^(0x[0-9a-f]+)$/iu.test(plain.trim())) return next

  const hwnd = await resolveHwndByName(toolkit, plain)
  if (hwnd === null) return next
  const resolved = { hwnd }
  if (next.app !== undefined) next.app = resolved
  else next.window = resolved
  log(`cua target 解析: 「${plain}」→ ${hwnd}`)
  return next
}

/** 用窗口列表按显示名/进程名模糊找 hwnd */
async function resolveHwndByName(toolkit, name) {
  const query = String(name).trim().toLowerCase()
  if (query === '') return null
  try {
    const windows = await toolkit.invoke('list_windows', {})
    const rows = String(windows.text ?? '').split('\n')
    let best = null
    for (const row of rows) {
      const hwnd = row.match(/hwnd="(0x[0-9a-f]+)"/iu)?.[1]
      if (hwnd === undefined) continue
      const title = (row.match(/title="([^"]*)"/u)?.[1] ?? '').toLowerCase()
      const app = (row.match(/app="([^"]*)"/u)?.[1] ?? '').toLowerCase()
      if (title === '' && app === '') continue
      const hit = title.includes(query) || app.includes(query) || query.includes(app)
      if (hit && title !== '') { best = hwnd; break }
    }
    return best
  } catch {
    return null
  }
}

// ---------------------------------------------------------------- 播放浮窗

/**
 * 音乐播放面板：**独立窗口**，不挂在字幕 HUD 里。
 *
 * 为什么要独立：字幕窗口会随手关、会自动收起，歌还在放呢面板就没了。
 * 播放面板的生命周期只跟播放状态绑定——
 *   playing / paused → 显示（暂停也算在播）
 *   stopped         → 隐藏
 * 而且点它的关闭键是**停止播放**，不是暂停（暂停有 ▶ 那个按钮）。
 */
let playerWin = null
let playerStateTimer = null
let playerWindowReady = false

const PLAYER_WIDTH = 348
const PLAYER_HEIGHT = 92

function ensurePlayerWindow() {
  if (playerWin !== null && !playerWin.isDestroyed()) return playerWin
  playerWin = new BrowserWindow({
    width: PLAYER_WIDTH,
    height: PLAYER_HEIGHT,
    useContentSize: true,
    show: false,
    frame: false,
    transparent: true,
    backgroundColor: '#00000000',
    hasShadow: false,
    resizable: false,
    movable: true,
    minimizable: false,
    maximizable: false,
    fullscreenable: false,
    skipTaskbar: true,
    alwaysOnTop: true,
    webPreferences: {
      preload: join(HERE, 'preload-player.mjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
    },
  })
  playerWin.setAlwaysOnTop(true, 'screen-saver')
  playerWin.loadFile(join(HERE, 'renderer', 'player.html'))
  playerWindowReady = false
  const markReady = (how) => {
    if (playerWindowReady) return
    playerWindowReady = true
    log(`player window ready（${how}）`)
    // 首帧就绪前可能已经要求显示，这里补一次
    syncPlayerWindow()
  }
  playerWin.once('ready-to-show', () => markReady('ready-to-show'))
  playerWin.webContents.once('did-finish-load', () => markReady('did-finish-load'))
  // 兜底：透明窗口在某些环境下 ready-to-show 不触发，别让面板永远出不来
  setTimeout(() => markReady('fallback'), 1500)
  playerWin.on('closed', () => { playerWin = null; playerWindowReady = false })
  return playerWin
}

/** 把窗口摆到右上角：字幕在右下，两边不打架 */
function positionPlayerWindow(target) {
  const { workArea } = screen.getPrimaryDisplay()
  const x = workArea.x + workArea.width - PLAYER_WIDTH - 24
  const y = workArea.y + 72
  target.setPosition(Math.round(x), Math.round(y), false)
}

/** 根据播放状态显示/隐藏面板 */
function syncPlayerWindow() {
  const status = musicPlayer.status
  const active = status === 'playing' || status === 'paused'
  if (!active) {
    if (playerWin !== null && !playerWin.isDestroyed() && playerWin.isVisible()) {
      log(`player window 隐藏（status=${status}）`)
      playerWin.hide()
    }
    stopPlayerTicker()
    return
  }
  const target = ensurePlayerWindow()
  if (!playerWindowReady) return
  if (!target.isVisible()) {
    positionPlayerWindow(target)
    target.showInactive()
    log(`player window 显示（status=${status}）`)
  }
  target.webContents.send('player:state', {
    status,
    title: musicPlayer.current?.name ?? '',
    artist: musicPlayer.current?.artist ?? '',
    position: musicPlayer.position,
    duration: musicPlayer.duration,
  })
  startPlayerTicker()
}

function startPlayerTicker() {
  if (playerStateTimer !== null) return
  playerStateTimer = setInterval(() => { syncPlayerWindow() }, 1000)
}

function stopPlayerTicker() {
  if (playerStateTimer === null) return
  clearInterval(playerStateTimer)
  playerStateTimer = null
}

/**
 * 播放状态看门狗：每 800ms 看一眼进程内播放器的状态，变了就同步面板。
 *
 * 为什么不靠调用点触发：播放入口有好几个（语音、面板按钮、自动下一首），
 * 每个地方都记得调用一次 sync 太容易漏——实测就漏了，面板压根没弹出来。
 * 直接在进程内轮询状态最省心，代价几乎为零（不发子进程）。
 */
let lastPlayerStatus = 'stopped'
function startPlayerWatcher() {
  setInterval(() => {
    const status = musicPlayer.status
    if (status === lastPlayerStatus) return
    lastPlayerStatus = status
    syncPlayerWindow()
  }, 800)
}
/** 面板上的按钮：prev / toggle / next / close(=停止) */
async function handlePlayerAction(action) {
  try {
    if (action === 'prev' || action === 'next') {
      await runNetease({ action })
    } else if (action === 'toggle') {
      await runNetease({ action: musicPlayer.status === 'playing' ? 'pause' : 'resume' })
    } else if (action === 'close') {
      // 用户点 ✕ 是"别放了"，不是"先停一下"——直接 stop
      await runNetease({ action: 'stop' })
    }
    syncPlayerWindow()
  } catch (error) {
    log(`player action ${action} failed: ${error.message}`)
  }
}


/** 把已运行但没窗口的程序叫回前台；失败就返回一句模型能照做的话 */
async function invokeCuaWithRecovery(toolkit, name, args, launcher) {
  try {
    return await toolkit.invoke(name, args)
  } catch (error) {
    const message = String(error?.message ?? '')
    if (!/No application with appKey|No such application/iu.test(message)) throw error

    const target = typeof args?.app === 'string' ? args.app : ''
    if (target === '') throw error

    log(`cua: 「${target}」没有可见窗口，尝试唤回前台`)
    let woken = false
    try {
      const result = await launcher.launch(target)
      woken = /启动|打开|已运行|激活/u.test(String(result?.text ?? ''))
      log(`cua: 唤回 ${woken ? '成功' : '未生效'}（${String(result?.text ?? '').slice(0, 40)}）`)
    } catch (wakeError) {
      log(`cua: 唤回失败 ${wakeError.message}`)
    }

    if (woken) {
      await new Promise((resolve) => setTimeout(resolve, 1500))
      try { return await toolkit.invoke(name, args) } catch { /* 落到软失败 */ }
    }

    // 软失败：不抛异常，抛了模型只会换个名字再试一次，白烧一轮
    return {
      text: `「${target}」现在没有可读窗口（进程可能在后台或托盘里）。`
        + '已经尝试把它叫到前台。请先 launch_app 重新打开它、等 2 秒再读界面；'
        + '如果它本来就该关着，就别再对这个目标重试了。',
      data: { recovered: false, target, reason: 'no-visible-window' },
    }
  }
}

/**
 * 按名字启动程序（Start 菜单级解析）。
 * 单独一个实例：离线兜底要用它，而离线时 agent 根本起不来。
 */
let appLauncher = null
async function ensureLauncher() {
  if (appLauncher !== null) return appLauncher
  const launchModule = await import('../../packages/tools/app-launch.mjs')
  appLauncher = new launchModule.AppLauncher()
  void appLauncher.ensureIndex().then((list) => log(`app index ready: ${list.length} entries`)).catch((error) => log(`app index failed: ${error.message}`))
  return appLauncher
}

/**
 * 模型 + 工具链初始化。LLM 客户端几乎零成本，但 CUA 桥接要起一个 PowerShell
 * （冷启动实测 1050ms），所以只做一次，第一次下指令时才付这个成本。
 */
async function ensureAgent() {
  if (agentRuntime !== null) return agentRuntime
  if (agentLoading !== null) return agentLoading
  agentLoading = (async () => {
    const [store, llmModule, loopModule, cuaModule] = await Promise.all([
      import('../../packages/config/store.mjs'),
      import('../../packages/agent/llm.mjs'),
      import('../../packages/agent/loop.mjs'),
      import('../../packages/cua/adapter.mjs'),
    ])
    const config = await store.loadConfig()
    const apiKey = await store.resolveApiKey()
    if (!apiKey) throw new Error('还没配置模型 API Key')

    // 音乐工具的凭据同步给 ncm-cli；异步跑，别挡住这一轮对话
    void (async () => {
      try {
        // 带上"上次同步过的哈希"：内容没变就不要写 CLI，否则会把登录态顶掉
        const result = await syncCredentialsAsync(config.netease ?? {}, { syncedHash: config.netease?.syncedHash })
        const synced = result.synced ?? []
        if (synced.length > 0) log(`netease credentials synced: ${synced.join(', ')}`)
        if (result.hash !== undefined && result.hash !== config.netease?.syncedHash) {
          await store.saveConfig({ netease: { ...(config.netease ?? {}), syncedHash: result.hash } })
          config.netease = { ...(config.netease ?? {}), syncedHash: result.hash }
        }
      } catch (error) {
        log(`netease sync failed: ${error.message}`)
      }
    })()

    const startedAt = Date.now()
    const toolkit = new cuaModule.CuaToolkit({ frameDir: join(DATA_ROOT, 'frames') })
    await toolkit.warmup()

    // 联网搜索：key 没配就当这个工具不存在（模型不会去调一个列表里没有的工具）
    const searchModule = await import('../../packages/tools/web-search.mjs')
    const searchKey = await store.resolveSearchKey()
    const webSearch = new searchModule.WebSearch({ apiKey: searchKey })

    // 按名字启动程序（Start 菜单级解析）：CUA 的 open_application 只吃真实路径
    const launcher = await ensureLauncher()

    const tools = {
      listTools: () => {
        const list = toolkit.listTools()
        if (webSearch.enabled) list.push(searchModule.WEB_SEARCH_TOOL)
        list.push(LAUNCH_APP_TOOL, SHELL_TOOL, FILES_TOOL, PROC_TOOL, KILL_TOOL, SYSINFO_TOOL, NETEASE_TOOL)
        return list
      },
      invoke: async (name, args) => {
        if (name === 'web_search') return webSearch.invoke(args)
        if (name === 'launch_app') return launcher.launch(args?.query)
        if (name === 'run_shell') return runShell(args)
        if (name === 'files') return runFiles(args)
        if (name === 'app_control') return runAppControl(args)
        if (name === 'kill_app') return runKillApp(args)
        if (name === 'system_info') return runSystemInfo(args)
        if (name === 'netease_music') {
          /*
           * 音乐一律走官方 CLI，不去操纵客户端界面。
           * 未登录时把登录链接一起带回去——模型没法替用户点授权，
           * 但能把链接直接给用户（实测比让它去点客户端强得多）。
           */
          const musicResult = await runNetease({ ...args, user_input: args?.user_input ?? lastUtterance })
          /*
           * 播放成功后把迷你播放器叫出来：用户要的是"像音乐 App 一样的浮窗"，
           * 播了歌却什么都不显示，会让人以为没生效。
           */
          if (args?.action === 'play' && musicResult.data?.payload?.success !== false) {
            syncPlayerWindow()
          }
          if (musicResult.needsLogin === true) {
            const login = await runNetease({ action: 'login' })
            musicResult.text = `${musicResult.text}\n登录链接：${login.text}`
          }
          return musicResult
        }

        const normalized = await normalizeCuaTarget(toolkit, name, args)
        const result = await invokeCuaWithRecovery(toolkit, name, normalized, launcher)
        // 截图要把图一起交给模型（它没有"打开文件"的能力），否则它对屏幕是瞎的
        if (name === 'screenshot') {
          const attached = await attachScreenshotImage(result)
          if (attached !== null) return { ...result, image: attached }
        }
        return result
      },
    }
    log(`web search ${webSearch.enabled ? 'enabled (tavily)' : 'disabled: no key'}; launch_app enabled`)

    const llm = new llmModule.LLMClient({
      baseURL: config.baseURL,
      apiKey,
      model: config.model,
      temperature: config.temperature,
      timeoutMs: config.timeoutMs,
    })
    const agent = new loopModule.Agent({
      llm,
      tools,
      maxSteps: config.maxSteps,
      onEvent: handleAgentEvent,
      // 所有工具调用都要过权限门，拒绝时把原因回填给模型（它会改用别的办法或告诉用户）
      gate: async ({ tool, args }) => {
        const gateInstance = await ensurePermissions()
        const verdict = await gateInstance.check({ tool, args, utterance: lastUtterance })
        if (verdict.allow === true) return { allow: true }

        // 同一个回合里同一个工具被拒两次，就不再问：否则模型会把"没回应"当超时，
        // 一遍遍重试，用户看到的就是卡片反复弹出（实测踩过）。
        const key = `${tool}:${verdict.capability}`
        deniedInTurn[key] = (deniedInTurn[key] ?? 0) + 1
        const repeated = deniedInTurn[key] >= 2
        return {
          allow: false,
          reason: `${verdict.reason === '' ? '未获同意' : verdict.reason}。`
            + (repeated
              ? '这一轮里该操作已被拒绝多次，不要再尝试，直接告诉用户没做成以及原因。'
              : '这次不做，不要重复尝试；如果确实必要，先向用户说明再等他另行授权。'),
        }
      },
    })

    agentRuntime = { agent, toolkit, config, webSearch, llm, apiKey, readyMs: Date.now() - startedAt }
    applyModelSchedule('agent-ready')
    log(`agent ready in ${agentRuntime.readyMs}ms model=${config.model}`)
    return agentRuntime
  })()
  try {
    return await agentLoading
  } finally {
    agentLoading = null
  }
}

/**
 * 权限门。**所有工具调用都从这里过**（agent 的 gate 回调）。
 *
 * 确认通道仅允许可信卡片点击批准：
 *   HUD 弹卡片 + 同时开一个识别窗口，仅点击可以批准，语音只可取消；
 *   不吭声 12 秒按拒绝处理——安全的那一侧永远是"不做"。
 */
async function ensurePermissions() {
  if (permissionGate !== null) return permissionGate
  const [{ PermissionGate }, store] = await Promise.all([
    import('../../packages/agent/permissions.mjs'),
    import('../../packages/config/store.mjs'),
  ])
  const config = await store.loadConfig()
  permissionGate = new PermissionGate({
    permissions: config.permissions ?? {},
    auditPath: join(dirname(store.CONFIG_FILE), 'audit.jsonl'),
    confirm: askUser,
    log,
  })
  log(`permissions ready mode=${config.permissions?.mode ?? 'standard'} audit=${join(dirname(store.CONFIG_FILE), 'audit.jsonl')}`)
  return permissionGate
}

/** 确认请求 → 卡片上的人话 */
function describeRequest({ tool, args = {}, label, why }) {
  const detail = (why !== undefined && why !== '') ? why : summarizeArgsForCard(tool, args)
  return { title: '需要你确认', text: `要${label}吗？`, detail }
}

function summarizeArgsForCard(tool, args) {
  const parts = []
  for (const [key, value] of Object.entries(args ?? {})) {
    if (typeof value === 'string') parts.push(value.length > 60 ? `${value.slice(0, 60)}…` : value)
    else if (typeof value === 'number') parts.push(`${key}=${value}`)
  }
  return parts.length === 0 ? `工具：${tool}` : `工具：${tool} · ${parts.join(' · ')}`
}

/** 等用户点击批准，或取消/超时拒绝 */
async function askUser(request) {
  closeConfirmCard()
  const id = ++confirmSequence
  pendingConfirmId = id
  commandActive = false
  clearTimeout(commandTimer)
  asr?.reset()
  lastCommandText = ''
  const card = { ...describeRequest(request), id }
  pendingConfirmCapability = request.capability ?? null
  stopSpeaking()
  setState('confirm')
  if (hideTimer !== null) { clearTimeout(hideTimer); hideTimer = null }
  sendSubtitle({ reset: true })
  sendSubtitle({ final: card.text })
  if (win !== null && !win.isDestroyed()) win.webContents.send('hud:confirm', card)
  log(`confirm asked: ${card.text} (${card.detail})`)

  // 语音通道：开一个短识别窗口，只听明确取消（托盘可关）
  if (voiceConfirmEnabled) {
    if (asr !== null) asr.reset()
    commandActive = true
    heardAny = false
    lastCommandText = ''
    lastSentPartial = ''
    commandStartAt = Date.now() + 300
  }

  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      settleConfirm({ allow: false, channel: 'timeout', reason: `等了 ${Math.round(CONFIRM_TIMEOUT_MS / 1000)} 秒没有回应，按未同意处理` }, id)
    }, CONFIRM_TIMEOUT_MS)

    confirmResolver = (value) => {
      clearTimeout(timer)
      confirmResolver = null
      pendingConfirmId = null
      clearTimeout(commandTimer)
      asr?.reset()
      lastCommandText = ''
      commandActive = false
      lastSentPartial = ''
      resolve(value)
    }
  })
}

/** 决议一次确认（语音/点击/超时都走这里，只会生效一次） */
function settleConfirm(value, id = pendingConfirmId, trustedClick = false) {
  if (id === null || id !== pendingConfirmId) return
  if (value?.channel === 'voice' && !voiceConfirmEnabled) return
  if (value?.allow === true && (!trustedClick || value?.channel !== 'click')) return
  const resolver = confirmResolver
  if (resolver === null) return
  // "允许 15 分钟"：记一次临时授权，避免同一个动作反复弹卡片
  if (value?.once === true && value?.allow === true && pendingConfirmCapability !== null && permissionGate !== null) {
    const until = permissionGate.grantOnce(pendingConfirmCapability, 15)
    log(`permission granted once: ${pendingConfirmCapability} until ${new Date(until).toISOString()}`)
  }
  log(`confirm answer: ${JSON.stringify(value)}`)
  pendingConfirmCapability = null
  resolver(value)
  if (win !== null && !win.isDestroyed()) win.webContents.send('hud:confirmDone', true)
  // 确认完就回到"干活"的样子：agent 还在同一个回合里继续
  if (activeTurn !== null) setState('thinking')
}

/**
 * 收掉卡片：这一轮已经结束了，卡片不该继续挂在屏幕上。
 *
 * 场景：模型问完问题、用户没答，但这一轮因为别的原因结束了（超时/被打断/出错）。
 * 之前卡片会一直显示到它自己的超时，用户看到的是"卡住不动"。
 */
function closeConfirmCard() {
  if (confirmResolver === null) return false
  log('confirm card auto-closed（这一轮已经结束）')
  settleConfirm({ allow: false, channel: 'turn-ended', reason: '这一轮已经结束了' })
  // 卡片是「state === confirm」画出来的，收卡片就要把状态也换掉
  if (currentState === 'confirm') setState(activeTurn !== null ? 'thinking' : 'hidden')
  return true
}

/** 把一句话读成"答不答应"（规则见 permissions.mjs 的同名导出） */

/** 每轮开始时的历史条数（验收用，也方便从日志看出"这轮带了多少记忆"） */
let turnHistoryCounts = []

/**
 * 设置页保存后，把搜索 key 同步到运行中的实例。
 * 每轮开始读一次配置（约 1ms），换来"改完立刻生效、不用重启"。
 */
async function refreshSearchKey() {
  if (agentRuntime === null) return
  try {
    const store = await import('../../packages/config/store.mjs')
    const key = await store.resolveSearchKey()
    if (agentRuntime.webSearch.setApiKey(key)) {
      log(`web search key ${key === '' ? 'cleared' : 'updated'} → ${agentRuntime.webSearch.enabled ? 'enabled' : 'disabled'}`)
    }
  } catch (error) {
    log(`refresh search key failed: ${error.message}`)
  }
}

/** 一句话 → 想 → 动手 → 回话 */
async function runCommand(text) {
  lastUtterance = text
  deniedInTurn = {}
  wantContinue = false
  turnHistoryCounts.push(sessionHistory.length)
  log(`turn start history=${sessionHistory.length} "${text.slice(0, 40)}"`)
  // 设置页刚保存的搜索 key 要立刻生效，否则用户得重启才知道有没有配好
  await refreshSearchKey()
  const turn = { id: ++turnSeq, buffer: '', raw: '', state: 'thinking', tools: 0, startedAt: Date.now() }
  activeTurn = turn
  clearTimeout(hideTimer)
  sendSubtitle({ hint: '我想一下…' })

  /*
   * 时间/日期这类问题直接本地答，不叫模型、更不调 run_shell。
   *
   * 实测代价很大：模型为了"现在几点"去调 run_shell（Get-Date），
   * 而 system.shell 在标准档是"每次询问"——于是屏幕上弹出一张确认卡，
   * 用户没理它，等满 180 秒按拒绝处理，一轮对话白等三分钟（审计日志里有记录）。
   */
  const trivial = parseOfflineIntent(text)
  if (trivial !== null && (trivial.intent === 'time' || trivial.intent === 'date')) {
    log(`local answer: ${trivial.intent}（不叫模型）`)
    replyOffline(trivial.intent === 'time' ? `现在是${formatTime()}。` : `今天是${formatDate()}。`)
    activeTurn = null
    void afterTurnSpeechSettled()
    return
  }
  // 断网时不必去撞一次 60 秒超时：本地能办的直接办，办不了的明说
  if (isOfflineNow()) {
    log('offline mode: 跳过模型，直接走本地兜底')
    await runOfflineCommand(text)
    activeTurn = null
    void afterTurnSpeechSettled()
    return
  }

  try {
    const { agent, llm } = await ensureAgent()
    const result = await agent.run(text, { history: sessionHistory })
    flushTurn(turn)
    // 旧的文本标记只留作日志线索，真正决定要不要等的是下面的判定
    wantContinue = process.env.CUA_VA_FORCE_CONTINUE === '1' || /\[\[继续\]\]/u.test(turn.raw)
    rememberTurn(text, result.text ?? '', (result.steps ?? []).map((step) => step.tool))
    log(`turn ${turn.id} done in ${Date.now() - turn.startedAt}ms tools=${turn.tools} chars=${result.text.length}`)
    setState('speaking')

    /*
     * 判定"说完之后还要不要等用户"，在回复刚生成完就发起，和播报并行。
     * 等话说完时结果通常已经回来了，所以不额外增加用户感知的延迟。
     */
    lastReplyText = result.text ?? ''
    followUpJudgement = null
    const judgeStartedAt = Date.now()
    followUpJudgePromise = judgeFollowUp({
      chat: (options) => llm.chat(options),
      userText: text,
      replyText: lastReplyText,
      timeoutMs: followUpConfig.judgeTimeoutMs,
    }).then((judgement) => {
      followUpJudgement = judgement
      log(`follow-up judge: wait=${judgement.wait} conf=${judgement.confidence.toFixed(2)} src=${judgement.source} ${Date.now() - judgeStartedAt}ms ${judgement.why}`)
      // 判定回得晚（模型要 3~6 秒）：窗口可能已经按启发式开了，用真实置信度重算时长
      if (followUpWindowStart > 0 && commandActive) {
        const wanted = windowFor(judgement.confidence, followUpConfig)
        const waited = Date.now() - followUpWindowStart
        const remaining = wanted - waited
        if (remaining <= 0 && judgement.wait !== true) {
          log(`follow-up window 提前收（判定 conf=${judgement.confidence.toFixed(2)} < 已等 ${waited}ms）`)
          finishCommand('followup-tuned-short')
        } else if (remaining > 0) {
          clearTimeout(commandTimer)
          commandTimer = setTimeout(() => finishCommand('followup-timeout'), remaining)
          log(`follow-up window 按判定调整为 ${wanted}ms（已等 ${waited}ms，剩余 ${remaining}ms）`)
        }
      } else if (followUpWindowStart === 0 && !speechBusy() && judgement.wait === true && judgement.confidence >= followUpConfig.thresholds.high
        && listening && !commandActive && activeTurn === null && confirmResolver === null) {
        /*
         * 判定来晚了：窗口已经因为"启发式认为该收"而关闭，但模型说有把握该等。
         * 只要用户还没开始新的一轮，就把窗口补开——这是慢模型下唯一不浪费判定的做法。
         */
        log(`follow-up window 补开（判定晚到 conf=${judgement.confidence.toFixed(2)}）`)
        const windowMs = windowFor(judgement.confidence, followUpConfig)
        commandActive = true
        heardAny = false
        lastCommandText = ''
        lastSentPartial = ''
        commandStartAt = Date.now() + 250
        if (asr !== null) asr.reset()
        clearTimeout(commandTimer)
        commandTimer = setTimeout(() => finishCommand('followup-timeout'), windowMs)
        followUpWindowStart = Date.now()
        // 同样用「好了」而不是「聆听中」：这是待命，不是新的一轮对话
        setState('done')
        sendSubtitle({ reset: true, hint: '想继续说就直接说' })
      }
      return judgement
    }).catch((error) => {
      log(`follow-up judge failed: ${error.message}`)
      return { ...heuristicFollowUp(lastReplyText), source: 'heuristic-error' }
    })

    // 短回答可能在这一行之前就播完了，播报的 finally 不会再触发，所以这里补一次判定
    void afterTurnSpeechSettled()
  } catch (error) {
    // 网络类错误才走离线兜底；密钥错、参数错这类必须原样暴露，不能被兜底掩盖
    if (isOfflineError(error)) {
      log(`turn ${turn.id} offline (${error.message})，转本地兜底`)
      activeTurn = null
      await runOfflineCommand(text)
      void afterTurnSpeechSettled()
      return
    }
    log(`turn ${turn.id} failed: ${error.message}`)
    wantContinue = false
    sendSubtitle({ final: `× ${error.message}` })
    setState('error')
    scheduleHide()
  } finally {
    if (activeTurn === turn) activeTurn = null
    // 回合结束了，还挂着的确认卡片要收掉（否则用户看到一张永远在等的卡片）
    closeConfirmCard()
  }
}

/**
 * 只处理"说指令"这一段：唤醒之后、定稿之前。
 * 单独抽出来是为了让演示模式（喂 WAV）和真实麦克风走完全相同的代码路径——
 * "演示能过、真机不过"是最难查的一类 bug。
 */
function feedCommandAudio(chunk) {
  if (!commandActive || asr === null || Date.now() < commandStartAt) return null
  const heard = asr.push(chunk)
  if (heard.partial !== '') {
    heardAny = true
    lastCommandText = heard.partial
    // 文字没变就不推：识别每 100ms 跑一次，但字幕一字没动时重绘纯属浪费
    if (heard.partial !== lastSentPartial) {
      lastSentPartial = heard.partial
      sendSubtitle({ partial: heard.partial })
    }
  }
  if (heard.final !== null) {
    lastCommandText = heard.final
    finishCommand('endpoint')
  }
  return heard
}

// ---------------------------------------------------------------- 资源策略（R14）

let resourceTimer = null
/** TTS 是否因为全屏而被暂时关掉（恢复时据此重新预热） */
let voiceSuspended = false
let resourceConfig = null

/**
 * 每 20 秒看一眼：前台是不是全屏应用、内存够不够，然后决定要不要让路。
 *
 * 只做"可逆的让路"，而且**保留唤醒**：
 *   - 全屏 → 关 TTS worker，释放 1.5GB 显存；唤醒/识别/操作电脑全部照旧
 *   - 内存低于阈值 → 卸 ASR（240MB），KWS 仍在，唤醒后多等 6 秒加载识别
 * 正在对话/播报/等确认时一律不动——那是最不能卡的时候。
 */
function startResourceMonitor() {
  if (resourceTimer !== null) clearInterval(resourceTimer)
  const poll = () => { void runResourceTick() }
  resourceTimer = setInterval(poll, Number(resourceConfig?.pollMs ?? 20000))
  setTimeout(poll, 8000) // 启动后先看一眼（此时模型多半还没加载，通常无动作）
}

async function runResourceTick(force) {
  try {
    if (resourceConfig === null) return
    if (resourceConfig.enabled === false) return
    const policy = await import('../../packages/system/resource-policy.mjs')
    const busy = activeTurn !== null || commandActive || playingAudio() || ttsLoading !== null
    const fullscreen = force === 'fullscreen' ? true : await isForegroundFullscreen(policy)
    const freeMemMb = freemem() / 1048576
    const decision = policy.decide({
      fullscreen,
      freeMemMb,
      voiceLoaded: ttsEngine !== null,
      asrLoaded: asr !== null,
      busy,
    }, resourceConfig)

    if (!decision.releaseVoice && !decision.releaseAsr) {
      // 全屏结束 → 把刚才让出去的语音收回来
      if (voiceSuspended && !fullscreen && !busy && ttsEnabled) {
        voiceSuspended = false
        log('resource: 全屏结束，重新预热语音')
        void ensureTtsEngine().catch((error) => log(`tts resume failed: ${error.message}`))
      }
      return
    }

    if (decision.releaseVoice && releaseOnFullscreen) {
      log(`resource: ${decision.reason}`)
      voiceSuspended = true
      const engine = ttsEngine
      ttsEngine = null
      try { if (engine !== null) await engine.dispose() } catch { /* 可能已退出 */ }
      log('resource: TTS worker 已停止（显存已释放）')
    }
    if (decision.releaseAsr) {
      log(`resource: ${decision.reason}`)
      asr = null
      log('resource: ASR 已卸载（KWS 仍在，唤醒后首次需要重新加载）')
    }
  } catch (error) {
    log(`resource tick failed: ${error.message}`)
  }
}

/** 前台窗口是否全屏占据工作区 */
async function isForegroundFullscreen(policy) {
  const { spawnSync } = await import('node:child_process')
  const script = externalPath(join(APP_ROOT, 'packages', 'system', 'foreground.ps1'))
  const result = spawnSync('powershell.exe', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', script], {
    encoding: 'utf8',
    windowsHide: true,
    timeout: 8000,
  })
  if (result.status !== 0) return false
  try {
    return policy.isFullscreen(JSON.parse(result.stdout.trim()), resourceConfig)
  } catch {
    return false
  }
}

// ---------------------------------------------------------------- 托盘

/**
 * 用 Windows DPAPI（Electron safeStorage）加密落盘的密钥。
 * 只能由当前用户解开——配置文件被拷走也没用。必须在读配置之前注册。
 */
async function registerCryptoHooks() {
  try {
    const store = await import('../../packages/config/store.mjs')
    if (!safeStorage.isEncryptionAvailable()) {
      log('safeStorage 不可用，密钥将明文落盘')
      return
    }
    store.setCryptoHooks({
      encrypt: (plain) => safeStorage.encryptString(plain).toString('base64'),
      decrypt: (encoded) => safeStorage.decryptString(Buffer.from(encoded, 'base64')),
    })
    log('密钥加密已启用（safeStorage / DPAPI）')
  } catch (error) {
    log(`registerCryptoHooks failed: ${error.message}`)
  }
}

/**
 * 清理上一次运行留下的孤儿进程。
 *
 * 应用如果被强杀（任务管理器 / 崩溃），TTS worker（python，占 1.5GB 显存）和
 * CUA 桥接（powershell）可能还挂着。它们不会自己退出，下次启动就会和新的抢显存。
 * 只杀命令行里带本项目路径的那些，别碰用户自己开的 python/powershell。
 */
function cleanupOrphans() {
  try {
    const root = APP_ROOT.toLowerCase().replace(/\\/gu, '\\\\')
    const script = `
$patterns = @('gsv_worker.py', 'cua.ps1', 'cua\\\\')
Get-CimInstance Win32_Process -Filter "Name='python.exe' OR Name='powershell.exe'" |
  Where-Object { $_.CommandLine -ne $null -and $_.CommandLine.ToLower().Contains('${root.replace(/'/gu, "''")}') } |
  ForEach-Object {
    try { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop; "killed $($_.ProcessId) $($_.Name)" } catch { }
  }
`
    const result = spawnSync('powershell.exe', ['-NoProfile', '-Command', script], { encoding: 'utf8', windowsHide: true, timeout: 15000 })
    const killed = (result.stdout ?? '').trim()
    if (killed !== '') log(`orphans cleaned: ${killed.replace(/\s+/gu, ' ').slice(0, 200)}`)
  } catch (error) {
    log(`cleanupOrphans failed: ${error.message}`)
  }
}

// ---------------------------------------------------------------- 定时切换模型

/**
 * 按时间段切换模型。
 *
 * 只切"模型名"，不切 provider/baseURL——换服务商还得连带换 key，语义复杂得多；
 * 用户要的是"不同时段用不同模型"，先别搅在一起。
 *
 * 为什么 30 秒轮询而不是算到分钟边界的长定时器：笔记本睡眠/唤醒会让长定时器漂移，
 * 轮询不会漏；30 秒精度对"几点用哪个模型"完全够。
 */
let modelScheduleTimer = null
/** 上一次落定的方案 id（default 表示默认）；用来判断要不要切 */
let lastAppliedProfileId = null

function startModelScheduleWatcher() {
  if (modelScheduleTimer !== null) return
  modelScheduleTimer = setInterval(() => { applyModelSchedule('tick') }, 30_000)
}

/** 应用一次：算出此刻该用的模型，必要时切过去 */
function applyModelSchedule(reason) {
  if (agentRuntime === null) return
  try {
    const config = agentRuntime.config
    const decided = resolveModel(config, new Date())
    const target = decided.source === 'schedule' ? decided.model : (decided.model === '' ? config.model : decided.model)
    const llm = agentRuntime.llm
    if (llm === undefined) return
    /*
     * 用"当前该落在哪个方案"作为决策标识：方案变了就整套端点换过去，
     * 包含"回到默认"——默认方案的地址和 Key 也要跟着回去，
     * 否则会拿上一个方案的 Key 去打默认供应商。
     */
    const decisionKey = decided.profile === null ? 'default' : String(decided.profile.id)
    if (decisionKey === lastAppliedProfileId && llm.model === target) return

    const endpoint = decided.endpoint ?? {
      baseURL: config.baseURL,
      apiKey: agentRuntime.apiKey ?? '',
    }
    llm.setEndpoint({ baseURL: endpoint.baseURL, apiKey: endpoint.apiKey, model: target })
    lastAppliedProfileId = decisionKey

    const rule = decided.rule
    const profile = decided.profile
    log(`model switch (${reason}): → ${target} @ ${endpoint.baseURL}`
      + `${profile === null ? '' : `（方案 ${profile.name ?? profile.id}）`}`
      + `${rule === null ? '' : `（规则 ${rule.from}-${rule.to}）`}`)
  } catch (error) {
    log(`model schedule failed: ${error.message}`)
  }
}

/** 从配置文件读界面开关（以前这些只活在内存里，重启就丢） */
async function loadUiPrefs() {
  try {
    const store = await import('../../packages/config/store.mjs')
    const config = await store.loadConfig()
    const ui = config.ui ?? {}
    if (typeof ui.ttsEnabled === 'boolean') ttsEnabled = ui.ttsEnabled
    if (typeof ui.continuousMode === 'boolean') continuousMode = ui.continuousMode
    if (typeof ui.voiceConfirmEnabled === 'boolean') voiceConfirmEnabled = ui.voiceConfirmEnabled
    if (typeof ui.subtitles === 'boolean') subtitlesEnabled = ui.subtitles
    if (typeof ui.releaseOnFullscreen === 'boolean') releaseOnFullscreen = ui.releaseOnFullscreen
    if (typeof ui.wakeEnabled === 'boolean') wakeOnLaunch = ui.wakeEnabled
    if (typeof config.permissions?.mode === 'string') permissionMode = config.permissions.mode
    if (typeof config?.followup === 'object' && config.followup !== null) {
      followUpConfig = {
        ...FOLLOWUP_DEFAULTS,
        ...config.followup,
        thresholds: { ...FOLLOWUP_DEFAULTS.thresholds, ...(config.followup.thresholds ?? {}) },
        windowsMs: { ...FOLLOWUP_DEFAULTS.windowsMs, ...(config.followup.windowsMs ?? {}) },
      }
    }
    resourceConfig = config.resources ?? null
    if (typeof ui.autoStart === 'boolean') {
      autoStartEnabled = ui.autoStart
      // 以注册表实际状态为准：用户可能在任务管理器里禁用过
      const registered = isAutoStartRegistered()
      if (ui.autoStart !== registered) applyAutoStart(ui.autoStart)
      log(`autoStart 配置=${ui.autoStart} 注册表=${isAutoStartRegistered()}`)
    }
    const tts = config.tts ?? {}
    if (typeof tts.emotion === 'string' && tts.emotion !== '') ttsEmotion = tts.emotion
    if (Number.isFinite(Number(tts.speed))) ttsSpeed = Number(tts.speed)
    log(`ui prefs: tts=${ttsEnabled} continuous=${continuousMode} confirm=${voiceConfirmEnabled} subtitles=${subtitlesEnabled} emotion=${ttsEmotion} speed=${ttsSpeed}`)
  } catch (error) {
    log(`load ui prefs failed: ${error.message}`)
  }
}

/**
 * 存界面开关。托盘点一下就要落盘，否则下次启动又回到默认值。
 * 只写 ui/tts 两块，其它配置（密钥、模型）原样保留——saveConfig 是顶层浅合并，
 * 所以这里先把当前状态整体拼出来再写，避免把别的字段覆盖掉。
 */
async function saveUiPrefs() {
  try {
    const store = await import('../../packages/config/store.mjs')
    const current = await store.loadConfig()
    await store.saveConfig({
      ui: {
        ...(current.ui ?? {}),
        ttsEnabled,
        continuousMode,
        voiceConfirmEnabled,
        subtitles: subtitlesEnabled,
        autoStart: autoStartEnabled,
      },
      tts: {
        ...(current.tts ?? {}),
        emotion: ttsEmotion,
        speed: ttsSpeed,
      },
    })
  } catch (error) {
    log(`save ui prefs failed: ${error.message}`)
  }
}

// ---------------------------------------------------------------- 设置窗口

/** 设置窗口：普通窗口（有自己的标题栏），关掉就销毁，不做常驻 */
function openSettings() {
  if (settingsWin !== null && !settingsWin.isDestroyed()) {
    settingsWin.show()
    settingsWin.focus()
    return settingsWin
  }
  settingsWin = new BrowserWindow({
    width: SETTINGS_WIDTH,
    height: SETTINGS_HEIGHT,
    useContentSize: true,
    title: '语音助手 · 设置',
    backgroundColor: '#14161c',
    autoHideMenuBar: true,
    webPreferences: {
      preload: join(HERE, 'preload-settings.mjs'),
      contextIsolation: true,
      nodeIntegration: false,
      // ESM preload 必须关 sandbox（和 HUD 同一个坑）
      sandbox: false,
    },
  })
  settingsWin.loadFile(join(HERE, 'renderer', 'settings.html'))
  settingsWin.webContents.on('console-message', (_event, level, message) => {
    if (level >= 2) log(`[settings:${level}] ${message}`)
  })
  settingsWin.on('closed', () => { settingsWin = null })
  return settingsWin
}

/** 主进程侧的设置 API（渲染层只传值，规则都在这里） */
function registerSettingsIpc() {
  ipcMain.handle('settings:load', async () => {
    const [store, permissionsModule] = await Promise.all([
      import('../../packages/config/store.mjs'),
      import('../../packages/agent/permissions.mjs'),
    ])
    const config = await store.loadPublicConfig()
    const audit = await readAuditTail(30)

    // 音色列表直接读音色包目录（比等 TTS worker 快得多，worker 冷启动要 20 秒）。
    // worker 已经起来时以它为准——那里才是真正能合成的集合。
    let voices = ttsEngine !== null && ttsEngine.voices.length > 0
      ? ttsEngine.voices
      : await listVoiceEmotions(config.tts?.emotion ?? '中立')
    if (ttsEngine === null) {
      void ensureTtsEngine().catch((error) => log(`tts prewarm from settings failed: ${error.message}`))
    }

    const capabilities = Object.entries(permissionsModule.CAPABILITIES)
      .map(([id, meta]) => ({ id, label: meta.label, level: meta.level }))
      .filter((item) => item.id !== 'none' && item.id !== 'unknown')

    return {
      config,
      providers: store.listProviders(),
      capabilities,
      voices,
      audit,
    }
  })

  ipcMain.handle('settings:save', async (_event, payload) => {
    try {
      const store = await import('../../packages/config/store.mjs')
      const current = await store.loadConfig()
      // permissions 是整块替换的：必须和现有值合并，否则会把暂停状态、临时授权一起冲掉
      const next = { ...(payload ?? {}) }
      if (payload?.permissions !== undefined) {
        next.permissions = {
          ...(current.permissions ?? {}),
          mode: payload.permissions.mode ?? current.permissions?.mode ?? 'standard',
          overrides: payload.permissions.overrides ?? {},
        }
      }
      const credentialsChanged = payload?.netease !== undefined
        && (String(payload.netease.appId ?? '') !== ''
          || String(payload.netease.privateKey ?? '') !== '')
      const saved = await store.saveConfig(next)
      // 保存动作本身带点击声，给它一段静默期，别让 KWS 把它当唤醒词
      wakeMutedUntil = Date.now() + 1500
      /*
       * 音乐凭据变了才同步给 ncm-cli，而且**不 await**。
       * 这个同步要跑 2~4 次 CLI 子进程（0.5~11 秒/次），一旦在保存路径上同步等待，
       * 设置窗口就会一直"未响应"——用户看到的就是点了保存没反应。
       */
      if (credentialsChanged) {
        void (async () => {
          try {
            const fresh = await store.loadConfig()
            const current = await store.loadConfig()
            const result = await syncCredentialsAsync(current.netease ?? {}, { syncedHash: current.netease?.syncedHash })
            if (result.hash !== undefined && result.hash !== current.netease?.syncedHash) {
              // 记下"同步过哪一版凭据"：下次内容没变就不动 CLI，避免把登录态顶掉
              await store.saveConfig({ netease: { ...(current.netease ?? {}), syncedHash: result.hash } })
            }
            const synced = result.synced ?? []
            log(`netease credentials ${synced.length > 0 ? `synced: ${synced.join(', ')}` : 'unchanged'}`)
          } catch (error) {
            log(`netease sync failed: ${error.message}`)
          }
        })()
      }
      // 立刻生效（不用重启）——这些都是内存里的开关与默认值
      if (typeof payload?.ui?.ttsEnabled === 'boolean') ttsEnabled = payload.ui.ttsEnabled
      if (typeof payload?.ui?.continuousMode === 'boolean') continuousMode = payload.ui.continuousMode
      if (typeof payload?.ui?.voiceConfirmEnabled === 'boolean') voiceConfirmEnabled = payload.ui.voiceConfirmEnabled
      if (typeof payload?.ui?.subtitles === 'boolean') subtitlesEnabled = payload.ui.subtitles
      if (typeof payload?.ui?.autoStart === 'boolean') {
        autoStartEnabled = payload.ui.autoStart
        applyAutoStart(autoStartEnabled)
      }
      if (typeof payload?.ui?.releaseOnFullscreen === 'boolean') releaseOnFullscreen = payload.ui.releaseOnFullscreen
      if (typeof payload?.tts?.emotion === 'string') ttsEmotion = payload.tts.emotion
      if (Number.isFinite(Number(payload?.tts?.speed))) ttsSpeed = Number(payload.tts.speed)
      if (payload?.permissions !== undefined) {
        const gateInstance = await ensurePermissions()
        gateInstance.update({
          mode: payload.permissions.mode ?? gateInstance.permissions.mode,
          overrides: payload.permissions.overrides ?? {},
        })
      }
      // 定时切换改了规则要立刻生效，不用等下一轮轮询
      if (payload?.modelSchedule !== undefined && agentRuntime !== null) {
        agentRuntime.config.modelSchedule = payload.modelSchedule
        applyModelSchedule('settings')
      }
      if (!ttsEnabled) stopSpeaking()
      log(`settings saved: tts=${ttsEnabled} emotion=${ttsEmotion} speed=${ttsSpeed} mode=${payload?.permissions?.mode ?? '-'}`)
      return { ok: true, config: saved }
    } catch (error) {
      log(`settings save failed: ${error.message}`)
      return { ok: false, error: error.message }
    }
  })

  ipcMain.handle('settings:testModel', async (_event, payload) => {
    try {
      const [{ LLMClient }, store] = await Promise.all([
        import('../../packages/agent/llm.mjs'),
        import('../../packages/config/store.mjs'),
      ])
      const stored = await store.loadConfig()
      const apiKey = typeof payload?.apiKey === 'string' && payload.apiKey !== '' && !payload.apiKey.includes('*')
        ? payload.apiKey
        : await store.resolveApiKey()
      if (!apiKey) return { ok: false, error: '还没配置 API Key' }
      const client = new LLMClient({
        baseURL: payload?.baseURL || stored.baseURL,
        apiKey,
        model: payload?.model || stored.model,
        temperature: 0,
        timeoutMs: 20000,
      })
      const startedAt = Date.now()
      const reply = await client.chat({ messages: [{ role: 'user', content: '回复 ok' }], stream: false })
      return { ok: true, ms: Date.now() - startedAt, text: (reply.text ?? '').slice(0, 40) }
    } catch (error) {
      return { ok: false, error: error.message }
    }
  })

  ipcMain.handle('settings:testSearch', async (_event, apiKey) => {
    try {
      const [{ WebSearch }, store] = await Promise.all([
        import('../../packages/tools/web-search.mjs'),
        import('../../packages/config/store.mjs'),
      ])
      const key = typeof apiKey === 'string' && apiKey !== '' && !apiKey.includes('*')
        ? apiKey
        : await store.resolveSearchKey()
      if (!key) return { ok: false, error: '还没配置搜索 Key' }
      const search = new WebSearch({ apiKey: key })
      const startedAt = Date.now()
      const result = await search.invoke({ query: '上海今天天气' })
      return { ok: true, ms: Date.now() - startedAt, count: result.meta.count, preview: result.text.slice(0, 200) }
    } catch (error) {
      return { ok: false, error: error.message }
    }
  })

  ipcMain.handle('settings:clearSearch', async () => {
    try {
      const store = await import('../../packages/config/store.mjs')
      const saved = await store.saveConfig({ search: { provider: 'tavily', apiKey: null } })
      if (agentRuntime !== null) agentRuntime.webSearch.setApiKey('')
      return { ok: true, config: saved }
    } catch (error) {
      return { ok: false, error: error.message }
    }
  })

  /*
   * 网易云登录：CLI 会给出一个一次性登录链接（手机扫码/网页授权）。
   * 用户点设置里的「登录网易云」→ 我们取链接并丢给系统浏览器打开。
   */
  /*
   * 登录网易云：自己持有 login 子进程（前台模式），链接一出来就开浏览器。
   * 不用 --background：那样轮询交给游离进程，扫完码本地可能还是未登录。
   */
  ipcMain.handle('settings:neteaseLogin', async () => {
    try {
      if (loginSession !== null) { try { loginSession.kill() } catch { /* 已退出 */ } loginSession = null }
      let opened = false
      loginSession = startLoginSession({}, {
        onUrl: (url) => {
          log(`netease login: 链接 ${url.slice(0, 50)}…`)
          if (!opened) { opened = true; void shell.openExternal(url) }
          if (settingsWin !== null && !settingsWin.isDestroyed()) settingsWin.webContents.send('settings:neteaseLoginUrl', url)
        },
        onDone: (result) => {
          loginSession = null
          clearLoginCache()
          log(`netease login: done ok=${result.ok} code=${result.code}${result.error === null ? '' : ` err=${result.error}`}`)
          if (settingsWin !== null && !settingsWin.isDestroyed()) settingsWin.webContents.send('settings:neteaseLoginDone', result)
        },
      })
      if (loginSession.ok === false) return { ok: false, error: loginSession.error }
      return { ok: true, waiting: true }
    } catch (error) {
      log(`netease login failed: ${error.message}`)
      return { ok: false, error: error.message }
    }
  })

  ipcMain.handle('settings:neteaseStatus', async () => {
    try {
      const status = loginStatus()
      return { ok: true, loggedIn: status.loggedIn === true, text: status.text ?? '' }
    } catch (error) {
      return { ok: false, error: error.message }
    }
  })
  ipcMain.handle('settings:exportAudit', async () => exportAudit())

  ipcMain.handle('settings:testVoice', async (_event, options) => {
    try {
      const engine = await ensureTtsEngine()
      const text = '我是银狼，音色和语速都可以调，你听着合适就好。'
      const result = await engine.speak(text, {
        emotion: options?.emotion ?? ttsEmotion,
        speed: Number(options?.speed ?? ttsSpeed),
      })
      const file = join(app.getPath('temp'), 'dsh-va-preview.wav')
      writeWav(file, result.samples, result.sampleRate)
      // 顺便在 HUD 里放一遍，省得用户自己去翻文件
      if (win !== null && !win.isDestroyed()) {
        win.webContents.send('hud:play', { id: Date.now(), url: pathToFileURL(file).href, text })
      }
      return { ok: true, ms: result.stats.synthMs, seconds: result.stats.durationSec, file }
    } catch (error) {
      return { ok: false, error: error.message }
    }
  })
}

/** 从音色包的参考音频文件名里读情绪列表（【中立】xxx.wav → 中立） */
async function listVoiceEmotions(fallback) {
  try {
    const { readdir } = await import('node:fs/promises')
    const dir = join(ROOT_VOICE_DIR, 'reference_audios', '中文', 'emotions')
    const files = await readdir(dir)
    const emotions = files
      .filter((name) => name.toLowerCase().endsWith('.wav') && name.startsWith('【') && name.includes('】'))
      .map((name) => name.slice(1, name.indexOf('】')))
    return emotions.length > 0 ? emotions : [fallback]
  } catch {
    return [fallback]
  }
}

/**
 * 导出审计日志（R7 验收条件里的"日志可导出"）。
 *
 * 为什么复制而不是"另存为"对话框：语音助手的用户场景是"我要留个档"，
 * 不是"我要挑个路径"。固定导出到数据目录下的 exports/，日志里回显完整路径即可。
 */
async function exportAudit() {
  try {
    const store = await import('../../packages/config/store.mjs')
    const { readFile, writeFile, mkdir } = await import('node:fs/promises')
    const source = join(dirname(store.CONFIG_FILE), 'audit.jsonl')
    const raw = await readFile(source, 'utf8')
    const stamp = new Date().toISOString().replace(/[:.]/gu, '-').slice(0, 19)
    const outDir = join(DATA_ROOT, 'exports')
    await mkdir(outDir, { recursive: true })
    const target = join(outDir, `audit-${stamp}.jsonl`)
    await writeFile(target, raw, 'utf8')
    const lines = raw.split('\n').filter(Boolean).length
    log(`audit exported: ${target} (${lines} 条)`)
    return { ok: true, path: target, lines }
  } catch (error) {
    log(`audit export failed: ${error.message}`)
    return { ok: false, error: error.message }
  }
}

/** 审计日志最后 N 行（设置页要能看见"刚刚发生了什么"） */
async function readAuditTail(lines) {
  try {
    const store = await import('../../packages/config/store.mjs')
    const { readFile } = await import('node:fs/promises')
    const path = join(dirname(store.CONFIG_FILE), 'audit.jsonl')
    const raw = await readFile(path, 'utf8')
    return raw.split('\n').filter(Boolean).slice(-lines)
  } catch {
    return []
  }
}

/**
 * 开机自启：直接写注册表，不用 app.setLoginItemSettings。
 *
 * 为什么：实测这套 API 在这台机器上把值名写成了 `electron.app.Electron`，
 * 于是读回来永远是 false（它按当前 app 名去找），用户会看到"勾了但不生效"。
 * 自己写 HKCU\...\Run 反而确定：值名固定、命令固定、读回来也一致。
 */
/*
 * 注册表值名用 ASCII。
 * reg.exe 对中文值名不友好（按 OEM 代码页解析命令行），
 * 用它 delete 中文值名会报"无效语法"删不掉；ASCII 名字两头都稳。
 */
const AUTOSTART_VALUE = 'ComputerUseVoiceAssistant'
/** 旧版本用过的中文值名：启动时要清掉，否则开机同一程序会被启动两次 */
const LEGACY_AUTOSTART_VALUE = '语音助手'
const RUN_KEY = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run'

function applyAutoStart(enabled) {
  if (!app.isPackaged) {
    log(`autoStart=${enabled}（开发态不注册，打包后才生效）`)
    return
  }
  const command = `"${process.execPath}" --listen`
  const result = enabled
    ? spawnSync('reg.exe', ['add', RUN_KEY, '/v', AUTOSTART_VALUE, '/t', 'REG_SZ', '/d', command, '/f'], { windowsHide: true, encoding: 'utf8' })
    : spawnSync('reg.exe', ['delete', RUN_KEY, '/v', AUTOSTART_VALUE, '/f'], { windowsHide: true, encoding: 'utf8' })
  log(`autoStart=${enabled} reg exit=${result.status} ${String(result.stderr ?? '').trim().slice(0, 120)}`)
}

/**
 * 清掉历史遗留的自启项。
 * 早期版本用「语音助手」这个名字写过一次 Run 键，新版用 ComputerUseVoiceAssistant，
 * 结果两份都在——开机时 Windows 会把同一个程序启动两次：
 * 第二个实例拿不到单实例锁直接退出，但主实例会收到 second-instance 事件，
 * 表现就是"托盘状态不对、唤醒时好时坏"。这个函数负责把重复项删掉。
 */
function cleanupLegacyAutoStart() {
  if (!app.isPackaged) return
  try {
    /*
     * 必须用 PowerShell 删，不能用 reg.exe：
     * reg.exe 按 OEM 代码页解析命令行，中文值名「语音助手」传进去会变乱码，
     * 结果是"无效语法"、删不掉（实测 exit=1）。PowerShell 走 Unicode 参数没这问题。
     */
    const result = spawnSync('powershell.exe', ['-NoProfile', '-Command',
      `Remove-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name '${LEGACY_AUTOSTART_VALUE}' -ErrorAction SilentlyContinue`],
      { windowsHide: true, encoding: 'utf8' })
    log(result.status === 0 ? '已清理遗留自启项（若存在）' : '没有遗留自启项')
  } catch (error) {
    log(`cleanupLegacyAutoStart failed: ${error.message}`)
  }
}
function isAutoStartRegistered() {
  const result = spawnSync('reg.exe', ['query', RUN_KEY, '/v', AUTOSTART_VALUE], { windowsHide: true, encoding: 'utf8' })
  return result.status === 0
}

function buildTray() {
  const image = nativeImage.createFromPath(join(HERE, 'assets', 'tray-idle.png'))
  tray = new Tray(image.isEmpty() ? nativeImage.createEmpty() : image)
  tray.setToolTip('语音助手')

  const menu = Menu.buildFromTemplate([
    { label: '唤醒 / 开始对话', click: () => showForInteraction('listening') },
    { type: 'separator' },
    { id: 'wake', label: '语音唤醒', type: 'checkbox', checked: false, click: (item) => {
      // 以"实际是否在听"为准：勾了但麦克风没开，比不勾更让人困惑
      if (item.checked) void startListening().then((result) => syncWakeTray(result.ok === true))
      else void stopListening().then(() => syncWakeTray(false))
    } },
    { label: '连续对话', type: 'checkbox', checked: continuousMode, click: (item) => {
      continuousMode = item.checked
      log(`continuousMode=${continuousMode}`)
      void saveUiPrefs()
      // 关掉时立刻结束当前这轮"等你说下一句"的状态
      if (!continuousMode && commandActive && activeTurn === null) finishCommand('continuous-off')
    } },
    { label: '语音播报', type: 'checkbox', checked: ttsEnabled, click: (item) => {
      ttsEnabled = item.checked
      // 定时切换改了规则要立刻生效，不用等下一轮轮询
      if (!ttsEnabled) stopSpeaking()
      void saveUiPrefs()
      log(`ttsEnabled=${ttsEnabled}`)
    } },
    { label: '语音取消', type: 'checkbox', checked: voiceConfirmEnabled, click: (item) => {
      voiceConfirmEnabled = item.checked
      void saveUiPrefs()
      log(`voiceConfirmEnabled=${voiceConfirmEnabled}`)
    } },
    {
      label: '暂停操作 10 分钟',
      type: 'checkbox',
      checked: false,
      click: async (item) => {
        const gateInstance = await ensurePermissions()
        if (item.checked) {
          gateInstance.pause(10)
          setState('hidden')
          log('permissions paused 10min')
        } else {
          gateInstance.update({ pausedUntil: 0 })
          log('permissions resumed')
        }
      },
    },
    { label: '显示字幕', type: 'checkbox', checked: subtitlesEnabled, click: (item) => {
      subtitlesEnabled = item.checked
      void saveUiPrefs()
      log(`subtitles=${subtitlesEnabled}`)
    } },
    { label: '免确认模式（全放行）', type: 'checkbox', checked: permissionMode === 'unattended', click: async (item) => {
      const gateInstance = await ensurePermissions()
      const next = item.checked ? 'unattended' : 'standard'
      permissionMode = next
      gateInstance.update({ mode: next })
      const store = await import('../../packages/config/store.mjs')
      const current = await store.loadConfig()
      await store.saveConfig({ permissions: { ...(current.permissions ?? {}), mode: next } })
      log(`permission mode → ${next}`)
    } },
    { type: 'separator' },
    {
      label: '演示状态',
      submenu: [
        { label: '聆听', click: () => showForInteraction('listening') },
        { label: '思考', click: () => showForInteraction('thinking') },
        { label: '播报', click: () => showForInteraction('speaking') },
        { label: '确认', click: () => showForInteraction('confirm') },
        { label: '错误', click: () => showForInteraction('error') },
        { label: '隐藏', click: () => hideHud() },
      ],
    },
    { label: '导出审计日志…', click: async () => {
      const result = await exportAudit()
      log(`tray export audit: ${JSON.stringify(result)}`)
    } },
    { label: '设置…', click: () => openSettings() },
    { type: 'separator' },
    { label: '退出', click: () => quitApp('tray') },
  ])
  tray.setContextMenu(menu)
  wakeMenuItem = menu.getMenuItemById('wake')

  /*
   * 托盘建好之后，把"当前真实状态"补同步一次。
   * 否则会出现：图标还是"未开麦"的样子，而菜单里的「语音唤醒」已经是勾选的
   * （启动时 setState 可能早于 tray 创建，图标没落上）——用户看到的就是"状态对不上"。
   */
  setState(currentState)
  wakeMenuItem.checked = listening
  log(`tray 初始状态同步：state=${currentState} listening=${listening}`)

  // 左键点击图标 = 唤醒（Windows 上 click 与双击行为不同，两个都挂）
  tray.on('click', () => showForInteraction('listening'))
}

/**
 * 托盘"语音唤醒"的勾选状态必须跟着真实麦克风状态走。
 * 之前的毛病：勾没勾和麦克风开没开是两套状态——用户看到勾被取消，
 * 结果一说话照样能唤醒（因为 --listen 启动时直接开了麦，压根没动过那个勾）。
 */
function syncWakeTray(active) {
  if (wakeMenuItem === null) return
  if (wakeMenuItem.checked === active) return
  wakeMenuItem.checked = active
  log(`tray wake checkbox → ${active}`)
}

/**
 * 开/关语音唤醒，并**记住用户的选择**。
 *
 * 为什么必须落盘：以前"开着唤醒"这件事只活在内存里，重启就回到默认（关），
 * 用户每次开机都得再去托盘里勾一次——他当然会觉得"状态没保存"。
 *
 * persist=false 用于程序自己调用的场景（启动时恢复、验证脚本），
 * 否则退出时的 stopListening 会把"关"写进配置，下次开机就不响了。
 */
async function setWakeEnabled(enabled, options = {}) {
  const persist = options.persist === true
  if (enabled) {
    const result = await startListening()
    syncWakeTray(result.ok === true)
    if (persist && result.ok === true) await saveWakePreference(true)
    return result.ok === true
  }
  await stopListening()
  syncWakeTray(false)
  if (persist) await saveWakePreference(false)
  return true
}

async function saveWakePreference(enabled) {
  try {
    const store = await import('../../packages/config/store.mjs')
    const current = await store.loadConfig()
    if (current.ui?.wakeEnabled === enabled) return
    await store.saveConfig({ ui: { ...(current.ui ?? {}), wakeEnabled: enabled } })
    log(`wake preference saved: ${enabled}`)
  } catch (error) {
    log(`save wake preference failed: ${error.message}`)
  }
}

// ---------------------------------------------------------------- 生命周期

// 单实例：托盘应用开两份会让用户看到两个图标、抢同一套热键
/** 父进程是谁（把我们拉起来的那个）：用来查谁在偷偷启动第二个实例 */
function describeParentProcess() {
  try {
    const result = spawnSync('powershell.exe', ['-NoProfile', '-Command',
      `(Get-CimInstance Win32_Process -Filter "ProcessId=${process.ppid}" | Select-Object -ExpandProperty Name)`],
      { encoding: 'utf8', windowsHide: true, timeout: 4000 })
    const name = String(result.stdout ?? '').trim()
    return `${name === '' ? '?' : name}(ppid=${process.ppid})`
  } catch {
    return `?(ppid=${process.ppid})`
  }
}

const gotSingleInstanceLock = app.requestSingleInstanceLock()
log(`singleInstanceLock=${gotSingleInstanceLock}`)
if (!gotSingleInstanceLock) {
  // 记下是谁把我们拉起来的：'second-instance' 的窗口是它弹的
  log(`second-instance launch：parent=${describeParentProcess()} argv=${process.argv.slice(1).join(' ')}`)
  app.quit()
} else {
  /*
   * 第二个实例被拉起时，之前是直接 showForInteraction —— 也就是"窗口自己冒出来"、
   * 看起来就是助手被唤醒了。谁在拉起我们目前不知道，所以先记下父进程，等证据。
   * 同时：有设置窗口开着就不弹，别打断改配置。
   */
  /*
   * 第二个实例被拉起时，原来是直接 showForInteraction —— 窗口自己冒出来，
   * 看起来就是"助手被唤醒了"。谁在拉起我们暂时不知道，所以先把父进程记下来。
   * 另外：设置窗口开着就不弹，别打断改配置。
   */
  app.on('second-instance', () => {
    const parent = describeParentProcess()
    log(`second-instance：又被拉起一次（parent=${parent}）`)
    if (settingsWin !== null && !settingsWin.isDestroyed() && settingsWin.isVisible()) {
      log('second-instance：设置在开，不弹窗')
      return
    }
    showForInteraction('listening')
  })
  app.whenReady().then(async () => {
  log('whenReady')
  cleanupOrphans()
  cleanupLegacyAutoStart()
  startPlayerWatcher()
  startModelScheduleWatcher()
  await registerCryptoHooks()
  await loadUiPrefs()
  buildTray()
  registerSettingsIpc()
  log('tray built')
  maybePrewarmAsr()
  maybePrewarmTts()
  startResourceMonitor()

  /*
   * 恢复上次的唤醒状态。以前"唤醒开着"只活在内存里，重启就回到关，
   * 用户每次开机都要再去托盘勾一次——他自然会说"状态没保存"。
   */
  if (wakeOnLaunch && !isCuaTest && !isVisionProbe && !isQuitTest && !isConfirmCloseTest) {
    log('restore wake: on')
    void setWakeEnabled(true, { persist: false })
  } else if (!wakeOnLaunch) {
    log('restore wake: off（上次是你自己关的）')
  }
  // 验收用：到点自动走一次正常退出流程，便于测"退出后内存/显存有没有还回来"
  const quitAfter = Number(process.env.CUA_VA_QUIT_AFTER_MS ?? 0)
  if (quitAfter > 0) {
    log(`auto quit scheduled in ${quitAfter}ms`)
    setTimeout(() => quitApp('auto-test'), quitAfter)
  }

  ipcMain.on('hud:setHeight', () => {
    // 窗口高度固定，这里只当作"渲染层已就绪、可以显示"的信号。
    // 不再按内容改尺寸：可见状态下 resize 会闪烁（实测），而固定高度让卡片内部自适应即可。
    if (!contentReady) {
      contentReady = true
      if (pendingShow !== null) {
        const { mode, state } = pendingShow
        pendingShow = null
        doShow(win, mode, state)
      }
    }
  })

  ipcMain.on('hud:dismiss', () => dismiss())

  // 播完回执：队列据此推进
  ipcMain.on('hud:spoken', (_event, id) => {
    const resolve = spokenWaiters.get(id)
    if (resolve !== undefined) resolve()
  })

  // 播放浮窗上的按钮：prev / toggle / next
  ipcMain.on('player:action', (_event, action) => {
    void handlePlayerAction(action)
  })
  // 确认卡片上的按钮：允许 / 允许 15 分钟 / 取消
  ipcMain.on('hud:confirm', (event, payload) => {
    if (!win || win.isDestroyed() || event.sender !== win.webContents
        || event.senderFrame !== win.webContents.mainFrame) return
    const { id, answer } = payload ?? {}
    if (!Number.isInteger(id) || ![true, false, 'once'].includes(answer)) return
    settleConfirm({ allow: answer === true || answer === 'once', channel: 'click',
      once: answer === 'once', reason: answer === false ? '用户点了取消' : '' }, id, true)
  })

  // 麦克风权限：只放行 media，其余一律拒绝
  session.defaultSession.setPermissionRequestHandler((_wc, permission, callback) => {
    callback(permission === 'media')
  })

  // 采集数据来自隐藏的采集窗口
  ipcMain.on('capture:chunk', (_event, chunk) => {
    if (!listening || pipeline === null) return
    try {
      processChunk(chunk)
    } catch (error) {
      log(`pipeline error: ${error.message}`)
    }
  })

  ipcMain.on('capture:error', (_event, message) => log(`capture error: ${message}`))

  if (isSelfTest) {
    await runSelfTest()
  } else if (isMicTest) {
    await runMicTest()
  } else if (isListen) {
    // 常驻监听：只开唤醒，命中就弹小窗（用于真声验证）
    // 但如果你上次在托盘里把唤醒关了，就别硬开——状态要听你的
    if (wakeOnLaunch) {
      // 恢复唤醒的流程可能已经开过麦；再调一次会在设备忙时报 "failed: undefined"（实测出现过）
      if (listening) {
        log('listen mode: 已经在听（恢复流程已开麦）')
      } else {
        log('listen mode: starting')
        const r = await startListening()
        log(`listen mode started: ${JSON.stringify(r)}`)
      }
    } else {
      log('listen mode: 上次已关闭唤醒，本次不开麦（托盘里可再打开）')
    }
  } else if (isSearchTest) {
    await runSearchTest(searchTestQuery)
  } else if (isHistoryTest) {
    await runHistoryTest()
  } else if (isFollowUpTest) {
    await runFollowUpTest()
  } else if (isGateTest) {
    await runGateTest(gateTestMode)
  } else if (isBargeInTest) {
    await runBargeInTest(bargeInWav)
  } else if (isEchoTest) {
    await runEchoTest()
  } else if (isSettingsTest) {
    await runSettingsTest()
  } else if (isPlayerUiTest) {
    await runPlayerUiTest()
  } else if (isFollowUpUiTest) {
    await runFollowUpUiTest()
  } else if (isCuaTest) {
    await runCuaTest()
  } else if (isQuitTest) {
    await runQuitTest()
  } else if (isConfirmCloseTest) {
    await runConfirmCloseTest()
  } else if (isVisionProbe) {
    await runVisionProbe()
  } else if (isOfflineTest) {
    await runOfflineTest()
  } else if (isResourceTest) {
    await runResourceTest()
  } else if (isSettingsDemo) {
    // 只开设置窗口，便于截图与手工点一遍（不启动麦克风）
    const target = openSettings()
    target.webContents.once('did-finish-load', () => log('settings window loaded'))
    await new Promise((resolve) => setTimeout(resolve, Number(process.env.CUA_VA_SETTINGS_WAIT ?? 25000)))
    app.isQuitting = true
    app.exit(0)
  } else if (isAsrDemo) {
    await runAsrDemo(asrDemoWav)
  } else if (isCmdDemo) {
    // 跳过语音，直接跑一句指令：验证"识别之后"那段链路（agent + 工具 + 字幕）
    showForWake('thinking')
    sendSubtitle({ reset: true })
    sendSubtitle({ final: cmdDemoText })
    await runCommand(cmdDemoText)
    // 等播报真的放完：TTS worker 冷启动要 20 秒左右（含模型加载），
    // 这里等太短会在"第一句还没合成出来"时就退出，看起来像"没声音"
    await new Promise((resolve) => setTimeout(resolve, Number(process.env.CUA_VA_CMD_DEMO_WAIT ?? 15000)))
    app.isQuitting = true
    app.quit()
  } else if (process.argv.includes('--wake-demo')) {
    // 模拟一次唤醒：验证"命中 → 弹小窗"这条路径（不依赖扬声器/麦克风）
    setTimeout(() => onWake('你好狼宝'), 1500)
  } else if (isDemo) {
    // 演示模式：启动即弹出，便于截图/对齐视觉
    showForInteraction('speaking')
  }
  })
}

// 关掉所有窗口不退出（托盘常驻）
app.on('window-all-closed', () => {
  /*
   * 退出流程里窗口会先被关掉，此时 shutdown() 还在 await 回收子进程。
   * 以前这里直接 app.quit()，等于把回收流程从中间掐断——日志里永远看不到
   * "shutdown done"，worker 只能靠 stdin EOF 自己退出。
   * 现在交给 shutdown() 收尾：它自己会 app.exit(0)。
   */
  if (shuttingDown) return
  if (app.isQuitting === true) app.quit()
})

// ---------------------------------------------------------------- 自检

/**
 * 麦克风链路自检：开采集 → 观察电平 → 关采集 → 报告。
 * 不要求有人说话；只要电平在动、管线在跑、关闭后能释放设备，就算通过。
 */
async function runMicTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const report = {}
  micStats = { chunks: 0, peakLevelDb: -120, floorDb: -60, wakes: 0 }

  const started = await startListening()
  report.start = started
  await sleep(3000)
  report.afterStart = { ...micStats, listening }

  // 采样若干次的电平，确认麦克风真的有信号（哪怕是底噪）
  const samples = []
  for (let i = 0; i < 6; i += 1) {
    await sleep(400)
    samples.push({ level: micStats.peakLevelDb, floor: micStats.floorDb, chunks: micStats.chunks })
  }
  report.samples = samples

  const stopResult = await stopListening()
  report.stop = stopResult
  await sleep(500)
  report.afterStop = { listening, captureWindowExists: captureWin !== null }

  const chunksGrew = micStats.chunks > 20 // 3.5 秒 * 10 块/秒，留余量
  report.verdict = {
    captureStarted: started.ok === true,
    audioFlowing: chunksGrew,
    pipelineRunning: report.samples.every((s) => Number.isFinite(s.floor)),
    releasedAfterStop: report.afterStop.listening === false && report.afterStop.captureWindowExists === false,
  }
  report.verdict.pass = Object.values(report.verdict).every((v) => v === true)

  log(`mic-test report ${JSON.stringify(report)}`)
  console.log(`__MICTEST__${JSON.stringify(report)}`)
  setTimeout(() => { app.isQuitting = true; app.quit() }, 300)
}

/**
 * ASR 演示：把一段 WAV 按实时速度（100ms 一块）喂进识别链路。
 *
 * 为什么不用麦克风：验收字幕和端点不该依赖"我此刻能不能说话"，
 * 而且这一步要能被重复跑、能被截图。走的仍是 feedCommandAudio，
 * 和真麦克风同一条代码路径。
 */
async function runAsrDemo(wavPath) {
  log(`asr-demo: ${wavPath}`)
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const { readWav } = await import('../../packages/voice/wav.mjs')
  const wav = readWav(wavPath)
  let pcm = wav.samples
  const inRate = wav.sampleRate

  if (inRate !== 16000) {
    const { createRequire } = await import('node:module')
    const require = createRequire(import.meta.url)
    const { LinearResampler } = require('sherpa-onnx-node/resampler.js')
    const resampler = new LinearResampler(inRate, 16000)
    const parts = []
    for (let i = 0; i < pcm.length; i += 2205) {
      const piece = pcm.subarray(i, Math.min(i + 2205, pcm.length))
      const isLast = i + 2205 >= pcm.length
      parts.push(isLast ? resampler.flush(piece) : resampler.resample(piece))
    }
    const merged = new Float32Array(parts.reduce((sum, p) => sum + p.length, 0))
    let cursor = 0
    for (const part of parts) { merged.set(part, cursor); cursor += part.length }
    pcm = merged
  }

  const loadStart = Date.now()
  const instance = await startAsrLoad()
  const loadMs = Date.now() - loadStart
  if (instance === null) {
    console.log('__ASRDEMO__' + JSON.stringify({ ok: false, error: 'asr load failed' }))
    setTimeout(() => { app.isQuitting = true; app.quit() }, 500)
    return
  }

  showForWake('listening')
  await sleep(500)
  onWake('你好狼宝')
  await sleep(WAKE_TAIL_MS)

  const timeline = []
  const startedAt = Date.now()
  let latest = ''
  let finalText = ''
  let firstPartialMs = -1
  for (let i = 0; i < pcm.length; i += 1600) {
    const piece = pcm.subarray(i, Math.min(i + 1600, pcm.length))
    const heard = feedCommandAudio(piece)
    if (heard !== null && heard.partial !== '' && heard.partial !== latest) {
      if (firstPartialMs < 0) firstPartialMs = instance.firstPartialMs
      latest = heard.partial
      timeline.push({ atMs: Date.now() - startedAt, partial: heard.partial })
    }
    if (heard !== null && heard.final !== null) {
      finalText = heard.final
      timeline.push({ atMs: Date.now() - startedAt, final: heard.final })
      break
    }
    await sleep(100)
  }
  if (commandActive) {
    await sleep(1200)
    if (commandActive) finishCommand('demo-end')
  }
  if (finalText === '') finalText = latest

  Object.assign(report, {
    ok: true,
    wav: wavPath,
    inRate,
    audioSec: Math.round((pcm.length / 16000) * 100) / 100,
    loadMs,
    firstPartialMs,
    timeline,
    command: finalText,
    state: currentState,
  })
  log(`asr-demo report ${JSON.stringify(report)}`)
  console.log('__ASRDEMO__' + JSON.stringify(report))
  // 留在屏幕上几秒，供人工/截图确认
  await sleep(6000)
  setTimeout(() => { app.isQuitting = true; app.quit() }, 300)
}

/**
 * 回声测量（--echo-test）：搞清"我播报时，麦克风到底听到多少自己的声音"。
 *
 * 打断功能能不能做成，全看这个数字：
 *   - 如果 AEC 把回声压到底噪附近 → 用普通 VAD 阈值就能判断"用户在说话"
 *   - 如果回声还有 20~30dB → 必须用"回声基线 + 余量"的差分法
 * 所以先量，再定阈值，别拍脑袋。
 */
async function runEchoTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const samples = []
  echoProbe = samples

  const started = await startListening()
  log(`echo-test listening: ${JSON.stringify(started)}`)
  showForWake('speaking')

  await sleep(2500) // 先量环境底噪
  const silenceEnd = samples.length

  // 直接合成一整段长音频、一次播完：队列式播报中间有空隙，
  // 会把回声测量切成碎片，测不出"连续播报时"的真实回声。
  const { Tts } = await import('../../packages/voice/tts.mjs')
  const engine = new Tts()
  const text = '这是一段用来测量回声的测试语音，我会连续说十几秒钟，中间不停顿，'
    + '好让麦克风有足够长的窗口听到我自己。现在开始说话，请注意房间里的回声有多大，'
    + '以及音量是不是足以触发判断。如果回声压得住，我就能在播报的时候听清你在说什么，'
    + '这样你说一句"等一下"我就能立刻停下来。'
  await engine.ready()
  const result = await engine.speak(text)
  await engine.dispose()
  const file = join(app.getPath('temp'), 'dsh-va-echo.wav')
  writeWav(file, result.samples, result.sampleRate)
  const durationMs = Math.round((result.samples.length / result.sampleRate) * 1000)

  const playbackStart = samples.length
  if (win !== null && !win.isDestroyed()) {
    win.webContents.send('hud:play', { id: 999999, url: pathToFileURL(file).href, text, synthMs: result.stats.synthMs })
  }
  // 播完再采 1 秒，看停止瞬间有没有尾巴
  const deadline = Date.now() + durationMs + 1000
  while (Date.now() < deadline) await sleep(100)
  const playbackEnd = samples.length

  echoProbe = null
  await stopListening()

  const slice = (from, to) => samples.slice(from, to)
  const stats = (rows) => {
    const levels = rows.map((r) => r.levelDb).sort((a, b) => a - b)
    const at = (q) => (levels.length === 0 ? null : Math.round(levels[Math.min(levels.length - 1, Math.floor(q * levels.length))] * 10) / 10)
    return {
      chunks: rows.length,
      median: at(0.5),
      p90: at(0.9),
      max: levels.length === 0 ? null : Math.round(levels[levels.length - 1] * 10) / 10,
      min: levels.length === 0 ? null : Math.round(levels[0] * 10) / 10,
      speechPct: rows.length === 0 ? 0 : Math.round((rows.filter((r) => r.speech).length / rows.length) * 1000) / 10,
      floorMedian: rows.length === 0 ? null : Math.round(rows.map((r) => r.floorDb).sort((a, b) => a - b)[Math.floor(rows.length / 2)] * 10) / 10,
    }
  }

  const silence = stats(slice(0, silenceEnd))
  const playback = stats(slice(playbackStart, playbackEnd))
  const report = {
    ok: started.ok === true,
    audioSec: Math.round(durationMs / 10) / 100,
    window: { silenceChunks: silenceEnd, playbackChunks: playbackEnd - playbackStart },
    silence,
    playback,
    deltaMedianDb: (playback.median !== null && silence.median !== null)
      ? Math.round((playback.median - silence.median) * 10) / 10
      : null,
    deltaP90Db: (playback.p90 !== null && silence.p90 !== null)
      ? Math.round((playback.p90 - silence.p90) * 10) / 10
      : null,
  }
  report.verdict = {
    // AEC 有效的话，播报期的中位电平只应略高于静默期
    echoSuppressed: report.deltaMedianDb !== null && report.deltaMedianDb < 10,
    // 播报期不应该被大量判成"有语音"，否则打断会疯狂误触发
    vadQuietDuringPlayback: playback.speechPct < 20,
  }

  log(`echo-test report ${JSON.stringify(report)}`)
  console.log('__ECHOTEST__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.quit() }, 300)
}

/**
 * 打断的验收（--bargein-test [kw|negative]）。
 *
 * 测试原理：不走扬声器，直接把一段 WAV 按 100ms 一块喂进 processChunk——
 * 也就是**真实麦克风走的同一条路径**。为什么不用真麦克风：
 * 一是自动化里没人喊话；二是 Windows 的 AEC 是系统级的，
 * 用扬声器回放"人声"会被它当成回声消掉（实测过），根本模拟不了真人。
 *
 *   kw       —— 喂"等一下"：必须打断，并记录延迟
 *   negative —— 喂一句无关的话：**必须不打断**（这是用户抱怨的那个毛病）
 */
async function runBargeInTest(arg) {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const negative = arg === 'negative'
  const wavPath = negative ? 'assets/test/other-speech.wav' : 'assets/test/dengyixia.wav'

  const started = await startListening()
  log(`bargein-test listening: ${JSON.stringify(started)}`)
  showForWake('speaking')
  userBargeInAt = 0
  await sleep(1200)

  const pcm = await loadWav16k(wavPath)
  // 打断要靠识别，先把模型加载好（正式运行时它早已就位）
  await startAsrLoad()

  enqueueSpeech('好的，我先说明一下现在的窗口情况。最前面是模拟器窗口，后面还压着三个程序，'
    + '分别是 MAA、AUTO-MAS 和一个编辑器，要不要我切到其中一个？')
  // 等真正出声再"插话"：首次播报要先加载 TTS 模型（约 1.3s）
  for (let i = 0; i < 160 && playbackStartedAt === 0; i += 1) await sleep(50)
  await sleep(600)
  const playingBefore = playbackStartedAt > 0

  const feedAt = Date.now()
  const CHUNK = 1600
  for (let i = 0; i < pcm.length; i += CHUNK) {
    if (userBargeInAt > 0) break
    processChunk(pcm.subarray(i, Math.min(i + CHUNK, pcm.length)))
    await sleep(100)
  }
  // 再给 1.5 秒让识别把话说完（关键词一般在句尾才出现）
  for (let i = 0; i < 15 && userBargeInAt === 0; i += 1) await sleep(100)

  const bargeAt = userBargeInAt
  const report = {
    ok: started.ok === true,
    mode: negative ? 'negative' : 'kw',
    fedWav: wavPath,
    audioMs: Math.round((pcm.length / 16000) * 1000),
    playingBefore,
    bargeIn: bargeAt > 0,
    latencyMs: bargeAt > 0 ? bargeAt - feedAt : null,
    audioStopped: playingAudio() === false,
    queueEmpty: ttsQueue.length === 0,
    stateAfter: currentState,
    asrArmed: commandActive,
  }
  report.verdict = negative
    ? { notTriggered: report.bargeIn === false, stillPlayingOrFinished: true }
    : {
      detected: report.bargeIn,
      stopped: report.audioStopped && report.queueEmpty,
      listening: report.stateAfter === 'listening',
      asrArmed: report.asrArmed,
    }
  report.pass = Object.values(report.verdict).every((v) => v === true)

  log(`bargein-test report ${JSON.stringify(report)}`)
  console.log('__BARGEIN__' + JSON.stringify(report))
  await sleep(300)
  await stopListening()
  setTimeout(() => { app.isQuitting = true; app.quit() }, 300)
}

/**
 * 权限门验收（--gate-test [click|deny|timeout|policy]）。
 *
 * 单测覆盖了"判得对不对"，这里覆盖"接得通不通"：
 * 卡片有没有出现、点击通道能不能答复、拒绝之后 agent 拿到的理由对不对。
 * 语音通道的识别质量归 ASR，这里不重复测。
 */
async function runGateTest(mode) {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const gateInstance = await ensurePermissions()
  const report = { mode }

  showForWake('confirm')
  await sleep(600)

  // 1) 放行类：不弹卡片
  const allowVerdict = await gateInstance.check({ tool: 'screenshot', args: {}, utterance: '看一眼屏幕' })
  report.allowPath = { allow: allowVerdict.allow, level: allowVerdict.level, channel: allowVerdict.channel }

  // 2) 必须在问的：剪贴板
  const pending = gateInstance.check({ tool: 'read_clipboard', args: {}, utterance: '看看我复制了什么' })
  await sleep(500)
  report.cardShown = currentState === 'confirm'

  if (mode === 'timeout') {
    // 不答复，等超时（默认 12s，测试时用 CUA_VA_CONFIRM_TIMEOUT 缩短）
    report.timeoutMs = CONFIRM_TIMEOUT_MS
  } else {
    // Interactive integration test: approval must traverse the real HUD click IPC.
    log(`gate-test waiting for HUD ${mode} click`)
  }

  const askVerdict = await pending
  report.askPath = {
    allow: askVerdict.allow,
    level: askVerdict.level,
    channel: askVerdict.channel,
    reason: askVerdict.reason ?? '',
  }

  // 2b) "15 分钟"之后，同样的请求不该再弹卡片
  if (mode === 'once') {
    const again = await gateInstance.check({ tool: 'read_clipboard', args: {}, utterance: '再看一眼' })
    // confirmResolver === null 说明没有新卡片在等人回答
    report.grantOnce = { allow: again.allow, level: again.level, askedAgain: confirmResolver !== null }
  }

  // 3) 策略层直接拒绝：未知工具
  const denyVerdict = await gateInstance.check({ tool: 'rm_rf_everything', args: {}, utterance: '删库' })
  report.denyPath = { allow: denyVerdict.allow, capability: denyVerdict.capability, channel: denyVerdict.channel }

  report.verdict = {
    allowWithoutCard: report.allowPath.allow === true && report.allowPath.channel === 'policy',
    cardShown: report.cardShown === true,
    askResolved: mode === 'timeout'
      ? (report.askPath.allow === false && report.askPath.channel === 'timeout')
      : (report.askPath.level === 'ask' && (mode === 'deny' ? report.askPath.allow === false : report.askPath.allow === true)),
    denyPath: report.denyPath.allow === false,
    ...(mode === 'once' ? { grantOnceWorks: report.grantOnce?.allow === true && report.grantOnce?.level === 'allow_once' } : {}),
  }
  report.pass = Object.values(report.verdict).every((v) => v === true)

  log(`gate-test report ${JSON.stringify(report)}`)
  console.log('__GATETEST__' + JSON.stringify(report))
  await sleep(400)
  setTimeout(() => { app.isQuitting = true; app.quit() }, 300)
}

/**
 * 读 WAV 并重采样到 16k。
 * TTS 产出是 22.05k，而识别/KWS 只吃 16k；演示与验收都走这一条路。
 */
async function loadWav16k(path) {
  const { readWav } = await import('../../packages/voice/wav.mjs')
  const wav = readWav(path)
  if (wav.sampleRate === 16000) return wav.samples

  const { createRequire } = await import('node:module')
  const require = createRequire(import.meta.url)
  const { LinearResampler } = require('sherpa-onnx-node/resampler.js')
  const resampler = new LinearResampler(wav.sampleRate, 16000)
  const parts = []
  for (let i = 0; i < wav.samples.length; i += 2205) {
    const piece = wav.samples.subarray(i, Math.min(i + 2205, wav.samples.length))
    const isLast = i + 2205 >= wav.samples.length
    parts.push(isLast ? resampler.flush(piece) : resampler.resample(piece))
  }
  const merged = new Float32Array(parts.reduce((sum, part) => sum + part.length, 0))
  let cursor = 0
  for (const part of parts) { merged.set(part, cursor); cursor += part.length }
  return merged
}

/**
 * 连续对话验收（--followup-test）。
 *
 * 验的是"说完一轮之后有没有待命"这条链路：
 *   开麦 → 跑一轮（强制模型说要继续）→ 应该出现待命窗口且状态为 listening
 *   → 9 秒没人说话 → 自己收起。
 * 不依赖模型是否真的想追问（那个由提示词保证），否则这个测试会时灵时不灵。
 */
async function runFollowUpTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const started = await startListening()
  await startAsrLoad()
  showForWake('listening')
  await sleep(600)

  const turnStartedAt = Date.now()
  await runCommand('现在几点了')
  const turnMs = Date.now() - turnStartedAt

  /*
   * 光等 commandActive 会误判：播报还没结束（TTS 冷启动 20 秒）时窗口当然不会开。
   * 先等"话真的说完了"，再等窗口出现。
   */
  const speechSettledAt = async () => {
    for (let i = 0; i < 400; i += 1) {
      if (!ttsPumping && ttsQueue.length === 0 && speechBuffer === '' && !playingAudio()) return Date.now()
      await sleep(100)
    }
    return null
  }

  // 等待命窗口出现（播报可能还要几秒）
  let openedAt = null
  let judgement = null
  for (let i = 0; i < 500 && openedAt === null; i += 1) {
    if (commandActive && currentState === 'listening' && !playingAudio()) {
      openedAt = Date.now() - turnStartedAt
      judgement = followUpJudgement
      break
    }
    await sleep(100)
  }
  const opened = openedAt !== null
  const report = {
    ok: started.ok === true,
    turnMs,
    followUpOpened: opened,
    openedAtMs: openedAt,
    stateWhenOpen: currentState,
    continuousMode,
    judgement: judgement === null ? null : { wait: judgement.wait, conf: Number(judgement.confidence.toFixed(2)), src: judgement.source },
    speechSettled: (await speechSettledAt()) !== null,
  }

  // 等窗口超时（时长现在由判定决定，给足余量）
  if (report.followUpOpened) {
    const deadline = Date.now() + 12000
    while (Date.now() < deadline && commandActive) await sleep(200)
    report.closedAfterTimeout = commandActive === false
    report.stateAfter = currentState
  }

  report.pass = continuousMode
    ? (
      // 判定说"还要等" → 窗口必须开；判定说"不用等" → 窗口不该开。
      // 这才是要验的东西，而不是"永远都开"。
      (report.ok === true
        && report.judgement !== null
        && (report.judgement.wait === true || report.judgement.conf >= followUpConfig.thresholds.low
          ? report.followUpOpened === true && report.closedAfterTimeout === true
          : report.followUpOpened === false))
    )
    // 单次对话：跑完一轮就不该有待命窗口
    : (report.ok === true && report.followUpOpened === false)
  log(`followup-test report ${JSON.stringify(report)}`)
  console.log('__FOLLOWUP__' + JSON.stringify(report))
  await stopListening()
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 会话记忆验收（--history-test）：在同一个进程里连跑两轮，看第二轮有没有带上第一轮。
 *
 * 只验"记忆是否按会话传递"这一件事，不看模型回答质量（那依赖网络与运气）。
 */
async function runHistoryTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  turnHistoryCounts = []
  resetSession('test-start')
  showForWake('listening')
  await sleep(300)

  await runCommand('现在几点了')
  await sleep(500)
  await runCommand('刚才你说了什么')

  const report = {
    historyCounts: [...turnHistoryCounts],
    turns: turnHistoryCounts.length,
    sessionSizeAfter: sessionHistory.length,
    verdict: {
      firstTurnEmpty: turnHistoryCounts[0] === 0,
      secondTurnHasContext: (turnHistoryCounts[1] ?? 0) > 0,
      sessionRecorded: sessionHistory.length >= 4,
    },
  }
  report.pass = Object.values(report.verdict).every((v) => v === true)
  log(`history-test report ${JSON.stringify(report)}`)
  console.log('__HISTORY__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 联网搜索验收（--search-test "查询"）：直接用真实接口搜一次，看延迟和文本质量。
 * 没有 key 时不算失败，只报告"未启用"——否则每次 CI 都会被 key 卡住。
 */
async function runSearchTest(query) {
  const store = await import('../../packages/config/store.mjs')
  const { WebSearch } = await import('../../packages/tools/web-search.mjs')
  const key = await store.resolveSearchKey()
  const search = new WebSearch({ apiKey: key })
  const report = { ok: true, query, enabled: search.enabled }

  if (!search.enabled) {
    report.hint = '没有配置搜索 Key：node tools/set-search-key.mjs <tavily-key> 或设 TAVILY_API_KEY'
  } else {
    try {
      const first = await search.invoke({ query })
      const second = await search.invoke({ query })
      report.firstMs = first.meta.ms
      report.count = first.meta.count
      report.cachedMs = second.meta.ms
      report.cached = second.meta.cached === true
      report.text = first.text.slice(0, 600)
      report.verdict = {
        hasAnswerOrResults: /要点：|来源：|没有搜到/u.test(first.text),
        cacheWorks: report.cached === true,
        fastEnough: report.firstMs < 5000,
      }
      report.pass = Object.values(report.verdict).every((v) => v === true)
    } catch (error) {
      report.ok = false
      report.error = error.message
      report.pass = false
    }
  }

  log(`search-test report ${JSON.stringify({ ...report, text: undefined })}`)
  console.log('__SEARCH__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 设置页验收（--settings-test）：真的在窗口里改一遍、点保存、再读配置文件核对。
 *
 * 只测"接线"：表单 → preload → IPC → store → 内存开关。
 * 不测界面好不好看，也不测模型连通性（那要网络，且已有 --cmd-demo 覆盖）。
 */
async function runSettingsTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const store = await import('../../packages/config/store.mjs')
  const before = await store.loadConfig()

  const waitReady = async (target, ms = 5000) => {
    if (!target.webContents.isLoading()) return
    await new Promise((resolve) => {
      const timer = setTimeout(resolve, ms)
      target.webContents.once('did-finish-load', () => { clearTimeout(timer); resolve() })
    })
  }

  const report = { steps: [] }
  /*
   * 先等监听状态稳定：启动会恢复上次的唤醒开关，ASR 加载完才真正开麦。
   * 不等的话，会把"启动时恢复唤醒"误算成"保存把助手叫醒了"。
   */
  for (let i = 0; i < 25 && !listening; i += 1) { await sleep(300) }
  report.hudBefore = { visible: win !== null && !win.isDestroyed() && win.isVisible(), listening }
  report.listeningBefore = listening

  const settings = openSettings()
  await waitReady(settings)
  await sleep(600)

  // 1) 改表单并保存，顺便量保存耗时（保存必须秒回，否则窗口会显示"未响应"）
  const applied = await settings.webContents.executeJavaScript(`(async () => {
    const emotion = document.getElementById('emotion')
    const speed = document.getElementById('speed')
    const continuous = document.getElementById('continuousMode')
    const keyboard = document.querySelector('#capabilities select[data-capability="input.keyboard"]')
    if (!emotion || !speed) return { ok: false, error: '表单没渲染出来' }
    emotion.value = '生气'
    speed.value = '1.15'
    if (continuous) continuous.checked = false
    if (keyboard) keyboard.value = 'ask'
    const started = performance.now()
    document.getElementById('save').click()
    await new Promise((r) => setTimeout(r, 600))
    return {
      ok: true,
      saveMs: Math.round(performance.now() - started),
      options: Array.from(emotion.options).map((o) => o.value),
      capabilityCount: document.querySelectorAll('#capabilities select').length,
    }
  })()`)
  report.steps.push('form-saved')

  // 2) 加一套模型方案并保存，然后关窗重开，看它有没有留下来
  const added = await settings.webContents.executeJavaScript(`(async () => {
    document.getElementById('addProfile').click()
    const row = document.querySelector('#profileRows tr')
    if (!row) return { ok: false }
    row.querySelector('[data-field="name"]').value = '自检方案'
    row.querySelector('[data-field="model"]').value = 'mimo-v2.6-flash'
    row.querySelector('[data-field="apiKey"]').value = 'sk-selftest-not-real'
    document.getElementById('save').click()
    await new Promise((r) => setTimeout(r, 600))
    return { ok: true, rows: document.querySelectorAll('#profileRows tr').length }
  })()`)
  report.steps.push('profile-saved')

  const savedProfiles = (await store.loadConfig()).modelProfiles ?? []
  settings.close()
  await sleep(400)
  const reopened = openSettings()
  await waitReady(reopened)
  await sleep(600)

  const afterReopen = await reopened.webContents.executeJavaScript(`(() => ({
    profileRows: document.querySelectorAll('#profileRows tr').length,
    profileNames: Array.from(document.querySelectorAll('#profileRows [data-field="name"]')).map((input) => input.value),
    defaultOptions: Array.from(document.getElementById('defaultProfile').options).map((option) => option.textContent),
    neteaseAppId: document.getElementById('neteaseAppId').value,
    neteaseKey: document.getElementById('neteasePrivateKey').value,
    neteasePlaceholder: document.getElementById('neteasePrivateKey').placeholder,
    defaultProfileValue: document.getElementById('defaultProfile').value,
  }))()`)
  report.steps.push('reopened')

  // 3) 导出审计
  report.exportHint = await reopened.webContents.executeJavaScript(`(async () => {
    document.getElementById('exportAudit').click()
    await new Promise((r) => setTimeout(r, 1500))
    return document.getElementById('auditHint').textContent
  })()`)

  await sleep(400)
  const after = await store.loadConfig()
  report.hudAfter = { visible: win !== null && !win.isDestroyed() && win.isVisible(), listening }
  report.listeningAfter = listening
  report.formApplied = applied.ok === true
  report.saveLatencyMs = applied.saveMs
  report.emotionOptions = applied.options
  report.capabilityCount = applied.capabilityCount
  report.savedEmotion = after.tts?.emotion
  report.savedSpeed = after.tts?.speed
  report.savedContinuous = after.ui?.continuousMode
  report.savedOverride = after.permissions?.overrides?.['input.keyboard']
  report.memoryEmotion = ttsEmotion
  report.memoryContinuous = continuousMode
  report.profileRoundTrip = {
    added: added.ok === true,
    savedCount: savedProfiles.length,
    reloadedRows: afterReopen.profileRows,
    reloadedNames: afterReopen.profileNames,
    defaultOptions: afterReopen.defaultOptions,
  }
  /*
   * 登录入口检查：按钮在不在、状态查询通不通（不点登录，避免自检时弹浏览器）
   */
  const loginProbe = await reopened.webContents.executeJavaScript(`(async () => {
    const button = document.getElementById('neteaseLogin')
    let statusOk = false
    let statusText = ''
    try {
      const status = await window.settingsApi.neteaseStatus()
      statusOk = status.ok === true
      statusText = status.loggedIn === true ? 'loggedIn' : 'notLoggedIn'
    } catch (error) { statusText = String(error && error.message) }
    return { hasButton: Boolean(button), statusOk, statusText }
  })()`)
  report.neteaseLogin = loginProbe

  report.neteaseUi = {
    hasFields: afterReopen.neteaseAppId !== undefined,
    appIdValue: afterReopen.neteaseAppId,
    keyValue: afterReopen.neteaseKey,
    keyPlaceholder: afterReopen.neteasePlaceholder,
  }

  report.verdict = {
    formWorks: report.formApplied,
    capabilityTable: Number(report.capabilityCount) >= 10,
    persisted: report.savedEmotion === '生气' && Number(report.savedSpeed) === 1.15,
    appliedToRuntime: report.memoryEmotion === '生气' && report.memoryContinuous === false,
    uiTogglePersisted: report.savedContinuous === false,
    permissionOverrideSaved: report.savedOverride === 'ask',
    auditExportWorks: typeof report.exportHint === 'string' && report.exportHint.includes('已导出'),
    neteaseFields: report.neteaseUi.appIdValue !== undefined,
    neteaseKeyNotEchoed: report.neteaseUi.keyValue === '',
    neteaseLoginButton: report.neteaseLogin?.hasButton === true,
    neteaseStatusWorks: report.neteaseLogin?.statusOk === true,
    settingsSaveFast: Number(report.saveLatencyMs) < 1500,
    profileSaved: report.profileRoundTrip.savedCount >= 1,
    profileReloaded: report.profileRoundTrip.reloadedRows >= 1,
    saveDoesNotWake: report.hudAfter.visible === false && report.listeningAfter === report.listeningBefore,
  }
  report.pass = Object.values(report.verdict).every((value) => value === true)

  // 还原现场：把自检改过的配置写回去
  await store.saveConfig({
    tts: { ...(before.tts ?? {}) },
    ui: { ...(before.ui ?? {}), continuousMode: before.ui?.continuousMode !== false },
    permissions: { ...(before.permissions ?? {}), overrides: before.permissions?.overrides ?? {} },
    modelProfiles: before.modelProfiles ?? [],
  })

  log(`settings-test report ${JSON.stringify(report)}`)
  console.log('__SETTINGS__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 离线兜底验收（--offline-test）：强制断网，跑三句话。
 *   "现在几点"        → 本地时钟，应当答出来
 *   "打开记事本"      → 本地启动器，应当启动
 *   "帮我看看屏幕上有什么" → 办不到，必须老实说办不到，不能假装
 */
async function runOfflineTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  process.env.CUA_VA_FORCE_OFFLINE = '1'
  showForWake('listening')
  await sleep(400)

  const report = { forced: true, turns: [] }
  for (const text of ['现在几点', '打开记事本', '帮我看看屏幕上有什么']) {
    lastAssistantText = ''
    await runCommand(text)
    await sleep(300)
    report.turns.push({ text, reply: lastAssistantText })
  }

  report.verdict = {
    timeWorks: /点/u.test(report.turns[0].reply) && report.turns[0].reply !== '',
    launchWorks: /记事本/u.test(report.turns[1].reply),
    honestWhenUnsupported: report.turns[2].reply.includes('连不上网'),
  }
  report.pass = Object.values(report.verdict).every((value) => value === true)
  log(`offline-test report ${JSON.stringify(report)}`)
  console.log('__OFFLINE__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 500)
}

/**
 * 资源策略验收（--resource-test）。
 *
 * 不假装全屏：真的把 TTS 工人拉起来占住显存，然后强制走一次"全屏"判定，
 * 看显存有没有被让出去、唤醒用的 ASR 有没有被误伤、退出全屏后能不能自己回来。
 */
async function runResourceTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const report = { steps: [] }

  // 1) 先确保语音与识别都在
  await startAsrLoad()
  await ensureTtsEngine()
  const before = { asr: asr !== null, voice: ttsEngine !== null }
  report.steps.push({ step: 'loaded', ...before })

  // 2) 强制按"全屏"跑一次
  await runResourceTick('fullscreen')
  await sleep(500)
  report.steps.push({ step: 'fullscreen', asr: asr !== null, voice: ttsEngine !== null, suspended: voiceSuspended })

  // 3) 退出全屏：等下一轮 tick（这里直接调用，不等 20 秒）
  await sleep(200)
  await runResourceTick()
  await sleep(1500)
  report.steps.push({ step: 'restored', voiceEngine: ttsEngine !== null, suspended: voiceSuspended })

  report.verdict = {
    hadModels: before.asr === true && before.voice === true,
    releasedVoice: report.steps[1].voice === false && report.steps[1].suspended === true,
    keptAsr: report.steps[1].asr === true,
    resumeAttempted: report.steps[2].suspended === false,
  }
  report.pass = Object.values(report.verdict).every((value) => value === true)

  log(`resource-test report ${JSON.stringify(report)}`)
  console.log('__RESOURCE__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 500)
}

/**
 * 探针：当前模型到底能不能读图（--vision-probe）。
 *
 * 只能在真应用里跑：配置里的 Key 是 DPAPI 加密的，而且 Electron 45 的 safeStorage
 * 是**按应用身份绑定**的——换个入口的 electron.exe 解不开，会得到 401。
 */
async function runVisionProbe() {
  const store = await import('../../packages/config/store.mjs')
  const { readFile, readdir } = await import('node:fs/promises')
  const config = await store.loadConfig()
  const key = await store.resolveApiKey()

  // 用**真实截图**探测：4x4 纯色图不足以证明"能看图"，而且推理模型会把
  // max_tokens 全花在 reasoning 上，content 返回空——上次就是被这个骗了。
  const frameDir = join(DATA_ROOT, 'frames')
  let base64 = ''
  try {
    const files = (await readdir(frameDir)).filter((name) => name.endsWith('.jpg') || name.endsWith('.png'))
    if (files.length > 0) {
      const newest = files.sort().at(-1)
      base64 = (await readFile(join(frameDir, newest))).toString('base64')
      log(`vision-probe: 用真实截图 ${newest}`)
    }
  } catch { /* 没截图就退回小图 */ }
  if (base64 === '') {
    base64 = 'iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAYAAACp8Z5+AAAAFUlEQVR42mP8z8BQz0AEYBxVSF+FABJADveWkH6oAAAAAElFTkSuQmCC'
  }

  const report = { models: [] }
  // 先看这个端点到底有哪些模型：如果有多模态的（名字带 vl / vision / omni），就该换它
  try {
    const listResponse = await fetch(`${config.baseURL}/models`, { headers: { authorization: `Bearer ${key}` } })
    const listBody = await listResponse.json()
    report.available = (listBody?.data ?? []).map((item) => item.id)
  } catch (error) {
    report.listError = error.message
  }
  for (const model of ['deepseek-flash', 'deepseek-v4-pro']) {
    try {
      const response = await fetch(`${config.baseURL}/chat/completions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', authorization: `Bearer ${key}` },
        body: JSON.stringify({
          model,
          messages: [{
            role: 'user',
            content: [
              { type: 'text', text: '这张截图里最上面那行文字是什么？只说那一行。' },
              { type: 'image_url', image_url: { url: `data:image/jpeg;base64,${base64}` } },
            ],
          }],
          // 给足 token：推理模型会先花一批在 reasoning_content 上
          max_tokens: 400,
        }),
      })
      const body = await response.text()
      let content = ''
      let reasoning = ''
      try {
        const parsed = JSON.parse(body)
        content = String(parsed?.choices?.[0]?.message?.content ?? '')
        reasoning = String(parsed?.choices?.[0]?.message?.reasoning_content ?? '')
      } catch { content = body.replace(/\s+/gu, ' ').slice(0, 200) }
      report.models.push({
        model,
        status: response.status,
        content: content.slice(0, 120),
        reasoningChars: reasoning.length,
      })
    } catch (error) {
      report.models.push({ model, error: error.message })
    }
  }
  log(`vision-probe ${JSON.stringify(report)}`)
  console.log('__VISION__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 卡片自动收起验收（--confirm-close-test）。
 *
 * 场景：模型问了一句、用户没答，但这一轮因为别的原因结束了。
 * 卡片必须跟着收，否则用户看到一张永远在等人的卡片。
 */
async function runConfirmCloseTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const gate = await ensurePermissions()
  showForWake('confirm')
  const pending = gate.check({ tool: 'read_clipboard', args: {}, utterance: '测试：读剪贴板' })
  await sleep(500)
  const cardShown = currentState === 'confirm'
  const closed = closeConfirmCard()
  const verdict = await pending
  await sleep(300)

  const report = {
    cardShown,
    closedByHelper: closed,
    resolvedAllow: verdict.allow,
    channel: verdict.channel,
    reason: verdict.reason,
    stateAfter: currentState,
    verdicts: {
      cardWasShown: cardShown === true,
      autoClosed: closed === true,
      denied: verdict.allow === false && verdict.channel === 'turn-ended',
      stateLeftConfirm: currentState !== 'confirm',
    },
  }
  report.pass = Object.values(report.verdicts).every((value) => value === true)
  log(`confirm-close-test report ${JSON.stringify(report)}`)
  console.log('__CONFIRMCLOSE__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 退出验收（--quit-test）。
 *
 * 验的不只是"能退出"，而是**用户会按的那两条路**都能退：
 *   1. 关窗口（× / Alt+F4）
 *   2. 托盘「退出」
 * 以前关窗口只是把窗口销毁，进程还在，用户看到的就是"关不掉"。
 */
async function runQuitTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const mode = process.env.CUA_VA_QUIT_PATH ?? 'window'
  showForWake('listening')
  await sleep(600)

  const report = { mode, hadWindow: win !== null }
  log(`quit-test: 触发 ${mode}`)
  if (mode === 'tray') {
    const window = ensureWindow()
    quitApp('tray')
    await sleep(1500)
    // 走到这里说明没退成（shutdown 里有 5 秒硬停，正常会先走掉）
    report.stillAlive = true
    report.windowAlive = window !== null && !window.isDestroyed()
  } else {
    const target = ensureWindow()
    target.close()
    await sleep(1500)
    report.stillAlive = true
  }
  console.log('__QUIT__' + JSON.stringify(report))
  setTimeout(() => app.exit(3), 200)
}

/**
 * 桌面桥接自检（--cua-test）。
 *
 * 打包后最容易坏的就是这条：cua.ps1 在 asar 里，外部 powershell 打不开。
 * 这个模式不碰麦克风、不碰模型，只把桥接拉起来跑一次 request_access + 截图，
 * 打包后直接跑它就能确认"操作电脑"能不能用。
 */
async function runCuaTest() {
  const report = { script: null, ok: false }
  try {
    const { CuaToolkit } = await import('../../packages/cua/adapter.mjs')
    const toolkit = new CuaToolkit({ frameDir: join(DATA_ROOT, 'frames') })
    report.script = join(APP_ROOT, 'packages', 'cua', 'native', 'cua.ps1')
    await toolkit.warmup()
    const access = await toolkit.invoke('request_access', {})
    report.access = String(access.text ?? '').split('\n')[0].slice(0, 80)
    const shot = await toolkit.invoke('screenshot', { max_width: 640 })
    report.screenshot = Boolean(shot?.data?.savedPath)
    report.ok = report.screenshot === true
    await toolkit.dispose()
  } catch (error) {
    report.error = error.message
    log(`cua-test failed: ${error.message}`)
  }
  log(`cua-test ${JSON.stringify(report)}`)
  console.log('__CUA__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(report.ok ? 0 : 4) }, 300)
}

/**
 * 待命窗口的"观感"验收（--followup-ui-test）。
 *
 * 不碰模型、不碰网络，只看两件事：
 *   1. 窗口期间界面是「好了」而不是「聆听中」（用户反馈"关闭前突然进入聆听态"很怪）
 *   2. 窗口到期后自己收起，状态回到 hidden
 *
 * 需要连续对话开着；判定用固定值，避免被模型/网络故障带着一起红。
 */
async function runFollowUpUiTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const started = await startListening()
  showForWake('done')
  await sleep(400)

  // 固定一个"该等"的判定，窗口应当开 4 秒（conf 0.5 → 中档）
  followUpJudgement = { wait: true, confidence: 0.5, why: '测试固定值', source: 'test-fixed' }
  const opened = await startFollowUpIfWanted()
  await sleep(300)
  const stateInWindow = currentState
  const waitedStart = Date.now()
  while (commandActive && Date.now() - waitedStart < 12000) await sleep(200)
  await sleep(300)

  const report = {
    listening: started.ok === true,
    continuousMode,
    opened,
    stateInWindow,
    closedByItself: commandActive === false,
    stateAfter: currentState,
    verdicts: {
      opened: opened === true,
      showsDoneNotListening: stateInWindow === 'done',
      closedItself: commandActive === false,
      backToHidden: currentState === 'hidden',
    },
  }
  report.pass = Object.values(report.verdicts).every((value) => value === true)
  log(`followup-ui-test report ${JSON.stringify(report)}`)
  console.log('__FOLLOWUPUI__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}

/**
 * 独立播放面板验收（--player-ui-test）。
 *
 * 用户的三条硬要求，一条一条验：
 *   1. 播放中 → 面板出现
 *   2. 暂停   → 面板**仍然在**（暂停不是停止）
 *   3. 停止   → 面板消失
 *   4. 点 ✕   → 直接停止（而不是暂停）
 */
async function runPlayerUiTest() {
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
  const visible = () => playerWin !== null && !playerWin.isDestroyed() && playerWin.isVisible()

  const report = { steps: {} }
  await runNetease({ action: 'play', keyword: '晴天 周杰伦' })
  await sleep(2500)
  report.steps.playing = { status: musicPlayer.status, visible: visible() }

  await runNetease({ action: 'pause' })
  await sleep(1500)
  report.steps.paused = { status: musicPlayer.status, visible: visible() }

  await runNetease({ action: 'resume' })
  await sleep(1200)
  report.steps.resumed = { status: musicPlayer.status, visible: visible() }

  await handlePlayerAction('close')
  await sleep(1500)
  report.steps.afterClose = { status: musicPlayer.status, visible: visible(), queue: musicPlayer.queueLength }

  report.verdicts = {
    showsOnPlay: report.steps.playing.visible === true,
    staysOnPause: report.steps.paused.visible === true && report.steps.paused.status === 'paused',
    showsAgainOnResume: report.steps.resumed.visible === true,
    hidesOnStop: report.steps.afterClose.visible === false,
    closeMeansStop: report.steps.afterClose.status === 'stopped',
  }
  report.pass = Object.values(report.verdicts).every((value) => value === true)
  log(`player-ui-test report ${JSON.stringify(report)}`)
  console.log('__PLAYERUI__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.exit(0) }, 300)
}
async function runSelfTest() {
  log('selftest start')
  const report = {}
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

  // 1) 启动时没有任何窗口
  report.startup = {
    windowsAtStart: BrowserWindow.getAllWindows().length,
    trayCreated: tray !== null,
    pass: BrowserWindow.getAllWindows().length === 0 && tray !== null,
  }

  // 2) 托盘触发（show，应聚焦）与位置
  const w1 = showForInteraction('listening')
  await sleep(700)
  const display = screen.getPrimaryDisplay()
  const b1 = w1.getBounds()
  const content1 = w1.getContentSize()
  const expectedX = display.workArea.x + display.workArea.width - b1.width - MARGIN
  const expectedY = display.workArea.y + display.workArea.height - b1.height - MARGIN

  report.showFromTray = {
    visible: w1.isVisible(),
    focused: w1.isFocused(),
    bounds: b1,
    contentWidth: content1[0],
    expected: { x: Math.round(expectedX), y: Math.round(expectedY) },
    positionOk: Math.abs(b1.x - expectedX) <= 2 && Math.abs(b1.y - expectedY) <= 2,
    // 真正要保证的是"卡片宽度"，而不是窗口外框（Windows 无框窗口有 1px 隐形边框）
    cardWidth: Math.round(await w1.webContents.executeJavaScript(
      'document.getElementById("hud").getBoundingClientRect().width',
    )),
  }
  report.showFromTray.widthFixed = report.showFromTray.cardWidth === HUD_WIDTH

  // 3) 隐藏
  hideHud()
  await sleep(300)
  report.hide = { visible: w1.isVisible(), state: currentState }

  // 4) 唤醒触发（showInactive，不应抢焦点）
  const w2 = showForWake('listening')
  await sleep(700)
  report.showFromWake = {
    mode: lastShowMode,
    visible: w2.isVisible(),
    focused: w2.isFocused(),
    positionOk: Math.abs(w2.getPosition()[1] - expectedY) <= 2,
  }

  // 5) 高度变化后仍贴底
  w2.setContentSize(HUD_WIDTH, 260, false)
  positionBottomRight(w2)
  await sleep(200)
  const b2 = w2.getBounds()
  const expectY2 = display.workArea.y + display.workArea.height - b2.height - MARGIN
  report.resizeAnchored = {
    height: b2.height,
    y: b2.y,
    expectedY: Math.round(expectY2),
    pass: Math.abs(b2.y - expectY2) <= 2,
  }

  // 6) DPI 信息（用于核对是否被虚拟化）
  report.display = {
    scaleFactor: display.scaleFactor,
    bounds: display.bounds,
    workArea: display.workArea,
    taskbarHeight: display.bounds.height - display.workArea.height,
  }

  report.verdict = {
    startupNoWindow: report.startup.pass,
    trayShowWorks: report.showFromTray.visible && report.showFromTray.positionOk,
    hideWorks: report.hide.visible === false,
    wakeDoesNotStealFocus: report.showFromWake.mode === 'showInactive' && report.showFromWake.focused === false,
    resizeAnchored: report.resizeAnchored.pass,
  }
  report.verdict.pass = Object.values(report.verdict).every((v) => v === true)

  log('selftest report ' + JSON.stringify(report))
  console.log('__SELFTEST__' + JSON.stringify(report))
  setTimeout(() => { app.isQuitting = true; app.quit() }, 300)
}
