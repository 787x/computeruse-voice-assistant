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
/**
 * 内置默认系统提示词：由主进程从 packages/agent/system-prompt.mjs 给过来。
 * 界面只做"显示/复制/当编辑起点"，自己绝不留一份文案副本。
 */
let defaultPrompt = ''
/**
 * 加载时框里的提示词原文（或 null）。
 * collect() 用它判断"用户有没有真的改提示词"：
 *  - 框值 == loadedPrompt → 没改，不交 systemPrompt
 *  - 框值 != loadedPrompt → 改了，交新值
 */
let loadedPrompt = null
/**
 * 在线获取到的模型候选（`{id, effort, ...}`）。
 * 它是**当前提供商**的列表：切提供商要重新取，不能把上一家的结果留着当候选。
 * null = 还没取过（此时候选来自注册表里的静态建议列表）。
 */
let discoveredModels = null
/** 上一次取列表失败的原因（界面要如实显示，并说明"仍可手填"） */
let modelsError = ''
/**
 * 用户是否已点击「恢复默认」并成功保存。
 * 为 true 时，切回自定义模式应以完整默认提示词为起点，
 * 而不是显示恢复默认前写的旧草稿。
 */
let restoredDefault = false

/* ---------------- 系统提示词 ---------------- */
function promptModeIsCustom() {
  return $('promptModeCustom').checked
}

function renderPromptCharCount() {
  const text = $('systemPrompt').value
  $('promptCustomChars').textContent = text === ''
    ? '（空着不保存：要用人设请选「使用默认提示词」）'
    : `${text.length} 字 / ${text.split('\n').length} 行`
}

/** 切模式：只做显示切换，落盘统一走右上角「保存」 */
function applyPromptModeUI() {
  const custom = promptModeIsCustom()
  $('promptCustomArea').hidden = !custom
  // 默认提示词那块常驻可看：默认模式下自动展开，方便对着原文改
  if (!custom) $('defaultPromptDetails').open = true
  if (custom && $('systemPrompt').value === '' && defaultPrompt !== '') {
    // 只有框是空的时候才填默认提示词当起点
    // 框里有内容（已保存的自定义 A、或用户未保存的草稿 B）就保留，不覆盖
    $('systemPrompt').value = defaultPrompt
  }
  renderPromptCharCount()
}

function fillPromptForm() {
  const custom = typeof state.config.systemPrompt === 'string' && state.config.systemPrompt.trim() !== ''
  $('promptModeDefault').checked = !custom
  $('promptModeCustom').checked = custom
  $('defaultPrompt').textContent = defaultPrompt === '' ? '（没拿到默认提示词）' : defaultPrompt
  if (custom) {
    $('systemPrompt').value = state.config.systemPrompt
    loadedPrompt = state.config.systemPrompt
  } else {
    $('systemPrompt').value = ''
    loadedPrompt = null
  }
  restoredDefault = false
  applyPromptModeUI()
}

/** 复制：先试 Clipboard API，再退回选区 + execCommand（本地窗口两种都够用） */
async function copyText(text, feedbackTarget) {
  const hint = $(feedbackTarget)
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      hint.textContent = '已复制到剪贴板'
      return
    }
  } catch { /* 落到下面的兜底 */ }
  try {
    const holder = document.createElement('textarea')
    holder.value = text
    holder.style.position = 'fixed'
    holder.style.opacity = '0'
    document.body.append(holder)
    holder.select()
    const ok = document.execCommand('copy')
    holder.remove()
    hint.textContent = ok ? '已复制到剪贴板' : '复制失败，请手动选中复制'
  } catch (error) {
    hint.textContent = `复制失败：${error.message}（请手动选中复制）`
  }
}

function setStatus(text, kind = '') {
  $('status').textContent = text
  $('status').className = kind
}

function fillProviderNote() {
  const provider = currentProvider()
  const list = $('modelList')
  list.innerHTML = ''
  for (const id of modelCandidates()) {
    const option = document.createElement('option')
    option.value = id
    list.append(option)
  }
  const notes = []
  if (provider?.notes) notes.push(provider.notes)
  if (discoveredModels !== null) notes.push(`模型列表：在线获取 ${discoveredModels.length} 个（${modelsFetchedAt ? modelsFetchedAt : '本次会话'}）`)
  if (modelsError !== '') notes.push(`在线列表没拿到：${modelsError}——可手填模型名，保存不受影响`)
  $('modelHint').textContent = notes.join('　')

  // 提供商控制台的入口：填 Key 时不用自己去搜地址
  const consoleUrl = provider?.keyConsoleUrl ?? ''
  $('openKeyConsole').hidden = consoleUrl === ''
  // Key 输入框的提示要跟着提供商走：MiMo 已存过 Key 时切回来不该看起来像"没配"
  const hasKey = state?.config?.hasProviderKey?.[provider?.id] === true
  if ($('apiKey').value.trim() === '') {
    $('apiKey').placeholder = hasKey ? '已保存，留空不修改' : (provider?.apiKeyRequired === false ? '本地端点可留空' : '未配置（可在控制台获取）')
  }
  renderReasoning()
}

/** 当前界面上选中的提供商对象 */
function currentProvider() {
  return providerList.find((item) => item.id === $('provider').value) ?? null
}

/**
 * 聊天模型候选：在线列表优先，其次注册表里的静态建议。
 * 静态列表只是"第一次还没联网时的提示"，不是永远完整的目录。
 */
function modelCandidates() {
  const provider = currentProvider()
  if (discoveredModels !== null && discoveredModels.length > 0) return discoveredModels.map((model) => model.id)
  return provider?.models ?? []
}

/* ---------------- 推理控制（MiMo 思考 / DeepSeek 等级） ---------------- */
let modelsFetchedAt = ''
/**
 * 界面上的档位草稿（用户改了但还没保存）。
 *
 * 为什么必须有：MiMo 和 DeepSeek 各一个下拉，当前提供商只会显示其中一个，另一个是隐藏的。
 * 隐藏不等于"用户想清空"——collect() 两个字段都要交，所以没被看见的那个必须继续
 * 交出**原来存着的值**，否则改 MiMo 就会顺手把 DeepSeek 的档位冲掉。
 * 保存成功后清空草稿（以磁盘为准），重新加载时也清空。
 */
const reasoningDraft = { mimoThinking: null, deepseekReasoningEffort: null }
/** 兜底档位表：主进程没给元数据时用（正常情况由 state.reasoning 提供） */
const FALLBACK_MIMO_OPTIONS = [
  { value: 'default', label: '默认（跟随服务端）' },
  { value: 'enabled', label: '开启思考' },
  { value: 'disabled', label: '关闭思考' },
]
const FALLBACK_DEEPSEEK_OPTIONS = [
  { value: 'default', label: '默认（跟随服务端）' },
  { value: 'none', label: '关闭' },
  { value: 'low', label: '低' },
  { value: 'high', label: '高' },
  { value: 'max', label: '极高' },
]
function mimoOptions() {
  const meta = state?.reasoning?.byProvider?.mimo?.options
  return Array.isArray(meta) && meta.length > 0 ? meta : FALLBACK_MIMO_OPTIONS
}
function deepseekOptions() {
  const meta = state?.reasoning?.byProvider?.deepseek?.options
  return Array.isArray(meta) && meta.length > 0 ? meta : FALLBACK_DEEPSEEK_OPTIONS
}

/**
 * 当前生效的控件描述。
 * 数据全部由主进程给（reasoning.byProvider / byHint），界面不许自己编能力表。
 */
function currentReasoningControl() {
  const provider = currentProvider()
  if (provider === undefined || provider === null) return null
  const meta = state?.reasoning ?? null
  const byProvider = meta?.byProvider?.[provider.id] ?? { kind: 'none', levels: [], note: '' }
  // 自定义端点：按用户选的协议提示决定控件（不按模型名猜）
  if (provider.id === 'openai-compatible') {
    const hint = $('providerHint')?.value ?? 'auto'
    const byHint = meta?.byHint?.[hint]
    return byHint ?? { kind: 'none', levels: [], note: '标准 OpenAI 兼容端点：不发送提供商专属推理参数' }
  }
  return byProvider
}

function renderReasoning() {
  const control = currentReasoningControl()
  const provider = currentProvider()
  const hintList = state?.reasoning?.hints ?? [{ id: 'auto', name: '自动（不发送提供商专属参数）' }]
  // 兼容协议提示：只在自定义端点上出现
  $('providerHintField').hidden = provider?.id !== 'openai-compatible'
  fillSelect($('providerHint'), hintList.map((item) => ({ value: item.id, label: item.name })),
    state?.config?.providerHint ?? 'auto')

  const mimo = control?.kind === 'mimo-thinking'
  const deepseek = control?.kind === 'deepseek-effort'
  $('reasoningRow').hidden = !(mimo || deepseek || provider?.id === 'openai-compatible')
  $('mimoThinkingField').hidden = !mimo
  $('deepseekField').hidden = !deepseek

  /*
   * 两个下拉都照常填选项；当前提供商用不到的那个只是**藏起来**。
   * 藏起来不等于清空：collect() 两个字段都要交，隐藏的那个继续交出原值，
   * 否则改 MiMo 会顺手把 DeepSeek 存好的档位冲成 default。
   */
  fillSelect($('mimoThinking'), mimoOptions(), reasoningValue('mimoThinking'))
  fillSelect($('deepseekEffort'), deepseekOptions(), reasoningValue('deepseekReasoningEffort'))

  const notes = []
  if (control?.note) notes.push(control.note)
  /*
   * 温度到底会不会被发送，由主进程那套规则决定（见 ll-params.buildRequestFields），
   * 界面只做如实说明。绝不允许"显示温度 0.3"而实际请求里模型用的是自己的默认值。
   */
  const thinkingOn = mimo ? ($('mimoThinking').value || 'default') !== 'disabled'
    : deepseek ? ($('deepseekEffort').value || 'default') !== 'none' : false
  let ignoresTemp = false
  if (mimo && thinkingOn) {
    ignoresTemp = true
    notes.push('MiMo V2.6 思考模式下 temperature/top_p 会被模型强制覆盖（1.0 / 0.95），实际请求不发送温度。')
  } else if (deepseek && thinkingOn) {
    ignoresTemp = true
    notes.push('DeepSeek 思考模式下 temperature/top_p 不生效（官方：传了不报错但也不生效），实际请求不发送温度。')
  }
  $('reasoningNote').textContent = notes.join(' ')

  // 温度框：该档位实际会忽略它时如实禁用，不做"显示能改其实无效"
  $('temperature').disabled = ignoresTemp
  $('temperatureHint').textContent = ignoresTemp ? '当前档位下模型会忽略温度，不发送该参数' : ''
  syncEffortAvailability()
}

/** 取某一档的界面值：草稿优先，其次磁盘配置，都没有才 default */
function reasoningValue(field) {
  const draft = reasoningDraft[field]
  if (typeof draft === 'string' && draft !== '') return draft
  return state?.config?.[field] ?? 'default'
}

/**
 * 某个提供商（+ 自定义端点的协议提示）该显示哪套推理控件。
 * 和主进程 reasoningControlFor 同一套判断依据：**看提供商，不看模型名**。
 */
function reasoningKindForProvider(providerId, hint) {
  const meta = state?.reasoning
  if (providerId === 'mimo') return 'mimo'
  if (providerId === 'deepseek') return 'deepseek'
  if (providerId === 'openai-compatible') {
    const picked = typeof hint === 'string' ? hint : 'auto'
    if (picked === 'mimo' || picked === 'deepseek') return picked
  }
  // 主进程没给元数据时（老壳子）也别硬编一份：按 id 认，认不出就不显示
  const byProvider = meta?.byProvider?.[providerId]
  if (byProvider?.kind === 'mimo-thinking') return 'mimo'
  if (byProvider?.kind === 'deepseek-effort') return 'deepseek'
  return 'none'
}

/**
 * 用在线列表带回的能力元数据限制档位选择。
 * DeepSeek 的 `/models` 会给 `effort.supported_levels`；模型不支持的档位就置灰，
 * 并说明原因——不假定未来模型的能力永远相同。
 */
function syncEffortAvailability() {
  if ($('deepseekField').hidden) return
  const selected = ($('model').value || '').trim().toLowerCase()
  const entry = (discoveredModels ?? []).find((model) => String(model.id).toLowerCase() === selected)
  const supported = entry?.effort?.supported_levels
  if (!Array.isArray(supported)) return
  const canonical = (level) => ({ minimal: 'low', medium: 'high', xhigh: 'high', ultra: 'max' })[level] ?? level
  const allowed = new Set(['default', 'none', ...supported.map(canonical)])
  let downgraded = false
  for (const option of $('deepseekEffort').options) {
    const ok = allowed.has(option.value)
    option.disabled = !ok
    if (!ok && $('deepseekEffort').value === option.value) downgraded = true
  }
  if (downgraded) {
    /*
     * 只在界面上降到该模型真支持的档位，并把选择记进草稿：
     * 请求组装时主进程也会做一次同样的就近降级，两边算出同一个结果，
     * 界面才不会"显示 max、实际发 low"。磁盘上用户原本选的档位不动。
     */
    const list = [...$('deepseekEffort').options].filter((option) => !option.disabled).map((option) => option.value)
    const next = list.includes(reasoningValue('deepseekReasoningEffort'))
      ? reasoningValue('deepseekReasoningEffort')
      : (list.includes('low') ? 'low' : (list.at(-1) ?? 'default'))
    $('deepseekEffort').value = next
    reasoningDraft.deepseekReasoningEffort = next
    $('reasoningNote').textContent = `${$('reasoningNote').textContent} ${entry.id} 只支持 ${supported.join('/')}，已按此选择。`.trim()
  }
}

function fillSelect(select, items, selected) {
  if (select === null || select === undefined) return
  select.innerHTML = ''
  for (const item of items) {
    const option = document.createElement('option')
    option.value = item.value
    option.textContent = item.label
    if (item.note) option.title = item.note
    select.append(option)
  }
  select.value = items.some((item) => item.value === selected) ? selected : (items[0]?.value ?? '')
}

/* ---------------- 在线模型列表 ---------------- */

/**
 * 在线模型列表获取的请求序号：每次发起都会 +1，回来时若不是最新序号就直接丢弃。
 * 解决"MiMo 请求还没回、用户已经切到 DeepSeek"的竞态——旧请求绝不能覆盖新提供商的候选。
 */
let modelsFetchSeq = 0

/**
 * 拉取当前提供商的聊天模型列表。
 * - `force` 跳过主进程的短期缓存（用户主动点“刷新模型列表”或改了 Key 时用）
 * - 失败时保留现有候选与用户已选的模型，只报错，不动选择
 * - 并发保护：每次调用取最新序号，返回时只有序号仍是最新才更新界面
 */
async function fetchModels({ force = false, silent = false } = {}) {
  const provider = currentProvider()
  if (provider === null || provider.modelDiscovery !== true || typeof window.settingsApi.models !== 'function') {
    discoveredModels = null
    modelsError = ''
    if (!silent) fillProviderNote()
    return
  }
  // 用户刚粘贴、还没保存的 Key 也要能取列表；主进程只把它用于这一次请求
  const key = $('apiKey').value.trim()
  const storedKey = state?.config?.hasProviderKey?.[provider.id] === true
    || state?.config?.hasApiKey === true
  if (key === '' && !storedKey) {
    modelsError = '还没有 API Key'
    discoveredModels = null
    $('modelsHint').textContent = '填了 API Key 才能获取模型列表（也可以直接手填模型名）'
    if (!silent) fillProviderNote()
    return
  }
  // 记录本次请求的提供商标识与序号；旧请求回来时发现序号/提供商不对就丢弃
  const seq = ++modelsFetchSeq
  const requestProviderId = provider.id
  if (!silent) $('modelsHint').textContent = '正在获取模型列表…'
  // 手动刷新必须真正绕过缓存：清缓存 + 显式 force；且无论有没有手填 Key 都该强制
  if (force) await window.settingsApi.clearModelsCache?.({ providerId: provider.id }).catch(() => null)
  let result
  try {
    result = await window.settingsApi.models({
      providerId: provider.id,
      baseURL: $('baseURL').value.trim() || provider.baseURL,
      apiKey: key,
      force,
    })
  } catch (error) {
    // IPC 都没通（主进程没实现/窗口刚关）也一样：只提示，不动用户选的模型，不阻止保存
    result = { ok: false, error: error.message ?? String(error) }
  }
  // 竞态丢弃：请求期间用户又切了提供商/换了输入，旧响应不得覆盖新页面
  if (seq !== modelsFetchSeq || requestProviderId !== currentProvider()?.id) return
  if (result.ok === true) {
    discoveredModels = result.models ?? []
    modelsError = ''
    modelsFetchedAt = result.cached === true ? '缓存' : '刚刚'
    $('modelsHint').textContent = `已获取 ${discoveredModels.length} 个聊天模型`
      + `${(result.dropped ?? []).length > 0 ? `（已排除 ${result.dropped.length} 个非聊天模型）` : ''}`
    // 在线列表是主要依据：当前模型确实不在列表里时只提示，**不擅自改写**用户的选择
    const current = ($('model').value || '').trim()
    if (current !== '' && !discoveredModels.some((model) => model.id === current)) {
      $('modelsHint').textContent += `（当前填写的 ${current} 不在列表里，可能是新模型或已下线）`
    }
  } else {
    modelsError = result.error ?? '获取失败'
    $('modelsHint').textContent = result.stale === true
      ? `获取失败：${modelsError}（先用上一次拿到的列表）`
      : `获取失败：${modelsError}（可以手填模型名，不影响保存）`
    // 失败不能阻止保存：这里只更新提示，不动 model 的值
  }
  fillProviderNote()
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
        // 推荐初始模型用该提供商自己的 defaultModel（MiMo 是 mimo-v2.6-flash），不是 models[0]
        const suggested = matched.defaultModel ?? matched.models?.[0] ?? ''
        if (modelInput.value.trim() === '' && suggested !== '') modelInput.value = suggested
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

    /*
     * 方案的推理策略：提供商、地址、Key、模型、推理参数是**一套**，
     * 定时切过来必须整套一致，否则会拿上一家的推理参数打这一家。
     * 一个下拉按该行的提供商显示对应的档位；collect 时写进对应字段，
     * 另一家的字段原样留着（用户以后切回去还是他选的那档）。
     */
    const reasoningCell = document.createElement('td')
    const reasoningSelect = document.createElement('select')
    reasoningSelect.dataset.field = 'reasoning'
    reasoningCell.append(reasoningSelect)
    const hintSelect = document.createElement('select')
    hintSelect.dataset.field = 'providerHint'
    hintSelect.className = 'hint-select'
    reasoningCell.append(hintSelect)
    const applyProfileReasoningUI = () => {
      const matched = providerList.find((item) => item.id === provider.value)
      const kind = reasoningKindForProvider(matched?.id, hintSelect.value)
      const options = kind === 'mimo' ? mimoOptions() : kind === 'deepseek' ? deepseekOptions() : []
      const stored = kind === 'mimo'
        ? (profile.mimoThinking ?? 'default')
        : kind === 'deepseek' ? (profile.deepseekReasoningEffort ?? 'default') : 'default'
      fillSelect(reasoningSelect, options, stored)
      reasoningSelect.hidden = options.length === 0
      fillSelect(hintSelect, (state?.reasoning?.hints ?? [{ id: 'auto', name: '自动' }]).map((item) => ({ value: item.id, label: item.name })), profile.providerHint ?? 'auto')
      hintSelect.hidden = matched?.id !== 'openai-compatible'
    }
    provider.addEventListener('change', applyProfileReasoningUI)
    hintSelect.addEventListener('change', applyProfileReasoningUI)
    fillSelect(hintSelect, (state?.reasoning?.hints ?? [{ id: 'auto', name: '自动' }]).map((item) => ({ value: item.id, label: item.name })), profile.providerHint ?? 'auto')
    applyProfileReasoningUI()

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

    row.append(nameCell, providerCell, urlCell, keyCell, modelCell, reasoningCell, delCell)
    body.append(row)
  }
}

function collectProfiles() {
  const rows = []
  for (const row of $('profileRows').querySelectorAll('tr')) {
    const pick = (name) => row.querySelector(`[data-field="${name}"]`)?.value ?? ''
    const providerId = pick('providerId')
    const kind = reasoningKindForProvider(providerId, pick('providerHint'))
    const level = pick('reasoning')
    const record = {
      id: row.dataset.id,
      name: pick('name').trim(),
      providerId,
      baseURL: pick('baseURL').trim(),
      model: pick('model').trim(),
      providerHint: pick('providerHint') || 'auto',
      // 空=不修改（主进程按掩码语义保留原 Key）
      apiKey: pick('apiKey').trim(),
    }
    /*
     * 只交当前提供商看得懂的那一档；另一档**整个字段都不交**（undefined），
     * 让主进程沿用这条方案原来存的那个值。
     * 注意不能交 'default'：'default' 是用户明确选的合法档位，
     * 当成"没选过"就会把这家存好的档位冲掉。
     */
    if (kind === 'mimo' && level !== '') record.mimoThinking = level
    if (kind === 'deepseek' && level !== '') record.deepseekReasoningEffort = level
    rows.push(record)
  }
  return rows
}

$('addProfile').addEventListener('click', () => {
  const current = collectProfiles()
  // 新方案的默认提供商：跟当前设置页选的提供商一致（新装就是 MiMo），
  // 而不是一律 openai-compatible——那样用户每次都要重挑一遍
  const provider = currentProvider() ?? providerList[0]
  current.push({
    id: profileRowId(),
    name: `方案 ${current.length + 1}`,
    providerId: provider?.id ?? 'openai-compatible',
    baseURL: provider?.baseURL ?? '',
    model: provider?.defaultModel ?? provider?.models?.[0] ?? '',
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
  // 提供商各自的 Key 分开存：这一格显示的是**当前提供商**那份
  const providerHasKey = config.hasProviderKey?.[config.providerId] === true
  $('apiKey').placeholder = providerHasKey
    ? `已保存（${config.providerKeys?.[config.providerId] ?? config.apiKey}），留空不修改`
    : (config.hasApiKey ? `已保存（${config.apiKey}），留空不修改` : '未配置')
  $('maxSteps').value = config.maxSteps
  $('temperature').value = config.temperature
  $('mimoThinking').value = config.mimoThinking ?? 'default'
  $('deepseekEffort').value = config.deepseekReasoningEffort ?? 'default'
  $('providerHint').value = config.providerHint ?? 'auto'

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
  fillPromptForm()
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
    // 推理档位：两家的字段都交（主进程按当前提供商取用，另一个字段原样留着）
    mimoThinking: reasoningValue('mimoThinking'),
    deepseekReasoningEffort: reasoningValue('deepseekReasoningEffort'),
    providerHint: $('providerHint').value || 'auto',
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
  /*
   * 系统提示词三态：
   *  - 自定义且用户改了 → 交文本（合法性由主进程判，界面不做二次加工、不 trim）
   *  - 自定义但用户没改 → 不交这个字段（避免无关设置顺手写一遍提示词）
   *  - 默认 + 当前配置是自定义 → 交 null（"我要恢复默认"，语义明确）
   *  - 默认 + 当前本来就是默认 → 不交这个字段
   *
   * 关键：用 loadedPrompt 而不是 state.config.systemPrompt 来判断"有没有改"。
   * 因为切模式时 applyPromptModeUI 会把框填成默认提示词，
   * 此时 state.config.systemPrompt 还是旧值，直接比较会误判成"用户改了"。
   */
  if (promptModeIsCustom()) {
    const newPrompt = $('systemPrompt').value
    if (newPrompt !== loadedPrompt) {
      payload.systemPrompt = newPrompt
    }
  } else if (typeof state.config.systemPrompt === 'string' && state.config.systemPrompt.trim() !== '') {
    payload.systemPrompt = null
  }
  return payload
}

async function load() {
  const data = await window.settingsApi.load()
  state = data
  providerList = data.providers ?? []
  capabilityList = data.capabilities ?? []
  voices = data.voices ?? []
  defaultPrompt = typeof data.defaultSystemPrompt === 'string' ? data.defaultSystemPrompt : ''
  // 重新加载 = 一切以磁盘为准：清掉未保存的草稿和上一轮的在线列表
  reasoningDraft.mimoThinking = null
  reasoningDraft.deepseekReasoningEffort = null
  discoveredModels = null
  modelsError = ''
  modelsFetchedAt = ''
  $('modelsHint').textContent = ''
  renderProviders()
  renderVoices()
  fillForm()
  $('audit').textContent = (data.audit ?? []).join('\n') || '（还没有记录）'
  // 有 Key 就顺手把在线模型列表取回来（失败只提示，不影响显示与保存）
  await maybeFetchModels()
}

/* ---------------- 系统提示词的交互 ---------------- */
for (const id of ['promptModeDefault', 'promptModeCustom']) {
  $(id).addEventListener('change', applyPromptModeUI)
}
$('systemPrompt').addEventListener('input', renderPromptCharCount)
$('copyDefaultPrompt').addEventListener('click', () => copyText(defaultPrompt, 'promptDefaultHint'))
$('copyCustomPrompt').addEventListener('click', () => copyText($('systemPrompt').value, 'promptCustomChars'))
/**
 * 「恢复默认」= 选中默认模式。
 * 真正的落盘还是走「保存」（payload 里 systemPrompt: null），
 * 这样取消/失败时的反馈和其它设置一致，不会出现"半个状态"。
 */
$('restoreDefaultPrompt').addEventListener('click', () => {
  $('promptModeDefault').checked = true
  applyPromptModeUI()
  $('promptDefaultHint').textContent = '已切回默认模式，点右上角「保存」生效。'
})

$('provider').addEventListener('change', async () => {
  const provider = currentProvider()
  if (provider) {
    $('baseURL').value = provider.baseURL
    // 新提供商用**它自己的推荐模型**（MiMo 是 mimo-v2.6-flash），不是上一家那个
    const suggested = provider.defaultModel ?? provider.models?.[0] ?? ''
    if (suggested !== '') $('model').value = suggested
  }
  // 在线列表按提供商各自取：清掉上一家的结果，避免拿 MiMo 的目录去填 DeepSeek
  discoveredModels = null
  modelsError = ''
  modelsFetchedAt = ''
  $('modelsHint').textContent = ''
  fillProviderNote()
  await maybeFetchModels()
})

/* 推理下拉一改就记进草稿，并重算提示与温度可用性 */
for (const [id, field] of [['mimoThinking', 'mimoThinking'], ['deepseekEffort', 'deepseekReasoningEffort'], ['providerHint', 'providerHint']]) {
  $(id).addEventListener('change', () => {
    if (field === 'providerHint') {
      if (state !== null) state.config.providerHint = $(id).value
    } else {
      reasoningDraft[field] = $(id).value
    }
    renderReasoning()
  })
}
$('model').addEventListener('change', () => {
  renderReasoning()
  syncEffortAvailability()
})
let keyFetchTimer = null
$('apiKey').addEventListener('input', () => {
  /*
   * 刚粘贴的 Key 值得重取列表（主进程只把它用于这一次请求）。
   * 但必须防抖：逐字符敲就逐字符打一次 API，429 是自找的。停手 700ms 才发一次。
   */
  if (keyFetchTimer !== null) clearTimeout(keyFetchTimer)
  keyFetchTimer = setTimeout(() => {
    keyFetchTimer = null
    if ($('apiKey').value.trim() !== '') void maybeFetchModels({ force: true })
  }, 700)
})
$('refreshModels').addEventListener('click', () => fetchModels({ force: true }))
$('openKeyConsole').addEventListener('click', () => {
  const url = currentProvider()?.keyConsoleUrl
  if (typeof url === 'string' && url !== '') void window.settingsApi.openKeyConsole({ url })
})

/**
 * 选中了会做在线发现的提供商、且已经有 Key（已保存或刚填）时，自动取一次列表。
 * 静默进行：失败只在提示里说明，不打扰、更不阻止保存。
 */
async function maybeFetchModels({ force = false } = {}) {
  const provider = currentProvider()
  if (provider?.modelDiscovery !== true) return
  const typed = $('apiKey').value.trim()
  const stored = state?.config?.hasProviderKey?.[provider.id] === true || state?.config?.hasApiKey === true
  if (typed === '' && !stored) return
  await fetchModels({ force, silent: true })
}

$('save').addEventListener('click', async () => {
  setStatus('保存中…')
  $('save').disabled = true
  const payload = collect()
  const result = await window.settingsApi.save(payload)
  $('save').disabled = false
  if (result.ok) {
    state.config = result.config
    // 已落盘：草稿作废，之后一律以磁盘上的档位为准
    reasoningDraft.mimoThinking = null
    reasoningDraft.deepseekReasoningEffort = null
    if (typeof state.config.providerHint !== 'string') state.config.providerHint = 'auto'
    fillForm()
    // 保存成功后再标记：如果这次保存是"恢复默认"，
    // 用户切回自定义时应看到完整默认提示词（fillForm 已清空框，applyPromptModeUI 会填默认）
    if (payload.systemPrompt === null) restoredDefault = true
    if (result.warnings && result.warnings.length > 0) {
      // 磁盘已保存但部分运行时同步失败——明确告知用户哪些没生效
      const warningText = `已保存，但有 ${result.warnings.length} 项未即时生效：${result.warnings.join('；')}`
      setStatus(warningText, 'warn')
      $('status').title = warningText
    } else {
      setStatus('已保存', 'ok')
      $('status').title = ''
    }
  } else {
    // 保存失败：loadedPrompt 不变（还是加载时的原值），
    // 用户草稿保留在框里，下次保存时会正确提交
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
