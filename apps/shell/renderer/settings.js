/**
 * 设置窗口渲染层：只做表单与 IPC 调用，所有校验与落盘都在主进程。
 *
 * 为什么不在渲染层直接算：密钥打码、权限合并、模型改名后的默认值这些规则
 * 必须只有一份实现，否则界面和真实行为迟早对不上。
 */
const $ = (id) => document.getElementById(id)

let state = null
let providerList = []
let capabilityList = []
let voices = []

function setStatus(text, kind = '') {
  $('status').textContent = text
  $('status').className = kind
}

function fillProviderNote() {
  const provider = providerList.find((item) => item.id === $('provider').value)
  const list = $('modelList')
  list.innerHTML = ''
  for (const model of provider?.models ?? []) {
    const option = document.createElement('option')
    option.value = model
    list.append(option)
  }
  $('modelHint').textContent = provider?.notes ?? ''
}

function renderProviders() {
  const select = $('provider')
  select.innerHTML = ''
  for (const provider of providerList) {
    const option = document.createElement('option')
    option.value = provider.id
    option.textContent = provider.name
    select.append(option)
  }
}

function renderVoices() {
  const select = $('emotion')
  select.innerHTML = ''
  for (const name of voices) {
    const option = document.createElement('option')
    option.value = name
    option.textContent = name
    select.append(option)
  }
}

function renderCapabilities() {
  const body = $('capabilities')
  body.innerHTML = ''
  const overrides = state.config.permissions?.overrides ?? {}
  for (const capability of capabilityList) {
    const row = document.createElement('tr')
    const name = document.createElement('td')
    name.textContent = capability.label
    const level = document.createElement('td')
    const select = document.createElement('select')
    select.dataset.capability = capability.id
    for (const [value, text] of [['', `跟随档位（${capability.level}）`], ['allow', '允许'], ['ask', '每次询问'], ['deny', '拒绝']]) {
      const option = document.createElement('option')
      option.value = value
      option.textContent = text
      select.append(option)
    }
    select.value = overrides[capability.id] ?? ''
    level.append(select)
    row.append(name, level)
    body.append(row)
  }
}

/* ---------------- 按时间段切换模型 ---------------- */
/** "1-5" / "1,3" → 数字数组（老配置回显用） */
function parseDaysText(value) {
  if (value === undefined || value === null || value === '') return []
  const out = new Set()
  for (const piece of String(value).split(/[,，\s]+/u)) {
    const range = piece.match(/^(\d)-(\d)$/u)
    if (range !== null) {
      for (let day = Number(range[1]); day <= Number(range[2]); day += 1) out.add(day)
      continue
    }
    const single = Number(piece)
    if (Number.isInteger(single) && single >= 1 && single <= 7) out.add(single)
  }
  return [...out]
}
function renderScheduleRows(rules) {
  const body = $('scheduleRows')
  body.innerHTML = ''
  for (const [index, rule] of rules.entries()) {
    const row = document.createElement('tr')
    const fromCell = document.createElement('td')
    const from = document.createElement('input')
    from.value = rule.from ?? '09:00'
    from.placeholder = 'HH:mm'
    from.dataset.field = 'from'
    fromCell.append(from)
    const toCell = document.createElement('td')
    const to = document.createElement('input')
    to.value = rule.to ?? '18:00'
    to.placeholder = 'HH:mm'
    to.dataset.field = 'to'
    toCell.append(to)
    const modelCell = document.createElement('td')
    const model = document.createElement('input')
    model.value = rule.model ?? ''
    model.placeholder = 'deepseek-flash'
    model.spellcheck = false
    model.dataset.field = 'model'
    modelCell.append(model)
    // 星期：七个可点的小标签，不选=每天
    const daysCell = document.createElement('td')
    const daysWrap = document.createElement('div')
    daysWrap.className = 'days'
    const currentDays = new Set(Array.isArray(rule.days) ? rule.days.map(Number) : parseDaysText(rule.days))
    const label = ['一', '二', '三', '四', '五', '六', '日']
    for (let day = 1; day <= 7; day += 1) {
      const chip = document.createElement('button')
      chip.type = 'button'
      chip.className = currentDays.has(day) ? 'chip on' : 'chip'
      chip.textContent = label[day - 1]
      chip.addEventListener('click', () => {
        if (currentDays.has(day)) currentDays.delete(day)
        else currentDays.add(day)
        chip.className = currentDays.has(day) ? 'chip on' : 'chip'
        daysCell.dataset.days = [...currentDays].sort((a, b) => a - b).join(',')
      })
      daysWrap.append(chip)
    }
    daysCell.dataset.days = [...currentDays].sort((a, b) => a - b).join(',')
    daysCell.append(daysWrap)

    const delCell = document.createElement('td')
    const del = document.createElement('button')
    del.textContent = '删'
    del.addEventListener('click', () => {
      const current = collectSchedule()
      current.splice(index, 1)
      renderScheduleRows(current)
    })
    delCell.append(del)
    row.append(fromCell, toCell, modelCell, daysCell, delCell)
    body.append(row)
  }
}

function collectSchedule() {
  const rows = []
  for (const row of $('scheduleRows').querySelectorAll('tr')) {
    const pick = (name) => row.querySelector(`[data-field="${name}"]`)?.value ?? ''
    const days = (row.querySelector('[data-days]')?.dataset.days ?? '').split(',').filter(Boolean).map(Number)
    rows.push({ from: pick('from').trim(), to: pick('to').trim(), model: pick('model').trim(), days })
  }
  return rows
}

$('addSchedule').addEventListener('click', () => {
  const current = collectSchedule()
  current.push({ from: '09:00', to: '18:00', model: $('model').value || state.config.model })
  renderScheduleRows(current)
})
function fillForm() {
  const config = state.config
  $('provider').value = config.providerId
  $('baseURL').value = config.baseURL
  $('model').value = config.model
  $('apiKey').value = ''
  $('apiKey').placeholder = config.hasApiKey ? `已保存（${config.apiKey}），留空不修改` : '未配置'
  $('maxSteps').value = config.maxSteps
  $('temperature').value = config.temperature

  $('searchKey').value = ''
  $('searchKey').placeholder = config.search?.hasApiKey ? `已保存（${config.search.apiKey}），留空不修改` : '未配置（可选）'

  // 网易云：appId 明文显示，私钥只显示"已保存"，避免把 1600 字符塞进输入框
  const netease = config.netease ?? {}
  $('neteaseAppId').value = netease.appId ?? ''
  $('neteasePrivateKey').value = ''
  $('neteasePrivateKey').placeholder = netease.privateKey === undefined || netease.privateKey === ''
    ? '粘贴私钥（PKCS8，base64 那一长串）'
    : `已保存（${String(netease.privateKey).length} 字符），留空不修改`

  $('emotion').value = config.tts?.emotion ?? '中立'
  $('speed').value = String(config.tts?.speed ?? 1)
  $('ttsEnabled').checked = config.ui?.ttsEnabled !== false
  $('continuousMode').checked = config.ui?.continuousMode !== false
  $('voiceConfirmEnabled').checked = config.ui?.voiceConfirmEnabled !== false
  $('subtitles').checked = config.ui?.subtitles !== false
  $('autoStart').checked = config.ui?.autoStart === true
  $('releaseOnFullscreen').checked = config.ui?.releaseOnFullscreen !== false

  $('mode').value = config.permissions?.mode ?? 'standard'
  const schedule = config.modelSchedule ?? { enabled: false, rules: [] }
  scheduleEnabled.checked = schedule.enabled === true
  renderScheduleRows(Array.isArray(schedule.rules) ? schedule.rules : [])

  fillProviderNote()
  renderCapabilities()
}

function collect() {
  const overrides = {}
  for (const select of document.querySelectorAll('#capabilities select')) {
    if (select.value !== '') overrides[select.dataset.capability] = select.value
  }
  // 选项还没渲染出来时 select.value 会是空串，别把空值写进配置
  const emotion = $('emotion').value || state.config.tts?.emotion || '中立'
  const payload = {
    providerId: $('provider').value,
    baseURL: $('baseURL').value.trim(),
    model: $('model').value.trim(),
    maxSteps: Number($('maxSteps').value),
    temperature: Number($('temperature').value),
    tts: { emotion, speed: Number($('speed').value) },
    ui: {
      ttsEnabled: $('ttsEnabled').checked,
      continuousMode: $('continuousMode').checked,
      voiceConfirmEnabled: $('voiceConfirmEnabled').checked,
      subtitles: $('subtitles').checked,
      autoStart: $('autoStart').checked,
      releaseOnFullscreen: $('releaseOnFullscreen').checked,
    },
    permissions: { mode: $('mode').value, overrides },
  }
  if ($('apiKey').value.trim() !== '') payload.apiKey = $('apiKey').value.trim()
  if ($('searchKey').value.trim() !== '') payload.search = { provider: 'tavily', apiKey: $('searchKey').value.trim() }
  // 网易云：两个都填了才覆盖，留空表示不动（跟 API Key 同一套语义）
  if ($('neteaseAppId').value.trim() !== '') payload.netease = { appId: $('neteaseAppId').value.trim() }
  if ($('neteasePrivateKey').value.trim() !== '') {
    payload.netease = { ...(payload.netease ?? {}), privateKey: $('neteasePrivateKey').value.trim() }
  }
  return payload
}

async function load() {
  const data = await window.settingsApi.load()
  state = data
  providerList = data.providers ?? []
  capabilityList = data.capabilities ?? []
  voices = data.voices ?? []
  renderProviders()
  renderVoices()
  fillForm()
  $('audit').textContent = (data.audit ?? []).join('\n') || '（还没有记录）'
}

$('provider').addEventListener('change', () => {
  const provider = providerList.find((item) => item.id === $('provider').value)
  if (provider) {
    $('baseURL').value = provider.baseURL
    if (provider.models.length > 0) $('model').value = provider.models[0]
  }
  fillProviderNote()
})

$('save').addEventListener('click', async () => {
  setStatus('保存中…')
  $('save').disabled = true
  const result = await window.settingsApi.save(collect())
  $('save').disabled = false
  if (result.ok) {
    state.config = result.config
    fillForm()
    setStatus('已保存', 'ok')
  } else {
    setStatus(result.error ?? '保存失败', 'bad')
  }
})

$('testModel').addEventListener('click', async () => {
  setStatus('测试模型…')
  $('testModel').disabled = true
  const result = await window.settingsApi.testModel(collect())
  $('testModel').disabled = false
  setStatus(result.ok ? `模型正常（${result.ms}ms）` : (result.error ?? '失败'), result.ok ? 'ok' : 'bad')
})

$('testSearch').addEventListener('click', async () => {
  setStatus('测试搜索…')
  $('testSearch').disabled = true
  const result = await window.settingsApi.testSearch($('searchKey').value.trim())
  $('testSearch').disabled = false
  $('searchHint').textContent = result.ok ? `${result.ms}ms / ${result.count} 条` : (result.error ?? '失败')
  setStatus(result.ok ? '搜索正常' : '搜索失败', result.ok ? 'ok' : 'bad')
})

/** 网易云凭据单独保存：只提交这两个字段，避免顺手把别的配置也写一遍 */
$('saveNetease').addEventListener('click', async () => {
  const appId = $('neteaseAppId').value.trim()
  const privateKey = $('neteasePrivateKey').value.trim()
  if (appId === '' && privateKey === '') {
    $('neteaseHint').textContent = '两个都空着，没什么可保存的'
    return
  }
  const payload = {}
  if (appId !== '') payload.appId = appId
  if (privateKey !== '') payload.privateKey = privateKey
  const result = await window.settingsApi.save({ netease: payload })
  $('neteaseHint').textContent = result.ok === true ? '已保存（只在本机）' : `保存失败：${result.error}`
  if (result.ok === true) await load()
})

$('clearNetease').addEventListener('click', async () => {
  // 传 null 才是"清除"，空串表示"不修改"
  const result = await window.settingsApi.save({ netease: { appId: null, privateKey: null } })
  $('neteaseHint').textContent = result.ok === true ? '已清除' : `清除失败：${result.error}`
  if (result.ok === true) await load()
})

$('clearSearch').addEventListener('click', async () => {
  const result = await window.settingsApi.clearSearch()
  if (result.ok) {
    state.config = result.config
    fillForm()
    setStatus('搜索 Key 已清除', 'ok')
  } else {
    setStatus(result.error ?? '清除失败', 'bad')
  }
})

$('testVoice').addEventListener('click', async () => {
  setStatus('合成中…')
  $('testVoice').disabled = true
  const result = await window.settingsApi.testVoice({
    emotion: $('emotion').value,
    speed: Number($('speed').value),
  })
  $('testVoice').disabled = false
  $('voiceHint').textContent = result.ok
    ? `${result.ms}ms / 音频 ${result.seconds}s → ${result.file}`
    : (result.error ?? '失败')
  setStatus(result.ok ? '试听已生成' : '合成失败', result.ok ? 'ok' : 'bad')
})

$('exportAudit').addEventListener('click', async () => {
  const result = await window.settingsApi.exportAudit()
  $('auditHint').textContent = result.ok ? `已导出 ${result.lines} 条 → ${result.path}` : (result.error ?? '导出失败')
})

await load()
