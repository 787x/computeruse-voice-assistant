/**
 * 用打包二进制（Electron）跑一个验收脚本（主进程侧 API + 生产 preload/renderer）。
 * 与 run-shell.mjs 同样的坑：必须清掉 ELECTRON_RUN_AS_NODE，否则 electron.exe
 * 退化成 Node，主进程 API（BrowserWindow 等）不可用。
 *
 * 用法：node tools/run-verify.mjs tools/verify-electron-wake-hint.mjs
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
// lx/ 发行版没有自己的 node_modules：找不到时向上退一级（仓库根的依赖）。
const candidates = [
  join(root, 'node_modules', 'electron', 'dist', 'electron.exe'),
  join(dirname(root), 'node_modules', 'electron', 'dist', 'electron.exe'),
]
const electron = candidates.find((p) => existsSync(p))
if (electron === undefined) {
  console.error(`找不到 Electron:\n  ${candidates.join('\n  ')}\n先执行 npm install`)
  process.exit(1)
}
const script = process.argv[2]
if (!script) {
  console.error('用法: node tools/run-verify.mjs <verify-*.mjs>')
  process.exit(2)
}
const env = { ...process.env }
delete env.ELECTRON_RUN_AS_NODE
const child = spawn(electron, [join(root, script), ...process.argv.slice(3)], {
  cwd: root,
  stdio: 'inherit',
  env,
})
child.on('exit', (code) => process.exit(code ?? 0))
