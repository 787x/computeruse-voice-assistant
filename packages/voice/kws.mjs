/**
 * 唤醒词（KWS）。
 *
 * 为什么需要它：VAD 只能判断"有人在说话"，分不清"我"和"室友"。
 * 唤醒词是唯一能在不做声纹的情况下把"在跟我说话"和"别人在聊天"分开的手段。
 */
import { createRequire } from 'node:module'
import { existsSync } from 'node:fs'
import { join } from 'node:path'

import { modelPath } from '../config/paths.mjs'

const require = createRequire(import.meta.url)
const sherpa = require('sherpa-onnx-node')

export const KWS_DEFAULTS = {
  modelDir: modelPath('sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01'),
  keywordsFile: modelPath('kws-keywords.txt'),
  keywordsScore: 1.0,
  keywordsThreshold: 0.25,
  numTrailingBlanks: 2,
  numThreads: 2,
}

export class Kws {
  #spotter
  #stream

  constructor(options = {}) {
    const cfg = { ...KWS_DEFAULTS, ...options }
    if (!existsSync(cfg.modelDir)) throw new Error(`KWS 模型目录不存在: ${cfg.modelDir}`)
    if (!existsSync(cfg.keywordsFile)) throw new Error(`关键词文件不存在: ${cfg.keywordsFile}`)

    this.#spotter = new sherpa.KeywordSpotter({
      featConfig: { sampleRate: 16000, featureDim: 80 },
      modelConfig: {
        transducer: {
          encoder: join(cfg.modelDir, 'encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx'),
          decoder: join(cfg.modelDir, 'decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx'),
          joiner: join(cfg.modelDir, 'joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx'),
        },
        tokens: join(cfg.modelDir, 'tokens.txt'),
        numThreads: cfg.numThreads,
        provider: 'cpu',
        debug: false,
      },
      keywordsFile: cfg.keywordsFile,
      keywordsScore: cfg.keywordsScore,
      keywordsThreshold: cfg.keywordsThreshold,
      numTrailingBlanks: cfg.numTrailingBlanks,
    })
    this.#stream = this.#spotter.createStream()
    this.options = cfg
  }

  get sampleRate() { return 16000 }

  /**
   * 喂一段 16k 单声道 float32 音频，返回命中的关键词（没有则 null）。
   * @param {Float32Array} samples
   * @param {number} sampleRate
   */
  accept(samples, sampleRate = 16000) {
    if (sampleRate !== 16000) {
      throw new Error(`KWS 只接受 16kHz，收到 ${sampleRate}`)
    }
    this.#stream.acceptWaveform({ sampleRate, samples })

    let hit = null
    while (this.#spotter.isReady(this.#stream)) {
      this.#spotter.decode(this.#stream)
      const result = this.#spotter.getResult(this.#stream)
      if (result?.keyword && result.keyword !== '' && result.keyword !== 'undefined') {
        hit = result.keyword
      }
    }
    return hit
  }

  /**
   * 收尾：告知流"没有更多音频了"，并把剩余帧解码完。
   * 离线测试必须调用，否则最后一段的检测结果不会被吐出。
   */
  finish() {
    this.#stream.inputFinished()
    let hit = null
    let guard = 0
    while (this.#spotter.isReady(this.#stream) && guard < 10000) {
      this.#spotter.decode(this.#stream)
      const result = this.#spotter.getResult(this.#stream)
      if (result?.keyword && result.keyword !== '') hit = result.keyword
      guard += 1
    }
    return hit
  }

  reset() {
    this.#stream = this.#spotter.createStream()
  }
}
