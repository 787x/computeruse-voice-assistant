// cua.ps1 握手与坐标系验证（P0 关键里程碑）
// 用法: node tools/probe-cua.mjs
import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const script = join(here, '..', 'refs', 'dsh-computer-use', 'src', 'native', 'cua.ps1')

const t0 = performance.now()
const child = spawn('powershell.exe', [
  '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script,
], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true })

let stderrBuf = ''
child.stderr.on('data', (d) => { stderrBuf += d.toString('utf8') })

const rl = createInterface({ input: child.stdout })
const waiters = new Map()
let seq = 0
const results = { ready: null, screen: null, shot: null, ping: null, errors: [] }

rl.on('line', (line) => {
  const t = line.trim()
  if (!t) return
  let msg
  try { msg = JSON.parse(t) } catch { results.errors.push({ kind: 'non-json-stdout', line: t.slice(0, 200) }); return }
  if (msg.event === 'ready' && msg.id === null) { results.ready = { ...msg.data, ms: Math.round(performance.now() - t0) }; return }
  const w = waiters.get(msg.id)
  if (w) { waiters.delete(msg.id); w(msg) }
})

function call(cmd, args = {}, timeoutMs = 15000) {
  const id = `probe-${++seq}`
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { waiters.delete(id); reject(new Error(`timeout ${cmd}`)) }, timeoutMs)
    waiters.set(id, (msg) => { clearTimeout(timer); msg.ok ? resolve(msg.data) : reject(new Error(`${msg.error?.code}: ${msg.error?.message}`)) })
    child.stdin.write(JSON.stringify({ id, cmd, args }) + '\n')
  })
}

const waitReady = () => new Promise((resolve, reject) => {
  const started = performance.now()
  const iv = setInterval(() => {
    if (results.ready) { clearInterval(iv); resolve() }
    if (performance.now() - started > 20000) { clearInterval(iv); reject(new Error('ready banner timeout')) }
  }, 50)
})
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

try {
  await waitReady()
  results.ping = await call('ping')
  results.screen = await call('screen_info')

  const s0 = performance.now()
  const shot = await call('screenshot', { max_width: 1280, format: 'png' })
  results.shot = {
    ms: Math.round(performance.now() - s0),
    frame_id: shot.frame_id,
    image: { width: shot.image.width, height: shot.image.height, bytes: shot.image.bytes, mediaType: shot.image.mediaType },
    source: shot.source, scale: shot.scale, screenshot_display: shot.screenshot_display,
  }
  // 二次截图：验证 frame_id 递增与几何稳定
  const shot2 = await call('screenshot', { max_width: 1280 })
  results.shot2 = { frame_id: shot2.frame_id, width: shot2.image.width, height: shot2.image.height, source: shot2.source }

  // 再探一次 ping，确认进程在多次调用后仍存活
  results.ping2 = await call('ping')
  console.log(JSON.stringify(results, null, 2))
} catch (e) {
  console.log(JSON.stringify({ ...results, fatal: String(e), stderr: stderrBuf.slice(-1500) }, null, 2))
  process.exitCode = 1
} finally {
  try { child.stdin.write(JSON.stringify({ id: 'bye', cmd: 'shutdown', args: {} }) + '\n') } catch {}
  await sleep(300)
  try { child.kill() } catch {}
}
