/**
 * 流式语音识别（ASR）。
 *
 * 三个模块的分工，别混：
 *   VAD  —— 有没有人在说话（不看内容）
 *   KWS  —— 是不是在叫我（只看几个字）
 *   ASR  —— 说了什么（这才是能执行的东西）
 *
 * 为什么必须是流式（在线）模型而不是离线模型：
 *   字幕要跟着说话同步出现。离线模型必须等整句说完才吐字，
 *   用户体验上等于"我闭嘴之后它才开始反应"，延迟感极强。
 *
 * 端点检测（endpoint）：模型自己判断"这句说完了"（尾部静音够长），
 *   比外挂 VAD 更准——它知道最后一个字有没有说完，VAD 只知道有没有声音。
 */
import { createRequire } from 'node:module'
import { existsSync, readdirSync } from 'node:fs'
import { join } from 'node:path'

import { modelPath } from '../config/paths.mjs'

const require = createRequire(import.meta.url)
const sherpa = require('sherpa-onnx-node')

export const ASR_DEFAULTS = {
  modelDir: modelPath('sherpa-onnx-x-asr-480ms-zh-en-punct-int8-2026-06-05'),
  numThreads: 2,
  decodingMethod: 'greedy_search',
  enableEndpoint: true,
  // 规则 1：说完一句（尾部静音 2.4s）——给"我还没说完"留足犹豫时间
  rule1MinTrailingSilence: 2.4,
  // 规则 2：出现标点后（尾部静音 1.2s）——逗号后面不算说完，但句号后面基本是
  rule2MinTrailingSilence: 1.2,
  // 规则 3：单句最长 15s，防止有人一直说导致内存和延迟爬升
  rule3MinUtteranceLength: 15,
  // 收尾补的静音：直接截断时模型会吞掉最后一个字（实测），补 0.4s 稳定
  tailPaddingSec: 0.4,
  hotwordsFile: '',
  hotwordsScore: 1.5,
}

/** 在一个模型目录里按优先级挑权重：int8 优先（体积小、CPU 快），其次 fp32/fp16 */function pickModel(dir, base) {
  const files = readdirSync(dir).filter((f) => f.startsWith(base) && f.endsWith('.onnx'))
  if (files.length === 0) throw new Error(`${dir} 里没有 ${base}*.onnx`)
  const int8 = files.find((f) => f.includes('int8'))
  return join(dir, int8 ?? files.sort()[0])
}

export class Asr {
  #recognizer
  #stream
  #cfg
  #partial = ''
  #firstSpeechAt = 0
  #firstPartialAt = 0
  #decodes = 0

  constructor(options = {}) {
    const cfg = { ...ASR_DEFAULTS, ...options }
    if (!existsSync(cfg.modelDir)) throw new Error(`ASR 模型目录不存在: ${cfg.modelDir}`)
    const tokens = join(cfg.modelDir, 'tokens.txt')
    if (!existsSync(tokens)) throw new Error(`ASR 缺少 tokens.txt: ${cfg.modelDir}`)

    this.#cfg = cfg
    this.modelFiles = {
      encoder: pickModel(cfg.modelDir, 'encoder'),
      decoder: pickModel(cfg.modelDir, 'decoder'),
      joiner: pickModel(cfg.modelDir, 'joiner'),
      tokens,
    }

    const modelConfig = {
      transducer: this.modelFiles,
      tokens,
      numThreads: cfg.numThreads,
      provider: 'cpu',
      debug: false,
    }
    if (cfg.hotwordsFile !== '' && existsSync(cfg.hotwordsFile)) {
      modelConfig.modelingUnit = 'bpe'
      const bpe = join(cfg.modelDir, 'bpe.model')
      if (existsSync(bpe)) modelConfig.bpeVocab = bpe
    }

    const recognizerConfig = {
      featConfig: { sampleRate: 16000, featureDim: 80 },
      modelConfig,
      decodingMethod: cfg.decodingMethod,
      enableEndpoint: cfg.enableEndpoint,
      rule1MinTrailingSilence: cfg.rule1MinTrailingSilence,
      rule2MinTrailingSilence: cfg.rule2MinTrailingSilence,
      rule3MinUtteranceLength: cfg.rule3MinUtteranceLength,
    }
    if (modelConfig.bpeVocab !== undefined) {
      recognizerConfig.hotwordsFile = cfg.hotwordsFile
      recognizerConfig.hotwordsScore = cfg.hotwordsScore
    }

    this.#recognizer = new sherpa.OnlineRecognizer(recognizerConfig)
    this.#stream = this.#recognizer.createStream()
    this.options = cfg
  }

  get sampleRate() { return 16000 }

  /** 当前未定稿的文字（字幕用这个，逐帧刷新） */
  get partial() { return this.#partial }

  /** 从开始喂音频到吐出第一段文字花了多久（毫秒），字幕延迟的核心指标 */
  get firstPartialMs() {
    return this.#firstPartialAt === 0 || this.#firstSpeechAt === 0
      ? -1
      : this.#firstPartialAt - this.#firstSpeechAt
  }

  /**
   * 喂一块 16k 单声道 float32。
   * @returns {{partial:string, final:string|null, endpointMs:number}}
   *   final 非 null 表示模型判定这句说完了，可以送去执行
   */
  push(samples, sampleRate = 16000) {
    if (sampleRate !== 16000) throw new Error(`ASR 只接受 16kHz，收到 ${sampleRate}`)
    if (this.#firstSpeechAt === 0) this.#firstSpeechAt = Date.now()

    this.#stream.acceptWaveform({ sampleRate, samples })
    this.#decode()

    const text = this.#currentText()
    if (text !== '' && this.#firstPartialAt === 0) this.#firstPartialAt = Date.now()
    this.#partial = text

    if (text !== '' && this.#recognizer.isEndpoint(this.#stream)) {
      // 端点：这句结束了。复用同一个 stream 前必须 Reset，否则上一句的
      // 假设会作为下一句的上下文继续解码（表现为"新句子带上一句的尾巴"）
      this.#recognizer.reset(this.#stream)
      this.#partial = ''
      this.#firstSpeechAt = 0
      this.#firstPartialAt = 0
      return { partial: '', final: text, endpointMs: 0 }
    }

    return { partial: text, final: null, endpointMs: 0 }
  }

  /**
   * 收尾：用户松手/超时/确认说完了，但模型还没判定端点时调用。
   * 返回这段里剩下的文字（可能为空）。
   */
  flush() {
    const pad = new Float32Array(Math.round(this.#cfg.tailPaddingSec * 16000))
    this.#stream.acceptWaveform({ sampleRate: 16000, samples: pad })
    this.#decode()
    this.#stream.inputFinished()
    this.#decode()

    const text = this.#currentText()
    // inputFinished 之后 stream 不能复用（标志位不会自己清），直接换新的
    this.#stream = this.#recognizer.createStream()
    this.#partial = ''
    this.#firstSpeechAt = 0
    this.#firstPartialAt = 0
    return text
  }

  /**
   * 开一条新的识别流，共用同一个模型（不额外占内存）。
   *
   * 为什么需要多条流：播报期间要同时听"等一下"，而主流程那条流可能正在等用户说指令。
   * 一条流只能有一个解码状态，两条互不干扰。
   */
  newSession() { return new AsrSession(this.#recognizer) }

  /** 丢掉当前这句（误唤醒时用） */
  reset() {
    this.#recognizer.reset(this.#stream)
    this.#partial = ''
    this.#firstSpeechAt = 0
    this.#firstPartialAt = 0
  }

  get stats() { return { decodes: this.#decodes, firstPartialMs: this.firstPartialMs } }

  #decode() {
    let guard = 0
    while (this.#recognizer.isReady(this.#stream) && guard < 1000) {
      this.#recognizer.decode(this.#stream)
      this.#decodes += 1
      guard += 1
    }
  }

  #currentText() {
    const result = this.#recognizer.getResult(this.#stream)
    return tidy((result?.text ?? ''))
  }
}

/**
 * 一条独立的识别流（共享同一个模型实例）。
 *
 * 生命周期自己在端点处打断语：一句说完就 Reset 重开，
 * 调用方只需要一直喂音频、看 partial 里有没有出现要等的那几个字。
 */
export class AsrSession {
  #recognizer
  #stream
  #partial = ''

  constructor(recognizer) {
    this.#recognizer = recognizer
    this.#stream = recognizer.createStream()
  }

  get partial() { return this.#partial }

  /**
   * @param {Float32Array} samples 16k 单声道
   * @returns {{partial:string, final:string|null}}
   */
  push(samples, sampleRate = 16000) {
    if (sampleRate !== 16000) throw new Error(`ASR 只接受 16kHz，收到 ${sampleRate}`)
    this.#stream.acceptWaveform({ sampleRate, samples })
    let guard = 0
    while (this.#recognizer.isReady(this.#stream) && guard < 200) {
      this.#recognizer.decode(this.#stream)
      guard += 1
    }
    const text = tidy(this.#recognizer.getResult(this.#stream)?.text ?? '')
    this.#partial = text

    if (text !== '' && this.#recognizer.isEndpoint(this.#stream)) {
      this.#recognizer.reset(this.#stream)
      this.#partial = ''
      return { partial: '', final: text }
    }
    return { partial: text, final: null }
  }

  reset() {
    this.#recognizer.reset(this.#stream)
    this.#partial = ''
  }
}

/**
 * 整理模型输出：它会在中文标点后留一个半角空格（"打开， 然后"）。
 * 字幕里那个空格很扎眼，但英文单词之间的空格必须留着，所以只清标点两侧。
 */
export function tidy(text) {
  return text
    .replace(/undefined/g, '')
    // 中文标点两侧的半角空格："打开， 然后" → "打开，然后"
    .replace(/[ \t]+([，。！？、；：）」』】])/g, '$1')
    .replace(/([，。！？、；：])[ \t]+/g, '$1')
    .replace(/([（「『【])[ \t]+/g, '$1')
    .trim()
}

/**
 * 按配置挑模型：优先中文专用（大小可控），找不到就用默认目录。
 * 放在这里而不是 store 里，是因为模型选择跟着"机器上装了哪个包"走。
 */
export function resolveModelDir(candidate) {
  if (candidate && existsSync(candidate)) return candidate
  const root = 'models'
  if (!existsSync(root)) return ASR_DEFAULTS.modelDir
  const dirs = readdirSync(root, { withFileTypes: true })
    .filter((d) => d.isDirectory() && d.name.includes('streaming') && d.name.includes('zh'))
    .map((d) => join(root, d.name))
  const withTokens = dirs.filter((d) => existsSync(join(d, 'tokens.txt')))
  return withTokens[0] ?? ASR_DEFAULTS.modelDir
}
