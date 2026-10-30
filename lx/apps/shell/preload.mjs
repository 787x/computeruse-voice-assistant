/**
 * preload：只暴露 HUD 需要的三件事，不做通用桥。
 */
import { contextBridge, ipcRenderer } from 'electron'

contextBridge.exposeInMainWorld('hud', {
  /** 主进程推来的状态：listening | thinking | speaking | confirm | error | hidden */
  onState: (handler) => {
    ipcRenderer.on('hud:state', (_event, state) => handler(state))
  },
  /** 内容高度变化时上报，主进程据此调整窗口高度并重新贴底 */
  setHeight: (height) => ipcRenderer.send('hud:setHeight', height),
  /**
   * 字幕流：{partial} 边说边变的那一行；{final} 定稿的一句话；
   * {reset:true} 清空（新的一轮对话开始）。
   */
  onSubtitle: (handler) => {
    ipcRenderer.on('hud:subtitle', (_event, payload) => handler(payload))
  },
  /** 播报一句话：主进程合成好 WAV，渲染层负责出声 */
  onPlay: (handler) => {
    ipcRenderer.on('hud:play', (_event, payload) => handler(payload))
  },
  /** 立刻停声（收起窗口时） */
  onStopAudio: (handler) => {
    ipcRenderer.on('hud:stopAudio', () => handler())
  },
  /** 播完回执：主进程据此播下一句 */
  spoken: (id) => ipcRenderer.send('hud:spoken', id),
  /** 确认卡片：主进程推来要确认的动作 */
  onConfirm: (handler) => {
    ipcRenderer.on('hud:confirm', (_event, payload) => handler(payload))
  },
  /** 确认结束（卡片收起） */
  onConfirmDone: (handler) => {
    ipcRenderer.on('hud:confirmDone', () => handler())
  },
  /** 用户点了"允许"(true) / "允许 15 分钟"('once') / "取消"(false) */
  answerConfirm: (answer) => ipcRenderer.send('hud:confirm', answer),
  /** 用户主动收起 */
  dismiss: () => ipcRenderer.send('hud:dismiss'),
})
