/**
 * 网易云音乐工具：包装官方 `@music163/ncm-cli`。
 *
 * 为什么包 CLI 而不是自己调 HTTP：鉴权（AppID + RSA 私钥签名）、登录态、
 * 播放器（mpv）、内容审核都在 CLI 里。自己重写等于复制一份会过期的实现。
 *
 * 为什么用 `node <dist/index.js>` 而不是 `ncm-cli.cmd`：
 * Windows 上 Node 18+ 不允许 spawnSync 直接执行 .cmd（EINVAL），
 * 走 shell 又要处理私钥里的 + / = 转义。直接跑 JS 入口最稳。
 *
 * 0.1.7 的真实能力（实测 `ncm-cli commands`）：
 *   play / pause / resume / stop / next / prev / seek / volume /
 *   queue / state / login / logout / config / cloudupload / cloud / diag / tui
 * **没有 search**（官方文档写了但 CLI 还没实现），所以"放某首歌"需要 ID 或链接。
 */
import { execFile, spawn, spawnSync } from 'node:child_process'
import { player } from './mpv-player.mjs'
import { callApi, captureIdentity, makeSpyEnv, hasIdentity } from './netease-api.mjs'
import { existsSync } from 'node:fs'
import { join } from 'node:path'
import { createHash } from 'node:crypto'

export const NETEASE_DEFAULTS = {
  timeoutMs: 30000,
  maxOutputChars: 2500,
}

/** CLI 的 JS 入口：优先用 npm 全局安装目录 */
export function resolveCliEntry() {
  const candidates = []
  if (process.env.CUA_VA_NCM_ENTRY) candidates.push(process.env.CUA_VA_NCM_ENTRY)
  const appData = process.env.APPDATA
  if (appData) candidates.push(join(appData, 'npm', 'node_modules', '@music163', 'ncm-cli', 'dist', 'index.js'))
  candidates.push(join(process.cwd(), 'node_modules', '@music163', 'ncm-cli', 'dist', 'index.js'))
  return candidates.find((path) => existsSync(path)) ?? null
}

/**
 * 子进程环境：**必须剥掉代理变量**。
 *
 * 这是踩了一整轮的坑：ncm-cli 的播放守护进程要通过 openapi.music.163.com 换播放地址，
 * 一旦继承 HTTP(S)_PROXY，请求会绕到本地代理上，3 秒内回不来，
 * CLI 就报「daemon 无响应（3s 超时）」然后静默失败——听起来像 CLI 的 bug，其实是环境变量。
 * 网易云接口在国内直连就够，不需要代理。
 */
function childEnv() {
  const env = { ...process.env }
  for (const key of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']) {
    delete env[key]
  }
  /*
   * 把 mpv 目录补进 PATH。
   * ncm-cli 的 daemon 靠 `mpv` 这个名字去起播放器；而应用进程是开机时启动的，
   * 它的 PATH 可能还是装 mpv 之前的那一份（用户 PATH 改了但进程没重启）。
   * 少了它，play 会"成功发出"却一点声音都没有。
   */
  const extra = ['C:\\Program Files\\MPV Player', 'C:\\Program Files\\mpv']
  const existing = String(env.PATH ?? env.Path ?? '')
  const missing = extra.filter((dir) => existsSync(join(dir, 'mpv.exe')) && !existing.includes(dir))
  if (missing.length > 0) {
    env.PATH = `${missing.join(';')};${existing}`
    delete env.Path
  }
  /*
   * 关键：打包后 process.execPath 是「语音助手.exe」而不是 node。
   * 不设 ELECTRON_RUN_AS_NODE，这些子进程会当成"又开了一次助手"启动，
   * 撞上单实例锁 → 主实例收到 second-instance → 窗口自己弹出来（用户看到的就是"保存就唤醒"）。
   * 实测日志：一次保存触发了 4 次这样的启动。
   */
  return { ...env, NO_COLOR: '1', ELECTRON_RUN_AS_NODE: '1' }
}

/** 执行一条 ncm-cli 命令（argv 数组，不经过 shell） */
export function runCli(argv, options = {}) {
  const cfg = { ...NETEASE_DEFAULTS, ...options }
  const entry = cfg.entry ?? resolveCliEntry()
  if (entry === null) {
    throw new Error('没找到 ncm-cli：先 `npm i -g @music163/ncm-cli`（或设 CUA_VA_NCM_ENTRY 指向 dist/index.js）')
  }
  const started = Date.now()
  const result = spawnSync(process.execPath, [entry, ...argv], {
    encoding: 'utf8',
    windowsHide: true,
    timeout: cfg.timeoutMs,
    shell: false,
    env: childEnv(),
  })
  const elapsed = Date.now() - started
  const raw = `${result.stdout ?? ''}${result.stderr ?? ''}`.trim()
  const text = raw.length > cfg.maxOutputChars ? `${raw.slice(0, cfg.maxOutputChars)}\n…（截断，共 ${raw.length} 字）` : raw
  return { code: result.status, text, raw, elapsedMs: elapsed }
}

/** 异步版：给"每秒轮询播放状态"这种高频调用用，别同步阻塞主进程 */
export function runCliAsync(argv, options = {}) {
  const cfg = { ...NETEASE_DEFAULTS, ...options }
  const entry = cfg.entry ?? resolveCliEntry()
  if (entry === null) return Promise.reject(new Error('没找到 ncm-cli'))
  return new Promise((resolve) => {
    execFile(process.execPath, [entry, ...argv], {
      encoding: 'utf8',
      windowsHide: true,
      signal: cfg.signal,
      timeout: cfg.timeoutMs,
      maxBuffer: 4 * 1024 * 1024,
      env: childEnv(),
    }, (error, stdout, stderr) => {
      const raw = `${stdout ?? ''}${stderr ?? ''}`.trim()
      resolve({ code: error === null ? 0 : 1, text: raw, raw })
    })
  })
}
/** 解析 JSON 输出（CLI 默认就是 json） */
function parseJson(raw) {
  try { return JSON.parse(raw) } catch { return null }
}

export const NETEASE_TOOL = {
  type: 'function',
  function: {
    name: 'netease_music',
    description: '网易云音乐：按歌名/ID 播放、暂停/继续/停止、上下曲、音量、进度、播放队列、播放状态、歌单、每日推荐、歌词、红心、账号。'
      + '用户只说歌名时**直接传 keyword**（例如 "晴天 周杰伦"），工具会自己搜第一首能播的再播，不用问他要 ID。'
      + '要放多首就依次 play 第一首、queue add 其余的。',
    parameters: {
      type: 'object',
      properties: {
        action: {
          type: 'string',
          enum: ['search', 'play', 'pause', 'resume', 'stop', 'next', 'prev', 'seek', 'volume', 'queue', 'state', 'login', 'logout', 'playlist', 'recommend', 'lyric', 'like', 'user'],
        },
        keyword: { type: 'string', description: '歌名/歌手（search 用；play 时给了它就不用给 id）' },
        id: { type: 'string', description: '歌曲/歌单 ID（加密 32 位 hex 或原始数字 ID）' },
        original_id: { type: 'string', description: '歌曲原始数字 ID（play 单曲时和 id 一起给）' },
        url: { type: 'string', description: '音频 URL（play / queue add 时可用）' },
        kind: { type: 'string', enum: ['song', 'playlist'], description: 'action=play 时的资源类型，默认 song' },
        seconds: { type: 'number', description: 'action=seek 的目标秒数' },
        level: { type: 'number', description: 'action=volume 的音量 0-100' },
        op: { type: 'string', enum: ['list', 'add', 'clear'], description: 'action=queue 时的子操作，默认 list' },
        user_input: { type: 'string', description: '用户原话（CLI 的内容审核上下文）' },
      },
      required: ['action'],
      additionalProperties: false,
    },
  },
}

/**
 * 把工具参数翻译成 CLI 命令。
 * 返回 { argv, needsLogin } 或抛错（参数不全时给模型能照做的提示）。
 */
export function buildCommand(args = {}) {
  const action = String(args.action ?? '')
  const userInput = String(args.user_input ?? '').slice(0, 200)
  const audit = userInput === '' ? [] : ['--userInput', userInput]

  switch (action) {
    case 'search': {
      const kind = ['song', 'all', 'playlist', 'album', 'mv', 'podcast', 'voice'].includes(String(args.kind))
        ? String(args.kind)
        : 'song'
      const keyword = String(args.keyword ?? args.id ?? '').trim()
      if (keyword === '') throw new Error('search 需要 keyword（歌名/歌手/专辑名）')
      const limit = Number.isFinite(Number(args.limit)) ? Math.min(30, Math.max(1, Math.round(Number(args.limit)))) : 5
      return { argv: ['search', kind, '--keyword', keyword, '--limit', String(limit), ...audit] }
    }
    case 'play': {
      const kind = args.kind === 'playlist' ? 'playlist' : 'song'
      const url = typeof args.url === 'string' ? args.url.trim() : ''
      if (url !== '') return { argv: ['play', url, ...audit] }
      const id = typeof args.id === 'string' ? args.id.trim() : ''
      if (id === '') throw new Error('play 需要 id 或 url：先用 search 拿到 id 和 originalId')
      const isEncrypted = /^[0-9a-f]{32}$/iu.test(id)
      const originalId = typeof args.original_id === 'string' || typeof args.original_id === 'number'
        ? String(args.original_id).trim()
        : ''
      // 官方要求单曲同时给两种 ID，缺一个就会播不起来
      if (kind === 'song') {
        const encrypted = isEncrypted ? id : ''
        const original = originalId !== '' ? originalId : (isEncrypted ? '' : id)
        if (encrypted === '' || original === '') {
          throw new Error('play 单曲需要 encryptedId + originalId 两个 ID（search 结果里的 id 和 originalId）')
        }
        return { argv: ['play', '--song', '--encrypted-id', encrypted, '--original-id', original, ...audit] }
      }
      return { argv: ['play', '--playlist', isEncrypted ? '--encrypted-id' : '--original-id', id, ...audit] }
    }
    case 'pause': return { argv: ['pause', ...audit] }
    case 'resume': return { argv: ['resume', ...audit] }
    case 'stop': return { argv: ['stop', ...audit] }
    case 'next': return { argv: ['next', ...audit] }
    case 'prev': return { argv: ['prev', ...audit] }
    case 'seek': {
      const seconds = Number(args.seconds)
      if (!Number.isFinite(seconds) || seconds < 0) throw new Error('seek 需要 seconds（≥0）')
      return { argv: ['seek', String(Math.round(seconds)), ...audit] }
    }
    case 'volume': {
      const level = Number(args.level)
      if (!Number.isFinite(level)) throw new Error('volume 需要 level（0-100）')
      return { argv: ['volume', String(Math.min(100, Math.max(0, Math.round(level)))), ...audit] }
    }
    case 'queue': {
      const op = String(args.op ?? 'list')
      if (op === 'clear') return { argv: ['queue', 'clear', ...audit] }
      if (op === 'add') {
        const url = typeof args.url === 'string' ? args.url.trim() : ''
        if (url !== '') return { argv: ['queue', 'add', url, ...audit] }
        const id = typeof args.id === 'string' ? args.id.trim() : ''
        const originalId = String(args.original_id ?? '').trim()
        if (id === '') throw new Error('queue add 需要 url 或 id（可以先用 search 拿 id）')
        const isEncrypted = /^[0-9a-f]{32}$/iu.test(id)
        // CLI 的 queue add 要的是 ID 标志位，不是位置参数
        const argv = ['queue', 'add']
        if (isEncrypted) argv.push('--encrypted-id', id)
        else argv.push('--original-id', id)
        if (originalId !== '') argv.push('--original-id', originalId)
        return { argv: [...argv, ...audit] }
      }
      return { argv: ['queue', ...audit] }
    }
    case 'state': return { argv: ['state'] }
    case 'playlist': {
      const op = String(args.op ?? 'collected')
      switch (op) {
        case 'create': {
          const name = String(args.name ?? '').trim()
          if (name === '') throw new Error('playlist create 需要 name')
          return { argv: ['playlist', 'create', '--name', name, ...audit] }
        }
        case 'add': {
          const playlistId = String(args.id ?? '').trim()
          const songId = String(args.song_id ?? '').trim()
          if (playlistId === '' || songId === '') throw new Error('playlist add 需要 id（歌单）和 song_id（歌曲加密ID）')
          return { argv: ['playlist', 'add', '--playlist-id', playlistId, '--song-id', songId, ...audit] }
        }
        case 'tracks': {
          const playlistId = String(args.id ?? '').trim()
          if (playlistId === '') throw new Error('playlist tracks 需要 id')
          return { argv: ['playlist', 'tracks', '--id', playlistId, ...audit] }
        }
        case 'created': return { argv: ['playlist', 'created', ...audit] }
        case 'collected': return { argv: ['playlist', 'collected', ...audit] }
        default: throw new Error(`playlist 不支持的 op「${op}」`)
      }
    }
    case 'recommend': {
      const kind = ['daily', 'fm', 'heartbeat'].includes(String(args.op)) ? String(args.op) : 'daily'
      const limit = Number.isFinite(Number(args.limit)) ? Math.min(30, Math.max(1, Math.round(Number(args.limit)))) : 10
      return { argv: ['recommend', kind, '--limit', String(limit), ...audit] }
    }
    case 'lyric': {
      const songId = String(args.id ?? '').trim()
      if (songId === '') throw new Error('lyric 需要 id（歌曲加密ID）')
      return { argv: ['song', 'lyric', '--id', songId] }
    }
    case 'like': {
      const songId = String(args.id ?? '').trim()
      const unlike = args.like === false
      if (songId === '') throw new Error('like 需要 id（歌曲加密ID）')
      return { argv: ['song', unlike ? 'dislike' : 'like', '--id', songId, ...audit] }
    }
    case 'user': return { argv: ['user', 'info'] }
    case 'login': return { argv: ['login', '--background'] }
    case 'logout': return { argv: ['logout'] }
    default:
      throw new Error(`netease_music: 不支持的 action「${action}」`)
  }
}

/** 把 CLI 的 JSON 输出翻成人话（模型读起来更省 token） */
export function describeResult(action, payload) {
  if (payload === null) return ''
  if (action === 'state' && payload.state !== undefined) {
    const state = payload.state
    const title = state.title === undefined ? '' : `「${state.title}」`
    const duration = Number(state.duration) > 0 ? `/${Math.round(Number(state.duration))}s` : ''
    return `状态 ${state.status}${title}，进度 ${Math.round(Number(state.position) || 0)}s${duration}，队列 ${state.queueLength} 首`
  }
  if (action === 'login') {
    // CLI 给的是 qrCodeUrl / clickableUrl（不是 url）；优先可点链接
    return String(payload.clickableUrl ?? payload.qrCodeUrl ?? payload.url ?? payload.message ?? '').trim()
  }
  if (payload.success === false) return `失败：${payload.message ?? '未知原因'}`
  if (action === 'play' && payload.message !== undefined) return String(payload.message)
  return ''
}

/** 搜索结果 → 紧凑清单（不要把整段 JSON 念给用户，也别塞满上下文） */
export function describeSearch(payload, limit = 5) {
  const records = payload?.data?.records ?? payload?.data?.bestMatchResources ?? []
  if (!Array.isArray(records) || records.length === 0) return '没搜到。'
  const lines = []
  for (const record of records.slice(0, limit)) {
    const song = record.resource ?? record
    const artists = (song.artists ?? song.fullArtists ?? []).map((a) => a.name).filter(Boolean).join('/')
    const playable = song.visible !== false
    const dur = Number(song.duration) > 0 ? `${Math.round(Number(song.duration) / 1000)}s` : ''
    lines.push(`${lines.length + 1}. ${song.name}${artists === '' ? '' : ` - ${artists}`}${dur === '' ? '' : ` (${dur})`}`
      + `${playable ? '' : ' [不可播]'} id=${song.id} oid=${song.originalId}`)
  }
  return lines.join('\n')
}

/** 搜索结果缓存：实测一次 search 要 11 秒，同一句话重复搜很亏 */
const searchCache = new Map()
const SEARCH_TTL_MS = 60_000

function searchCached(keyword, limit, options) {
  const key = `${keyword}::${limit}`
  const hit = searchCache.get(key)
  if (hit !== undefined && Date.now() - hit.at < SEARCH_TTL_MS) return hit.payload
  const result = runCli(['search', 'song', '--keyword', keyword, '--limit', String(limit)], options)
  const payload = parseJson(result.raw)
  if (payload !== null) searchCache.set(key, { at: Date.now(), payload })
  return payload
}
/** 找第一首可播放的结果 */
export function pickPlayable(payload) {
  const records = payload?.data?.records ?? []
  for (const record of records) {
    if (record?.visible === false) continue
    if (typeof record?.id === 'string' && record.id !== '') return record
  }
  return null
}

/**
 * 主入口：执行一个音乐动作。
 * 登录态相关的结果会带 needsLogin，模型据此引导用户点链接登录。
 */
/** 读本地配置里的 appId/私钥（直连播放要用它签名） */
async function loadCredentials(options) {
  if (options.appId !== undefined && options.privateKey !== undefined) return options
  const store = await import('../config/store.mjs')
  const config = await store.loadConfig()
  return { ...options, appId: config.netease?.appId ?? '', privateKey: config.netease?.privateKey ?? '' }
}

/**
 * 自己签名的搜索：CLI 的 search 单次要 11 秒（它每次都先拉 manifest），
 * 直连接口只要 300ms 左右。签名规则和 detail 一样。
 */
async function searchDirect(keyword, limit, options) {
  const creds = await loadCredentials(options)
  if (creds.appId === '' || creds.privateKey === '') return null
  const word = String(keyword ?? '').trim()
  if (word === '') return null
  const cacheKey = `direct:${word}:${limit}`
  const hit = searchCache.get(cacheKey)
  if (hit !== undefined && Date.now() - hit.at < SEARCH_TTL_MS) return hit.payload
  const result = await callApi('/openapi/music/basic/search/song/get/v3', {
    keyword: word,
    limit: String(limit),
    offset: 0,
    qualityFlag: false,
    trialScene: 'cli',
  }, creds)
  if (result.body?.data !== undefined) searchCache.set(cacheKey, { at: Date.now(), payload: result.body })
  return result.body
}
/** 用官方接口取播放地址（detail 接口的 withUrl 会给 playUrl，比 playurl 接口权限宽松） */
async function resolveSong({ id, keyword, options }) {
  const creds = await loadCredentials(options)
  if (creds.appId === '' || creds.privateKey === '') throw new Error('还没配置网易云 appId/私钥')

  let songId = String(id ?? '').trim()
  if (songId === '' && typeof keyword === 'string' && keyword.trim() !== '') {
    // 先看登录态：没登录就别浪费 11 秒去抓 token
  if (loginStatus(options).loggedIn !== true) return { error: '还没登录网易云音乐（设置 → 网易云音乐 → 登录网易云）', needLogin: true }
  const payload = (await searchDirect(keyword.trim(), 5, options)) ?? searchCached(keyword.trim(), 5, options)
    const pick = pickPlayable(payload)
    if (pick === null) return null
    songId = String(pick.id)
  }
  if (songId === '') return null

  const detail = await callApi('/openapi/music/basic/song/detail/get/v2', { songId, withUrl: true }, creds)
  const data = detail.body?.data
  if (data === undefined || data === null || typeof data.playUrl !== 'string' || data.playUrl === '') {
    return { error: detail.body?.message ?? '没拿到播放地址（可能是版权/VIP 限制）', data }
  }
  return {
    song: {
      id: data.id,
      name: data.name,
      artist: data.artistName,
      duration: data.duration,
      url: data.playUrl,
      cover: data.coverImgUrl,
    },
  }
}

/**
 * 直连播放：自己取地址 + 自己喂 mpv，绕开 ncm-cli 那个不稳定的守护进程。
 * 这也是现在的主路径——实测 CLI 的 play 命令十次有八次超时。
 */
async function playDirect(args, options) {
  const resolved = await resolveSong({ id: args.id, keyword: args.keyword, options })
  if (resolved === null) return { text: `没搜到能放的「${args.keyword ?? args.id}」。`, data: { playing: false } }
  if (resolved.error !== undefined) return { text: `放不了：${resolved.error}`, data: { playing: false } }

  const extra = []
  // 一次给多首时（keyword 搜出多条），把后面的排进队列
  if (Array.isArray(args.playlist_ids)) {
    for (const songId of args.playlist_ids.slice(0, 20)) {
      const item = await resolveSong({ id: songId, options })
      if (item?.song !== undefined) extra.push(item.song)
    }
  }
  await player.playQueue([resolved.song, ...extra])
  await new Promise((resolve) => setTimeout(resolve, 1500))
  return {
    text: `正在放：${resolved.song.name}${resolved.song.artist ? ` - ${resolved.song.artist}` : ''}（${player.status}）`,
    data: { playing: player.status === 'playing', song: resolved.song, status: player.status },
  }
}

/** 播放控制：优先用我们自己的 mpv（直连模式的产物），没在放才回落到 CLI */
async function controlPlayer(action, args) {
  const active = player.status !== 'stopped'
  if (!active) return null
  if (action === 'pause') { player.pause(); return { text: '已暂停', data: { status: player.status } } }
  if (action === 'resume') { player.resume(); return { text: '继续放', data: { status: player.status } } }
  if (action === 'stop') { player.stop(); return { text: '停了', data: { status: player.status } } }
  if (action === 'next') { await player.next(); return { text: '下一首', data: { status: player.status } } }
  if (action === 'prev') { await player.prev(); return { text: '上一首', data: { status: player.status } } }
  if (action === 'seek') { player.seek(args.seconds); return { text: `跳到 ${Math.round(Number(args.seconds) || 0)}s`, data: {} } }
  if (action === 'volume') { player.setVolume(args.level); return { text: `音量 ${Math.round(Number(args.level) || 0)}`, data: {} } }
  return null
}

export async function runNetease(args = {}, options = {}) {
  const action = String(args.action ?? '')

  if (action === 'play') {
    return { ...(await playDirect(args, options)), needsLogin: false }
  }
  const controlled = await controlPlayer(action, args)
  if (controlled !== null) return { ...controlled, needsLogin: false }

  // 剩下的动作走 CLI（搜索/推荐/歌词/歌单/账号这些读接口一直稳定）
  let effective = args
  let searchNote = ''
  if (action === 'play' && (args.id === undefined || args.id === '') && typeof args.keyword === 'string' && args.keyword.trim() !== '') {
    const keyword = args.keyword.trim()
    const searchResult = runCli(['search', 'song', '--keyword', keyword, '--limit', '5'], options)
    const payload = parseJson(searchResult.raw)
    const pick = pickPlayable(payload)
    if (pick === null) {
      return {
        text: `没搜到能放的「${keyword}」。`,
        needsLogin: false,
        data: { search: 'no-playable' },
      }
    }
    searchNote = `选中：${pick.name}${(pick.artists ?? [])[0]?.name ? ` - ${(pick.artists ?? [])[0].name}` : ''}`
    effective = { ...args, id: pick.id, original_id: String(pick.originalId) }
  }

  const { argv } = buildCommand(effective)
  // 搜索走缓存：CLI 的 search 单次要 11 秒，同一句话重复搜很亏
  const cachedSearch = action === 'search'
    ? ((await searchDirect(String(args.keyword ?? '').trim(), Number(args.limit ?? 5), options)) ?? searchCached(String(args.keyword ?? '').trim(), Number(args.limit ?? 5), options))
    : null
  const result = cachedSearch !== null
    ? { code: 0, text: JSON.stringify(cachedSearch), raw: JSON.stringify(cachedSearch), elapsedMs: 0 }
    : runCli(argv, options)
  const payload = parseJson(result.raw)
  let summary = describeResult(action, payload)

  // 播完等一下，把真实状态带回去——避免"已发送播放"但其实没响
  let stateNote = ''
  if (action === 'play' && payload?.success !== false) {
    await new Promise((resolve) => setTimeout(resolve, 1200))
    const stateResult = runCli(['state'], options)
    const statePayload = parseJson(stateResult.raw)
    stateNote = describeResult('state', statePayload)
    if (statePayload?.state?.status === 'stopped') {
      stateNote += '（没响：先确认 mpv 在 PATH 里、且没有代理变量）'
    }
  }

  const needsLogin = payload !== null && payload.success === false && /登录|login|未授权/iu.test(String(payload.message ?? ''))
  const text = [
    searchNote,
    summary,
    action === 'search' ? describeSearch(payload) : '',
    stateNote,
    result.text === '' && stateNote === '' ? '（没有输出）' : '',
  ].filter((line) => line !== '').join('\n')

  return {
    text,
    needsLogin,
    data: { argv, code: result.code, elapsedMs: result.elapsedMs, payload },
  }
}

/** 登录状态（不抛错） */
/**
 * 前台登录会话：自己盯着 CLI 的 login 进程，链接一出来就交给回调。
 *
 * 为什么不用 `login --background`：那会把轮询丢给一个游离进程，
 * 我们既看不到它是否还活着，也拿不到"扫码成功后 token 到底写没写进去"。
 * 实测就出现过浏览器说登录成功、本地状态还是未登录（轮询进程早死了/链接是旧的）。
 * 前台模式由我们自己持有子进程，退出码就是结论。
 */
export function startLoginSession(options = {}, handlers = {}) {
  const entry = options.entry ?? resolveCliEntry()
  if (entry === null) return { ok: false, error: '没找到 ncm-cli' }
  /*
   * 登录时顺便抓"设备身份"：login 成功后 CLI 会带新 token 请求一次 profile，
   * 那条请求里就有 device + accessToken，正好是直连播放要的。
   * 否则登录完还得再跑一次 play 探针——而 token 刚写入时探针可能还拿不到。
   */
  let spy = null
  try { spy = makeSpyEnv('login') } catch { spy = null }
  const child = spawn(process.execPath, [entry, 'login'], {
    windowsHide: true,
    env: spy === null ? childEnv() : spy.env,
  })
  let url = ''
  let output = ''
  const onChunk = (chunk) => {
    const text = String(chunk)
    output += text
    if (url === '') {
      const match = text.match(/https?:\/\/[^\s"'）)]+/u)
      if (match !== null) {
        url = match[0]
        handlers.onUrl?.(url)
      }
    }
  }
  child.stdout?.on('data', onChunk)
  child.stderr?.on('data', onChunk)
  child.on('error', (error) => { handlers.onDone?.({ ok: false, error: error.message }) })
  child.on('exit', (code) => {
    const loggedIn = code === 0
    let identity = null
    if (loggedIn && spy !== null) {
      try { identity = spy.read() } catch { identity = null }
    }
    clearLoginCache()
    handlers.onDone?.({
      ok: loggedIn,
      url,
      code,
      identityCaptured: identity !== null,
      error: loggedIn ? null : (output.trim().split('\n').at(-1) ?? '登录未完成').slice(0, 160),
    })
  })
  return { ok: true, url, kill: () => { try { child.kill() } catch { /* 已退出 */ } } }
}
/** 登录态缓存 30 秒：每次查都要起一个 CLI 子进程，问太勤会把响应拖慢 */
let loginCache = { at: 0, result: null }

export function loginStatus(options = {}) {
  if (loginCache.result !== null && Date.now() - loginCache.at < 30_000) return loginCache.result
  /*
   * 判定顺序很重要：
   *   1. 手上有登录身份（device + accessToken）→ 直接算已登录。
   *      这才是直连播放真正依赖的东西，比 CLI 自己的 --check 可信。
   *   2. 没有身份才去问 CLI。
   * 之前只信 CLI 的 --check：token 在本地被判过期时它一律报未登录，
   * 而那时直连接口其实还能用——用户看到的就是"明明登录了却说没登录"。
   */
  if (hasIdentity()) {
    const status = { loggedIn: true, text: '已登录（本机身份有效）', nickname: '' }
    loginCache = { at: Date.now(), result: status }
    return status
  }
  const result = runCli(['login', '--check'], options)
  const payload = parseJson(result.raw)
  const status = {
    loggedIn: payload?.success === true && !/未登录/u.test(String(payload.message ?? '')),
    text: payload === null ? result.text : String(payload.message ?? ''),
    nickname: String(payload?.nickname ?? payload?.data?.nickname ?? ''),
  }
  loginCache = { at: Date.now(), result: status }
  return status
}

/** 登录成功后清缓存，下一次查询立刻反映真实状态 */
export function clearLoginCache() {
  loginCache = { at: 0, result: null }
}

/**
 * 异步版同步：给主进程的"保存设置"用。
 *
 * 为什么必须异步：spawnSync 会把主进程整个卡住——设置窗口随即变成"未响应"。
 * 实测每次保存都要等 2~4 次 CLI 调用（0.5~11 秒），用户体感就是点保存后界面冻住。
 */
export async function syncCredentialsAsync(credentials = {}, options = {}) {
  options.signal?.throwIfAborted()
  const appId = String(credentials.appId ?? '').trim()
  const privateKey = String(credentials.privateKey ?? '').trim()
  /*
   * 关键：`config get privateKey` 返回的是打码值（MIIEvAIB***），
   * 拿它跟真私钥比对永远不相等 → 每次保存设置都会重写私钥；
   * 而重写凭据会把已有登录态顶掉（实测：登录成功 24 秒后 token 就 Expired）。
   * 所以用"上次同步内容的哈希"判断要不要写。
   */
  const hash = createHash('sha256').update(`${appId}:${privateKey}`).digest('hex').slice(0, 16)
  if (typeof options.syncedHash === 'string' && options.syncedHash === hash) {
    return { synced: [], hash, skipped: true }
  }
  const synced = []
  for (const [key, value] of [['appId', appId], ['privateKey', privateKey]]) {
    options.signal?.throwIfAborted()
    if (value === '') continue
    const current = await runCliAsync(['config', 'get', key], options)
    options.signal?.throwIfAborted()
    if (key === 'appId' && current.code === 0 && current.raw.trim() === value) continue
    const set = await runCliAsync(['config', 'set', key, value], options)
    options.signal?.throwIfAborted()
    if (set.code !== 0) {
      const error = new Error(`网易云凭据字段 ${key} 同步失败（CLI 非零退出）`)
      error.code = 'CREDENTIAL_SYNC_FAILED'
      error.synced = [...synced]
      throw error
    }
    synced.push(key)
  }
  return { synced, hash }
}
/** 把本地配置里的凭证同步给 CLI（不一致时才写） */
export function syncCredentials(credentials = {}, options = {}) {
  const appId = String(credentials.appId ?? '').trim()
  const privateKey = String(credentials.privateKey ?? '').trim()
  const synced = []
  for (const [key, value] of [['appId', appId], ['privateKey', privateKey]]) {
    if (value === '') continue
    const current = runCli(['config', 'get', key], options)
    if (current.raw.includes(value.slice(0, 24))) continue
    const set = runCli(['config', 'set', key, value], options)
    if (set.code === 0) synced.push(key)
  }
  return synced
}
