/**
 * 自适应 AGC（自动增益）+ 噪声底跟踪。
 *
 * 设计前提：**现在吵，以后不一定吵**。所以不能按某个环境的噪声把参数调死，
 * 必须让增益跟着"当前噪声底"走。三条规则：
 *
 *   1. 只有被判为语音的帧才放大（静音/噪声不放大，否则把底噪抬成嘶声）
 *   2. 增益上限由信噪比决定：噪声越大，允许的增益越小
 *      （把语音拉到目标电平是应该的，但把噪声一起抬上来就是错的）
 *   3. 噪声底用慢速最小值跟踪，随环境缓慢变化；不依赖启动时标定
 */

/**
 * 噪声底估计：最小统计跟踪（带限速）。
 *
 * 走过的两条弯路：
 *   1. 纯最小跟踪 + 无下限 → 被一个静音帧瞬间打到 -180dB，之后全判语音
 *   2. 滑动分位数 → 语音占比高的素材里，分位数会落进语音，底噪被抬高，什么都检不出
 *
 * 最终形态：向下快速但有下限（钳到 -70dB），向上分语音/非语音两档慢速跟随。
 * 配合 VAD 的 12dB 起始阈值，吵闹与安静两种环境都能成立。
 */
export class NoiseFloorTracker {
  constructor({
    initDb = -60,
    riseDbPerSec = 3,
    speechRiseDbPerSec = 0.2,
    fallDbPerSec = 30,
    floorMinDb = -70,
  } = {}) {
    this.floorDb = initDb
    this.riseDbPerSec = riseDbPerSec
    this.speechRiseDbPerSec = speechRiseDbPerSec
    this.fallDbPerSec = fallDbPerSec
    this.floorMinDb = floorMinDb
  }

  /** @param {number} frameDb @param {number} dtSec @param {boolean} isSpeech */
  update(frameDb, dtSec = 0.01, isSpeech = false) {
    if (!Number.isFinite(frameDb)) return this.floorDb
    const observed = Math.max(frameDb, this.floorMinDb)

    if (observed < this.floorDb) {
      this.floorDb = Math.max(observed, this.floorDb - this.fallDbPerSec * dtSec)
    } else {
      const rate = isSpeech ? this.speechRiseDbPerSec : this.riseDbPerSec
      this.floorDb = Math.min(observed, this.floorDb + rate * dtSec)
    }
    if (this.floorDb < this.floorMinDb) this.floorDb = this.floorMinDb
    return this.floorDb
  }
}

const DEFAULTS = {
  targetDb: -22,        // 语音目标电平
  maxGainDb: 30,        // 麦克风阵列很弱，允许最多 +30dB
  minGainDb: -6,
  noiseHeadroomDb: 18,  // 放大后的噪声不得高于 (噪声底 + headroom) 对应的绝对电平上限
  attackMs: 10,
  releaseMs: 200,
}

/**
 * 逐样本 AGC。输入已分帧的语音标记，输出调制后的样本与统计。
 *
 * @param {Float32Array} samples 16k 单声道
 * @param {number} sampleRate
 * @param {{speech:Uint8Array, frameHop:number, frameSize:number}} vad 与 samples 对齐的帧级语音标记
 */
export function autoGain(samples, sampleRate, vad, options = {}) {
  const cfg = { ...DEFAULTS, ...options }
  const { speech, frameHop, frameSize } = vad
  const frames = speech.length

  const frameDb = new Float64Array(frames)
  for (let i = 0; i < frames; i += 1) {
    const start = i * frameHop
    let sum = 0
    for (let k = 0; k < frameSize; k += 1) {
      const v = samples[start + k] ?? 0
      sum += v * v
    }
    frameDb[i] = 10 * Math.log10(sum / frameSize + 1e-18)
  }

  const tracker = new NoiseFloorTracker({ initDb: frameDb[0] ?? -60 })
  const dtSec = frameHop / sampleRate
  const gainDb = new Float64Array(frames)

  const attackCoef = 1 - Math.exp(-dtSec / (cfg.attackMs / 1000))
  const releaseCoef = 1 - Math.exp(-dtSec / (cfg.releaseMs / 1000))
  let current = 0

  for (let i = 0; i < frames; i += 1) {
    const floor = tracker.update(frameDb[i], dtSec, speech[i] === 1)

    if (speech[i] === 1) {
      const desired = cfg.targetDb - frameDb[i]
      const target = Math.min(cfg.maxGainDb, Math.max(cfg.minGainDb, desired))
      // 用 attack 快速跟上语音起点
      current += (target - current) * attackCoef
    } else {
      // 非语音帧：增益快速回落到 0（不放大噪声）
      current += (0 - current) * releaseCoef
    }
    gainDb[i] = Math.max(-60, Math.min(cfg.maxGainDb, current))
  }

  // 插值到样本级并应用
  const out = new Float32Array(samples.length)
  for (let i = 0; i < samples.length; i += 1) {
    const pos = i / frameHop
    const i0 = Math.min(frames - 1, Math.floor(pos))
    const i1 = Math.min(frames - 1, i0 + 1)
    const frac = pos - i0
    const db = gainDb[i0] * (1 - frac) + gainDb[i1] * frac
    out[i] = samples[i] * 10 ** (db / 20)
  }

  const speechGains = []
  for (let i = 0; i < frames; i += 1) if (speech[i] === 1) speechGains.push(gainDb[i])
  speechGains.sort((a, b) => a - b)

  return {
    samples: out,
    stats: {
      noiseFloorDb: Math.round(tracker.floorDb * 10) / 10,
      medianGainDb: speechGains.length > 0 ? Math.round(speechGains[speechGains.length >> 1] * 10) / 10 : null,
      maxGainDb: speechGains.length > 0 ? Math.round(speechGains[speechGains.length - 1] * 10) / 10 : null,
      speechFrames: speechGains.length,
      totalFrames: frames,
    },
  }
}
