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

/* ---------------- 下拉框选择保持（修复"改了就弹回旧值"） ---------------- */

/**
 * 为什么需要这张表：`fillSelect()` 每次都会重建 `<option>` 列表，
 * 而重建后 `select.value = 某个不在列表里的值` 会让浏览器把选择**静默重置成第一项**。
 * 之前 providerHint / 方案行推理档位 等下拉在每次 render 时都会被重填一遍，
 * 用户刚选的值就这么"弹回旧值"，看起来像界面不认他的操作。
 *
 * 现在统一规则：
 *  1. 用户**手动选过**的值记进 `choiceMemory`（键 = 控件标识），这是最高优先级；
 *  2. 重建选项后按「记忆 → 传入的 selected」顺序恢复，两者都不在列表里才落到第一项；
 *  3. 保存成功/重新加载时清空记忆（以磁盘为准）。
 */
const choiceMemory = new Map()

function rememberChoice(key, value) {
  if (typeof key !== 'string' || key === '' || typeof value !== 'string') return
  choiceMemory.set(key, value)
}

function recallChoice(key) {
  return choiceMemory.get(key)
}

function clearChoices() {
  choiceMemory.clear()
}

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
    // 第一次切自定义：拿完整默认提示词当起点，方便只改人设或个别规则
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
  restoredDefault = !custom
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

/**
 * 上下文预算的说明文字：把"窗口 × 百分比 = 多少 tokens 才开始压"算给用户看。
 * 规则本体在主进程（agent/context.mjs），界面只做换算与提示，不自己定阈值。
 */
function renderContextHint() {
  const hint = $('contextHint')
  if (hint === null) return
  const window = Number($('contextWindow').value)
  const percent = Number($('contextPercent').value)
  if (!Number.isFinite(window) || window <= 0 || !Number.isFinite(percent) || percent <= 0) {
    hint.textContent = '窗口与百分比都要是正数；填错会回退到默认 1,000,000 tokens / 90%'
    return
  }
  const trigger = Math.floor(window * Math.min(99, Math.max(1, percent)) / 100)
  hint.textContent = `窗口 ${window.toLocaleString('zh-CN')} tokens，到 ${trigger.toLocaleString('zh-CN')} tokens（${percent}%）才开始压缩；低于这个量不丢历史。`
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
    state?.config?.providerHint ?? 'auto', 'providerHint')

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
  fillSelect($('mimoThinking'), mimoOptions(), reasoningValue('mimoThinking'), 'mimoThinking')
  fillSelect($('deepseekEffort'), deepseekOptions(), reasoningValue('deepseekReasoningEffort'), 'deepseekReasoningEffort')

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

/** 取某一档的界面值：用户刚选过的优先，其次草稿，最后磁盘配置 */
function reasoningValue(field) {
  const remembered = recallChoice(field)
  if (typeof remembered === 'string' && remembered !== '') return remembered
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
     * 只在界面上降到该模型真支持的档位，并把选择记进草稿**与记忆**：
     * 请求组装时主进程也会做一次同样的就近降级，两边算出同一个结果，
     * 界面才不会"显示 max、实际发 low"。磁盘上用户原本选的档位不动。
     *
     * 必须同时写记忆：否则下一次 render 时记忆里的旧值（比如 high）会把
     * 刚降下来的 low 又顶回去，界面与实际请求就对不上了。
     */
    const list = [...$('deepseekEffort').options].filter((option) => !option.disabled).map((option) => option.value)
    const next = list.includes(reasoningValue('deepseekReasoningEffort'))
      ? reasoningValue('deepseekReasoningEffort')
      : (list.includes('low') ? 'low' : (list.at(-1) ?? 'default'))
    $('deepseekEffort').value = next
    reasoningDraft.deepseekReasoningEffort = next
    rememberChoice('deepseekReasoningEffort', next)
    $('reasoningNote').textContent = `${$('reasoningNote').textContent} ${entry.id} 只支持 ${supported.join('/')}，已按此选择。`.trim()
  }
}

/**
 * 重建一个 select 的选项并恢复选择。
 *
 * 恢复顺序：用户手动选过的（choiceMemory）→ 传入的 selected → 第一项。
 * 这个顺序是修 bug 的关键：以前只用 selected（来自磁盘配置），
 * 于是"用户刚改、还没保存"的选择会在下一次 render 时被磁盘值顶掉。
 *
 * @param {HTMLSelectElement} select
 * @param {Array<{value:string,label:string,note?:string}>} items
 * @param {string} selected 期望选中的值（通常是磁盘配置里的值）
 * @param {string} [memoryKey] 记忆键（不给就不记，比如权限级别下拉）
 */
function fillSelect(select, items, selected, memoryKey) {
  if (select === null || select === undefined) return
  const remembered = memoryKey === undefined ? undefined : recallChoice(memoryKey)
  const wanted = remembered ?? selected
  select.innerHTML = ''
  for (const item of items) {
    const option = document.createElement('option')
    option.value = item.value
    option.textContent = item.label
    if (item.note) option.title = item.note
    select.append(option)
  }
  const values = items.map((item) => item.value)
  const next = values.includes(wanted) ? wanted : (values.includes(selected) ? selected : (values[0] ?? ''))
  select.value = next
  if (memoryKey !== undefined) rememberChoice(memoryKey, next)
}

/* ---------------- 在线模型列表 ---------------- */

/**
 * 拉取当前提供商的聊天模型列表。
 * - `force` 跳过主进程的短期缓存（改了 Key / 地址时用）
 * - 失败时保留现有候选与用户已选的模型，只报错，不动选择
 */
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
  // 手动刷新必须真正绕过缓存：清缓存 + 显式 force
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

/**
 * 方案行里**还没保存**的 Key 草稿，键是「行 id + 提供商」。
 *
 * 只存在于内存（关掉设置窗口就没了，和全局那份一致）。作用是：
 *  - 切换提供商时，旧家的草稿留在旧家名下，不会变成新家的 Key；
 *  - 切回旧家时把草稿还回来，用户不必重填；
 *  - 同一个提供商改 Base URL 时草稿不受影响。
 * 保存成功后按行清掉（磁盘为准，见 save 处理）。
 */
const profileKeyDrafts = new Map()
/** 记住某一行在某提供商下**还没保存**的 Key（空内容 = 用户删掉了，草稿也要清掉） */
function rememberProfileKeyDraft(key, input) {
  const value = typeof input?.value === 'string' ? input.value.trim() : ''
  if (value === '') profileKeyDrafts.delete(key)
  else profileKeyDrafts.set(key, value)
}
function recallProfileKeyDraft(key) {
  return profileKeyDrafts.get(key) ?? ''
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
  // 用户可能刚在这个下拉里选过（还没保存），不能被磁盘值顶回去
  const remembered = recallChoice('defaultProfile')
  const wanted = remembered ?? (typeof selectedId === 'string' ? selectedId : '')
  const values = [...select.options].map((option) => option.value)
  select.value = values.includes(wanted) ? wanted : ''
  rememberChoice('defaultProfile', select.value)
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
    /*
     * 这一行未保存的 Key 草稿按**提供商**记，和上面全局那份同一套语义。
     * 为什么必须在切换前先存：`change` 事件触发时 provider.value 已经是新提供商，
     * 直接拿它当归属就会把 DeepSeek 的 Key A 记到 MiMo 名下——用户只是换了个
     * 提供商、没重填 Key，保存下去 MiMo 方案就带上了 DeepSeek 的凭据。
     */
    const apiKeyDraftKey = (id) => `profileKey:${row.dataset.id}:${id}`
    let rowProviderId = provider.value
    provider.addEventListener('change', () => {
      const matched = providerList.find((item) => item.id === provider.value)
      if (matched !== undefined) {
        const modelInput = row.querySelector('[data-field="model"]')
        const keyInput = row.querySelector('[data-field="apiKey"]')
        row.querySelector('[data-field="baseURL"]').value = matched.baseURL
        // 推荐初始模型：只有空值或「上一家推荐值」才替换；用户手填的自定义模型保留
        const suggested = matched.defaultModel ?? matched.models?.[0] ?? ''
        const currentModel = modelInput.value.trim()
        const oldSuggestion = profile.model ?? ''
        if (currentModel === '' || currentModel === oldSuggestion) {
          if (suggested !== '') modelInput.value = suggested
        } else {
          modelInput.title = `当前填写的 «${currentModel}» 可能不属于 ${matched.name}，保存前请确认`
        }
        /*
         * 草稿搬家：先把框里的内容记到**切换前**的提供商名下，再把新家自己的草稿取回来。
         * 这样"DeepSeek 填了 A → 切 MiMo → 不填 → 切回 DeepSeek"能拿回 A，
         * 而 MiMo 那一份是空的（不会被 A 顶掉）。
         */
        rememberProfileKeyDraft(apiKeyDraftKey(rowProviderId), keyInput)
        rowProviderId = matched.id
        const restored = recallProfileKeyDraft(apiKeyDraftKey(rowProviderId))
        keyInput.value = typeof restored === 'string' ? restored : ''
        // Key 提示按「该提供商」判断：换提供商才需要重填，改地址不用
        const hasForProvider = state?.config?.hasProviderKey?.[provider.value] === true
        keyInput.placeholder = hasForProvider ? '已保存，留空不修改' : (matched.apiKeyRequired === false ? '本地端点可留空' : '未配置（可在控制台获取）')
        keyInput.title = hasForProvider || matched.apiKeyRequired === false
          ? ''
          : '该提供商还没保存过 Key，需要填写后这套方案才能使用'
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
    // Key 按提供商判断（不再按 providerId|baseURL 的认证目标）：同一个提供商换地址不用重填
    const providerHasKey = state?.config?.hasProviderKey?.[profile.providerId ?? ''] === true
    key.placeholder = providerHasKey ? '已保存，留空不修改'
      : (profile.hasApiKey === true ? `已保存（${profile.apiKey}），留空不修改` : 'sk-…')
    key.dataset.field = 'apiKey'
    keyCell.append(key)

    const modelCell = document.createElement('td')
    const model = document.createElement('input')
    model.value = profile.model ?? ''
    model.placeholder = 'mimo-v2.6-flash'
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
    /*
     * 这一行两个下拉的记忆键。用行 id 而不是数组下标：
     * 删掉一行之后下标会整体前移，按下标记会把上一行的选择搬到下一行。
     *
     * 推理档位**按协议分开记**（`:mimo` / `:deepseek`）：两家档位集合不重叠，
     * 共用一个键时，切到 MiMo 会因为 'low' 不在 MiMo 选项里而落到 default，
     * 并把记忆覆盖成 default——切回 DeepSeek 时用户刚选的 low 就没了。
     */
    const reasoningKeyFor = (kind) => `profile:${row.dataset.id}:reasoning:${kind}`
    const hintKey = `profile:${row.dataset.id}:providerHint`
    const applyProfileReasoningUI = () => {
      const matched = providerList.find((item) => item.id === provider.value)
      const kind = reasoningKindForProvider(matched?.id, hintSelect.value)
      const options = kind === 'mimo' ? mimoOptions() : kind === 'deepseek' ? deepseekOptions() : []
      const stored = kind === 'mimo'
        ? (profile.mimoThinking ?? 'default')
        : kind === 'deepseek' ? (profile.deepseekReasoningEffort ?? 'default') : 'default'
      fillSelect(reasoningSelect, options, stored, kind === 'none' ? undefined : reasoningKeyFor(kind))
      reasoningSelect.hidden = options.length === 0
      fillSelect(hintSelect, (state?.reasoning?.hints ?? [{ id: 'auto', name: '自动' }]).map((item) => ({ value: item.id, label: item.name })), profile.providerHint ?? 'auto', hintKey)
      hintSelect.hidden = matched?.id !== 'openai-compatible'
    }
    // 用户手动改这两个下拉时要先记住，再重画（否则会被磁盘里的旧值顶回去）
    reasoningSelect.addEventListener('change', () => {
      const kind = reasoningKindForProvider(provider.value, hintSelect.value)
      if (kind !== 'none') rememberChoice(reasoningKeyFor(kind), reasoningSelect.value)
    })
    hintSelect.addEventListener('change', () => {
      rememberChoice(hintKey, hintSelect.value)
      applyProfileReasoningUI()
    })
    provider.addEventListener('change', applyProfileReasoningUI)
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
    /*
     * Key 只交**当前选中的这家**的：
     * 框里留着的是上一家的草稿（切换时已搬回上一家名下、这里也换了内容），
     * 万一用户没触发切换就直接敲了字，那也是给当前这家的。
     * 空 = 不修改（主进程按掩码语义保留这条方案已存的 Key）。
     */
    rememberProfileKeyDraft(`profileKey:${row.dataset.id}:${providerId}`, row.querySelector('[data-field="apiKey"]'))
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
      const scheduleKey = `schedule:${index}:profileId`
      for (const profile of profileList) {
        const option = document.createElement('option')
        option.value = profile.id
        option.textContent = profile.name !== '' ? profile.name : (profile.model !== '' ? profile.model : profile.id)
        select.append(option)
      }
      // 用户刚选过就按记忆恢复，否则按规则里存着的方案；都没有才落到第一项
      const remembered = recallChoice(scheduleKey)
      const wanted = remembered ?? (typeof rule.profileId === 'string' && rule.profileId !== '' ? rule.profileId : '')
      const values = [...select.options].map((option) => option.value)
      select.value = values.includes(wanted) ? wanted : (values[0] ?? '')
      rememberChoice(scheduleKey, select.value)
      select.addEventListener('change', () => rememberChoice(scheduleKey, select.value))
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
  /*
   * API Key 输入框：初始无值，占位按「该提供商是否已存过 Key」显示。
   * 草稿只按**提供商**记（不再按 providerId|baseURL 的认证目标）：
   * 同一个提供商换地址是常规操作，不该让用户以为 Key 丢了、还要重填一遍。
   */
  const providerId = config.providerId ?? ''
  const savedForKey = config.providerKeys?.[providerId]
  const draft = apiKeyDrafts[providerId]
  $('apiKey').value = typeof draft === 'string' ? draft : ''
  // 记下"此刻界面显示的是哪一家"：切提供商的 change 事件用它当草稿归属
  lastRenderedProviderId = providerId
  $('apiKey').placeholder = (typeof savedForKey === 'string' && savedForKey !== '')
    ? `已保存（${savedForKey}），留空不修改`
    : (typeof draft === 'string' && draft !== '' ? '未保存的草稿（保存后生效）' : '未配置（可在控制台获取）')
  $('maxSteps').value = config.maxSteps
  $('temperature').value = config.temperature
  $('contextWindow').value = String(config.contextWindow ?? 1000000)
  $('contextPercent').value = String(config.contextCompressPercent ?? 90)
  renderContextHint()

  $('searchKey').value = ''
  $('searchKey').placeholder = config.search?.hasApiKey ? `已保存（${config.search.apiKey}），留空不修改` : '未配置（可选）'


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
    // 上下文预算（tokens）：默认 1M 窗口、到 90% 才压缩
    contextWindow: Number($('contextWindow').value),
    contextCompressPercent: Number($('contextPercent').value),
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
  // 提交前把当前输入框的草稿按提供商记下（为切回恢复）；payload 只带当前输入框的值
  commitApiKeyDraft()
  if ($('apiKey').value.trim() !== '') payload.apiKey = $('apiKey').value.trim()
  if ($('searchKey').value.trim() !== '') payload.search = { provider: 'tavily', apiKey: $('searchKey').value.trim() }
  /*
   * 系统提示词三态：
   *  - 自定义 → 交文本（合法性由主进程判，界面不做二次加工、不 trim）
   *  - 默认 + 当前配置是自定义 → 交 null（"我要恢复默认"，语义明确）
   *  - 默认 + 当前本来就是默认 → 不交这个字段（无关设置不必顺手写一遍提示词）
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
  // 重新加载 = 一切以磁盘为准：清掉未保存的草稿、下拉记忆和上一轮的在线列表
  reasoningDraft.mimoThinking = null
  reasoningDraft.deepseekReasoningEffort = null
  clearChoices()
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
  /*
   * 先把当前输入框里未保存的 Key 草稿按**切换前**的提供商存起来，切回来时还能恢复。
   *
   * 顺序是关键：change 事件触发时 `$('provider').value` 已经是新提供商了，
   * 直接读它就会把 MiMo 的 Key 记到 DeepSeek 名下——切到 DeepSeek 后那颗 Key
   * 会被当成 DeepSeek 的草稿用于取模型列表/保存，等于把 A 家的凭据发给 B 家。
   * 所以记忆目标必须是「上一帧真正显示的那个提供商」。
   */
  rememberApiKeyDraft(lastRenderedProviderId)
  const provider = currentProvider()
  if (provider) {
    $('baseURL').value = provider.baseURL
    // 新提供商用**它自己的推荐模型**（MiMo 是 mimo-v2.6-flash），不是上一家那个
    const suggested = provider.defaultModel ?? provider.models?.[0] ?? ''
    if (suggested !== '') $('model').value = suggested
  }
  lastRenderedProviderId = provider?.id ?? ''
  // 换提供商后：恢复该提供商之前留下的 Key 草稿（没有则是空白，暗示需填写）
  restoreApiKeyDraft()
  // 在线列表按提供商各自取：清掉上一家的结果，避免拿 MiMo 的目录去填 DeepSeek
  discoveredModels = null
  modelsError = ''
  modelsFetchedAt = ''
  $('modelsHint').textContent = ''
  fillProviderNote()
  await maybeFetchModels()
})

/** Base URL 修改不改变"这是哪个提供商"，Key 与草稿都保持不动（常规切换不需要重填） */
$('baseURL').addEventListener('change', () => {
  // 先把刚敲进框里的（还没保存的）Key 记到当前提供商名下，再恢复——
  // 直接 restore 会把它清空，用户就得重填一遍
  rememberApiKeyDraft()
  restoreApiKeyDraft()
  fetchModels({ force: true })
})
/**
 * 未保存的 API Key 草稿，按**提供商**记。
 *
 * 目的只有一个：用户在 MiMo 输入 Key（还没点保存）后切到 DeepSeek，
 * 那颗 Key 不能被自动用于 DeepSeek 的模型发现/测试/保存；切回 MiMo 时自动恢复。
 *
 * 刻意**不**按 `providerId|baseURL` 拆：同一个提供商换地址（官方 → 自建代理、
 * 按量付费 → Token Plan）是常规操作，拆成不同槽位会让用户以为 Key 丢了。
 */
const apiKeyDrafts = {}
/**
 * 上一次真正渲染到界面上的提供商 id。
 *
 * 存在的唯一原因：`change` 事件里 `$('provider').value` 已经变成新值，
 * 拿它当"草稿属于谁"会把旧提供商的 Key 记到新提供商名下。
 */
let lastRenderedProviderId = ''
function draftKeyForUI() {
  return currentProvider()?.id ?? ''
}
/** @param {string} [providerId] 草稿归属；缺省 = 界面上当前选中的提供商 */
function rememberApiKeyDraft(providerId = draftKeyForUI()) {
  const value = $('apiKey').value.trim()
  const key = typeof providerId === 'string' ? providerId : ''
  if (key === '') return
  /*
   * 空输入框 = 用户把草稿删掉了，草稿也要一起清掉。
   *
   * 以前这里是"空就不写"，于是删掉的草稿还留在表里：切走再切回来，被删掉的 Key
   * 又出现在输入框里，用户会以为删不掉。注意这只影响**草稿**——
   * 已保存的 Key 在磁盘上，清空输入框的语义仍是"不修改"，不会被这里抹掉。
   */
  if (value === '') { delete apiKeyDrafts[key]; return }
  apiKeyDrafts[key] = value
}
function restoreApiKeyDraft() {
  const key = draftKeyForUI()
  const draft = apiKeyDrafts[key]
  $('apiKey').value = typeof draft === 'string' ? draft : ''
  // 占位提示随该提供商是否有已保存凭据/草稿变化
  const provider = currentProvider()
  const savedForKey = state?.config?.providerKeys?.[key]
  $('apiKey').placeholder = (typeof savedForKey === 'string' && savedForKey !== '')
    ? '已保存，留空不修改'
    : (provider?.apiKeyRequired === false ? '本地端点可留空' : (typeof draft === 'string' && draft !== '' ? '未保存的草稿（保存后生效）' : '未配置（可在控制台获取）'))
}
/** 提交前把当前草稿也存好（用户点保存时） */
function commitApiKeyDraft() {
  rememberApiKeyDraft()
}

/* 推理下拉一改就记进草稿与记忆，并重算提示与温度可用性 */
for (const [id, field] of [['mimoThinking', 'mimoThinking'], ['deepseekEffort', 'deepseekReasoningEffort'], ['providerHint', 'providerHint']]) {
  $(id).addEventListener('change', () => {
    // 用户的手动选择必须先落进记忆，否则下一次 render 会被磁盘值顶回去（就是这个 bug）
    rememberChoice(field, $(id).value)
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
for (const id of ['contextWindow', 'contextPercent']) {
  const element = $(id)
  if (element !== null) element.addEventListener('input', renderContextHint)
}
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
    // 已落盘：档位、下拉记忆与 Key 草稿作废（磁盘为准）；其它提供商的草稿保留供切回恢复
    reasoningDraft.mimoThinking = null
    reasoningDraft.deepseekReasoningEffort = null
    clearChoices()
    if (typeof state.config.providerHint !== 'string') state.config.providerHint = 'auto'
    const savedProvider = draftKeyForUI()
    if (payload.apiKey !== undefined) delete apiKeyDrafts[savedProvider]
    /*
     * 方案行里已经提交出去的 Key 草稿同样作废：磁盘上已经有了，留着只会在
     * 下次切提供商时把旧值又填回输入框（看起来像"我明明清掉了它又回来了"）。
     */
    for (const profile of payload.modelProfiles ?? []) {
      if (profile.apiKey !== undefined && profile.apiKey !== '') {
        profileKeyDrafts.delete(`profileKey:${profile.id}:${profile.providerId}`)
      }
    }
    fillForm()
    if (payload.systemPrompt === null) restoredDefault = true
    if (result.warnings && result.warnings.length > 0) {
      // 磁盘已保存但部分运行时同步失败——明确告知用户哪些没生效
      setStatus(`已保存，但有 ${result.warnings.length} 项未即时生效：${result.warnings.join('；')}`, 'warn')
    } else {
      setStatus('已保存', 'ok')
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
// 登录过程的两条推送：链接到了 / 扫完了

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

/* 音乐：只检查 LX 的开放 API 能不能连上 */
$('checkLx').addEventListener('click', async () => {
  $('lxHint').textContent = '正在连 LX…'
  const result = await window.settingsApi.lxStatus()
  $('lxHint').textContent = result.ok === true
    ? `连着（${result.status}${result.title ? `：${result.title}` : ''}）`
    : `连不上：${result.error}${result.launched ? '（已尝试启动 LX）' : ''}`
})
