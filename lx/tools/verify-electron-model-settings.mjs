/**
 * 设置页「模型提供商 + 推理控制 + 在线模型列表」的真实界面验收（Electron + 生产 preload/renderer）。
 *
 * 为什么必须用真界面：这些行为的对错只有点一下才知道——
 *   - 选中 MiMo 时是不是只显示思考开关（而不是伪造 low/high/max）
 *   - 切走再切回来，两家各自选过的档位有没有被冲掉
 *   - 该提供商实际会忽略温度时，界面上是不是如实禁用（而不是显示能改其实无效）
 *   - 模型列表拉取失败时，用户已选的模型还能不能保住、保存还成不成立
 *   - 刚粘贴还没保存的 Key，是不是也能取到列表
 * Node 侧的静态接线检查证明不了这些，所以加载真正的 settings.html + preload-settings.mjs，
 * 断言**送到主进程 IPC 的 payload** 与界面状态。模型列表与落盘用可控替身（不发真实请求）。
 *
 * 用法：node tools/run-verify.mjs tools/verify-electron-model-settings.mjs
 */
import { app, BrowserWindow, ipcMain } from 'electron'
import assert from 'node:assert/strict'
import { mkdtempSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { DEFAULT_SYSTEM_PROMPT } from '../packages/agent/system-prompt.mjs'
import { buildReasoningMeta } from '../packages/agent/ll-params.mjs'
import { PROVIDERS, defaultConfig } from '../packages/config/registry.mjs'

const tree = import.meta.url.includes('/lx/') ? 'lx' : 'root'
const root = dirname(dirname(fileURLToPath(import.meta.url)))
const report = (value) => console.log(JSON.stringify({ tree, ...value }))

const results = []
function record(name, pass, detail = '') {
  results.push({ name, pass: pass === true, detail })
  report({ check: name, pass: pass === true, detail })
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/* ---------------- 替身：配置、模型列表、保存 ---------------- */
let configState = {}
let savedPayloads = []
let modelsCalls = []
let modelsMode = 'ok'
let saveMode = 'ok'
let saveResult = null

/** 主进程落盘逻辑的替身：只实现这次要验的字段语义 */
function fakeSave(payload) {
  if (saveMode === 'fail') return { ok: false, error: 'baseURL 必须是 http(s) 地址' }
  if (typeof payload.apiKey === 'string' && payload.apiKey !== '' && !payload.apiKey.includes('*')) {
    configState.providerKeys = { ...(configState.providerKeys ?? {}), [payload.providerId]: payload.apiKey }
    configState.hasProviderKey = { ...(configState.hasProviderKey ?? {}), [payload.providerId]: true }
    configState.apiKey = 'sk-m****fake'
  }
  if ('mimoThinking' in payload) configState.mimoThinking = payload.mimoThinking
  if ('deepseekReasoningEffort' in payload) configState.deepseekReasoningEffort = payload.deepseekReasoningEffort
  if ('providerHint' in payload) configState.providerHint = payload.providerHint
  if ('model' in payload) configState.model = payload.model
  if ('providerId' in payload) configState.providerId = payload.providerId
  if (payload.modelProfiles !== undefined) configState.modelProfiles = payload.modelProfiles
  return { ok: true, config: { ...configState }, warnings: [] }
}

const MIMO_LIST = [
  { id: 'mimo-v2.6-flash', effort: null },
  { id: 'mimo-v2.6-pro', effort: null },
  { id: 'mimo-v2.6-pro-ultraspeed', effort: null },
]
const DS_LIST = [
  { id: 'deepseek-flash', effort: { supported_levels: ['low', 'high', 'max'], default_level: 'high' } },
  { id: 'deepseek-v4-pro', effort: { supported_levels: ['low', 'max'], default_level: 'high' } },
]

function registerIpc() {
  ipcMain.handle('settings:load', async () => ({
    config: configState,
    providers: PROVIDERS,
    capabilities: [{ id: 'input.keyboard', label: '键盘输入', level: 'ask' }],
    voices: ['中立'],
    audit: [],
    defaultSystemPrompt: DEFAULT_SYSTEM_PROMPT,
    reasoning: buildReasoningMeta(PROVIDERS),
  }))
  ipcMain.handle('settings:save', async (_event, payload) => {
    savedPayloads.push(payload ?? {})
    saveResult = fakeSave(payload ?? {})
    return saveResult
  })
  ipcMain.handle('settings:models', async (_event, payload) => {
    modelsCalls.push(payload)
    // 竞态测试：若设置了对该提供商的“暂停”，先等 releaseModels
    if (modelsHold !== null && modelsHold.providerId === payload?.providerId) {
      await modelsHold.promise
    }
    if (modelsMode === 'fail') return { ok: false, error: '模型服务返回 401（API Key 无效或未填）', models: [], stale: false }
    if (modelsMode === 'fail-with-cache') {
      return { ok: false, error: '模型列表返回 429（请求过于频繁，稍后再试）', models: MIMO_LIST, stale: true, cached: true }
    }
    if (modelsMode === 'empty') return { ok: false, error: '该端点没有返回任何模型', models: [], stale: false }
    if (payload?.providerId === 'deepseek') {
      return { ok: true, models: DS_LIST, dropped: [], cached: false, stale: false }
    }
    return { ok: true, models: MIMO_LIST, dropped: [{ id: 'mimo-v2.5-asr' }, { id: 'mimo-v2.5-tts' }], cached: false, stale: false }
  })
  ipcMain.handle('settings:models:clear', () => ({ ok: true }))
  ipcMain.handle('settings:openKeyConsole', (_event, payload) => {
    openConsoleCalls.push(payload?.url ?? '')
    return { ok: true }
  })
  ipcMain.handle('settings:testModel', () => ({ ok: true, ms: 5, text: 'ok', reasoningChars: 0 }))
  ipcMain.handle('settings:testSearch', () => ({ ok: true, ms: 1, count: 0, preview: '' }))
  ipcMain.handle('settings:clearSearch', () => ({ ok: true, config: { ...configState } }))
  ipcMain.handle('settings:testVoice', () => ({ ok: false, error: '本测试不合成' }))
  ipcMain.handle('settings:exportAudit', () => ({ ok: false, error: '本测试不导出' }))
  ipcMain.handle('settings:neteaseStatus', () => ({ ok: true, loggedIn: false, text: '' }))
  ipcMain.handle('settings:neteaseLogin', () => ({ ok: false, error: '本测试不登录' }))
  ipcMain.handle('settings:lxStatus', () => ({ ok: false, error: '本测试不连 LX', launched: false }))
}
const openConsoleCalls = []
/** 竞态测试用：暂停某个提供商的模型列表响应，等测试放行 */
let modelsHold = null
function holdModels(providerId) {
  releaseModels()
  const holder = { providerId }
  holder.promise = new Promise((resolve) => { holder.resolve = resolve })
  modelsHold = holder
}
function releaseModels() {
  if (modelsHold === null) return
  const resolve = modelsHold.resolve
  modelsHold = null
  resolve?.()
}

const js = (win, code) => win.webContents.executeJavaScript(code)

async function main() {
  const watchdog = setTimeout(() => {
    report({ pass: false, error: 'watchdog timeout' })
    app.exit(1)
  }, 60000)
  const isolated = mkdtempSync(join(app.getPath('temp'), 'model-settings-electron-'))
  app.setPath('userData', isolated)
  // 隔离配置目录：本用例绝不读写仓库里的 .config/agent.json
  process.env.CUA_VA_CONFIG_DIR = join(isolated, 'config')
  let win = null
  try {
    await app.whenReady()
    registerIpc()
    // 起始状态 = 全新安装：默认提供商 MiMo，没有任何 Key
    configState = { ...defaultConfig(), apiKey: '', hasApiKey: false, providerKeys: {}, hasProviderKey: {},
      modelProfiles: [], modelSchedule: { enabled: false, rules: [] },
      search: { provider: 'tavily', apiKey: '', hasApiKey: false },
      permissions: { mode: 'standard', overrides: {} },
      tts: { emotion: '中立', speed: 1 },
      ui: { ttsEnabled: true, continuousMode: true, voiceConfirmEnabled: true, subtitles: true, autoStart: false, releaseOnFullscreen: true, wakeEnabled: true } }

    win = new BrowserWindow({
      width: 820, height: 900, show: false, backgroundColor: '#14161c',
      webPreferences: { preload: join(root, 'apps/shell/preload-settings.mjs'), contextIsolation: true, nodeIntegration: false, sandbox: false },
    })
    await win.loadFile(join(root, 'apps/shell/renderer/settings.html'))
    await sleep(600)
    report({ stage: 'loaded', electron: process.versions.electron })

    /* ---------------- 1. 新装默认 ---------------- */
    const initial = await js(win, `(() => ({
      provider: document.getElementById('provider').value,
      baseURL: document.getElementById('baseURL').value,
      model: document.getElementById('model').value,
      candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value),
      mimoVisible: !document.getElementById('mimoThinkingField').hidden,
      dsVisible: !document.getElementById('deepseekField').hidden,
      mimoLevels: Array.from(document.getElementById('mimoThinking').options).map((o) => o.value),
      mimoSelected: document.getElementById('mimoThinking').value,
      keyConsoleHidden: document.getElementById('openKeyConsole').hidden,
      rowHidden: document.getElementById('reasoningRow').hidden,
    }))()`)
    record('新装默认提供商是 Xiaomi MiMo', initial.provider === 'mimo', initial.provider)
    record('默认地址为官方按量付费端点', initial.baseURL === 'https://api.xiaomimimo.com/v1', initial.baseURL)
    record('默认建议模型 mimo-v2.6-flash', initial.model === 'mimo-v2.6-flash', initial.model)
    record('MiMo 显示思考开关、不显示 DeepSeek 等级',
      initial.mimoVisible === true && initial.dsVisible === false && initial.rowHidden === false, JSON.stringify(initial))
    record('MiMo 只有三档（不伪造强度档）',
      JSON.stringify(initial.mimoLevels) === JSON.stringify(['default', 'enabled', 'disabled']), initial.mimoLevels)
    record('MiMo 默认档不是"关闭思考"', initial.mimoSelected === 'default', initial.mimoSelected)
    record('MiMo 有"获取 API Key"入口', initial.keyConsoleHidden === false)
    record('候选列表不含 ASR/TTS',
      initial.candidates.every((id) => !id.includes('asr') && !id.includes('tts')) && initial.candidates.length === 3,
      initial.candidates)

    /* ---------------- 2. 无 Key 时不发列表请求 ---------------- */
    record('没有 API Key 时不自动请求模型列表', modelsCalls.length === 0, JSON.stringify(modelsCalls))

    /* ---------------- 3. 粘贴未保存的 Key → 自动取列表 ---------------- */
    modelsMode = 'ok'
    await js(win, `(async () => {
      const key = document.getElementById('apiKey');
      key.value = 'sk-freshly-pasted-not-saved';
      key.dispatchEvent(new Event('input', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 1400));
      return document.getElementById('modelsHint').textContent;
    })()`)
    await sleep(400)
    const fetched = await js(win, `(() => ({
      hint: document.getElementById('modelsHint').textContent,
      candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value),
      model: document.getElementById('model').value,
    }))()`)
    record('刚粘贴未保存的 Key 也能获取模型列表',
      modelsCalls.length > 0 && modelsCalls.at(-1).apiKey === 'sk-freshly-pasted-not-saved',
      `calls=${modelsCalls.length} key=${modelsCalls.at(-1)?.apiKey}`)
    record('列表里只有聊天模型并说明已排除数量',
      fetched.candidates.length === 3 && fetched.hint.includes('3') && fetched.hint.includes('排除'),
      `${fetched.hint} | ${fetched.candidates}`)
    record('取列表不会改写用户已选的模型', fetched.model === 'mimo-v2.6-flash', fetched.model)

    /* ---------------- 4. 档位与温度提示 ---------------- */
    const mimoUi = async (value) => await js(win, `(async () => {
      const sel = document.getElementById('mimoThinking');
      sel.value = ${JSON.stringify(value)};
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 50));
      return {
        tempDisabled: document.getElementById('temperature').disabled,
        tempHint: document.getElementById('temperatureHint').textContent,
        note: document.getElementById('reasoningNote').textContent,
      };
    })()`)
    const def = await mimoUi('default')
    record('MiMo 默认档：不发送温度（思考默认开且会覆盖）',
      def.tempDisabled === true && def.tempHint !== '' && def.note.includes('不发送温度'), `${def.tempDisabled}/${def.tempHint}`)
    const off = await mimoUi('disabled')
    record('MiMo 关闭思考：温度恢复可发送',
      off.tempDisabled === false && off.tempHint === '', `${off.tempDisabled}/${off.tempHint}`)
    const on = await mimoUi('enabled')
    record('MiMo 开启思考：如实说明参数被覆盖', on.tempDisabled === true && on.note.includes('强制覆盖'), on.note.slice(0, 40))

    /* ---------------- 5. 保存 payload ---------------- */
    await js(win, `(async () => { document.getElementById('save').click(); await new Promise((r) => setTimeout(r, 400)); })()`)
    const savedMimo = savedPayloads.at(-1)
    record('保存把 MiMo 档位送进 IPC', savedMimo?.mimoThinking === 'enabled', savedMimo?.mimoThinking)
    record('保存同时保留 DeepSeek 档位字段（不省略成空）',
      typeof savedMimo?.deepseekReasoningEffort === 'string' && savedMimo.deepseekReasoningEffort !== '',
      savedMimo?.deepseekReasoningEffort)

    /* ---------------- 6. 切提供商：两家的档位互不冲掉 ---------------- */
    /*
     * 先把"两家都存过 Key"这件事真的走一遍保存：界面读的是上一次保存回传的 config，
     * 直接改替身内部状态是不会生效的（没有已存 Key 时界面本就不该去打 API）。
     */
    configState.hasProviderKey = { ...(configState.hasProviderKey ?? {}), mimo: true, deepseek: true }
    await js(win, `(async () => {
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 500));
    })()`)
    const switchToDs = await js(win, `(async () => {
      const p = document.getElementById('provider');
      p.value = 'deepseek';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 700));
      const sel = document.getElementById('deepseekEffort');
      sel.value = 'max';
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 50));
      return {
        baseURL: document.getElementById('baseURL').value,
        model: document.getElementById('model').value,
        mimoHidden: document.getElementById('mimoThinkingField').hidden,
        dsHidden: document.getElementById('deepseekField').hidden,
        dsLevels: Array.from(sel.options).map((o) => o.value),
        dsSelected: sel.value,
        mimoStill: document.getElementById('mimoThinking').value,
        tempDisabled: document.getElementById('temperature').disabled,
        candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value),
      };
    })()`)
    record('切到 DeepSeek 用它的地址与推荐模型',
      switchToDs.baseURL === 'https://api.deepseek.com/v1' && switchToDs.model === 'deepseek-flash',
      `${switchToDs.baseURL}/${switchToDs.model}`)
    record('DeepSeek 显示五档等级（无 medium 独立档）',
      JSON.stringify(switchToDs.dsLevels) === JSON.stringify(['default', 'none', 'low', 'high', 'max']) && switchToDs.dsSelected === 'max',
      switchToDs.dsLevels)
    record('切提供商时 MiMo 那档仍然留着', switchToDs.mimoStill === 'enabled', switchToDs.mimoStill)
    record('DeepSeek 思考档下温度不发送', switchToDs.tempDisabled === true)
    record('切提供商后取的是 DeepSeek 的模型列表',
      modelsCalls.at(-1)?.providerId === 'deepseek' && switchToDs.candidates.includes('deepseek-flash'),
      `calls=${JSON.stringify(modelsCalls.map((c) => c.providerId))} candidates=${switchToDs.candidates}`)

    /* ---------------- 7. 能力元数据限制档位 ---------------- */
    // 先选 high（v4-pro 不支持），再把模型切到 v4-pro：界面要置灰并就近降级
    const effortMeta = await js(win, `(async () => {
      const sel = document.getElementById('deepseekEffort');
      sel.value = 'high';
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 80));
      const m = document.getElementById('model');
      m.value = 'deepseek-v4-pro';
      m.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 80));
      return {
        disabled: Array.from(sel.options).filter((o) => o.disabled).map((o) => o.value),
        selected: sel.value,
        note: document.getElementById('reasoningNote').textContent,
        candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value),
      };
    })()`)
    record('DeepSeek 在线列表作为候选来源', effortMeta.candidates.includes('deepseek-v4-pro'), effortMeta.candidates)
    record('模型声明不支持的档位被置灰', effortMeta.disabled.includes('high'), JSON.stringify(effortMeta.disabled))
    record('选了不支持的档位会就近降级并说明',
      effortMeta.selected === 'low' && effortMeta.note.includes('只支持'), `${effortMeta.selected} | ${effortMeta.note.slice(-40)}`)

    /* ---------------- 8. 切回 MiMo：档位回来 ---------------- */
    const backToMimo = await js(win, `(async () => {
      const p = document.getElementById('provider');
      p.value = 'mimo';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 700));
      return {
        mimo: document.getElementById('mimoThinking').value,
        ds: document.getElementById('deepseekEffort').value,
        placeholder: document.getElementById('apiKey').placeholder,
      };
    })()`)
    record('切回 MiMo 仍是用户选的那一档', backToMimo.mimo === 'enabled', backToMimo.mimo)
    // 上一步按 v4-pro 的能力把 max 就近降到 low（界面降级后的值），切回来应该还是它
    record('切回后 DeepSeek 档位也没丢', backToMimo.ds === 'low', backToMimo.ds)

    await js(win, `(async () => { document.getElementById('save').click(); await new Promise((r) => setTimeout(r, 400)); })()`)
    const savedBoth = savedPayloads.at(-1)
    record('一次保存同时落两家档位且各归各的',
      savedBoth?.mimoThinking === 'enabled' && savedBoth?.deepseekReasoningEffort === 'low',
      `${savedBoth?.mimoThinking}/${savedBoth?.deepseekReasoningEffort}`)

    /* ---------------- 9. 自定义端点：协议提示决定控件 ---------------- */
    const custom = await js(win, `(async () => {
      const p = document.getElementById('provider');
      p.value = 'openai-compatible';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 300));
      const before = {
        hintHidden: document.getElementById('providerHintField').hidden,
        hintLevels: Array.from(document.getElementById('providerHint').options).map((o) => o.value),
        mimoHidden: document.getElementById('mimoThinkingField').hidden,
        dsHidden: document.getElementById('deepseekField').hidden,
        consoleHidden: document.getElementById('openKeyConsole').hidden,
      };
      const h = document.getElementById('providerHint');
      h.value = 'deepseek';
      h.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 120));
      const after = { dsHidden: document.getElementById('deepseekField').hidden, mimoHidden: document.getElementById('mimoThinkingField').hidden };
      h.value = 'auto';
      h.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 120));
      const back = { dsHidden: document.getElementById('deepseekField').hidden };
      return { before, after, back };
    })()`)
    record('自定义端点出现兼容协议选择（默认自动）',
      custom.before.hintHidden === false && custom.before.hintLevels[0] === 'auto', custom.before.hintLevels)
    record('自动 = 不显示任何推理控件（不发专属参数）',
      custom.before.mimoHidden === true && custom.before.dsHidden === true && custom.before.consoleHidden === true,
      JSON.stringify(custom.before))
    record('声明 DeepSeek 协议后才出现等级控件', custom.after.dsHidden === false && custom.after.mimoHidden === true)
    record('改回自动后等级控件消失', custom.back.dsHidden === true)

    /* ---------------- 10. 列表失败：保住模型、不阻止保存 ---------------- */
    await js(win, `(async () => {
      const p = document.getElementById('provider');
      p.value = 'mimo';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 700));
    })()`)
    const beforeFail = await js(win, `(() => document.getElementById('model').value)()`)
    modelsMode = 'fail'
    await js(win, `(async () => { document.getElementById('refreshModels').click(); await new Promise((r) => setTimeout(r, 500)); })()`)
    const afterFail = await js(win, `(() => ({
      model: document.getElementById('model').value,
      hint: document.getElementById('modelsHint').textContent,
      candidates: Array.from(document.getElementById('modelList').options).length,
      tempDisabled: document.getElementById('temperature').disabled,
    }))()`)
    record('列表获取失败不改变已选模型', afterFail.model === beforeFail, `${beforeFail} → ${afterFail.model}`)
    record('失败时提示可手填（不拦住保存）', afterFail.hint.includes('401') && afterFail.hint.includes('手填'), afterFail.hint)
    saveMode = 'ok'
    const saveAfterFail = await js(win, `(async () => {
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 500));
      return document.getElementById('status').className;
    })()`)
    record('列表失败之后仍然能保存设置', saveAfterFail === 'ok', saveAfterFail)

    modelsMode = 'fail-with-cache'
    await js(win, `(async () => { document.getElementById('refreshModels').click(); await new Promise((r) => setTimeout(r, 500)); })()`)
    const staleUi = await js(win, `(() => ({ hint: document.getElementById('modelsHint').textContent, candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value) }))()`)
    record('429 时沿用上一次拿到的列表并说明', staleUi.hint.includes('先用上一次') && staleUi.candidates.length === 3, staleUi.hint)

    modelsMode = 'empty'
    await js(win, `(async () => { document.getElementById('refreshModels').click(); await new Promise((r) => setTimeout(r, 500)); })()`)
    const emptyUi = await js(win, `(() => ({ model: document.getElementById('model').value, candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value) }))()`)
    record('空列表不会清空候选或已选模型', emptyUi.candidates.length === 3 && emptyUi.model === beforeFail,
      `${emptyUi.candidates.length}/${emptyUi.model}`)
    modelsMode = 'ok'

    /* ---------------- 11. 控制台入口 ---------------- */
    await js(win, `(async () => { document.getElementById('openKeyConsole').click(); await new Promise((r) => setTimeout(r, 200)); })()`)
    record('控制台入口打开的是官方 Key 页面',
      openConsoleCalls.at(-1) === 'https://platform.xiaomimimo.com/#/console/api-keys', openConsoleCalls.at(-1))

    /* ---------------- 12. 模型方案的推理列 ---------------- */
    const profileUi = await js(win, `(async () => {
      document.getElementById('addProfile').click();
      await new Promise((r) => setTimeout(r, 150));
      const row = document.querySelector('#profileRows tr');
      const sel = row.querySelector('[data-field="reasoning"]');
      const out = {
        fields: Array.from(row.querySelectorAll('[data-field]')).map((el) => el.dataset.field),
        provider: row.querySelector('[data-field="providerId"]').value,
        levels: Array.from(sel.options).map((o) => o.value),
        hidden: sel.hidden,
        model: row.querySelector('[data-field="model"]').value,
      };
      const p = row.querySelector('[data-field="providerId"]');
      p.value = 'deepseek';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 120));
      out.afterSwitch = Array.from(sel.options).map((o) => o.value);
      sel.value = 'low';
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 600));
      return out;
    })()`)
    record('方案行带推理档位与协议提示',
      profileUi.fields.includes('reasoning') && profileUi.fields.includes('providerHint'), profileUi.fields)
    record('新方案跟随当前提供商（MiMo 三档）',
      profileUi.provider === 'mimo' && JSON.stringify(profileUi.levels) === JSON.stringify(['default', 'enabled', 'disabled'])
        && profileUi.hidden === false && profileUi.model === 'mimo-v2.6-flash',
      `${profileUi.provider}/${profileUi.model}`)
    record('方案换成 DeepSeek 后档位集合跟着变',
      JSON.stringify(profileUi.afterSwitch) === JSON.stringify(['default', 'none', 'low', 'high', 'max']), profileUi.afterSwitch)
    const savedProfile = savedPayloads.at(-1)?.modelProfiles?.[0]
    record('保存方案把该提供商的档位写进对应字段',
      savedProfile?.deepseekReasoningEffort === 'low' && savedProfile?.providerId === 'deepseek',
      JSON.stringify({ p: savedProfile?.providerId, e: savedProfile?.deepseekReasoningEffort, t: savedProfile?.mimoThinking }))

    /* ---------------- 13. PR3 提示词语义未受影响 ---------------- */
    const noPrompt = !('systemPrompt' in savedPayloads.at(-1))
    record('保存模型设置不会顺手重写系统提示词', noPrompt, `keys=${Object.keys(savedPayloads.at(-1)).join(',')}`)

    /* ---------------- 14. 保存失败时不谎报生效 ---------------- */
    saveMode = 'fail'
    const failedSave = await js(win, `(async () => {
      document.getElementById('save').click();
      await new Promise((r) => setTimeout(r, 500));
      return { cls: document.getElementById('status').className, text: document.getElementById('status').textContent };
    })()`)
    record('保存失败时界面显示失败（不显示已生效）',
      failedSave.cls === 'bad' && failedSave.text.includes('baseURL'), failedSave.text)

    /* ---------------- 15. 模型列表异步竞态：旧响应不覆盖新选择 ---------------- */
    saveMode = 'ok'
    // 先切到 MiMo：帮它设置“请求暂停”，让 MiMo 的列表请求挂起
    await js(win, `(async () => {
      const p = document.getElementById('provider');
      p.value = 'mimo';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 300));
    })()`)
    holdModels('mimo')
    const mimoStarted = modelsCalls.length
    await js(win, `(async () => {
      document.getElementById('refreshModels').click();
      await new Promise((r) => setTimeout(r, 120));
    })()`)
    const afterMimoStart = await js(win, `(() => document.getElementById('modelsHint').textContent)()`)
    record('15a MiMo 手动刷新请求已发出（挂起中）',
      modelsCalls.length === mimoStarted + 1 && afterMimoStart.includes('正在获取'),
      `calls=${modelsCalls.length} hint=${afterMimoStart}`)

    // 切到 DeepSeek：DeepSeek 请求不挂起，应该先返回并显示 DeepSeek 列表
    await js(win, `(async () => {
      const p = document.getElementById('provider');
      p.value = 'deepseek';
      p.dispatchEvent(new Event('change', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 500));
    })()`)
    const dsAfterSwitch = await js(win, `(() => ({
      provider: document.getElementById('provider').value,
      candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value),
      hint: document.getElementById('modelsHint').textContent,
    }))()`)
    record('15b 切到 DeepSeek 后显示 DeepSeek 列表',
      dsAfterSwitch.provider === 'deepseek' && dsAfterSwitch.candidates.includes('deepseek-flash'),
      JSON.stringify({ p: dsAfterSwitch.provider, cands: dsAfterSwitch.candidates.length, hint: dsAfterSwitch.hint }))

    // 释放 MiMo 的旧请求：返回后不得把页面改成 MiMo 列表
    releaseModels()
    await sleep(400)
    const afterRelease = await js(win, `(() => ({
      provider: document.getElementById('provider').value,
      candidates: Array.from(document.getElementById('modelList').options).map((o) => o.value),
      hint: document.getElementById('modelsHint').textContent,
    }))()`)
    record('15c 释放后旧 MiMo 响应不覆盖当前 DeepSeek 页面',
      afterRelease.provider === 'deepseek' && afterRelease.candidates.includes('deepseek-flash')
        && !afterRelease.candidates.includes('mimo-v2.6-pro'),
      JSON.stringify({ p: afterRelease.provider, cands: afterRelease.candidates, hint: afterRelease.hint }))
    releaseModels()

    clearTimeout(watchdog)
    const failed = results.filter((r) => !r.pass).length
    report({ suite: 'model provider + reasoning settings UI', electron: process.versions.electron,
      cases: results.length, failed, pass: failed === 0, modelCalls: modelsCalls.length })
    app.exit(failed === 0 ? 0 : 1)
  } catch (error) {
    clearTimeout(watchdog)
    report({ pass: false, error: error.stack ?? String(error) })
    console.error(error.stack ?? error)
    app.exit(1)
  } finally {
    if (win !== null && !win.isDestroyed()) win.destroy()
  }
}

void main()
