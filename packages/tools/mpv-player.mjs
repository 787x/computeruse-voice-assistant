/**
 * 极简 mpv 播放器：直接放 URL，不依赖 ncm-cli 的守护进程。
 *
 * 为什么要有它：ncm-cli 0.1.7 的 PlayerDaemon 在 Windows 上时好时坏，
 * 而它的**读接口**一直正常（song/detail/get/v2 能拿到 playUrl）。
 * 于是"取地址"用它的接口，"出声"我们自己来——把不稳定的那一环绕过去。
 *
 * 与 mpv 的通信走命名管道（--input-ipc-server），JSON 行协议：
 *   { "command": ["loadfile", url] }
 *   { "command": ["set_property", "pause", true] }
 * 用管道而不是杀进程/发信号，是因为要能暂停、跳转、读进度。
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { connect } from 'node:net'
import { join } from 'node:path'

const PIPE_NAME = '\\\\.\\pipe\\voiceassist-mpv'
/**
 * mpv 可执行文件位置。
 * 不靠 PATH：应用常常是开机自启的，进程环境里的 PATH 可能还是装 mpv 之前那份。
 */
function resolveMpv() {
  const candidates = [
    process.env.MPV_EXE,
    'C:\\Program Files\\MPV Player\\mpv.exe',
    'C:\\Program Files\\mpv\\mpv.exe',
    join(process.env.LOCALAPPDATA ?? '', 'Programs', 'mpv', 'mpv.exe'),
  ].filter((item) => typeof item === 'string' && item !== '')
  const found = candidates.find((path) => existsSync(path))
  return found ?? 'mpv'
}

class MpvPlayer {
  #process = null
  #socket = null
  #queue = []
  #index = 0
  #startedAt = 0
  #duration = 0
  #paused = false
  #connected = null

  /** 当前状态（给浮窗/state 用） */
  get status() {
    if (this.#process === null) return 'stopped'
    if (this.#paused) return 'paused'
    return 'playing'
  }

  get current() {
    return this.#queue[this.#index] ?? null
  }

  get position() {
    if (this.#process === null || this.#startedAt === 0) return 0
    return Math.max(0, (Date.now() - this.#startedAt) / 1000)
  }

  get duration() {
    return this.#duration > 0 ? this.#duration : (Number(this.current?.duration) || 0) / 1000
  }

  get queueLength() {
    return this.#queue.length
  }

  /** 起一个 mpv 进程并连上它的 IPC */
  async #ensureProcess() {
    if (this.#process !== null) return
    this.#process = spawn(resolveMpv(), [
      '--no-video', '--no-terminal', '--really-quiet',
      '--force-window=no',
      `--input-ipc-server=${PIPE_NAME}`,
      '--idle=yes',
    ], { windowsHide: true, stdio: 'ignore' })
    this.#process.on('error', (error) => {
      this.#process = null
      this.#socket = null
      this.#connected = null
      // 不抛异常（异步事件里抛会直接崩进程），把原因记在 lastError 里供上层读取
      this.lastError = `启动 mpv 失败（${error.message}）——先安装 mpv 并确认路径`
    })
    this.#process.on('exit', () => {
      this.#process = null
      this.#socket = null
      this.#connected = null
      this.#startedAt = 0
      this.#paused = false
    })
    // 等管道就绪再连接
    for (let i = 0; i < 40; i += 1) {
      try {
        this.#connected = this.#connect()
        await this.#connected
        return
      } catch {
        await new Promise((resolve) => setTimeout(resolve, 50))
      }
    }
  }

  #connect() {
    return new Promise((resolve, reject) => {
      const socket = connect(PIPE_NAME)
      socket.once('connect', () => {
        this.#socket = socket
        socket.on('data', (chunk) => this.#onData(chunk))
        resolve()
      })
      socket.once('error', reject)
    })
  }

  #onData(chunk) {
    for (const line of chunk.toString('utf8').split('\n')) {
      if (line.trim() === '') continue
      try {
        const message = JSON.parse(line)
        if (message.event === 'file-loaded' && Number.isFinite(Number(message.duration))) {
          this.#duration = Number(message.duration)
        } else if (message.event === 'end-file') {
          // 播完自动下一首
          if (message.reason === 'eof') void this.next()
        } else if (Number.isFinite(Number(message.duration))) {
          this.#duration = Number(message.duration)
        }
      } catch { /* 忽略非 JSON 行 */ }
    }
  }

  #send(command) {
    if (this.#socket === null) return
    try { this.#socket.write(`${JSON.stringify({ command })}\n`) } catch { /* 管道已断 */ }
  }

  /** 播放一首：song 是 { id, name, artist, duration } */
  async playSong(song) {
    await this.#ensureProcess()
    this.#startedAt = Date.now()
    this.#paused = false
    this.#duration = (Number(song?.duration) || 0) / 1000
    this.#send(['loadfile', song.url, 'replace'])
  }

  /** 替换整个队列并播放第一首 */
  async playQueue(songs) {
    this.#queue = songs.filter((song) => typeof song?.url === 'string' && song.url !== '')
    this.#index = 0
    if (this.#queue.length === 0) throw new Error('没有可播放的地址')
    await this.playSong(this.#queue[0])
  }

  async append(song) {
    this.#queue.push(song)
  }

  async next() {
    if (this.#index + 1 >= this.#queue.length) {
      this.stop()
      return
    }
    this.#index += 1
    await this.playSong(this.#queue[this.#index])
  }

  async prev() {
    if (this.#index === 0) {
      await this.playSong(this.#queue[0])
      return
    }
    this.#index -= 1
    await this.playSong(this.#queue[this.#index])
  }

  pause() {
    this.#paused = true
    this.#send(['set_property', 'pause', true])
  }

  resume() {
    this.#paused = false
    this.#send(['set_property', 'pause', false])
  }

  seek(seconds) {
    this.#startedAt = Date.now() - Math.max(0, Number(seconds) || 0) * 1000
    this.#send(['seek', Number(seconds) || 0, 'absolute'])
  }

  setVolume(level) {
    this.#send(['set_property', 'volume', Math.min(100, Math.max(0, Number(level) || 0))])
  }

  stop() {
    this.#queue = []
    this.#index = 0
    this.#startedAt = 0
    this.#paused = false
    if (this.#process !== null) {
      try { this.#process.kill() } catch { /* 已退出 */ }
      this.#process = null
    }
    this.#socket = null
    this.#connected = null
  }
}

/** 单例：一个进程里只要一个 mpv */
export const player = new MpvPlayer()
