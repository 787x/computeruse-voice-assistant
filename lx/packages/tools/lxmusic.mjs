/**
 * 音乐播放：走 LX Music（lx-music-desktop）的开放 API + Scheme URL。
 *
 * 为什么换成 LX：
 *   之前那套（ncm-cli + 自己签名直连 + mpv）里，CLI 的播放守护进程不稳、
 *   签名直连要自己管 token（24 小时就过期，还得扫码），mpv 又要单独装。
 *   LX Music 自带播放内核和音源，对外只暴露两样东西：
 *     1. 本地 HTTP 开放 API（默认 http://127.0.0.1:23330）——查状态、控制播放；
 *     2. Scheme URL（lxmusic://…）——搜索并播放、切歌、收藏等。
 *   我们只当"遥控器"，不再自己取流、不再自己管 token。
 *
 * 文档：https://lxmusic.toside.cn/desktop/open-api 、 /desktop/scheme-url
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join } from 'node:path'

const BASE = process.env.CUA_VA_LX_BASE ?? 'http://127.0.0.1:23330'
const TIMEOUT_MS = 2500

/** LX 的状态值：playing / paused / error / stoped（注意官方拼写就是 stoped） */
const STOPPED = new Set(['stoped', 'error'])

async function get(pathAndQuery) {
  const response = await fetch(`${BASE}${pathAndQuery}`, { signal: AbortSignal.timeout(TIMEOUT_MS) })
  if (!response.ok) throw new Error(`LX API ${response.status}`)
  return response.text()
}

/** LX 在不在跑（启动时探一次，决定要不要提示用户先开 LX） */
export async function lxAvailable() {
  try {
    await get('/status?filter=status')
    return true
  } catch {
    return false
  }
}

/**
 * 播放器状态。
 * 字段名对齐我们自己的面板，免得再各处做映射。
 */
export async function lxStatus() {
  const raw = await get('/status?filter=status,name,singer,albumName,duration,progress,volume,mute,lyricLineText,picUrl')
  const data = JSON.parse(raw)
  const status = String(data.status ?? 'stoped')
  return {
    ok: true,
    status: STOPPED.has(status) ? 'stopped' : status,
    rawStatus: status,
    title: String(data.name ?? ''),
    artist: String(data.singer ?? ''),
    album: String(data.albumName ?? ''),
    duration: Number(data.duration) || 0,
    position: Number(data.progress) || 0,
    volume: data.volume === undefined ? null : Number(data.volume),
    mute: data.mute === true,
    lyric: String(data.lyricLineText ?? ''),
    cover: typeof data.picUrl === 'string' ? data.picUrl : '',
  }
}

/** 控制类接口（GET，无参数或 query 参数） */
const CONTROL_PATH = {
  play: '/play',
  pause: '/pause',
  next: '/skip-next',
  prev: '/skip-prev',
  collect: '/collect',
  uncollect: '/uncollect',
}

/**
 * 打开 scheme URL。
 * 用系统协议处理器（cmd start）而不是 Electron 的 shell，工具层不依赖 electron，
 * 这样自检脚本也能直接跑。
 */
/*
 * scheme 的打开方式可注入：
 *   - 应用里跑 → 主进程注入 electron 的 shell.openExternal（等同浏览器点链接，最可靠）
 *   - 命令行自检 → 退回系统 ShellExecute（cmd start）
 * 直接在命令行里用 cmd start 证明不了应用里也会失败——两者走的 ShellExecute 上下文不同。
 */
let schemeOpener = null

/** 主进程注入：用 electron 的 shell.openExternal 打开 scheme */
export function setSchemeOpener(opener) {
  schemeOpener = opener
}

function openScheme(url) {
  if (typeof schemeOpener === 'function') {
    try {
      const result = schemeOpener(url)
      return Promise.resolve(result !== false)
    } catch {
      return Promise.resolve(false)
    }
  }
  return new Promise((resolve) => {
    try {
      const child = spawn('cmd', ['/c', 'start', '', url], { windowsHide: true, detached: true, stdio: 'ignore' })
      child.on('error', () => resolve(false))
      child.on('spawn', () => { child.unref(); resolve(true) })
    } catch {
      resolve(false)
    }
  })
}

/** scheme URL 里参数要单独编码，否则带 / 或空格会被截断 */
export function schemeUrl(path, params = {}) {
  if (Object.keys(params).length === 0) return `lxmusic://${path}`
  const data = encodeURIComponent(JSON.stringify(params))
  return `lxmusic://${path}?data=${data}`
}

/**
 * 按"歌名 歌手"搜索并播放。
 * 走 music/searchPlay：LX 会自己挑音源、自己取流、自己播。
 */
export async function playByKeyword(keyword) {
  const text = String(keyword ?? '').trim()
  if (text === '') throw new Error('要播放什么？给我个歌名')
  // name-singer 形式；用户说"晴天 周杰伦"时拆成 名字-歌手 更准
  const [name, ...rest] = text.split(/\s+/u)
  const singer = rest.join(' ')
  const target = singer === '' ? name : `${name}-${singer}`
  const url = `lxmusic://music/searchPlay/${encodeURIComponent(target)}`
  const opened = await openScheme(url)
  if (!opened) throw new Error('没能唤起 LX Music（确认它已安装并在运行）')

  // 等它真的开始播：最多 8 秒，命中就返回真实状态
  const deadline = Date.now() + 8000
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 400))
    try {
      const status = await lxStatus()
      if (status.status === 'playing' && status.title !== '') return status
    } catch { /* LX 还没反应过来，继续等 */ }
  }
  // 没等到也别报错：LX 可能弹了搜索结果让用户挑
  try {
    return await lxStatus()
  } catch {
    throw new Error('LX 没响应，点开 LX 看看是不是卡住了')
  }
}

/** 按 LX 可执行文件路径启动（用户没开 LX 时尝试拉起） */
export function launchLx() {
  const candidates = [
    process.env.LX_MUSIC_EXE,
    join(process.env.LOCALAPPDATA ?? '', 'Programs', 'lx-music-desktop', 'lx-music-desktop.exe'),
    'C:\\Program Files\\lx-music-desktop\\lx-music-desktop.exe',
    join(process.env.LOCALAPPDATA ?? '', 'lx-music-desktop', 'lx-music-desktop.exe'),
  ].filter((item) => typeof item === 'string' && item !== '')
  const exe = candidates.find((path) => existsSync(path))
  if (exe === undefined) return false
  try {
    const child = spawn(exe, [], { windowsHide: false, detached: true, stdio: 'ignore' })
    child.unref()
    return true
  } catch {
    return false
  }
}

/**
 * 工具定义：给模型看的。
 *
 * 必须是 { type:'function', function:{ name, description, parameters } } 的嵌套格式，
 * 和其他工具（shell/files/…）保持一致——写成平铺的会被模型 API 422 拒收
 * （实测报 "tools[19]: missing field `function`"）。
 */
export const LX_MUSIC_TOOL = {
  type: 'function',
  function: {
    name: 'music',
    description: [
      '控制 LX Music 播放音乐。用户说"放首歌/听xxx/暂停/下一首/声音小点/唱到哪了"都用它。',
      'play 只要给 keyword（歌名，可带歌手，例如"晴天 周杰伦"），会搜索并直接播放。',
      'state 返回当前歌曲、进度、音量；lyric 返回当前歌词行。',
      'LX 没有"停止"，close/stop 用 pause 代替。',
    ].join(''),
    parameters: {
      type: 'object',
      properties: {
        action: {
          type: 'string',
          enum: ['play', 'pause', 'resume', 'toggle', 'next', 'prev', 'seek', 'volume', 'state', 'lyric', 'collect'],
          description: '动作',
        },
        keyword: { type: 'string', description: 'play 时必填：歌名，可带歌手' },
        seconds: { type: 'number', description: 'seek 时必填：跳到第几秒' },
        level: { type: 'number', description: 'volume 时必填：0-100' },
      },
      required: ['action'],
      additionalProperties: false,
    },
  },
}

/**
 * 工具入口：返回 { text, data }。
 * text 是给模型/用户看的一句话，data 是结构化结果（面板也用）。
 */
export async function runLx(args = {}) {
  const action = String(args.action ?? '')
  try {
    if (action === 'play' || action === 'resume') {
      /*
       * 语义区分：
       *   play 带 keyword  → 搜索并播放（scheme 的 music/searchPlay）
       *   play 不带 keyword / resume → 继续播放（开放 API 的 GET /play）
       * 之前 resume 没实现，"恢复播放"会报"要播放什么？给我个歌名"。
       */
      const keyword = String(args.keyword ?? '').trim()
      if (action === 'resume' || keyword === '') {
        await get('/play')
        const status = await lxStatus()
        return { text: '继续放', data: { ok: true, ...status } }
      }
      const status = await playByKeyword(keyword)
      const title = status.title === '' ? keyword : `${status.title}${status.artist === '' ? '' : ` - ${status.artist}`}`
      return {
        text: status.status === 'playing' ? `正在放：${title}` : `已让 LX 搜索「${keyword}」，当前状态 ${status.rawStatus}${title === '' ? '' : `（${title}）`}`,
        data: { ok: true, ...status },
      }
    }
    if (action === 'toggle') {
      // 用 scheme：LX 官方就有"切换播放/暂停"
      await openScheme('lxmusic://player/togglePlay')
      await new Promise((resolve) => setTimeout(resolve, 400))
      const status = await lxStatus()
      return { text: status.status === 'playing' ? '继续放了' : '暂停了', data: { ok: true, ...status } }
    }
    if (action === 'seek') {
      const seconds = Math.max(0, Number(args.seconds) || 0)
      await get(`/seek?offset=${Math.round(seconds)}`)
      return { text: `跳到 ${Math.round(seconds)} 秒`, data: { ok: true } }
    }
    if (action === 'volume') {
      const level = Math.min(100, Math.max(0, Number(args.level) || 0))
      await get(`/volume?volume=${Math.round(level)}`)
      return { text: `音量 ${Math.round(level)}`, data: { ok: true } }
    }
    if (action === 'collect') {
      await get('/collect')
      return { text: '收藏了', data: { ok: true } }
    }
    if (CONTROL_PATH[action] !== undefined) {
      await get(CONTROL_PATH[action])
      const label = { play: '继续放', pause: '暂停了', next: '下一首', prev: '上一首' }[action] ?? action
      return { text: label, data: { ok: true } }
    }
    if (action === 'state' || action === 'lyric') {
      const status = await lxStatus()
      if (action === 'lyric') {
        return {
          text: status.lyric === '' ? '这会儿没有歌词' : status.lyric,
          data: { ok: true, ...status },
        }
      }
      if (status.status === 'stopped') return { text: '现在没在放歌', data: { ok: true, ...status } }
      const progress = `${Math.round(status.position)}s/${Math.round(status.duration)}s`
      return {
        text: `${status.status === 'playing' ? '在放' : '暂停中'}「${status.title}${status.artist === '' ? '' : ` - ${status.artist}`}」${progress}`,
        data: { ok: true, ...status },
      }
    }
    return { text: `不认识的音乐动作：${action}`, data: { ok: false, error: 'unknown action' } }
  } catch (error) {
    const hint = /fetch|ECONNREFUSED|timeout|LX API/iu.test(error.message)
      ? '（LX Music 好像没开，或者没开「开放 API 服务」）'
      : ''
    return { text: `音乐操作失败：${error.message}${hint}`, data: { ok: false, error: error.message } }
  }
}
