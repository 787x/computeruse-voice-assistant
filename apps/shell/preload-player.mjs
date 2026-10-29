/** 独立播放面板的 preload：只暴露两件事——收状态、点按钮 */
import { contextBridge, ipcRenderer } from 'electron'

contextBridge.exposeInMainWorld('playerApi', {
  onState: (handler) => {
    ipcRenderer.on('player:state', (_event, payload) => handler(payload))
  },
  /** prev / toggle / next / close（close = 停止播放） */
  action: (action) => ipcRenderer.send('player:action', action),
})
