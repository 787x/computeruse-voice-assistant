/**
 * 设置页「系统提示词」的真实界面验收（lx 版：Electron + 生产 preload/renderer）。
 *
 * 为什么需要它：Node 侧的静态接线检查证明不了"用户点这一下会发生什么"。
 * 这里加载真正的 settings.html + preload-settings.mjs，用 DOM 事件驱动界面，
 * 断言**送到主进程 IPC 的 payload**：
 *   - 默认模式时 textarea 不出现、默认文案来自单一来源
 *   - 切自定义时以完整默认提示词为编辑起点
 *   - 保存时文本原样送达（换行/缩进不丢）
 *   - 「恢复默认」送 systemPrompt:null（而不是省略字段）
 *   - 本来就是默认时保存别的设置不带 systemPrompt 字段
 *   - 保存失败时状态栏给出失败反馈
 * 不做真实落盘（那是 verify-system-prompt.mjs 的职责），配置用可控替身。
 *
 * 用法：node lx/tools/run-verify.mjs lx/tools/verify-electron-settings-prompt.mjs
 */
import { app, BrowserWindow, ipcMain } from 'electron'
import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { DEFAULT_SYSTEM_PROMPT } from '../packages/agent/system-prompt.mjs'

const tree = import.meta.url.includes('/lx/') ? 'lx' : 'root'
const root = dirname(dirname(fileURLToPath(import.meta.url)))
mkdirSync(join(root, '.tmp'), { recursive: true })
const report = (value) => {
  const text = JSON.stringify({ tree, ...value })
  console.log(text)
  return text
}

const results = []
function record(name, pass, detail) {
  results.push({ name, pass, detail })
  report({ check: name, pass, detail })
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

let savedPayloads = []
let saveMode = 'ok'
/** 界面侧看到的"当前配置"：save 之后按提交值更新，模拟主进程落盘再回读 */
let configState = {}

function applyFakeSave(payload) {
  if (saveMode === 'fail') return { ok: false, error: '自定义系统提示词不能是空白；要恢复默认请选「使用默认提示词」' }
  if ('systemPrompt' in payload) configState.systemPrompt = payload.systemPrompt
  return { ok: true, config: { ...configState } }
}

function registerIpc() {
  ipcMain.handle('settings:load', async () => ({
    config: configState,
    providers: [],
    capabilities: [],
    voices: [],
    audit: [],
    defaultSystemPrompt: DEFAULT_SYSTEM_PROMPT,
  }))
  ipcMain.handle('settings:save', async (_event, payload) => {
    savedPayloads.push(payload)
    return applyFakeSave(payload ?? {})
  })
  ipcMain.handle('settings:testModel', async () => ({ ok: true, ms: 1, text: 'ok' }))
  ipcMain.handle('settings:testSearch', async () => ({ ok: true, ms: 1, count: 0, preview: '' }))
  ipcMain.handle('settings:clearSearch', async () => ({ ok: true, config: { ...configState } }))
  ipcMain.handle('settings:testVoice', async () => ({ ok: false, error: '本测试不合成' }))
  ipcMain.handle('settings:exportAudit', async () => ({ ok: false, error: '本测试不导出' }))
  ipcMain.handle('settings:lxStatus', async () => ({ ok: false, error: '本测试不连 LX', launched: false }))
}

async function main() {
  const watchdog = setTimeout(() => {
    report({ pass: false, error: 'watchdog timeout' })
    app.exit(1)
  }, 30000)
  const isolated = mkdtempSync(join(app.getPath('temp'), 'settings-prompt-lx-electron-'))
  app.setPath('userData', isolated)
  let win = null
  try {
    await app.whenReady()
    registerIpc()
    configState = {
      providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash',
      apiKey: '', hasApiKey: false, maxSteps: 16, temperature: 0.3, systemPrompt: null,
      search: { provider: 'tavily', apiKey: '', hasApiKey: false },
      tts: { emotion: '中立', speed: 1 },
      ui: { ttsEnabled: true, continuousMode: true, voiceConfirmEnabled: true, subtitles: true, autoStart: false, releaseOnFullscreen: true, wakeEnabled: true },
      permissions: { mode: 'standard', overrides: {} },
      modelProfiles: [], modelSchedule: { enabled: false, rules: [] }, 
    }

    win = new BrowserWindow({
      width: 760, height: 720, show: false, backgroundColor: '#14161c',
      webPreferences: { preload: join(root, 'apps/shell/preload-settings.mjs'), contextIsolation: true, nodeIntegration: false, sandbox: false },
    })
    await win.loadFile(join(root, 'apps/shell/renderer/settings.html'))
    await sleep(500)
    report({ stage: 'loaded', electron: process.versions.electron })

    const js = (code) => win.webContents.executeJavaScript(code)

    // 1) 默认模式：控件状态 + 默认文案确实来自单一来源
    record('默认模式：显示默认提示词全文（与 Agent 同源）', await js(`(() => {
      const d = document.getElementById('promptModeDefault');
      const c = document.getElementById('promptModeCustom');
      const shown = document.getElementById('defaultPrompt').textContent;
      return d.checked === true && c.checked === false
        && document.getElementById('promptCustomArea').hidden === true
        && document.getElementById('defaultPromptDetails').open === true
        && shown.length > 500 && shown.includes('你叫银狼');
    })()`))

    // 2) 切到自定义：多行文本框出现，且以完整默认提示词为编辑起点（默认全文仍可查）
    record('切自定义：出现可编辑多行框并预填完整默认提示词', await js(`(() => {
      document.getElementById('promptModeCustom').click();
      const box = document.getElementById('systemPrompt');
      const shown = document.getElementById('defaultPrompt').textContent;
      return box.tagName === 'TEXTAREA'
        && document.getElementById('promptCustomArea').hidden === false
        && document.getElementById('copyDefaultPrompt') !== null
        && shown.length > 500
        && box.value === shown && box.value.length > 500;
    })()`))

    // 3) 改成自定义 A（含换行与行首空格），保存 → 送到 IPC 的必须一字不差
    const promptA = '你是自定义人设 A。\n  这行行首有两个空格，结尾有空格：  \n最后一行'
    const payloadA = await js(`(async () => {
      const box = document.getElementById('systemPrompt');
      box.value = ${JSON.stringify(promptA)};
      box.dispatchEvent(new Event('input', { bubbles: true }));
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 400));
      return { status: document.getElementById('status').textContent,
               statusClass: document.getElementById('status').className,
               chars: document.getElementById('promptCustomChars').textContent,
               value: box.value };
    })()`)
    const sentA = savedPayloads.at(-1)
    record('保存 A：payload 是原样文本（不 trim、不加工）',
      typeof sentA?.systemPrompt === 'string' && sentA.systemPrompt === promptA
        && payloadA.value === promptA,
      `sent=${JSON.stringify(sentA?.systemPrompt?.slice(0, 12))}…`)
    record('保存 A：状态栏显示成功反馈', payloadA.statusClass === 'ok' && payloadA.status.includes('已保存'), payloadA.status)
    record('保存 A：字数提示随输入更新（字数/行数都对得上）',
      payloadA.chars.includes(`${promptA.length} 字`) && payloadA.chars.includes(`${promptA.split('\n').length} 行`),
      payloadA.chars)

    // 4) 重新加载设置页（模拟重启后再打开）：仍是自定义，文本还是 A
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('reload 超时')), 8000)
      win.webContents.once('did-finish-load', () => { clearTimeout(timer); resolve() })
      win.webContents.reload()
    })
    await sleep(500)
    const reopened = await js(`(() => {
      const box = document.getElementById('systemPrompt');
      return { custom: document.getElementById('promptModeCustom').checked,
               defaultChecked: document.getElementById('promptModeDefault').checked,
               hidden: document.getElementById('promptCustomArea').hidden,
               value: box.value };
    })()`)
    record('重新打开设置页：自定义模式与文本保持不变',
      reopened.custom === true && reopened.defaultChecked === false && reopened.hidden === false
        && reopened.value === promptA,
      `value=${JSON.stringify(reopened.value.slice(0, 10))}…`)

    // 5) 「恢复默认」按钮 → 保存时送 systemPrompt:null（显式语义，不是省略字段）
    const restored = await js(`(async () => {
      document.getElementById('restoreDefaultPrompt').click();
      const hidden = document.getElementById('promptCustomArea').hidden;
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 400));
      return { hidden, status: document.getElementById('status').className,
               defaultOpen: document.getElementById('defaultPromptDetails').open };
    })()`)
    const sentRestore = savedPayloads.at(-1)
    record('恢复默认：切回默认视图（默认全文自动展开）并送 null',
      restored.hidden === true && restored.defaultOpen === true
        && 'systemPrompt' in sentRestore && sentRestore.systemPrompt === null
        && restored.status === 'ok',
      JSON.stringify(sentRestore?.systemPrompt))

    // 6) 默认模式下保存其它设置：不带 systemPrompt 字段（缺省=不修改）
    const other = await js(`(async () => {
      document.getElementById('temperature').value = '0.9';
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 400));
      return document.getElementById('status').className;
    })()`)
    const sentOther = savedPayloads.at(-1)
    record('默认模式保存其它设置：payload 不含 systemPrompt',
      other === 'ok' && !('systemPrompt' in sentOther) && sentOther.temperature === 0.9,
      `keys=${Object.keys(sentOther).length}`)

    // 7) 保存失败要有可见反馈（非法提示词由主进程拒绝，界面如实显示）
    saveMode = 'fail'
    const failed = await js(`(async () => {
      document.getElementById('promptModeCustom').click();
      const box = document.getElementById('systemPrompt');
      box.value = '   ';
      box.dispatchEvent(new Event('input', { bubbles: true }));
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 400));
      return { status: document.getElementById('status').textContent, cls: document.getElementById('status').className };
    })()`)
    record('保存失败：状态栏显示错误信息（bad）',
      failed.cls === 'bad' && failed.status.includes('空白'), failed.status)
    saveMode = 'ok'

    // 8) 复制按钮存在且走的是"默认文本"这一份（不校验剪贴板内容，无头环境不可靠）
    record('默认/自定义两处复制入口都在', await js(`(() => {
      const a = document.getElementById('copyDefaultPrompt');
      const b = document.getElementById('copyCustomPrompt');
      return a.tagName === 'BUTTON' && b.tagName === 'BUTTON';
    })()`))

    clearTimeout(watchdog)
    const failedCount = results.filter((r) => !r.pass).length
    report({ suite: 'settings prompt UI (production preload/renderer)', electron: process.versions.electron,
      cases: results.length, failed: failedCount, pass: failedCount === 0,
      ipcSaves: savedPayloads.length })
    app.exit(failedCount === 0 ? 0 : 1)
  } catch (error) {
    clearTimeout(watchdog)
    report({ pass: false, error: error.stack ?? String(error) })
    console.error(error.stack ?? error)
    app.exit(1)
  } finally {
    if (win !== null && !win.isDestroyed()) win.destroy()
  }
}

void main()
