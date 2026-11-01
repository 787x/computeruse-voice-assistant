/**
 * 设置窗口的 preload：只暴露设置页需要的那几个调用，不做通用桥。
 *
 * 渲染层拿不到 Node，也拿不到 ipcRenderer —— 所有规则（打码、合并、校验）都在主进程。
 */
import { contextBridge, ipcRenderer } from 'electron'

contextBridge.exposeInMainWorld('settingsApi', {
  /** 一次性拿全：配置（打码）+ 提供商 + 能力表 + 音色 + 最近审计 */
  load: () => ipcRenderer.invoke('settings:load'),
  /** 保存表单 */
  save: (payload) => ipcRenderer.invoke('settings:save', payload),
  /** 测模型连通性 */
  testModel: (payload) => ipcRenderer.invoke('settings:testModel', payload),
  /**
   * 取在线模型列表（GET /v1/models）。
   * payload 可带 providerId / baseURL / apiKey（**包括还没保存的 Key**）；
   * Key 只在主进程这一次请求里用，不进缓存、不落盘。
   */
  models: (payload) => ipcRenderer.invoke('settings:models', payload),
  /** 让下一次取列表强制走网络 */
  clearModelsCache: (payload) => ipcRenderer.invoke('settings:models:clear', payload),
  /** 在系统浏览器打开提供商控制台（拿 API Key） */
  openKeyConsole: (payload) => ipcRenderer.invoke('settings:openKeyConsole', payload),
  /** 测搜索 */
  testSearch: (apiKey) => ipcRenderer.invoke('settings:testSearch', apiKey),
  /** 清除搜索 key */
  clearSearch: () => ipcRenderer.invoke('settings:clearSearch'),
  /** 用当前情绪/语速合成一句试听 */
  testVoice: (options) => ipcRenderer.invoke('settings:testVoice', options),
  /** 导出审计日志到数据目录的 exports/ */
  exportAudit: () => ipcRenderer.invoke('settings:exportAudit'),
  /** 网易云登录：主进程取一次性链接并用系统浏览器打开 */
  neteaseLogin: () => ipcRenderer.invoke('settings:neteaseLogin'),
  /** 查登录态（昵称/是否已登录） */
  neteaseStatus: () => ipcRenderer.invoke('settings:neteaseStatus'),
  /** 登录过程推送：拿到链接 / 登录结束 */
  onNeteaseLoginUrl: (handler) => { ipcRenderer.on('settings:neteaseLoginUrl', (_e, url) => handler(url)) },
  onNeteaseLoginDone: (handler) => { ipcRenderer.on('settings:neteaseLoginDone', (_e, result) => handler(result)) },
})
