// Run with an Electron executable: node tools/run-verify.mjs tools/verify-electron-wake-hint.mjs
// 或者：electron tools/verify-electron-wake-hint.mjs
//
// 加载**生产** preload + renderer/index.html（不是复制的假页面），用真实的
// hud:wakeHint IPC 验证 HUD 文案跟着实际生效的关键词走：
//   - 未推提示（管线还没就绪）→ 默认「小爱同学」，不能空白；
//   - 推单个自定义词 → 显示那个词；
//   - 推 2~3 个词 → 列出全部；
//   - 推 4 个以上 → 通用提示（不会显示不全）；
//   - 重载失败路径不会推新值（静态检查在 verify-keyword-reload.mjs 里）。
import { app, BrowserWindow, ipcMain } from 'electron'
import assert from 'node:assert/strict'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { appendFileSync, mkdirSync, mkdtempSync } from 'node:fs'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
mkdirSync(join(root, '.tmp'), { recursive: true })
const report = (value) => {
  const text = JSON.stringify(value)
  appendFileSync(join(root, '.tmp/electron-wake-hint-results.log'), text + '\n')
  console.log(text)
}
report({ stage: 'start' })
const watchdog = setTimeout(() => { report({ pass: false, error: 'test watchdog timeout' }); app.exit(1) }, 20000)
const isolated = mkdtempSync(join(tmpdir(), 'wake-hint-electron-'))
app.setPath('userData', isolated)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

let win
async function main() {
  try {
    await app.whenReady()
    win = new BrowserWindow({
      width: 500,
      height: 400,
      show: false,
      webPreferences: {
        preload: join(root, 'apps/shell/preload.mjs'),
        contextIsolation: true,
        nodeIntegration: false,
        sandbox: false,
      },
    })
    await win.loadFile(join(root, 'apps/shell/renderer/index.html'))
    await sleep(150)
    report({ stage: 'loaded' })

    // 真实 main.mjs 里的推送方式：win.webContents.send('hud:wakeHint', labels)
    const pushHint = (labels) => win.webContents.send('hud:wakeHint', labels)
    const subText = () => win.webContents.executeJavaScript('document.getElementById("sub").textContent')
    const forceDone = () => win.webContents.executeJavaScript('window.__forceDone ? window.__forceDone() : null')

    // done 态文案需要一个已加载的 done 状态；apply(state) 由 hud:state 驱动，
    // 生产 renderer 没有暴露 window.__forceDone，直接用 IPC 推到 done。
    const showDone = async () => {
      win.webContents.send('hud:state', 'done')
      await sleep(60)
      return subText()
    }

    // 1) 没有推送提示时：done 文案必须是默认「小爱同学」（安装包默认，不空白）
    const initial = await showDone()
    assert.match(initial, /喊「小爱同学」/, `初始 done 文案应回落默认词，实际: ${initial}`)
    report({ stage: 'default-hint', text: initial })

    // 2) 单个自定义词
    pushHint(['蒋友伯'])
    const single = await showDone()
    assert.equal(single, '需要我做什么，喊「蒋友伯」', `自定义单词未生效: ${single}`)
    report({ stage: 'custom-single', text: single })

    // 3) 多词（2~3）：全部列出
    pushHint(['蒋友伯', '法国'])
    const multi = await showDone()
    assert.equal(multi, '需要我做什么，喊「蒋友伯」或「法国」', `双词提示不对: ${multi}`)
    report({ stage: 'custom-multi', text: multi })

    // 4) 过多关键词：通用提示，绝不截断或漏词误导
    pushHint(['a', 'b', 'c', 'd'])
    const many = await showDone()
    assert.equal(many, '需要我做什么，喊唤醒词', `4+ 词应给通用提示: ${many}`)
    report({ stage: 'custom-many', text: many })

    // 5) 推回默认：文案回到「小爱同学」（用户恢复默认后重启的场景）
    pushHint(['小爱同学'])
    const back = await showDone()
    assert.equal(back, '需要我做什么，喊「小爱同学」', `恢复默认未生效: ${back}`)
    report({ stage: 'back-to-default', text: back })

    // 6) 空数组不得改写文案（主进程 pushWakeHint 已过滤空 labels；这里防串台）
    pushHint([])
    const kept = await showDone()
    assert.equal(kept, '需要我做什么，喊「小爱同学」', `空数组不应清掉提示: ${kept}`)
    report({ stage: 'empty-ignored', text: kept })

    clearTimeout(watchdog)
    report({ pass: true })
    app.exit(0)
  } catch (error) {
    clearTimeout(watchdog)
    report({ pass: false, error: error.message, stack: error.stack })
    app.exit(1)
  }
}

void main()
// ipcMain 引用可留空：本测试只走主进程→渲染层推送方向。
void ipcMain
