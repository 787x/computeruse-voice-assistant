/**
 * P0 验收脚本：不依赖 DSH、不依赖 cordis，直接用 vendor 的 cua 核心操作桌面。
 *
 * 验收标准（v0.4 §9 P0）：
 *   1. 后端进程能起来（probe ok / dpiAware）
 *   2. 截图可用且耗时在语音可接受范围
 *   3. 能通过 open_application 打开记事本
 *   4. 能读到窗口列表，确认记事本真的出现了
 */
import { CuaToolkit, DEFAULT_TOOL_NAMES } from '../packages/cua/adapter.mjs'

const t = () => performance.now()
const ms = (v) => Math.round(v)

const toolkit = new CuaToolkit({ frameDir: 'packages/cua/frames' })
const report = {}

try {
  // 1) 预热
  let t0 = t()
  const ready = await toolkit.warmup()
  report.warmup = { ms: ms(t() - t0), protocol: ready?.protocol, ps: ready?.psVersion, dpiAware: ready?.dpiAware }

  // 2) 工具表
  const tools = toolkit.listTools()
  report.tools = { count: tools.length, names: tools.map((x) => x.function.name) }

  // 3) 后端自检
  t0 = t()
  const access = await toolkit.invoke('request_access', {})
  report.requestAccess = { ms: ms(t() - t0), text: access.text.slice(0, 200) }

  // 4) 截图（语音场景的关键路径）
  t0 = t()
  const shot = await toolkit.invoke('screenshot', {})
  report.screenshot = {
    ms: ms(t() - t0),
    text: shot.text.split('\n')[0],
    saved: shot.data?.savedPath ?? null,
    frameId: shot.data?.frame_id ?? null,
  }

  // 5) 打开记事本
  t0 = t()
  const opened = await toolkit.invoke('open_application', { app: 'notepad', wait_ms: 1200 })
  report.openApplication = { ms: ms(t() - t0), text: opened.text.slice(0, 200) }

  // 6) 确认窗口真的存在
  t0 = t()
  const windows = await toolkit.invoke('list_windows', {})
  const hasNotepad = /notepad|记事本/i.test(windows.text)
  report.listWindows = { ms: ms(t() - t0), hasNotepad, preview: windows.text.split('\n').slice(0, 4).join(' | ') }

  report.verdict = {
    backendUp: report.warmup.dpiAware === true,
    screenshotFast: report.screenshot.ms < 600,
    notepadOpened: hasNotepad,
  }
  report.verdict.pass = Object.values(report.verdict).every((v) => v === true)
} catch (error) {
  report.error = error instanceof Error ? `${error.name}: ${error.message}` : String(error)
  report.verdict = { pass: false }
} finally {
  await toolkit.dispose()
}

console.log(JSON.stringify(report, null, 1))
process.exitCode = report.verdict?.pass === true ? 0 : 1
