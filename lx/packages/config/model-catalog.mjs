/**
 * 模型列表解析与筛选（纯逻辑，便于单测）。
 *
 * 为什么单独一个文件：`GET /v1/models` 返回的是**整个账号可用的模型目录**，
 * 里面会混着 ASR / TTS / 图像 / embedding 这类**不能用于 Chat Completions** 的条目
 * （MiMo 就是这样：`mimo-v2.5-asr`、`mimo-v2.5-tts*` 和 `mimo-v2.6-*` 一起返回）。
 * 把语音模型当聊天模型选上，用户下一次对话就是一条报错，所以这里必须过滤。
 *
 * 但过滤只能「宁可多留、不可误删」：
 *  - 有明确元数据（modalities / type / capabilities）时按元数据判；
 *  - 没有元数据时只按**已知的非聊天标识**（-asr / -tts / whisper / embedding…）剔除；
 *  - 认不出来的新模型一律保留 —— 提供商随时可能上新聊天模型，
 *    静态列表不是永远完整的目录，不能因为"没见过"就当它不存在。
 */

/**
 * 已知不用于 Chat Completions 的模型名标识（小写、按**整段**匹配）。
 * 只收语音 / 向量 / 重排这类确定不能对话的；图像视频各家接法不统一，
 * 宁可留着让用户自己选，也不误删一个可能能用的聊天模型（认不出就保留）。
 */
const NON_CHAT_SEGMENTS = new Set([
  'asr', 'tts', 'stt',
  'voice', 'voices', 'voiceclone', 'voicedesign', 'voiceconversion',
  'audio', 'speech', 'whisper', 'transcribe', 'transcription', 'parakeet', 'coqui',
  'embed', 'embedding', 'embeddings', 'rerank', 'reranker',
])

/** 元数据里表示「能当聊天模型用」的信号 */
const CHAT_TASK_TYPES = new Set(['text-generation', 'chat', 'chat-completions', 'conversational', 'text'])
const NON_CHAT_TASK_TYPES = new Set([
  'audio-to-text', 'text-to-audio', 'speech-recognition', 'speech-synthesis',
  'automatic-speech-recognition', 'text-to-speech', 'text-to-image', 'image-generation',
  'text-to-video', 'feature-extraction', 'sentence-similarity', 'rerank', 'fill-mask',
])

/** 把 id / 名字切成可读段（- _ . 空格 都算分隔），便于整段比对，避免 `tts` 误伤 `attest` */
function segments(text) {
  return String(text ?? '').toLowerCase().split(/[^a-z0-9]+/u).filter(Boolean)
}

/** 从一条 /models 记录里挑出可能的「类型 / 模态」信息（各家字段名不统一，兼容取法） */
function metadataHints(entry) {
  const types = []
  const push = (value) => {
    if (typeof value === 'string') types.push(value.trim().toLowerCase())
    else if (Array.isArray(value)) for (const item of value) push(item)
  }
  push(entry?.type ?? entry?.modelType ?? entry?.model_type)
  push(entry?.task ?? entry?.task_type ?? entry?.capabilities?.task)
  for (const key of ['input_modalities', 'output_modalities', 'modalities']) push(entry?.[key])
  return types.filter(Boolean)
}

/**
 * 这条记录能不能用于 Chat Completions？
 * @param {string|object} entry `/models` 里的一项（给字符串时只看 id）
 * @returns {{chat:boolean, id:string, reason:string}}
 */
export function classifyModel(entry) {
  const id = typeof entry === 'string' ? entry : String(entry?.id ?? entry?.name ?? '')
  if (id === '') return { chat: false, id, reason: '空 id' }

  const types = metadataHints(entry)
  // 元数据明确说自己是生成类/聊天类 → 留（优先级最高，新模型上新最快）
  if (types.some((type) => CHAT_TASK_TYPES.has(type))) return { chat: true, id, reason: '元数据标为文本生成' }
  if (types.some((type) => NON_CHAT_TASK_TYPES.has(type))) return { chat: false, id, reason: `元数据标为 ${types.find((t) => NON_CHAT_TASK_TYPES.has(t))}` }
  // 明确给了 output_modalities 且完全不含 text → 不是聊天模型（DeepSeek 会带这个字段）
  const outputs = Array.isArray(entry?.output_modalities) ? entry.output_modalities.map((v) => String(v).toLowerCase()) : null
  if (outputs !== null && outputs.length > 0 && !outputs.includes('text')) {
    return { chat: false, id, reason: `输出模态 ${outputs.join('/')}` }
  }

  // 没有可靠元数据：只按已知非聊天标识剔除
  const parts = segments(id)
  const hit = parts.find((part) => NON_CHAT_SEGMENTS.has(part))
  if (hit !== undefined) return { chat: false, id, reason: `模型名含 ${hit}` }
  return { chat: true, id, reason: '默认保留' }
}

/**
 * 解析 `GET /v1/models` 的响应体。
 * 各家字段基本一致（`{object:"list", data:[...]}`），少数端点直接返回数组，都兼容。
 * @returns {{models:Array<{id:string,entry:object,chat:boolean,reason:string,effort?:object,contextWindow?:number,maxOutputTokens?:number}>, raw:number}}
 */
export function parseModelResponse(json) {
  const list = Array.isArray(json) ? json : (Array.isArray(json?.data) ? json.data : (Array.isArray(json?.models) ? json.models : []))
  const seen = new Set()
  const models = []
  for (const item of list) {
    const entry = item !== null && typeof item === 'object' ? item : { id: String(item ?? '') }
    const { chat, id, reason } = classifyModel(entry)
    if (id === '' || seen.has(id)) continue
    seen.add(id)
    const model = { id, entry, chat, reason }
    // 能力元数据：DeepSeek 会给 effort.supported_levels / default_level
    if (entry.effort !== undefined && entry.effort !== null) model.effort = entry.effort
    const contextWindow = Number(entry.context_window ?? entry.contextWindow ?? entry.max_context_length)
    if (Number.isFinite(contextWindow)) model.contextWindow = contextWindow
    const maxOutput = Number(entry.max_output_tokens ?? entry.maxOutputTokens ?? entry.max_completion_tokens)
    if (Number.isFinite(maxOutput)) model.maxOutputTokens = maxOutput
    models.push(model)
  }
  return { models, raw: list.length }
}
