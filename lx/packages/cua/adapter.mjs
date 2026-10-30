/**
 * CuaToolkit —— 把 vendor 来的 dsh-computer-use 核心包装成"没有 cordis 也能用"的工具集。
 *
 * 上游 `src/index.js` 是 cordis 插件入口（唯一与 DSH 耦合的地方），其余文件零外部依赖。
 * 本适配器只做三件事：
 *   1. 构造 `runtime`（上游 tools.js 需要的那个依赖面）
 *   2. 把工具定义转成 OpenAI function-calling 格式
 *   3. 执行时补上参数校验（沿用上游 schema.js，语义与官方一致）
 *
 * 默认只暴露 12 个工具（见 DEFAULT_TOOL_NAMES）。上游有 25 个，语音场景不需要那么多：
 * 工具越多，模型在短指令上的选择错误率越高，且每个 schema 都会常驻上下文。
 */
import { mkdir, writeFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { NativeBridge } from './bridge.js'
import { createToolDefinitions } from './tools.js'
import { assertArgs, coerceArgs } from './schema.js'
import { externalPath } from '../config/paths.mjs'

const HERE = dirname(fileURLToPath(import.meta.url))

/** 语音场景默认工具白名单（12 个）。其余可通过 exclude 参数关闭裁剪。 */
export const DEFAULT_TOOL_NAMES = [
  'request_access',
  'list_windows',
  'get_app_state',
  'screenshot',
  'open_application',
  'left_click',
  'type',
  'key',
  'scroll',
  'set_value',
  'wait',
  'read_clipboard',
]

/** 实测：截图必须用 JPEG 小栅格，PNG 全屏要 1379ms，不可用于语音交互。 */
const SCREENSHOT_DEFAULTS = { max_width: 1024, format: 'jpeg', quality: 70 }
const SCREENSHOT_BUDGET_BYTES = 240 * 1024
const JPEG_QUALITY = 70

export class CuaToolkit {
  #bridge
  #definitions
  #frameDir
  #frameSeq = 0

  constructor(options = {}) {
    this.#frameDir = options.frameDir ?? join(HERE, 'frames')
    this.#bridge = new NativeBridge({
      // 外部 powershell 读不到 asar，打包后必须指向 app.asar.unpacked
      scriptPath: externalPath(join(HERE, 'native', 'cua.ps1')),
      executable: options.executable ?? 'powershell.exe',
      defaultTimeoutMs: options.defaultTimeoutMs ?? 30000,
      startupTimeoutMs: options.startupTimeoutMs ?? 20000,
      maxRestarts: options.maxRestarts ?? 3,
      onDiagnostic: options.onDiagnostic ?? (() => {}),
    })

    const runtime = this.#createRuntime(options.allowAllTools === true)
    const { native } = createToolDefinitions(runtime)
    this.#definitions = new Map(native.map((definition) => [definition.name, definition]))
  }

  #createRuntime(allowAllTools) {
    return {
      // 上游用法是 call(cmd, args, opts, 'long')，第 4 个参数表示长耗时命令
      call: (cmd, args = {}, opts = {}, mode) => {
        const timeoutMs = mode === 'long' ? 60000 : opts.timeoutMs
        return this.#bridge.call(cmd, args, timeoutMs === undefined ? opts : { ...opts, timeoutMs })
      },
      probe: async () => {
        try {
          await this.#bridge.probe()
          return { ok: true, info: this.#bridge.readyInfo ?? null }
        } catch (error) {
          return { ok: false, reason: error instanceof Error ? error.message : String(error) }
        }
      },
      accessReport: () => undefined,
      defaultScreenshotArgs: () => ({ ...SCREENSHOT_DEFAULTS }),
      screenshotBudget: () => SCREENSHOT_BUDGET_BYTES,
      jpegQuality: () => JPEG_QUALITY,
      saveFrame: async (base64, meta = {}) => {
        await mkdir(this.#frameDir, { recursive: true })
        const ext = meta.mediaType === 'image/jpeg' ? 'jpg' : 'png'
        this.#frameSeq += 1
        const name = `${String(this.#frameSeq).padStart(4, '0')}-${meta.frameId ?? 'frame'}${meta.zoomed ? '-zoom' : ''}.${ext}`
        const target = join(this.#frameDir, name)
        await writeFile(target, Buffer.from(base64, 'base64'))
        return target
      },
      // 我们不走 DSH 的附件系统：图像以文件路径交给 agent loop 自行决定如何注入
      attachImage: async () => undefined,
      ...(allowAllTools ? {} : {}),
    }
  }

  get readyInfo() {
    return this.#bridge.readyInfo ?? null
  }

  /** 启动后端进程并等待就绪横幅（约 1050ms，必须开机预热）。 */
  async warmup() {
    await this.#bridge.ensureStarted()
    return this.readyInfo
  }

  /** 供 agent loop 使用的 OpenAI function-calling 工具表。 */
  listTools(names = DEFAULT_TOOL_NAMES) {
    return names
      .map((name) => this.#definitions.get(name))
      .filter((definition) => definition !== undefined)
      .map((definition) => ({
        type: 'function',
        function: {
          name: definition.name,
          description: definition.description,
          parameters: definition.parameters,
        },
      }))
  }

  has(name) {
    return this.#definitions.has(name)
  }

  /**
   * 执行一次工具调用。
   * @param {string} name 工具名
   * @param {unknown} rawArgs 模型产出的参数
   * @returns {Promise<{name: string, text: string, meta?: object, data: object}>}
   */
  async invoke(name, rawArgs) {
    const definition = this.#definitions.get(name)
    if (definition === undefined) throw new Error(`unknown computer-use tool: ${name}`)

    // 与上游 createGuardedExecute 同一套校验，保证行为一致
    const coerced = coerceArgs(definition.parameters, rawArgs ?? {})
    assertArgs(definition.parameters, coerced)

    const value = await definition.execute(coerced, {})
    return { name, text: value?.text ?? '', meta: value?.meta, data: value ?? {} }
  }

  async dispose() {
    await this.#bridge.dispose()
  }
}
