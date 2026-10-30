/**
 * TTS（当前实现：GPT-SoVITS + GSV-TTS-Lite，银狼音色）。
 *
 * 和上一版（sherpa-onnx VITS / bronya）的差别：
 *   - 合成在**独立 Python 进程**里（模型 1.5GB 显存、加载 6.5 秒，不能放主进程反复重建）
 *   - 输出 32kHz，比原来的 22.05kHz 更清楚
 *   - 音色/情绪解耦：换一条参考音频就是换语气，不用重训
 *   - 不再做 compressStress（那是 VITS 专用的重音压缩，GPT-SoVITS 自带韵律，叠两次会变平）
 *
 * 对外接口保持 speak(text) → {samples, sampleRate, stats}，所以调用方（Electron 主进程）不用改。
 */
import { spawn } from 'node:child_process'
import { existsSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { readWav } from './wav.mjs'
import { runtimePath } from '../config/paths.mjs'

const HERE = dirname(fileURLToPath(import.meta.url))

export const TTS_DEFAULTS = {
  pythonPath: runtimePath('venv', 'Scripts', 'python.exe'),
  /**
   * 打包后 worker 脚本不在 asar 里（Python 读不到 asar），由主进程用
   * CUA_VA_WORKER_SCRIPT 指到 resources/gsv_worker.py。
   */
  workerScript: process.env.CUA_VA_WORKER_SCRIPT ?? join(HERE, 'gsv_worker.py'),
  voiceDir: runtimePath('voices', 'v2ProPlus', '银狼'),
  outDir: runtimePath('tts-out'),
  /** 模型加载实测 6.5s；首次运行还要下预训练，给足余量 */
  readyTimeoutMs: 180_000,
  /** 单句合成超时（实测长句 0.7s，卡住多半是显存被抢） */
  synthTimeoutMs: 20_000,
  defaultEmotion: '中立',
}

export class TtsError extends Error {}

export class Tts {
  #cfg
  #child = null
  #pending = new Map()
  #seq = 0
  #readyPromise = null
  #resolveReady = null
  #rejectReady = null
  #stderrTail = []
  #restarts = 0
  #disposed = false

  constructor(options = {}) {
    this.#cfg = { ...TTS_DEFAULTS, ...options }
    if (!existsSync(this.#cfg.pythonPath)) {
      throw new TtsError(`找不到 Python 运行时：${this.#cfg.pythonPath}（GSV-TTS-Lite 环境没装好）`)
    }
    if (!existsSync(this.#cfg.voiceDir)) {
      throw new TtsError(`找不到音色目录：${this.#cfg.voiceDir}`)
    }
    mkdirSync(this.#cfg.outDir, { recursive: true })
  }

  get sampleRate() { return 32000 }
  get voices() { return this.#readyInfo?.voices ?? [] }
  get emotion() { return this.#cfg.defaultEmotion }

  #readyInfo = null

  /** 等 worker 把模型加载好（首次 6.5s）。并发调用共享同一个 Promise。 */
  ready() {
    if (this.#disposed) return Promise.reject(new TtsError('TTS 已关闭'))
    if (this.#readyPromise !== null) return this.#readyPromise

    this.#readyPromise = new Promise((resolve, reject) => {
      this.#resolveReady = resolve
      this.#rejectReady = reject
    })

    const child = spawn(this.#cfg.pythonPath, [
      this.#cfg.workerScript,
      this.#cfg.voiceDir,
      this.#cfg.outDir,
    ], {
      cwd: dirname(this.#cfg.workerScript),
      windowsHide: true,
      env: { ...process.env, PYTHONIOENCODING: 'utf-8' },
    })
    this.#child = child

    const timer = setTimeout(() => {
      this.#failAll(new TtsError(`TTS worker 启动超时（${Math.round(this.#cfg.readyTimeoutMs / 1000)}s）`))
    }, this.#cfg.readyTimeoutMs)

    let buffer = ''
    child.stdout.on('data', (chunk) => {
      buffer += chunk.toString('utf8')
      let newline = buffer.indexOf('\n')
      while (newline >= 0) {
        const line = buffer.slice(0, newline).trim()
        buffer = buffer.slice(newline + 1)
        newline = buffer.indexOf('\n')
        if (line === '') continue
        let message
        try { message = JSON.parse(line) } catch { continue }
        this.#onMessage(message, timer)
      }
    })

    child.stderr.on('data', (chunk) => {
      // Python 的日志（含 gsv_tts 的 INFO）走 stderr，留最后几行用于报错
      const text = chunk.toString('utf8')
      for (const line of text.split('\n')) {
        if (line.trim() === '') continue
        this.#stderrTail.push(line.trim())
        if (this.#stderrTail.length > 12) this.#stderrTail.shift()
      }
    })

    child.on('error', (error) => {
      clearTimeout(timer)
      this.#failAll(new TtsError(`TTS worker 启动失败：${error.message}`))
    })

    child.on('exit', (code, signal) => {
      clearTimeout(timer)
      this.#child = null
      if (this.#disposed) return
      const tail = this.#stderrTail.slice(-3).join(' | ')
      this.#failAll(new TtsError(`TTS worker 退出（code=${code} signal=${signal}）${tail === '' ? '' : `：${tail}`}`))
    })

    return this.#readyPromise
  }

  #onMessage(message, timer) {
    if (message.type === 'ready') {
      clearTimeout(timer)
      this.#readyInfo = message
      this.#resolveReady?.(message)
      return
    }
    if (message.type === 'fatal') {
      clearTimeout(timer)
      this.#failAll(new TtsError(`TTS worker 初始化失败：${message.message}`))
      return
    }
    const pending = this.#pending.get(message.id)
    if (pending === undefined) return
    this.#pending.delete(message.id)
    clearTimeout(pending.timer)
    if (message.type === 'error') pending.reject(new TtsError(message.message))
    else pending.resolve(message)
  }

  #failAll(error) {
    this.#rejectReady?.(error)
    this.#resolveReady = null
    this.#rejectReady = null
    this.#readyPromise = null
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer)
      pending.reject(error)
    }
    this.#pending.clear()
  }

  /**
   * 合成一句。
   * @param {string} text
   * @param {{speed?:number, emotion?:string}} [options]
   * @returns {Promise<{samples:Float32Array, sampleRate:number, stats:object}>}
   */
  async speak(text, options = {}) {
    await this.ready()
    const id = String(++this.#seq)
    const payload = {
      id,
      op: 'speak',
      text,
      emotion: options.emotion ?? this.#cfg.defaultEmotion,
      speed: options.speed ?? 1,
    }

    const startedAt = performance.now()
    const message = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id)
        reject(new TtsError(`合成超时（${Math.round(this.#cfg.synthTimeoutMs / 1000)}s）：${text.slice(0, 20)}`))
      }, this.#cfg.synthTimeoutMs)
      this.#pending.set(id, { resolve, reject, timer })
      try {
        this.#child?.stdin.write(`${JSON.stringify(payload)}\n`)
      } catch (error) {
        clearTimeout(timer)
        this.#pending.delete(id)
        reject(new TtsError(`写入 worker 失败：${error.message}`))
      }
    })

    const { samples, sampleRate } = readWav(message.path)
    const durationSec = message.audioSec ?? samples.length / sampleRate
    return {
      samples,
      sampleRate,
      stats: {
        text,
        emotion: message.emotion,
        synthMs: Math.round(message.ms ?? (performance.now() - startedAt)),
        firstChunkMs: Math.round(message.ms ?? (performance.now() - startedAt)),
        durationSec: Math.round(durationSec * 100) / 100,
        rtf: Math.round(((message.ms ?? 0) / 1000 / durationSec) * 1000) / 1000,
        backend: 'gsv-lite',
      },
    }
  }

  /** 停 worker（应用退出时调用，回收 1.5GB 显存） */
  async dispose() {
    if (this.#disposed) return
    this.#disposed = true
    const child = this.#child
    if (child === null) return
    try { child.stdin.write(`${JSON.stringify({ id: 'bye', op: 'shutdown' })}\n`) } catch { /* 可能已退出 */ }
    await new Promise((resolve) => {
      const timer = setTimeout(() => { try { child.kill() } catch { /* 已退出 */ } resolve() }, 1500)
      child.once('exit', () => { clearTimeout(timer); resolve() })
    })
    this.#child = null
  }
}
