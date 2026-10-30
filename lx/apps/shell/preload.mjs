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
  /**
   * 唤醒提示：主进程推来**实际加载成功**的关键词标签数组（可能为 null=管线未就绪）。
   * HUD 用它显示「喊『X』」，与真实生效的 KWS 配置一致；重载失败时主进程不会推新值。
   */
  onWakeHint: (handler) => {
    ipcRenderer.on('hud:wakeHint', (_event, labels) => handler(labels))
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
  /** 用户主动收起 */
  dismiss: () => ipcRenderer.send('hud:dismiss'),
})

// Kept in the isolated world: page scripts cannot call the IPC sender or set this ID.
let confirmationId = null
ipcRenderer.on('hud:confirm', (_event, card) => { confirmationId = card?.id ?? null })
ipcRenderer.on('hud:confirmDone', () => { confirmationId = null })
window.addEventListener('DOMContentLoaded', () => {
  window.addEventListener('keydown', (event) => {
    if (!event.isTrusted || event.repeat || event.key !== 'Enter' || confirmationId === null) return
    if (event.target?.closest?.('button,input,textarea,select,[contenteditable]')) return
    event.preventDefault()
    const id = confirmationId
    confirmationId = null
    ipcRenderer.send('hud:confirm', { id, answer: true })
  })
  for (const [name, answer] of [['allow', true], ['allowOnce', 'once'], ['deny', false]]) {
    const button = document.getElementById(name)
    button?.addEventListener('click', (event) => {
      if (!event.isTrusted || event.currentTarget !== button || confirmationId === null) return
      const id = confirmationId
      confirmationId = null // One submission per displayed request.
      ipcRenderer.send('hud:confirm', { id, answer })
    })
  }
}, { once: true })
