/**
 * AudioWorklet：把 128 帧的渲染量子攒成 1600 帧（16k 下 100ms）再发给主线程。
 * 不在主线程做音频处理，避免界面卡顿影响拾音。
 */
class CaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super()
    this.buffer = new Float32Array(1600)
    this.filled = 0
  }

  process(inputs) {
    const channel = inputs[0]?.[0]
    if (!channel) return true

    for (let i = 0; i < channel.length; i += 1) {
      this.buffer[this.filled] = channel[i]
      this.filled += 1
      if (this.filled === this.buffer.length) {
        // 拷贝一份发出去，避免后续写入覆盖已发送的数据
        this.port.postMessage(this.buffer.slice(0))
        this.filled = 0
      }
    }
    return true
  }
}

registerProcessor('capture-processor', CaptureProcessor)
