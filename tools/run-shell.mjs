/**
 * 启动 Electron 壳（跨环境可用）。
 *
 * 为什么需要这层：某些宿主环境会设置 ELECTRON_RUN_AS_NODE=1，
 * 那会让 electron.exe 退化成 Node 跑，主进程直接报
 * "does not provide an export named 'BrowserWindow'"。
 * 这里统一清掉该变量再启动，避免每次手敲。
 *
 * 用法：
 *   node tools/run-shell.mjs            # 托盘常驻（默认）
 *   node tools/run-shell.mjs --listen   # 启动即开唤醒监听
 *   node tools/run-shell.mjs --demo     # 启动即弹小窗（看视觉）
 *   node tools/run-shell.mjs --mic-test # 麦克风链路自检
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const electron = join(root, 'node_modules', 'electron', 'dist', 'electron.exe')

if (!existsSync(electron)) {
  console.error(`找不到 Electron: ${electron}\n先执行 npm install`)
  process.exit(1)
}

const env = { ...process.env }
delete env.ELECTRON_RUN_AS_NODE

const child = spawn(electron, ['.', ...process.argv.slice(2)], {
  cwd: root,
  stdio: 'inherit',
  env,
})

child.on('exit', (code) => process.exit(code ?? 0))
