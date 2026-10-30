/**
 * 量一下 ASR 常驻内存——这决定"要不要预加载"，以及 R14 的销毁阈值定在哪。
 * 用法：node tools/probe-asr-mem.mjs
 */
import { Asr } from '../packages/voice/asr.mjs'

const rss = () => Math.round(process.memoryUsage().rss / 1048576)
const before = rss()
const t0 = performance.now()
const asr = new Asr()
const initMs = Math.round(performance.now() - t0)
const afterLoad = rss()

// 喂 1 秒静音，看推理过程会不会再涨（ONNX 会惰性分配工作缓冲）
const silence = new Float32Array(16000)
for (let i = 0; i < 10; i += 1) asr.push(silence, 16000)
const afterInfer = rss()

console.log(JSON.stringify({
  initMs,
  rssBeforeLoadMB: before,
  rssAfterLoadMB: afterLoad,
  loadDeltaMB: afterLoad - before,
  rssAfterInferMB: afterInfer,
  inferDeltaMB: afterInfer - afterLoad,
}, null, 1))
