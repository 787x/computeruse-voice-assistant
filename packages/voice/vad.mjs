/**
 * 自适应 VAD（语音活动检测）。
 *
 * 为什么不用固定阈值：**现在吵，以后不一定吵**。
 * 固定 -40dB 在安静房间会一直误触发，在吵闹环境又完全不动。
 * 所以阈值跟着"当前噪声底"走，噪声底本身用最小值跟踪自动更新。
 *
 * 状态机：静音 → 候选 → 语音 → 拖尾 → 静音
 *   - 进入候选需要连续 N 帧超过 (噪声底 + onDb)
 *   - 进入语音需要候选持续够久（过滤掉关门声这类短促噪声）
 *   - 退出语音需要连续 M 帧低于 (噪声底 + offDb)（迟滞，避免字间停顿被切断）
 */
import { NoiseFloorTracker } from './agc.mjs'

export const VAD_DEFAULTS = {
  frameMs: 30,
  hopMs: 10,
  onDb: 12,         // 起始阈值：高于噪声底 12dB（噪声波动不至于触发）
  offDb: 7,         // 结束阈值：高于噪声底 7dB（迟滞）
  minSpeechMs: 120, // 最短语音段
  hangoverMs: 300,  // 语音结束后继续算语音的时间（要短于句间停顿，否则整段粘成一句）
}

/**
 * @param {Float32Array} samples
 * @param {number} sampleRate
 * @returns {{speech: Uint8Array, frameHop:number, frameSize:number, segments:Array, floorDb:number}}
 */
export function detectSpeech(samples, sampleRate, options = {}) {
  const cfg = { ...VAD_DEFAULTS, ...options }
  const frameSize = Math.round((cfg.frameMs / 1000) * sampleRate)
  const frameHop = Math.round((cfg.hopMs / 1000) * sampleRate)
  const frames = Math.max(1, 1 + Math.floor((samples.length - frameSize) / frameHop))

  const db = new Float64Array(frames)
  for (let i = 0; i < frames; i += 1) {
    const start = i * frameHop
    let sum = 0
    for (let k = 0; k < frameSize; k += 1) {
      const v = samples[start + k] ?? 0
      sum += v * v
    }
    db[i] = 10 * Math.log10(sum / frameSize + 1e-18)
  }

  // 噪声底初始化：取前 1 秒能量的 20 分位数。
  // 不能用第 0 帧（麦克风刚打开时可能极安静，会把底噪锁死在 -80dB，
  // 结果整段噪声都被判成语音——实测踩过）。
  const bootFrames = Math.min(frames, Math.round(1000 / cfg.hopMs))
  const boot = Array.from(db.subarray(0, bootFrames)).filter((v) => v > -80).sort((a, b) => a - b)
  const initFloor = boot.length > 0 ? boot[Math.floor(boot.length * 0.2)] : -60
  const tracker = new NoiseFloorTracker({ initDb: initFloor })
  const dtSec = frameHop / sampleRate
  const speech = new Uint8Array(frames)

  const minSpeechFrames = Math.max(1, Math.round(cfg.minSpeechMs / cfg.hopMs))
  const hangoverFrames = Math.max(1, Math.round(cfg.hangoverMs / cfg.hopMs))

  let state = 'silence'   // silence | candidate | speech | hangover
  let runLength = 0
  let quietRun = 0
  let candidateStart = 0
  let segStart = 0
  const segments = []

  for (let i = 0; i < frames; i += 1) {
    const isActive = speech[i] === 1
    // 先用上一帧的语音状态更新噪声底，再用它做本帧判定
    const floor = tracker.update(db[i], dtSec, isActive)
    const overOn = db[i] > floor + cfg.onDb
    const overOff = db[i] > floor + cfg.offDb

    switch (state) {
      case 'silence':
        if (overOn) { state = 'candidate'; runLength = 1; candidateStart = i }
        break

      case 'candidate':
        if (overOn) {
          runLength += 1
          if (runLength >= minSpeechFrames) {
            state = 'speech'
            segStart = candidateStart
            for (let j = candidateStart; j <= i; j += 1) speech[j] = 1
            quietRun = 0
          }
        } else {
          state = 'silence'
          runLength = 0
        }
        break

      case 'speech':
        speech[i] = 1
        if (!overOff) {
          quietRun += 1
          if (quietRun >= hangoverFrames) {
            const end = i - quietRun
            for (let j = end + 1; j <= i; j += 1) speech[j] = 0
            segments.push({ startFrame: segStart, endFrame: end })
            state = 'silence'
            runLength = 0
            quietRun = 0
          }
        } else {
          quietRun = 0
        }
        break

      default:
        break
    }
  }

  if (state === 'speech') {
    segments.push({ startFrame: segStart, endFrame: frames - 1 })
  }

  const hopSec = frameHop / sampleRate
  return {
    speech,
    frameHop,
    frameSize,
    floorDb: Math.round(tracker.floorDb * 10) / 10,
    segments: segments.map((s) => ({
      startSec: Math.round(s.startFrame * hopSec * 100) / 100,
      endSec: Math.round(s.endFrame * hopSec * 100) / 100,
      durationSec: Math.round((s.endFrame - s.startFrame) * hopSec * 100) / 100,
    })),
  }
}
