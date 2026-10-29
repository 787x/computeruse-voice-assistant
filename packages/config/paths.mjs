/**
 * 路径解析：开发态跑在项目根目录，打包后跑在 resources 目录。
 *
 * 为什么单独一个文件：模型、音色运行时、配置文件这三个位置的推导规则必须只有一份。
 * 之前每处都写 `'models/...'` 相对路径，开发态没问题，一旦打包（asar + resources）全错。
 *
 * 为什么是函数而不是常量：ESM 的 import 会在主进程设置环境变量之前求值，
 * 常量会在那一刻被钉死成 cwd。全部改成调用时读取，才不会踩这个坑。
 *
 * 约定：
 *   CUA_VA_ROOT     —— 模型与运行时所在的根（打包后 = process.resourcesPath）
 *   CUA_VA_DATA_DIR —— 用户数据（配置、审计）。打包后放 userData，绝不放安装目录
 */
import { join, sep } from 'node:path'

export function appRoot() {
  return process.env.CUA_VA_ROOT ?? process.cwd()
}

/** 模型目录（ASR / KWS / VAD） */
export function modelPath(...parts) {
  return join(appRoot(), 'models', ...parts)
}

/** 语音运行时目录（venv、音色包、合成输出） */
export function runtimePath(...parts) {
  return join(appRoot(), '.runtime', ...parts)
}

/** 用户数据目录（配置、审计、日志） */
export function dataPath(...parts) {
  const root = process.env.CUA_VA_DATA_DIR ?? join(appRoot(), '.config')
  return join(root, ...parts)
}

/**
 * 外部进程（PowerShell / Python）能读的真实路径。
 *
 * 为什么必须有它：asar 是 Electron 自己的虚拟文件系统，Node 的 fs 能读，
 * 但 `powershell.exe` / `python.exe` 是独立进程，只会老老实实按路径找文件——
 * 拿到 `…\app.asar\packages\cua\native\cua.ps1` 就报"路径不存在"。
 * 打包时这些脚本会通过 asarUnpack 解到 `app.asar.unpacked` 下，这里做一次映射。
 */
export function externalPath(path) {
  const inAsar = `app.asar${sep}`
  if (!path.includes(inAsar)) return path
  return path.replace(inAsar, `app.asar.unpacked${sep}`)
}
