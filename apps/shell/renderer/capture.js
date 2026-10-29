/**
 * 采集渲染进程：只做三件事——拿麦克风、转成 16k 单声道 PCM、交给主进程。
 *
 * 为什么放在渲染进程：Electron 主进程没有 Web 音频 API，
 * getUserMedia / AudioContext 只在渲染进程可用。
 *
 * 关键约束：
 *   - 采样率固定 16k（KWS 模型要求），由 AudioContext 自己重采样
 *   - 用 AudioWorklet 而不是 ScriptProcessor（后者在主线程，会掉帧）
 *   - 停止时必须 stop 所有 track，否则麦克风被一直占用（用户会看到托盘图标常亮）
 */

let context = null
let stream = null
let node = null

async function start() {
  if (stream !== null) return { ok: true, already: true }

  // 回声消除默认开启：防止我们自己的 TTS 被麦克风拾回造成自触发。
  // 但做"扬声器放唤醒词"这类声学闭环测试时，AEC 会把测试音也消掉，所以允许关掉。
  const noAec = new URLSearchParams(location.search).get('noaec') === '1'

  stream = await navigator.mediaDevices.getUserMedia({
    audio: {
      channelCount: 1,
      echoCancellation: !noAec,
      noiseSuppression: false, // 降噪交给后端，采集端保持原始信号
      autoGainControl: false,  // 同理：增益由我们的自适应 AGC 负责
    },
    video: false,
  })

  context = new AudioContext({ sampleRate: 16000 })
  await context.audioWorklet.addModule('worklet.js')

  const source = context.createMediaStreamSource(stream)
  node = new AudioWorkletNode(context, 'capture-processor')
  node.port.onmessage = (event) => {
    // 直接传 Float32Array（结构化克隆会保留类型，比转数组省一个数量级的开销）
    window.capture.push(event.data)
  }
  source.connect(node)
  // 不连到 destination，避免把麦克风回放出来（会啸叫）

  const track = stream.getAudioTracks()[0]
  return {
    ok: true,
    sampleRate: context.sampleRate,
    device: track?.label ?? '',
    echoCancellation: noAec ? 'off' : 'on',
    settings: track?.getSettings?.() ?? {},
  }
}

async function stop() {
  if (node !== null) { try { node.disconnect() } catch { /* 已断开 */ } node = null }
  if (stream !== null) {
    for (const track of stream.getTracks()) track.stop()
    stream = null
  }
  if (context !== null) { try { await context.close() } catch { /* 已关闭 */ } context = null }
  return { ok: true }
}

window.captureControl = { start, stop }

// 主进程通过 executeJavaScript 调用，或者由 capture.html 的 ?autostart=1 自动启动
if (new URLSearchParams(location.search).get('autostart') === '1') {
  start().catch((error) => window.capture.pushError?.(String(error)))
}
