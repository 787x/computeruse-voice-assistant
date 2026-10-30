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

/* ---------------- 模型方案（供应商 + Key + 模型） ---------------- */
function profileRowId() {
  return `p${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`
}

/** 默认方案下拉：空值 = 用顶部那套全局模型 */
function renderDefaultProfileOptions(selectedId) {
  const select = $('defaultProfile')
  select.innerHTML = ''
  const fallback = document.createElement('option')
  fallback.value = ''
  fallback.textContent = '（用顶部全局模型）'
  select.append(fallback)
  for (const profile of collectProfiles()) {
    const option = document.createElement('option')
    option.value = profile.id
    option.textContent = profile.name !== '' ? profile.name : (profile.model !== '' ? profile.model : profile.id)
    select.append(option)
  }
  select.value = typeof selectedId === 'string' ? selectedId : ''
}
function renderProfileRows(profiles) {
  const body = $('profileRows')
  body.innerHTML = ''
  for (const [index, profile] of profiles.entries()) {
    const row = document.createElement('tr')
    row.dataset.id = profile.id ?? profileRowId()

    const nameCell = document.createElement('td')
    const name = document.createElement('input')
    name.value = profile.name ?? ''
    name.placeholder = '白天'
    name.dataset.field = 'name'
    nameCell.append(name)

    const providerCell = document.createElement('td')
    const provider = document.createElement('select')
    provider.dataset.field = 'providerId'
    for (const item of providerList) {
      const option = document.createElement('option')
      option.value = item.id
      option.textContent = item.name
      provider.append(option)
    }
    provider.value = profile.providerId ?? providerList[0]?.id ?? 'openai-compatible'
    provider.addEventListener('change', () => {
      const matched = providerList.find((item) => item.id === provider.value)
      if (matched !== undefined) {
        row.querySelector('[data-field="baseURL"]').value = matched.baseURL
        const modelInput = row.querySelector('[data-field="model"]')
        if (modelInput.value.trim() === '' && matched.models.length > 0) modelInput.value = matched.models[0]
      }
    })
    providerCell.append(provider)

    const urlCell = document.createElement('td')
    const url = document.createElement('input')
    url.value = profile.baseURL ?? ''
    url.placeholder = 'https://…'
    url.spellcheck = false
    url.dataset.field = 'baseURL'
    urlCell.append(url)

    const keyCell = document.createElement('td')
    const key = document.createElement('input')
    key.type = 'password'
    key.value = ''
    key.placeholder = profile.hasApiKey === true ? `已保存（${profile.apiKey}）` : 'sk-…'
    key.dataset.field = 'apiKey'
    keyCell.append(key)

    const modelCell = document.createElement('td')
    const model = document.createElement('input')
    model.value = profile.model ?? ''
    model.placeholder = 'deepseek-flash'
    model.spellcheck = false
    model.dataset.field = 'model'
    modelCell.append(model)

    const delCell = document.createElement('td')
    const del = document.createElement('button')
    del.textContent = '删'
    del.addEventListener('click', () => {
      const current = collectProfiles()
      current.splice(index, 1)
      renderProfileRows(current)
      renderScheduleRows(collectSchedule())
    })
    delCell.append(del)

    row.append(nameCell, providerCell, urlCell, keyCell, modelCell, delCell)
    body.append(row)
  }
}

function collectProfiles() {
  const rows = []
  for (const row of $('profileRows').querySelectorAll('tr')) {
    const pick = (name) => row.querySelector(`[data-field="${name}"]`)?.value ?? ''
    rows.push({
      id: row.dataset.id,
      name: pick('name').trim(),
      providerId: pick('providerId'),
      baseURL: pick('baseURL').trim(),
      model: pick('model').trim(),
      // 空=不修改（主进程按掩码语义保留原 Key）
      apiKey: pick('apiKey').trim(),
    })
  }
  return rows
}

$('addProfile').addEventListener('click', () => {
  const current = collectProfiles()
  const provider = providerList[0]
  current.push({
    id: profileRowId(),
    name: `方案 ${current.length + 1}`,
    providerId: provider?.id ?? 'openai-compatible',
    baseURL: provider?.baseURL ?? '',
    model: provider?.models?.[0] ?? '',
    apiKey: '',
  })
  renderProfileRows(current)
})
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
    // 时段内用哪套方案；一个方案都没有时退回"直接填模型名"
    const modelCell = document.createElement('td')
    const profileList = collectProfiles()
    if (profileList.length > 0) {
      const select = document.createElement('select')
      select.dataset.field = 'profileId'
      for (const profile of profileList) {
        const option = document.createElement('option')
        option.value = profile.id
        option.textContent = profile.name !== '' ? profile.name : (profile.model !== '' ? profile.model : profile.id)
        select.append(option)
      }
      select.value = typeof rule.profileId === 'string' && rule.profileId !== '' ? rule.profileId : profileList[0].id
      modelCell.append(select)
    } else {
      const model = document.createElement('input')
      model.value = rule.model ?? ''
      model.placeholder = '先在上面加方案，或直接填模型名'
      model.spellcheck = false
      model.dataset.field = 'model'
      modelCell.append(model)
    }
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
    rows.push({
      from: pick('from').trim(),
      to: pick('to').trim(),
      model: pick('model').trim(),
      profileId: pick('profileId'),
      days,
    })
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
    : `已保存（${netease.privateKeyLength ?? 0} 字符），留空不修改`

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
  renderProfileRows(Array.isArray(config.modelProfiles) ? config.modelProfiles : [])
  renderDefaultProfileOptions(schedule.defaultProfileId)
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
    // 模型方案与定时规则：之前这两个字段压根没进 payload，所以用户"保存不下来"
    modelProfiles: collectProfiles(),
    modelSchedule: {
      enabled: $('scheduleEnabled').checked,
      defaultProfileId: $('defaultProfile').value,
      rules: collectSchedule(),
    },
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

/**
 * 登录网易云：拿一次性链接 → 系统浏览器打开。
 * 登录态由 ncm-cli 保存，扫码后点「检查登录状态」就能看到昵称。
 */
$('neteaseLogin').addEventListener('click', async () => {
  $('neteaseHint').textContent = '正在获取登录链接…'
  const result = await window.settingsApi.neteaseLogin()
  $('neteaseHint').textContent = result.ok === true
    ? '等待扫码…登录成功会自动提示'
    : `打不开登录页：${result.error}`
})

// 登录过程的两条推送：链接到了 / 扫完了
window.settingsApi.onNeteaseLoginUrl((url) => {
  $('neteaseHint').textContent = `已在浏览器打开 ${url}，用手机网易云扫码`
})
window.settingsApi.onNeteaseLoginDone((result) => {
  $('neteaseHint').textContent = result.ok === true
    ? '登录成功 ✓ 可以直接点歌了'
    : `登录没完成：${result.error ?? '超时或取消'}（再点一次登录可重试）`
})

$('neteaseStatus').addEventListener('click', async () => {
  $('neteaseHint').textContent = '正在查登录状态…'
  const result = await window.settingsApi.neteaseStatus()
  if (result.ok !== true) {
    $('neteaseHint').textContent = `查询失败：${result.error}`
    return
  }
  $('neteaseHint').textContent = result.loggedIn === true
    ? `已登录${result.text === '' ? '' : `（${result.text}）`}`
    : '还没登录（扫码后稍等几秒再查）'
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
