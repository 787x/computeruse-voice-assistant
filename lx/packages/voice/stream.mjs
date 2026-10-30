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

  /** 静音了多久（秒）：用来定期把 KWS 流清干净 */
  #silentSeconds = 0

  get sampleRate() { return 16000 }

  /** 当前 KWS 实际使用的关键词文件（含来源），用于启动日志与验收。 */
  get keywordInfo() { return this.#kws?.keywordInfo ?? null }

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

    /*
     * KWS 流的两条重置规则（根因：sherpa 的关键词流在长时间静音后会漏检，
     * 实测唤醒词前面有 0.5~20 秒静音时命中率极不稳定，只有"开口前清一次流"才稳定命中）：
     *   1. 说话起点：静音→开口时重置，保证每次唤醒都从干净的流开始；
     *   2. 长期静音：每 3 秒清一次，避免流里堆积几十秒静音。
     * 麦克风空闲时几乎一直静音，所以"开机后第一次唤醒失败、手动关开一次才好"就是这个原因。
     */
    if (this.#kws !== null) {
      const wasSilent = this.#silentSeconds >= 0.3
      if (speaking && wasSilent) {
        this.#kws.reset()
        this.#silentSeconds = 0
      } else if (speaking) {
        this.#silentSeconds = 0
      } else {
        this.#silentSeconds += dt
        if (this.#silentSeconds >= 3) {
          this.#kws.reset()
          this.#silentSeconds = 0
        }
      }
    }
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

    /*
     * 唤醒用**原始**音频，不用 AGC 后的。
     *
     * 实测（tools/verify-wake-runtime.mjs）：同一段「小爱同学」，
     * 直接喂给 Kws 能命中，经 AGC 放大后再喂就漏检——增益把波形削顶了，
     * 关键词模型的特征就乱了。KWS 本身有小模型自带的前端，不需要我们再帮它调音量。
     */
    let wake = null
    if (this.#kws !== null) wake = this.#kws.accept(chunk, 16000)

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
