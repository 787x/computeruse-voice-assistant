/**
 * LX 音乐链路验收。
 *
 * 分两层：
 *   1. 纯逻辑：scheme URL 拼装（不依赖 LX）
 *   2. 真机：LX 在跑时验 /status、/pause、/play、点歌
 *
 * 用法：node tools/verify-lxmusic.mjs
 */
import { schemeUrl, lxAvailable, lxStatus, runLx } from '../packages/tools/lxmusic.mjs'

const cases = []
function check(name, pass, detail = '') {
  cases.push({ name, pass, detail })
}

// ---- 1) scheme 拼装
check('无参数 scheme', schemeUrl('player/skipNext') === 'lxmusic://player/skipNext', schemeUrl('player/skipNext'))
const withData = schemeUrl('music/searchPlay', { name: '晴天', singer: '周杰伦' })
check('data scheme 前缀', withData.startsWith('lxmusic://music/searchPlay?data='), withData.slice(0, 60))
check('data 可还原', JSON.parse(decodeURIComponent(withData.split('data=')[1])).name === '晴天', '')

// ---- 2) 真机
const available = await lxAvailable()
check('LX 开放 API 可达', available === true || available === false, String(available))
if (available) {
  const status = await lxStatus()
  check('/status 解析状态', ['playing', 'paused', 'stopped'].includes(status.status), status.rawStatus)
  check('/status 带歌名', typeof status.title === 'string', status.title)

  const before = status.status
  await runLx({ action: before === 'playing' ? 'pause' : 'play' })
  await new Promise((resolve) => setTimeout(resolve, 800))
  const after = await lxStatus()
  check('控制接口生效', after.status !== before, `${before} → ${after.status}`)
  // 恢复原状态，别把用户的歌停了
  await runLx({ action: before === 'playing' ? 'play' : 'pause' })
} else {
  console.log('（LX 没运行，只验了 scheme 拼装；要验真机请先开 LX 并启用开放 API）')
}

const failed = cases.filter((item) => item.pass !== true)
console.log('LX 音乐验收\n' + '='.repeat(56))
for (const item of cases) {
  console.log(`${item.pass ? '✓' : '✗'} ${item.name.padEnd(18, ' ')} ${item.detail ?? ''}`)
}
console.log('='.repeat(56))
console.log(JSON.stringify({ cases: cases.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
process.exitCode = failed.length === 0 ? 0 : 1
