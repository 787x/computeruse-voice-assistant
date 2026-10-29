/**
 * 流式语音前端：把每 100ms 一块的 16k PCM 喂进来，输出电平/语音标记/唤醒命中。
 *
 * 与离线版（vad.mjs / agc.mjs）的关系：
 *   离线版对整段音频跑状态机；流式版不能等整段，所以按块处理：
 *   - 噪声底：用同一套跟踪器，逐块更新
 *   - 增益：块级平滑（攻击/释放），不做样本级插值
 *   - 唤醒：KWS 本身就是流式模型，直接喂
 */
import { Kws } from './kws.mjs'
import { NoiseFloorTracker } from './agc.mjs'

export const STREAM_DEFAULTS = {
  targetDb: -22,
  maxGainDb: 24,
  minGainDb: -6,
  attackMs: 10,
  releaseMs: 200,
  vadOnDb: 12,
  vadOffDb: 7,
}

export class StreamingPipeline {
  #kws
  #tracker
  #cfg
  #gainDb = 0
  #speaking = false
  #chunks = 0

  constructor(options = {}) {
    this.#cfg = { ...STREAM_DEFAULTS, ...options }
    this.#kws = options.disableKws === true ? null : new Kws(options.kwsOptions)
    this.#tracker = new NoiseFloorTracker({ initDb: -60 })
  }

  get sampleRate() { return 16000 }

  /**
   * @param {Float32Array} chunk 16k 单声道，建议 1600 采样/块
   * @returns {{levelDb:number, floorDb:number, speech:boolean, gainDb:number, wake:string|null}}
   */
  push(chunk) {
    this.#chunks += 1
    const dt = chunk.length / 16000

    let sum = 0
    for (let i = 0; i < chunk.length; i += 1) sum += chunk[i] * chunk[i]
    const levelDb = 10 * Math.log10(sum / chunk.length + 1e-18)

    const floorDb = this.#tracker.update(levelDb, dt, this.#speaking)

    // 迟滞判定：进入要 +12dB，退出要 +7dB
    const threshold = this.#speaking ? this.#cfg.vadOffDb : this.#cfg.vadOnDb
    const speaking = levelDb > floorDb + threshold
    if (speaking !== this.#speaking) this.#speaking = speaking

    // 块级增益：只放大语音块，静音块不放大
    const target = speaking
      ? Math.min(this.#cfg.maxGainDb, Math.max(this.#cfg.minGainDb, this.#cfg.targetDb - levelDb))
      : 0
    const coef = 1 - Math.exp(-dt / ((target > this.#gainDb ? this.#cfg.attackMs : this.#cfg.releaseMs) / 1000))
    this.#gainDb += (target - this.#gainDb) * coef

    const gain = 10 ** (this.#gainDb / 20)
    const boosted = new Float32Array(chunk.length)
    for (let i = 0; i < chunk.length; i += 1) boosted[i] = chunk[i] * gain

    let wake = null
    if (this.#kws !== null) wake = this.#kws.accept(boosted, 16000)

    return {
      levelDb: Math.round(levelDb * 10) / 10,
      floorDb: Math.round(floorDb * 10) / 10,
      speech: speaking,
      gainDb: Math.round(this.#gainDb * 10) / 10,
      wake,
    }
  }

  /** 判断是否有过语音（用于"说完自动结束"）。 */
  snapshot() {
    return { speaking: this.#speaking, gainDb: this.#gainDb, chunks: this.#chunks }
  }
}
