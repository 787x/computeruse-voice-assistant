/**
 * 截取指定应用窗口（用于 UI 参考分析）。
 *
 * 用法：node tools/capture-window.mjs <窗口标题关键字> [输出前缀]
 * 做法：先 list_windows 找到窗口矩形，再按矩形区域截图（比全屏更能看清 UI）。
 */
import { CuaToolkit } from '../packages/cua/adapter.mjs'

const keyword = (process.argv[2] ?? '').toLowerCase()
const toolkit = new CuaToolkit({ frameDir: 'packages/cua/frames' })

try {
  await toolkit.warmup()
  const windows = await toolkit.invoke('list_windows', {})
  const lines = windows.text.split('\n').filter((line) => line.includes('hwnd='))
  console.log('窗口清单：')
  for (const line of lines) console.log('  ' + line.trim())

  const target = lines.find((line) => line.toLowerCase().includes(keyword))
  if (!target) {
    console.log(`\n没有匹配 "${keyword}" 的窗口`)
    process.exitCode = 2
  } else {
    // screenshot 工具本身支持按窗口截取，直接传 hwnd 最准
    const hwnd = target.match(/hwnd="([^"]+)"/u)?.[1]
    const shot = await toolkit.invoke('screenshot', { window: hwnd, max_width: 1400, format: 'jpeg' })
    console.log(`\n已截取: ${shot.data?.savedPath}`)
    console.log(shot.text.split('\n')[0])
  }
} finally {
  await toolkit.dispose()
}
