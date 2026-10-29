/**
 * 采集窗口的 preload：只暴露"推一块音频"和"报错"两个口子。
 */
import { contextBridge, ipcRenderer } from 'electron'

contextBridge.exposeInMainWorld('capture', {
  /** @param {Float32Array} chunk 16k 单声道 PCM */
  push: (chunk) => ipcRenderer.send('capture:chunk', chunk),
  pushError: (message) => ipcRenderer.send('capture:error', String(message)),
})
