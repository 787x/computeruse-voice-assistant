/**
 * 独立播放面板的渲染逻辑。
 *
 * 与 HUD 完全解耦：这个窗口只关心"在不在播、放到哪了"，
 * 字幕窗口开合、语音播报都影响不到它。
 */
const els = {
  title: document.getElementById('title'),
  artist: document.getElementById('artist'),
  fill: document.getElementById('fill'),
  toggle: document.getElementById('toggle'),
}

/** 主进程每秒推一次 */
window.playerApi.onState((state) => {
  if (state === null) return
  if (typeof state.title === 'string' && state.title !== '') els.title.textContent = state.title
  els.artist.textContent = typeof state.artist === 'string' ? state.artist : ''

  const duration = Number(state.duration) || 0
  const position = Number(state.position) || 0
  els.fill.style.width = duration > 0 ? `${Math.min(100, (position / duration) * 100)}%` : '0%'

  // 暂停时显示 ▶（点一下继续放），播放中显示 ⏸
  els.toggle.textContent = state.status === 'paused' ? '▶' : '⏸'
})

for (const [id, action] of [['prev', 'prev'], ['toggle', 'toggle'], ['next', 'next'], ['close', 'close']]) {
  const el = document.getElementById(id)
  if (el !== null) el.addEventListener('click', () => window.playerApi.action(action))
}
