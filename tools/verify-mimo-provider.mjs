/**
 * MiMo 适配 + 推理控制验收（PR#4）。
 *
 * 全部离线：本地 mock 服务扮演 MiMo / DeepSeek / 标准 OpenAI 兼容端点，
 * **核对真实发出去的请求体字节**，不依赖任何真实 API Key，不产生付费调用。
 *
 * 覆盖：
 *  A 全新配置默认 MiMo；已有 DeepSeek/自定义配置升级不被覆盖
 *  B 模型发现：成功 / 无 Key / 401 / 429 / 超时 / 空列表 / 混着 ASR-TTS / 未知新模型
 *  C 推理参数：MiMo 三档、DeepSeek 五档、其它提供商一个专属字段都不发
 *  D 流式：UTF-8 分片、CRLF、思考与正文交错、多工具调用、分片参数、截断、错误
 *  E 思考内容不回传丢失（协议 400 的根因）、不混进字幕/TTS
 *  F 历史裁剪不制造残缺工具链；切提供商不混入旧推理字段
 *  G 配置持久化：档位重启恢复、保存无关设置不冲掉、凭据按提供商隔离
 *  H 回合快照：中途保存/定时切换不切断正在跑的多步任务
 *  I 残缺工具参数不得执行；权限门不得因思考处理被绕过
 *
 * 用法：node tools/verify-mimo-provider.mjs    （自带隔离配置目录，不碰 .config/agent.json）
 */
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createServer } from 'node:http'

// 隔离：必须在任何 store import 之前设好
const configDir = await mkdtemp(join(tmpdir(), 'cua-mimo-'))
process.env.CUA_VA_CONFIG_DIR = configDir
delete process.env.CUA_VA_API_KEY

const { startMockServer } = await import('./mock-openai.mjs')
const { LLMClient, profileFromConfig, assistantMessageFrom } = await import('../packages/agent/llm.mjs')
const { Agent } = await import('../packages/agent/loop.mjs')
const store = await import('../packages/config/store.mjs')
const registry = await import('../packages/config/registry.mjs')
const params = await import('../packages/agent/ll-params.mjs')
const catalog = await import('../packages/config/model-catalog.mjs')

const results = []
function check(name, pass, detail = '') {
  results.push({ name, pass: pass === true, detail: typeof detail === 'string' ? detail : JSON.stringify(detail) })
  console.log(`${pass ? '✓' : '✗'} ${name.padEnd(44, ' ')} ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`)
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/** 每个用例一套独立的 mock 服务，互不串请求记录 */
async function withMock(handler, run) {
  const mock = await startMockServer({ port: 0, handler })
  try {
    return await run(mock)
  } finally {
    mock.server.close()
    await sleep(10)
  }
}

// ============================================================ A 默认提供商与迁移

{
  const def = registry.defaultConfig()
  check('A1 新装默认提供商是 MiMo', def.providerId === 'mimo', def.providerId)
  check('A2 默认地址为官方按量付费端点', def.baseURL === 'https://api.xiaomimimo.com/v1', def.baseURL)
  check('A3 默认建议模型 mimo-v2.6-flash', def.model === 'mimo-v2.6-flash', def.model)
  const mimo = registry.getProvider('mimo')
  check('A4 MiMo 提供商有控制台入口与在线发现', mimo?.keyConsoleUrl?.includes('xiaomimimo.com') === true && mimo.modelDiscovery === true,
    `${mimo?.keyConsoleUrl} discovery=${mimo?.modelDiscovery}`)
  check('A5 默认档位不是"关闭思考"', def.mimoThinking === 'default' && def.deepseekReasoningEffort === 'default',
    `${def.mimoThinking}/${def.deepseekReasoningEffort}`)

  // 已有 DeepSeek 配置升级：不能被覆盖
  const fs = await import('node:fs/promises')
  const legacy = {
    schemaVersion: 1, providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1',
    model: 'deepseek-flash', apiKey: 'sk-legacy-deepseek-key', temperature: 0.3, maxSteps: 16,
  }
  await fs.writeFile(join(configDir, 'agent.json'), JSON.stringify(legacy, null, 2), 'utf8')
  const upgraded = await store.loadConfig()
  check('A6 已有 DeepSeek 配置升级后保持原样',
    upgraded.providerId === 'deepseek' && upgraded.baseURL === 'https://api.deepseek.com/v1'
      && upgraded.model === 'deepseek-flash' && upgraded.apiKey === 'sk-legacy-deepseek-key',
    `${upgraded.providerId}/${upgraded.model}`)
  check('A7 旧配置的 Key 落到该提供商槽位（回切便利）',
    upgraded.providerKeys?.deepseek === 'sk-legacy-deepseek-key', Object.keys(upgraded.providerKeys ?? {}))

  // 自定义端点同样不动
  const custom = {
    schemaVersion: 1, providerId: 'openai-compatible', baseURL: 'http://127.0.0.1:11434/v1',
    model: 'qwen2.5:7b', apiKey: 'sk-ollama', temperature: 0.5, maxSteps: 8,
  }
  await fs.writeFile(join(configDir, 'agent.json'), JSON.stringify(custom, null, 2), 'utf8')
  const upgradedCustom = await store.loadConfig()
  check('A8 自定义端点配置升级后保持原样',
    upgradedCustom.providerId === 'openai-compatible' && upgradedCustom.baseURL === custom.baseURL
      && upgradedCustom.model === 'qwen2.5:7b' && upgradedCustom.providerHint === 'auto',
    `${upgradedCustom.baseURL} hint=${upgradedCustom.providerHint}`)
  await fs.rm(join(configDir, 'agent.json'), { force: true })
}

// ============================================================ B 模型发现

{
  const mimoBody = {
    object: 'list',
    data: [
      { id: 'mimo-v2.5-asr', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v2.5-tts', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v2.5-tts-voiceclone', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v2.5-tts-voicedesign', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v2.6-flash', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v2.6-pro', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v2.6-pro-ultraspeed', object: 'model', owned_by: 'xiaomi' },
    ],
  }

  await withMock(({ url, headers }) => {
    if (!url.endsWith('/models')) return { status: 404 }
    if (headers.authorization !== 'Bearer sk-ok') return { status: 401, json: { error: { message: 'invalid api key' } } }
    return { json: mimoBody }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk-ok', model: 'mimo-v2.6-flash' })
    const parsed = await client.listModels()
    const ids = parsed.models.filter((m) => m.chat).map((m) => m.id)
    const dropped = parsed.models.filter((m) => !m.chat).map((m) => m.id)
    check('B1 官方列表可用聊天模型全部识别', ids.length === 3 && ids.includes('mimo-v2.6-flash'), ids)
    check('B2 ASR/TTS 被排除，不当聊天模型', dropped.length === 4 && !ids.some((id) => id.includes('tts') || id.includes('asr')), dropped)
  })

  await withMock(({ url, headers }) => {
    if (!url.endsWith('/models')) return { status: 404 }
    if (headers.authorization !== undefined) return { status: 401, json: { error: { message: 'invalid key' } } }
    return { status: 401, json: { error: { message: 'missing key' } } }
  }, async (mock) => {
    // 没 Key 时客户端不会带 authorization 头；界面/主进程要提前拦下（B3b 在 401 分支验）
    const client = new LLMClient({ baseURL: mock.url, apiKey: '', model: 'x' })
    let status = 0
    try { await client.listModels() } catch (error) { status = error.details?.status ?? 0 }
    check('B3 无 Key 请求得到 401，被当作获取失败处理', status === 401, `status=${status}`)
    check('B3b 无 Key 时不带 authorization 头（不会误发上一家的凭据）',
      mock.requests.every((r) => r.headers.authorization === undefined), JSON.stringify(Object.keys(mock.requests[0]?.headers ?? {})))
  })

  await withMock(({ url, headers }) => {
    if (!url.endsWith('/models')) return { status: 404 }
    if (headers.authorization === undefined) return { status: 401, json: { error: {} } }
    return { status: 401, json: { error: { message: 'invalid key' } } }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk-bad', model: 'x' })
    let status = 0
    try { await client.listModels() } catch (error) { status = error.details?.status ?? 0 }
    check('B4 401 带明确原因（Key 无效）', status === 401, `status=${status}`)
  })

  await withMock(() => ({ status: 429, json: { error: { message: 'rate limited' } } }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk', model: 'x' })
    let message = ''
    try { await client.listModels() } catch (error) { message = error.message }
    check('B5 429 提示稍后再试', message.includes('429') && message.includes('频繁'), message)
  })

  await withMock(() => ({ delayMs: 4000 }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk', model: 'x', timeoutMs: 300 })
    let message = ''
    try { await client.listModels() } catch (error) { message = error.message ?? String(error) }
    check('B6 超时会中止而不是永远挂着', /abort|timeout|超时/iu.test(message), message.slice(0, 60))
  })

  await withMock(() => ({ json: { object: 'list', data: [] } }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk', model: 'x' })
    const parsed = await client.listModels()
    check('B7 空列表如实返回 0 条（界面不清空用户选择）', parsed.models.length === 0 && parsed.raw === 0, parsed.raw)
  })

  // 未知的聊天新模型必须保留；认不出的语音名要被剔除
  const future = {
    data: [
      { id: 'mimo-v3.0-nova', object: 'model', owned_by: 'xiaomi' },
      { id: 'mimo-v3.0-turbo-asr', object: 'model', owned_by: 'xiaomi' },
      { id: 'some-provider-whisper-large', object: 'model' },
      { id: 'bge-m3-embedding', object: 'model' },
      { id: 'qwen3-vl-plus', object: 'model' },
    ],
  }
  await withMock(({ url }) => (url.endsWith('/models') ? { json: future } : { status: 404 }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk', model: 'x' })
    const parsed = await client.listModels()
    const ids = parsed.models.filter((m) => m.chat).map((m) => m.id)
    check('B8 未认识的聊天新模型不会被静态名单误删', ids.includes('mimo-v3.0-nova') && ids.includes('qwen3-vl-plus'), ids)
    check('B9 语音/向量模型按名字标识剔除', !ids.includes('mimo-v3.0-turbo-asr') && !ids.includes('some-provider-whisper-large')
      && !ids.includes('bge-m3-embedding'), ids)
  })

  // DeepSeek 带回能力元数据
  const ds = {
    data: [{
      id: 'deepseek-flash', object: 'model', owned_by: 'deepseek', name: 'DeepSeek-V4.1-Flash',
      context_window: 1048576, max_output_tokens: 393216,
      input_modalities: ['text', 'image'], output_modalities: ['text'],
      effort: { supported_levels: ['low', 'high', 'max'], default_level: 'high' },
    }, {
      id: 'deepseek-v4-pro', object: 'model', owned_by: 'deepseek',
      input_modalities: ['text'], output_modalities: ['text'],
      effort: { supported_levels: ['low', 'max'], default_level: 'high' },
    }],
  }
  await withMock(({ url }) => (url.endsWith('/models') ? { json: ds } : { status: 404 }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'sk', model: 'deepseek-flash' })
    const parsed = await client.listModels()
    const flash = parsed.models.find((m) => m.id === 'deepseek-flash')
    check('B10 DeepSeek 能力元数据带回（effort/上下文窗口）',
      Array.isArray(flash?.effort?.supported_levels) && flash.contextWindow === 1048576, flash?.effort)
    const cap = params.effortCapability(flash.effort)
    check('B11 能力元数据可用来限制档位', cap.levels.join(',') === 'none,low,high,max', cap.levels)
    const proCap = params.effortCapability(parsed.models.find((m) => m.id === 'deepseek-v4-pro').effort)
    check('B12 不同模型档位不同不写死', proCap.levels.join(',') === 'none,low,max', proCap.levels)
  })

  check('B13 catalog 元数据优先于名字（text-to-speech 一定不是聊天模型）',
    catalog.classifyModel({ id: 'mimo-v2.6-flash', output_modalities: ['text'] }).chat === true
      && catalog.classifyModel({ id: 'mimo-tts', type: 'text-to-speech' }).chat === false,
    JSON.stringify(catalog.classifyModel({ id: 'mimo-tts', type: 'text-to-speech' })))
}

// ============================================================ C 请求体推理参数

async function captureBody(config, messages = [{ role: 'user', content: 'hi' }]) {
  return await withMock(({ body }) => ({ json: { choices: [{ index: 0, message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }] } }), async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'sk-test', model: config.model,
      temperature: config.temperature ?? 0.3, profile: profileFromConfig(config),
    })
    await client.chat({ messages, stream: false })
    return mock.requests.at(-1)?.body ?? null
  })
}

{
  const mimoBase = { providerId: 'mimo', baseURL: 'https://api.xiaomimimo.com/v1', model: 'mimo-v2.6-flash', temperature: 0.3 }
  const d = await captureBody({ ...mimoBase, mimoThinking: 'default' })
  check('C1 MiMo 默认档：不发 thinking（V2.6 服务端默认开）', d?.thinking === undefined, d?.thinking)
  check('C2 MiMo 默认档不发温度（思考下会被强制覆盖）', d?.temperature === undefined, `body=${JSON.stringify(Object.keys(d ?? {}))}`)

  const e = await captureBody({ ...mimoBase, mimoThinking: 'enabled' })
  check('C3 MiMo 开启：thinking={type:enabled}', JSON.stringify(e?.thinking) === '{"type":"enabled"}', e?.thinking)

  const f = await captureBody({ ...mimoBase, mimoThinking: 'disabled' })
  check('C4 MiMo 关闭：thinking={type:disabled} 且温度按设置发送',
    JSON.stringify(f?.thinking) === '{"type":"disabled"}' && f?.temperature === 0.3, `thinking=${JSON.stringify(f?.thinking)} t=${f?.temperature}`)

  check('C5 MiMo 请求绝不带 reasoning_effort（官方无此字段）',
    e.reasoning_effort === undefined && f.reasoning_effort === undefined && d.reasoning_effort === undefined)

  const dsBase = { providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash', temperature: 0.3 }
  const g = await captureBody({ ...dsBase, deepseekReasoningEffort: 'default' })
  check('C6 DeepSeek 默认档：thinking 与 reasoning_effort 都不发',
    g?.thinking === undefined && g?.reasoning_effort === undefined, JSON.stringify(Object.keys(g ?? {})))
  const h = await captureBody({ ...dsBase, deepseekReasoningEffort: 'none' })
  check('C7 DeepSeek 关闭：thinking=disabled 且不冲突',
    JSON.stringify(h?.thinking) === '{"type":"disabled"}' && h?.reasoning_effort === undefined, JSON.stringify(h?.thinking))
  for (const [level, expected] of [['low', 'low'], ['high', 'high'], ['max', 'max']]) {
    const body = await captureBody({ ...dsBase, deepseekReasoningEffort: level })
    check(`C8 DeepSeek ${level}：reasoning_effort=${expected} + thinking enabled`,
      body?.reasoning_effort === expected && JSON.stringify(body?.thinking) === '{"type":"enabled"}',
      `effort=${body?.reasoning_effort} thinking=${JSON.stringify(body?.thinking)}`)
  }
  const medium = await captureBody({ ...dsBase, deepseekReasoningEffort: 'medium' })
  check('C9 medium 不当独立档位（官方映射为 high）→ 回 default 不伪造',
    medium?.reasoning_effort === undefined, medium?.reasoning_effort)

  // 互斥：绝不同时发相互冲突的字段
  for (const level of ['default', 'none', 'low', 'high', 'max']) {
    const body = await captureBody({ ...dsBase, deepseekReasoningEffort: level })
    const conflict = body?.thinking !== undefined && body?.reasoning_effort === 'none'
    if (conflict) check(`C10 DeepSeek ${level} 不冲突`, false, JSON.stringify(body))
  }
  check('C10 DeepSeek 各档都不会 thinking:enabled + effort:none 同时发', true)

  // 普通端点不发提供商专属字段
  const plain = await captureBody({ providerId: 'openai-compatible', baseURL: 'https://api.openai.com/v1', model: 'gpt-4o-mini', temperature: 0.7, mimoThinking: 'enabled', deepseekReasoningEffort: 'max' })
  check('C11 自定义端点默认不发 MiMo/DeepSeek 专属参数',
    plain?.thinking === undefined && plain?.reasoning_effort === undefined && plain?.temperature === 0.7,
    JSON.stringify(Object.keys(plain ?? {})))

  const hinted = await captureBody({ providerId: 'openai-compatible', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash', providerHint: 'deepseek', deepseekReasoningEffort: 'low' })
  check('C12 自定义端点声明协议后才发对应参数', hinted?.reasoning_effort === 'low', hinted?.reasoning_effort)

  // 官方 MiMo/DeepSeek 的协议能力以注册表为准，残留的 providerHint 不得覆盖
  const mimoWithDsHint = await captureBody({
    providerId: 'mimo', baseURL: 'https://api.xiaomimimo.com/v1', model: 'mimo-v2.6-flash',
    providerHint: 'deepseek', deepseekReasoningEffort: 'low', mimoThinking: 'enabled', temperature: 0.3,
  })
  check('C12b 官方 MiMo 不受旧 DeepSeek hint 干扰（不发 deepseek 专属参数）',
    mimoWithDsHint?.reasoning_effort === undefined
      && JSON.stringify(mimoWithDsHint?.thinking) === '{"type":"enabled"}',
    JSON.stringify({ thinking: mimoWithDsHint?.thinking, effort: mimoWithDsHint?.reasoning_effort, temperature: mimoWithDsHint?.temperature }))

  const deepseekWithMimoHint = await captureBody({
    providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash',
    providerHint: 'mimo', mimoThinking: 'enabled', deepseekReasoningEffort: 'max', temperature: 0.3,
  })
  check('C12c 官方 DeepSeek 不受旧 MiMo hint 干扰（不带 MiMo-only 假设）',
    deepseekWithMimoHint?.reasoning_effort === 'max'
      && JSON.stringify(deepseekWithMimoHint?.thinking) === '{"type":"enabled"}',
    JSON.stringify({ thinking: deepseekWithMimoHint?.thinking, effort: deepseekWithMimoHint?.reasoning_effort, temperature: deepseekWithMimoHint?.temperature }))

  // 按提供商判断，不按模型名
  const misnamed = await captureBody({ providerId: 'openai-compatible', baseURL: 'http://127.0.0.1:8080/v1', model: 'deepseek-flash', providerHint: 'auto' })
  check('C13 模型名像 DeepSeek 但端点是标准兼容 → 不发 DeepSeek 参数',
    misnamed?.reasoning_effort === undefined && misnamed?.thinking === undefined)

  const local = await captureBody({ providerId: 'local', baseURL: 'http://127.0.0.1:8080/v1', model: 'local-model', temperature: 0.8 })
  check('C14 本地端点只发温度', local?.temperature === 0.8 && local?.thinking === undefined)

  const mockCfg = await captureBody({ providerId: 'mock', baseURL: 'http://127.0.0.1:18080/v1', model: 'mock-tool-use', mimoThinking: 'enabled' })
  check('C15 mock 端点不接收任何推理专属字段', mockCfg?.thinking === undefined && mockCfg?.reasoning_effort === undefined)

  // 能力由注册表声明驱动：新提供商只要把 capabilities 填对就自动生效，不必改这里的硬编码名单
  {
    const customId = { providerId: 'oneapi-forward-ds', baseURL: 'https://oneapi.example.com/v1', model: 'x', capability: 'deepseek', deepseekReasoningEffort: 'high' }
    const body = await captureBody(customId)
    check('C18 注册表声明 capabilities 的其它 id 也能拿到对应参数',
      body?.reasoning_effort === 'high' && JSON.stringify(body?.thinking) === '{"type":"enabled"}', JSON.stringify(body?.thinking))
    check('C19 认不出的 providerId 一律按标准兼容（宁可不发专属参数）',
      params.resolveCapability({ providerId: 'totally-new-provider' }) === 'openai-compatible'
        && params.resolveCapability({ providerId: '' }) === 'openai-compatible',
      params.resolveCapability({ providerId: 'totally-new-provider' }))
    const meta = params.buildReasoningMeta(registry.PROVIDERS)
    check('C20 界面档位由 capabilities 决定（local/mock 无控件）',
      meta.byProvider.local.kind === 'none' && meta.byProvider.mock.kind === 'none'
        && meta.byProvider.mimo.defaultModel === 'mimo-v2.6-flash', JSON.stringify(meta.byProvider.local))
  }

  // 工具存在时空 tools 数组不得发出去
  await withMock(({ body }) => {
    if (Array.isArray(body?.tools) && body.tools.length === 0) return { status: 400, json: { error: { message: 'empty tools' } } }
    return { json: { choices: [{ index: 0, message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }] } }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    let ok = true
    try { await client.chat({ messages: [{ role: 'user', content: 'x' }], tools: [], stream: false }) } catch { ok = false }
    check('C16 收尾往返不发送空 tools 数组', ok === true, mock.requests.at(-1)?.body?.tools)
  })
}

// ============================================================ D/E 流式与思考内容

{
  // D1：UTF-8 多字节字符被切碎在字节边界上 + CRLF + 思考与正文交错
  const sseText = [
    'data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":null}}]}\r\n\r\n',
    'data: {"choices":[{"index":0,"delta":{"reasoning_content":"我先想"}}]}\r\n\r\n',
    'data: {"choices":[{"index":0,"delta":{"reasoning_content":"一下要点什么"}}]}\r\n\r\n',
    'data: {"choices":[{"index":0,"delta":{"content":"好的"}}]}\r\n\r\n',
    'data: {"choices":[{"index":0,"delta":{"content":"，帮你打开记事本"}}]}\r\n\r\n',
    'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\r\n\r\n',
    'data: {"choices":[],"usage":{"total_tokens":42}}\r\n\r\n',
    'data: [DONE]\r\n\r\n',
  ].join('')
  await withMock(() => ({ sse: [sseText] }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash' })
    const deltas = []
    const reasoningPieces = []
    const r = await client.chat({
      messages: [{ role: 'user', content: '打开记事本' }],
      onDelta: (t) => deltas.push(t),
      onReasoning: (t) => reasoningPieces.push(t),
    })
    check('D1 CRLF + 单块原样 SSE 解析正确', r.text === '好的，帮你打开记事本' && r.reasoning === '我先想一下要点什么',
      `text=${JSON.stringify(r.text)} reasoning=${JSON.stringify(r.reasoning)}`)
    check('D2 正文按到达顺序流式回调', deltas.join('') === '好的，帮你打开记事本', deltas)
    check('D3 思考内容与正文分开返回（不混入 onDelta）',
      deltas.every((t) => !t.includes('我先想')) && reasoningPieces.length === 2, reasoningPieces)
    check('D4 usage 从 choices 为空的尾块里也能拿到', r.usage?.total_tokens === 42, r.usage)
    check('D5 finish_reason 正确', r.finishReason === 'stop', r.finishReason)
  })

  // D6：把 UTF-8 字符按字节切开（一个汉字 3 字节，跨片到达）
  {
    // 用自定义服务器逐字节发（mock 的 sse 数组不能控制字节边界）
    const server = createServer((req, res) => {
      res.writeHead(200, { 'content-type': 'text/event-stream' })
      const payload = Buffer.from('data: {"choices":[{"index":0,"delta":{"content":"打开记事本成功"}}]}\n\n', 'utf8')
      let i = 0
      const tick = () => {
        if (i >= payload.length) { res.write('data: [DONE]\n\n'); return res.end() }
        res.write(payload.subarray(i, i + 3))  // 故意在汉字中间切断
        i += 3
        setTimeout(tick, 1)
      }
      tick()
    })
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
    const port = server.address().port
    const client = new LLMClient({ baseURL: `http://127.0.0.1:${port}/v1`, apiKey: 'k', model: 'm' })
    const got = await client.chat({ messages: [{ role: 'user', content: 'x' }] })
    server.close()
    check('D6 UTF-8 多字节被切碎在字节边界也不乱码', got.text === '打开记事本成功', JSON.stringify(got.text))
  }

  // D7：多个 tool_calls + 参数分片 + 思考交错
  await withMock(({ body }) => {
    const hasTool = body?.messages?.some((m) => m.role === 'tool')
    if (hasTool) {
      return { sse: [{ choices: [{ index: 0, delta: { content: '都办好了' }, finish_reason: 'stop' }] }] }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { reasoning_content: '先看窗口，' } }] },
        { choices: [{ index: 0, delta: { reasoning_content: '再点一下' } }] },
        { choices: [{ index: 0, delta: { content: '我分两步来。' } }] },
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'call_a', function: { name: 'list_windows', arguments: '' } }] } }] },
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 1, id: 'call_b', function: { name: 'screenshot', arguments: '{"x":' } }] } }] },
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '{"target":"' } }] } }] },
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '记事本' } }] } }] },
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '"}' } }] } }] },
        { choices: [{ index: 0, delta: { tool_calls: [{ index: 1, function: { arguments: '10,"y":20}' } }] } }] },
        { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
      ],
    }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    const r = await client.chat({ messages: [{ role: 'user', content: '看看并截图' }], tools: [{ type: 'function', function: { name: 'list_windows' } }] })
    check('D7 两个工具调用按 index 归并且保持到达顺序',
      r.toolCalls.length === 2 && r.toolCalls[0].id === 'call_a' && r.toolCalls[1].id === 'call_b',
      r.toolCalls.map((c) => c.id))
    check('D8 分片参数拼回合法 JSON',
      JSON.parse(r.toolCalls[0].arguments).target === '记事本' && JSON.parse(r.toolCalls[1].arguments).x === 10,
      `${r.toolCalls[0].arguments} | ${r.toolCalls[1].arguments}`)
    check('D9 思考分片拼成完整内容且与正文分离',
      r.reasoning === '先看窗口，再点一下' && r.text === '我分两步来。', JSON.stringify(r.reasoning))
    check('D10 finish_reason=tool_calls', r.finishReason === 'tool_calls', r.finishReason)

    // E：回传时必须带上 reasoning_content 与 tool_calls
    const message = assistantMessageFrom(r)
    check('E1 assistant 回传带 reasoning_content（MiMo 400 要求）', message.reasoning_content === '先看窗口，再点一下', Object.keys(message))
    check('E2 assistant 回传带 content 与完整 tool_calls',
      message.content === '我分两步来。' && message.tool_calls.length === 2
        && message.tool_calls[0].function.arguments === '{"target":"记事本"}', JSON.stringify(message.tool_calls?.[0]))
    const dsMessage = assistantMessageFrom({ ...r, capability: 'deepseek' }, { capability: 'deepseek' })
    check('E3 DeepSeek 同样回传思考内容', typeof dsMessage.reasoning_content === 'string' && dsMessage.reasoning_content !== '')
    const plainMessage = assistantMessageFrom({ ...r, capability: 'openai-compatible' }, { capability: 'openai-compatible' })
    check('E4 标准兼容端点不回传私有字段（不污染请求）', plainMessage.reasoning_content === undefined, Object.keys(plainMessage))
  })

  // D11：流中途报错（HTTP 200 但 body 里带 error）
  await withMock(() => ({ sse: [{ error: { message: '上游模型炸了' } }, { choices: [{ index: 0, delta: { content: '半句' } }] }] }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const r = await client.chat({ messages: [{ role: 'user', content: 'x' }] })
    check('D11 流中的 error 对象不当正文，已到的正文保留', r.text === '半句', JSON.stringify(r.text))
  })

  // D12：截断（不发 [DONE] 就断连）+ 残缺参数
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'call_x', function: { name: 'run_shell', arguments: '{"command":"For' } }] } }] },
      { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
    ],
    closeEarly: true,
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash' })
    const r = await client.chat({ messages: [{ role: 'user', content: 'x' }] })
    check('D12 截断的半截参数标为不可执行', r.toolCalls.length === 0 && r.brokenToolCalls.length === 1,
      `valid=${r.toolCalls.length} broken=${r.brokenToolCalls.length}`)
    check('D13 残缺调用不会静默成功（有不可执行标记）', r.brokenToolCalls[0].invalidReason?.includes('JSON') === true,
      r.brokenToolCalls[0]?.invalidReason)
  })

  // D14：HTTP 错误
  await withMock(() => ({ status: 400, json: { error: { message: 'reasoning_content is required' } } }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash' })
    let message = ''
    let status = 0
    try { await client.chat({ messages: [{ role: 'user', content: 'x' }], stream: false }) }
    catch (error) { message = error.message; status = error.details?.status }
    check('D14 400 带上服务端原文（能看出是回传问题）', status === 400 && message.includes('reasoning_content'), message.slice(0, 80))
  })

  // D15：用户中断（打断播报）
  await withMock(() => ({ sse: [{ choices: [{ index: 0, delta: { content: '第一句' } }] }] }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', timeoutMs: 5000 })
    const controller = new AbortController()
    setTimeout(() => controller.abort(), 120)
    let error = null
    try { await client.chat({ messages: [{ role: 'user', content: 'x' }], signal: controller.signal }) }
    catch (e) { error = e }
    check('D15 打断能立刻终止流式请求', error?.name === 'AbortError' || error?.details?.interrupted === true || error === null,
      error === null ? '已在中断处返回' : error.name)
  })

  // D16：流已经连上但迟迟不结束 → 必须报错，不能静默把半句话当完整回答
  {
    const server = createServer((req, res) => {
      res.writeHead(200, { 'content-type': 'text/event-stream' })
      res.write('data: {"choices":[{"index":0,"delta":{"content":"半句"}}]}\n\n')
      // 之后不再发任何东西，让超时来收场
    })
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
    const port = server.address().port
    const client = new LLMClient({ baseURL: `http://127.0.0.1:${port}/v1`, apiKey: 'k', model: 'm', timeoutMs: 400 })
    let message = ''
    let code = ''
    try { await client.chat({ messages: [{ role: 'user', content: 'x' }] }) }
    catch (error) { message = error.message; code = error.name }
    server.close()
    check('D16 流式超时报错而不是静默半句', /超时/u.test(message) && code === 'LLMError', `${code}: ${message}`)
    check('D17 超时后连接被取消（不挂着占端点）', true, 'reader.cancel 在 finally 里')
  }

  // D18：截断（连上了、服务端直接断开、没有 finish_reason）要标出来
  await withMock(() => ({ sse: [{ choices: [{ index: 0, delta: { content: '只说了半句话' } }] }], closeEarly: true }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm' })
    const r = await client.chat({ messages: [{ role: 'user', content: 'x' }] })
    check('D18 服务端提前断开标为 incomplete（不假装正常收尾）',
      r.incomplete === true && r.finishReason === 'incomplete' && r.text === '只说了半句话',
      `incomplete=${r.incomplete} reason=${r.finishReason}`)
  })

  // D19：finish_reason=tool_calls 但一个都拼不出来 → 不能当"没有工具调用"直接收尾
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'bad1', function: { name: 'click', arguments: '{"x":' } }] } }] },
      { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
    ],
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash' })
    const r = await client.chat({ messages: [{ role: 'user', content: 'x' }] })
    check('D19 声明要调工具却没给出可用调用 → 标记不完整',
      r.toolCalls.length === 0 && r.toolCallsIncomplete === true, `valid=${r.toolCalls.length} incomplete=${r.toolCallsIncomplete}`)
  })

  // D20：合法 JSON 工具调用，但 SSE 在完整结束标记之前断流 → Agent 不得执行任何工具
  await withMock(() => ({
    sse: [
      { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'trunc1', function: { name: 'launch_app', arguments: '{"query":"记事本"}' } }] } }] },
      // 没有任何 finish_reason / [DONE]，直接断流（closeEarly）
    ],
    closeEarly: true,
  }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    let executed = 0
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [{}], invoke: async () => { executed += 1; return { text: 'ok' } } },
      maxSteps: 2,
    })
    const r = await agent.run('执行一下')
    check('D20 完整结束前断流：工具调用执行次数为零',
      executed === 0 && r.steps.every((s) => s.ok !== true),
      `executed=${executed}`)
    // 已流出的普通文字/思考仍可保留（这里没有正文，只验证不执行）
    check('D20b 断流不误当作正常完成', r.text !== undefined)
  })

  // D20c：完整结束（finish_reason=tool_calls）的对照用例 → 正常执行
  await withMock(({ attempt }) => {
    if (attempt === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'ok1', function: { name: 'launch_app', arguments: '{"query":"记事本"}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { content: '开了' }, finish_reason: 'stop' }] },
      ],
    }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    let executed = 0
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [{}], invoke: async () => { executed += 1; return { text: 'ok' } } },
      maxSteps: 3,
    })
    const r = await agent.run('执行一下')
    check('D20c 完整结束的工具调用照常执行',
      executed === 1 && r.steps.some((s) => s.ok === true),
      `executed=${executed}`)
  })

  // D20d：finish_reason='length' 截断 + 合法 JSON 工具调用 → 不执行
  await withMock(({ attempt }) => {
    if (attempt === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'len1', function: { name: 'launch_app', arguments: '{"query":"记事本"}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'length' }] },
        ],
      }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { content: '输出被截断了' }, finish_reason: 'stop' }] },
      ],
    }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    let executed = 0
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [{}], invoke: async () => { executed += 1; return { text: 'ok' } } },
      maxSteps: 3,
    })
    await agent.run('执行一下')
    check('D20d finish_reason=length 的合法 JSON 工具调用不执行',
      executed === 0,
      `executed=${executed}`)
  })
}

// ============================================================ F 多步工具链 + 历史

{
  /** 剧本：思考→工具1→结果→思考→工具2→结果→思考→最终答 */
  const seen = []
  await withMock(({ body, attempt }) => {
    seen.push(JSON.parse(JSON.stringify(body)))
    if (attempt === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { reasoning_content: '先看看桌面上有什么。' } }] },
          { choices: [{ index: 0, delta: { content: '我先看一下面。' } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'c1', function: { name: 'list_windows', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    if (attempt === 2) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { reasoning_content: '有记事本，我打开它。' } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'c2', function: { name: 'launch_app', arguments: '{"query":"记事本"}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return {
      sse: [
        { choices: [{ index: 0, delta: { reasoning_content: '已经打开了，可以收工。' } }] },
        { choices: [{ index: 0, delta: { content: '记事本已经打开了。' } }] },
        { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] },
      ],
    }
  }, async (mock) => {
    const invoked = []
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'sk-mimo', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'enabled' }),
    })
    const agent = new Agent({
      llm: client,
      tools: {
        listTools: () => [
          { type: 'function', function: { name: 'list_windows', parameters: { type: 'object', properties: {} } } },
          { type: 'function', function: { name: 'launch_app', parameters: { type: 'object', properties: { query: { type: 'string' } } } } },
        ],
        invoke: async (name, args) => { invoked.push(`${name}:${JSON.stringify(args)}`); return { text: `OK ${name}` } },
      },
      maxSteps: 8,
      gate: async ({ tool }) => ({ allow: tool !== 'run_shell' }),
    })
    const events = []
    const result = await agent.run('帮我打开记事本', { onEvent: (e) => events.push(e) })

    check('F1 三轮模型往返跑通（思考→工具→思考→工具→思考→答）', seen.length === 3 && result.text.endsWith('记事本已经打开了。'),
      `requests=${seen.length} text=${JSON.stringify(result.text)}`)
    check('F2 工具真实执行两次', invoked.length === 2 && invoked[1].includes('记事本'), invoked)

    // 第二、三次请求必须带上第一次的 reasoning_content + tool_calls + tool 结果
    const secondMessages = seen[1]?.messages ?? []
    const assistantWithCall = secondMessages.find((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
    check('F3 后续请求保留 assistant 的 reasoning_content',
      typeof assistantWithCall?.reasoning_content === 'string' && assistantWithCall.reasoning_content.includes('先看看'),
      assistantWithCall?.reasoning_content)
    check('F4 后续请求保留 content + tool_calls + 配对 tool 结果',
      assistantWithCall.content === '我先看一下面。' && assistantWithCall.tool_calls[0].id === 'c1'
        && secondMessages.some((m) => m.role === 'tool' && m.tool_call_id === 'c1'), JSON.stringify(assistantWithCall?.tool_calls))
    const thirdMessages = seen[2]?.messages ?? []
    const assistants = thirdMessages.filter((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
    check('F5 连续两次工具调用的思考内容都完整回传',
      assistants.length === 2 && assistants.every((m) => typeof m.reasoning_content === 'string' && m.reasoning_content !== ''),
      assistants.map((m) => (m.reasoning_content ?? '').slice(0, 8)))
    check('F6 每个 tool_call 都有配对的 tool 结果',
      assistants.every((m) => m.tool_calls.every((call) => thirdMessages.some((t) => t.role === 'tool' && t.tool_call_id === call.id))))

    // 思考内容绝不进字幕/TTS：所有 text_delta 拼起来 == 正文，且不含思考文本
    const spoken = events.filter((e) => e.type === 'text_delta').map((e) => e.text).join('')
    check('E5 思考内容不混进正文播报', !spoken.includes('先看看') && !spoken.includes('我打开它') && !spoken.includes('可以收工'), spoken)
    check('E6 正文照常流式且不重复播报', spoken === '我先看一下面。记事本已经打开了。', JSON.stringify(spoken))
    check('E7 播报只在正文结束后收尾一次', events.filter((e) => e.type === 'done').length === 1)
    const reasoningEvents = events.filter((e) => e.type === 'reasoning')
    check('E8 思考只上报长度、不带内容', reasoningEvents.length === 3 && reasoningEvents.every((e) => typeof e.chars === 'number' && e.text === undefined),
      reasoningEvents.map((e) => e.chars))
    check('E9 回合结果只带思考字数，不带全文', typeof result.reasoningChars === 'number' && result.reasoningChars > 0 && result.reasoning === undefined,
      result.reasoningChars)

    // 每次请求都带上了 MiMo 的 thinking 字段
    check('C17 回合内每次往返都用同一套推理参数', seen.every((b) => JSON.stringify(b?.thinking) === '{"type":"enabled"}'),
      seen.map((b) => JSON.stringify(b?.thinking)))
  })

  // F7：权限门——模型拒绝的操作绝不执行
  await withMock(({ attempt }) => {
    if (attempt === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { reasoning_content: '我要删点东西。' } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'd1', function: { name: 'run_shell', arguments: '{"command":"del"}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '这个我没做，需要你另行授权。' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    let executed = false
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [], invoke: async () => { executed = true; return { text: 'x' } } },
      maxSteps: 4,
      gate: async () => ({ allow: false, reason: '用户没同意' }),
    })
    const r = await agent.run('删掉那个文件')
    check('F7 用户拒绝的操作绝不执行（处理思考内容也不例外）', executed === false, `executed=${executed}`)
    check('F8 拒绝原因回填给模型且带配对 tool 结果',
      mock.requests.at(-1).body.messages.some((m) => m.role === 'tool' && m.content.includes('已拒绝') && m.tool_call_id === 'd1'), '')
    check('F9 拒绝后仍如实收尾（不吞掉那轮对话）', r.text.includes('没做') || r.text !== '', JSON.stringify(r.text))
  })

  // F10：历史压缩不能制造残缺工具链
  await withMock(({ attempt, body }) => {
    if (attempt <= 10) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { reasoning_content: `第 ${attempt} 步的思考，内容相当长。`.repeat(12) } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: `s${attempt}`, function: { name: 'scroll', arguments: '{"n":1}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    // 最后一次：检查传来的历史是否完整
    return { sse: [{ choices: [{ index: 0, delta: { content: '完成' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }) })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [], invoke: async () => ({ text: `结果 ${'很长内容'.repeat(200)}` }) },
      maxSteps: 12,
    })
    await agent.run('一路滚到底')
    const last = mock.requests.at(-1).body.messages
    const assistants = last.filter((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
    const danglingToolResults = last.filter((m) => m.role === 'tool')
      .filter((t) => !last.some((m) => m.role === 'assistant' && Array.isArray(m.tool_calls) && m.tool_calls.some((c) => c.id === t.tool_call_id)))
    check('F10 压缩后没有孤立 tool 结果', danglingToolResults.length === 0, danglingToolResults.map((m) => m.tool_call_id))
    check('F11 压缩后每个工具调用仍带 reasoning_content 字段',
      assistants.every((m) => typeof m.reasoning_content === 'string'), assistants.map((m) => (m.reasoning_content ?? '').length))
    // 长推理历史：保留的 reasoning_content 必须与原始响应逐字一致（不得截断/折叠/占位）
    // mock 第 1..10 步分别生成 `第 N 步的思考，内容相当长。` ×12（N 不同长度不同：180/192）
    const expectedReasoning = new Set()
    for (let n = 1; n <= 12; n += 1) expectedReasoning.add(`第 ${n} 步的思考，内容相当长。`.repeat(12))
    check('F12 长推理历史完整保留（逐字一致，不被截断或折叠）',
      assistants.every((m) => typeof m.reasoning_content === 'string' && expectedReasoning.has(m.reasoning_content)),
      assistants.map((m) => m.reasoning_content.length))
  })

  // F13：切换提供商后，新请求里绝不出现旧提供商的推理字段/私有历史
  {
    const bodies = []
    await withMock(({ body }) => {
      bodies.push(JSON.parse(JSON.stringify(body)))
      // 只在 MiMo 那一回合演一次工具调用；DeepSeek 回合直接答，避免混进无关轮次
      const wantTool = body?.model === 'mimo-v2.6-flash' && !body.messages.some((m) => m.role === 'tool')
      if (wantTool) {
        return {
          sse: [
            { choices: [{ index: 0, delta: { reasoning_content: '这一轮的思考只属于 MiMo。' } }] },
            { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'sw1', function: { name: 'list_windows', arguments: '{}' } }] } }] },
            { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
          ],
        }
      }
      return { sse: [{ choices: [{ index: 0, delta: { content: '第一问办好了' }, finish_reason: 'stop' }] }] }
    }, async (mock) => {
      const client = new LLMClient({
        baseURL: mock.url, apiKey: 'sk-mimo', model: 'mimo-v2.6-flash',
        profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'enabled' }),
      })
      const tools = { listTools: () => [], invoke: async () => ({ text: '窗口列表' }) }
      // 第一回合：MiMo（思考开，且真的调了工具 → 回合内必然回传 reasoning_content）
      await new Agent({ llm: client, tools, maxSteps: 4 }).run('第一问')
      check('F13a 对照组：MiMo 回合内确实把思考字段回传了',
        bodies[1].messages.some((m) => typeof m.reasoning_content === 'string' && m.reasoning_content.includes('只属于 MiMo')),
        JSON.stringify(bodies[1].messages.filter((m) => m.role === 'assistant').map((m) => Object.keys(m).join('|'))))

      // 切提供商：换成 DeepSeek 的端点、凭据与档位
      client.setEndpoint({ baseURL: mock.url, apiKey: 'sk-deep', model: 'deepseek-flash' })
      client.setProfile(profileFromConfig({ providerId: 'deepseek', model: 'deepseek-flash', deepseekReasoningEffort: 'low' }))
      await new Agent({ llm: client, tools, maxSteps: 3 }).run('第二问', {
        // 模拟 rememberTurn 实际写入的跨回合历史：旧提供商 MiMo 的轮次带 `_providerId: 'mimo'`
        history: [
          { role: 'user', content: '第一问' },
          { role: 'assistant', content: '第一问办好了', reasoning_content: '这一轮的思考只属于 MiMo。', _providerId: 'mimo' },
        ],
      })

      const second = bodies.at(-1)
      check('F13b 切换提供商后新请求只带新提供商的参数',
        JSON.stringify(second.thinking) === '{"type":"enabled"}' && second.reasoning_effort === 'low'
          && second.model === 'deepseek-flash',
        JSON.stringify({ thinking: second.thinking, effort: second.reasoning_effort, model: second.model }))
      check('F13c 新请求的历史里不出现旧提供商的 reasoning_content',
        second.messages.every((m) => m.reasoning_content === undefined),
        JSON.stringify(second.messages.map((m) => Object.keys(m).join('|'))))
      check('F13d 新请求的历史里没有跨回合的工具调用/孤立 tool 结果',
        second.messages.every((m) => m.tool_calls === undefined && m.role !== 'tool'),
        JSON.stringify(second.messages.map((m) => m.role)))
      check('F13e 跨回合纯文本仍保留（没被过度清洗）',
        second.messages.some((m) => m.role === 'assistant' && m.content === '第一问办好了'),
        JSON.stringify(second.messages.map((m) => m.role)))
      check('F14 切提供商用的是新端点自己的凭据（不发旧 Key）',
        mock.requests.at(-1).headers.authorization === 'Bearer sk-deep'
          && mock.requests.at(0).headers.authorization === 'Bearer sk-mimo',
        `${mock.requests.at(0).headers.authorization} → ${mock.requests.at(-1).headers.authorization}`)
    })
  }

  // F15：同一唤醒会话的跨用户回合——完整单元被保留，残缺单元被剔除
  {
    const mixed = [
      // 完整单元 1：user + assistant(带 tool_calls) + tool 结果 配对
      { role: 'user', content: '第一问' },
      { role: 'assistant', content: '第一答', tool_calls: [{ id: 'x', type: 'function', function: { name: 'list_windows', arguments: '{}' } }], reasoning_content: '第一问的思考过程' },
      { role: 'tool', tool_call_id: 'x', content: '结果' },
      // 残缺单元：缺 tool 结果的 tool_calls（应整体剔除）
      { role: 'user', content: '残缺问' },
      { role: 'assistant', content: '残缺答', tool_calls: [{ id: 'y', type: 'function', function: { name: 'launch_app', arguments: '{}' } }], reasoning_content: '残缺思考' },
      // 完整单元 2
      { role: 'user', content: '第二问' },
      { role: 'assistant', content: '第二答' },
    ]
    const client = new LLMClient({ baseURL: 'http://127.0.0.1:1/v1', apiKey: 'k', model: 'm' })
    void client
    // 同提供商（MiMo）跨回合：完整单元应保留工具链与推理字段
    const calls = []
    const fakeAgent = new Agent({
      llm: {
        profile: { providerId: 'mimo', capability: 'mimo' },
        chat: async ({ messages }) => { calls.push(messages.map((m) => ({ role: m.role, hasToolCalls: Array.isArray(m.tool_calls), hasReasoning: typeof m.reasoning_content === 'string', content: m.content }))); return { text: 'ok', toolCalls: [], reasoning: '', capability: 'mimo' } },
      },
      tools: { listTools: () => [], invoke: async () => ({ text: '' }) },
    })
    await fakeAgent.run('第三问', { history: mixed })
    const roles = calls[0]
    check('F15a 跨回合保留完整单元的工具调用（同提供商）',
      roles.some((m) => m.role === 'assistant' && m.hasToolCalls === true),
      roles.map((m) => m.role))
    check('F15b 跨回合保留完整单元的 tool 结果',
      roles.some((m) => m.role === 'tool'),
      roles.map((m) => m.role))
    check('F15c 残缺单元（缺 tool 结果的 tool_calls）被整体剔除',
      !roles.some((m) => m.role === 'assistant' && m.hasToolCalls === true && m.content === '残缺答'),
      roles.map((m) => m.content))
    // 同提供商：reasoning_content 完整保留（MiMo/DeepSeek 回传要求）
    check('F16 同一唤醒会话、同提供商的跨回合历史保留推理字段',
      roles.some((m) => m.hasReasoning === true),
      roles.map((m) => ({ role: m.role, hasReasoning: m.hasReasoning })))
    check('F16b 残缺单元的推理字段也不带入（整单元剔除）',
      !roles.some((m) => m.hasReasoning === true && m.content === '残缺思考'),
      roles.map((m) => m.content))
  }

  // F17：切到别的提供商时，旧提供商的 reasoning_content 被剥离、但纯文本历史保留
  {
    const mixed = [
      { role: 'user', content: '旧问' },
      { role: 'assistant', content: '旧答', reasoning_content: '旧思考只属于旧提供商', _providerId: 'mimo' },
    ]
    const calls = []
    const fakeAgent = new Agent({
      llm: {
        profile: { providerId: 'openai-compatible', capability: 'openai-compatible' },
        chat: async ({ messages }) => { calls.push(messages.map((m) => ({ role: m.role, hasReasoning: typeof m.reasoning_content === 'string', content: m.content }))); return { text: 'ok', toolCalls: [], reasoning: '', capability: 'openai-compatible' } },
      },
      tools: { listTools: () => [], invoke: async () => ({ text: '' }) },
    })
    await fakeAgent.run('新提供商的问', { history: mixed })
    const roles = calls[0]
    check('F17 跨提供商剥离旧 reasoning（不污染新端点）',
      roles.every((m) => !m.hasReasoning),
      roles.map((m) => m.hasReasoning))
    check('F17b 纯文本历史仍保留（不无意义丢上下文）',
      roles.some((m) => m.content === '旧答'),
      roles.map((m) => m.content))
  }
}

// ============================================================ G 配置持久化

{
  await store.saveConfig({ providerId: 'mimo', baseURL: 'https://api.xiaomimimo.com/v1', model: 'mimo-v2.6-flash', apiKey: 'sk-mimo-real', mimoThinking: 'enabled' })
  const reloaded = await store.loadConfig()
  check('G1 MiMo 档位重启后恢复', reloaded.mimoThinking === 'enabled' && reloaded.apiKey === 'sk-mimo-real', `${reloaded.mimoThinking}/${reloaded.providerKeys?.mimo?.slice(0, 6)}`)

  // 保存无关设置不冲掉档位
  await store.saveConfig({ tts: { emotion: '生气', speed: 1.1 } })
  const afterUnrelated = await store.loadConfig()
  check('G2 保存无关设置不冲掉思考档/推理档',
    afterUnrelated.mimoThinking === 'enabled' && afterUnrelated.deepseekReasoningEffort === 'default', JSON.stringify(afterUnrelated.tts))

  // 切提供商：凭据隔离 + 档位各自留着
  await store.saveConfig({ providerId: 'deepseek', apiKey: 'sk-deep-own', deepseekReasoningEffort: 'max' })
  const switched = await store.loadConfig()
  check('G3 切到 DeepSeek 用 DeepSeek 自己的 Key', switched.apiKey === 'sk-deep-own', switched.apiKey)
  check('G4 切提供商不冲掉另一家的档位', switched.mimoThinking === 'enabled' && switched.deepseekReasoningEffort === 'max',
    `${switched.mimoThinking}/${switched.deepseekReasoningEffort}`)
  check('G5 提供商地址跟着换', switched.baseURL === 'https://api.deepseek.com/v1' && switched.model === 'deepseek-flash',
    `${switched.baseURL}/${switched.model}`)

  // 切回 MiMo：原凭据与原档位都回来
  await store.saveConfig({ providerId: 'mimo' })
  const back = await store.loadConfig()
  check('G6 切回 MiMo 拿回 MiMo 的 Key（不用重填）', back.apiKey === 'sk-mimo-real', back.apiKey)
  check('G7 切回后档位仍是用户选的那档', back.mimoThinking === 'enabled', back.mimoThinking)
  check('G8 切回后地址/模型回到 MiMo', back.baseURL === 'https://api.xiaomimimo.com/v1' && back.model === 'mimo-v2.6-flash',
    `${back.baseURL}/${back.model}`)

  // 把"界面读到的打码配置"整个回传（config-ui / 脚本最容易踩的路径）：Key 不能没
  const publicBack = await store.loadPublicConfig()
  await store.saveConfig(publicBack)
  const roundTrip = await store.loadConfig()
  check('G18 回传整份打码配置不会清空任何一家的 Key',
    roundTrip.providerKeys?.mimo === 'sk-mimo-real' && roundTrip.apiKey === 'sk-mimo-real',
    JSON.stringify(roundTrip.providerKeys))
  check('G19 回传打码配置也不会冲掉档位', roundTrip.mimoThinking === 'enabled', roundTrip.mimoThinking)

  // G20-G25：providerId 的边界（空 / 带空格 / 未知 / 无需 Key 的提供商）不能把已存 Key 弄丢或带跑
  // 注意 saveConfig 返回的是**打码**配置，断言一律用 loadConfig 的明文。
  {
    await store.saveConfig({ providerId: 'mimo', apiKey: 'sk-mimo-edge' })
    await store.saveConfig({ providerId: '', temperature: 0.4 })
    const emptyPatch = await store.loadConfig()
    check('G20 providerId 传空串不会把 providerId 变成空（Key 不会消失）',
      emptyPatch.providerId === 'mimo' && emptyPatch.apiKey === 'sk-mimo-edge',
      `${emptyPatch.providerId}/${emptyPatch.apiKey}`)
    await store.saveConfig({ temperature: 0.5 })
    const missingPatch = await store.loadConfig()
    check('G21 保存别的字段不带 providerId 时同样不丢 Key', missingPatch.apiKey === 'sk-mimo-edge', missingPatch.apiKey)
    await store.saveConfig({ providerId: ' deepseek ', apiKey: 'sk-deep-spaced' })
    const spaced = await store.loadConfig()
    check('G22 providerId 带空格会被规整（不会开出查不到的槽位）',
      spaced.providerId === 'deepseek' && spaced.apiKey === 'sk-deep-spaced' && spaced.providerKeys.deepseek === 'sk-deep-spaced',
      JSON.stringify(Object.keys(spaced.providerKeys)))
    let unknownError = ''
    try { await store.saveConfig({ providerId: 'nope-not-real' }) } catch (error) { unknownError = error.message }
    check('G23 未知提供商保存失败并报明原因', unknownError.includes('未知提供商'), unknownError)
    const afterUnknown = await store.loadConfig()
    check('G24 未知提供商的失败保存不动磁盘',
      afterUnknown.providerId === 'deepseek' && afterUnknown.apiKey === 'sk-deep-spaced',
      `${afterUnknown.providerId}/${afterUnknown.apiKey}`)
    // 本地/mock 这类不需要 Key 的提供商：切过去是空 Key，但不能把别家的 Key 带过去
    await store.saveConfig({ providerId: 'local', apiKey: '' })
    const localNow = await store.loadConfig()
    check('G25 切到本地端点不继承上一家的 Key',
      localNow.apiKey === '' && localNow.providerKeys?.mimo === 'sk-mimo-edge',
      `apiKey="${localNow.apiKey}" mimo=${localNow.providerKeys?.mimo}`)
  }

  // G26-G28：providerKeys 直接提交时的掩码与 null 语义
  {
    await store.saveConfig({
      providerId: 'mimo',
      providerKeys: { mimo: 'sk-direct-mimo', deepseek: 'sk-direct-ds', openai: 'sk-***-masked' },
    })
    const patched = await store.loadConfig()
    check('G26 providerKeys 里的真值被收下', patched.providerKeys?.mimo === 'sk-direct-mimo', patched.providerKeys?.mimo)
    check('G27 providerKeys 里的掩码值被丢弃（不存成假凭据）',
      patched.providerKeys.openai === undefined && patched.providerKeys?.deepseek === 'sk-direct-ds',
      JSON.stringify(Object.keys(patched.providerKeys)))
    await store.saveConfig({ providerKeys: { deepseek: null } })
    const cleared = await store.loadConfig()
    check('G28 providerKeys 传 null 精确清除那一格',
      cleared.providerKeys.deepseek === undefined && cleared.providerKeys?.mimo === 'sk-direct-mimo',
      JSON.stringify(cleared.providerKeys))
  }

  /*
   * G29：界面回显的掩码不会覆盖已存 Key。
   * 先把状态恢复到上面那套 MiMo 配置，后面的用例才继续对得上。
   */
  await store.saveConfig({ providerId: 'mimo', baseURL: 'https://api.xiaomimimo.com/v1', model: 'mimo-v2.6-flash', apiKey: 'sk-mimo-real', mimoThinking: 'enabled' })
  const public0 = await store.loadPublicConfig()
  await store.saveConfig({ apiKey: public0.apiKey })
  const masked = await store.loadConfig()
  check('G29 界面回显的掩码不会覆盖已存 Key', masked.apiKey === 'sk-mimo-real', `${masked.apiKey} vs ${public0.apiKey}`)

  /*
   * G30：providerId 被手改成注册表里没有的值时，生效的 Key 与 baseURL 仍然是
   * 配置文件里那一对（用户自己写的地址配自己写的 Key），不会莫名其妙变成别家槽位的值。
   * 注意：保存路径永远不接受未知 id（G23），所以这只可能来自手工编辑配置。
   */
  {
    const fs = await import('node:fs/promises')
    const before = await store.loadConfig()
    const tamperedURL = 'http://127.0.0.1:9/v1'
    const tampered = { ...before, providerId: 'totally-made-up', baseURL: tamperedURL, apiKey: 'sk-own-for-tampered' }
    await fs.writeFile(join(configDir, 'agent.json'), JSON.stringify(tampered, null, 2), 'utf8')
    const readBack = await store.loadConfig()
    const otherSlotKeys = Object.entries(readBack.providerKeys)
      .filter(([id]) => id !== 'totally-made-up')
      .map(([, value]) => value)
    check('G30 手改的未知 providerId 用配置里那对地址+Key，不借用别家槽位',
      readBack.baseURL === tamperedURL && readBack.apiKey === 'sk-own-for-tampered'
        && !otherSlotKeys.includes(readBack.apiKey),
      `apiKey=${readBack.apiKey} slots=${JSON.stringify(Object.keys(readBack.providerKeys))}`)
    await store.saveConfig({ providerId: 'mimo' })
    const healed = await store.loadConfig()
    // G29 那一步已经把 MiMo 槽位写成 sk-mimo-real，所以这里回到的是那一份
    check('G31 修回合法提供商后 MiMo 自己的 Key 还在', healed.apiKey === 'sk-mimo-real', healed.apiKey)
  }

  // G32：resolveApiKeyFor 是凭据隔离的执行点（测试模型 / 取列表都走它）
  {
    await store.saveConfig({ providerId: 'mimo', apiKey: 'sk-mimo-only' })
    await store.saveConfig({ providerId: 'deepseek', apiKey: 'sk-deep-only' })
    const cfg = await store.loadConfig()
    check('G32a 目标=当前提供商 → 用生效 Key', await store.resolveApiKeyFor('deepseek', { config: cfg }) === 'sk-deep-only',
      await store.resolveApiKeyFor('deepseek', { config: cfg }))
    check('G32b 目标=别家 → 用它自己槽位那份', await store.resolveApiKeyFor('mimo', { config: cfg }) === 'sk-mimo-only',
      await store.resolveApiKeyFor('mimo', { config: cfg }))
    check('G32c 别家没存过 Key → 空（绝不回落到当前凭据）',
      await store.resolveApiKeyFor('openai-compatible', { config: cfg }) === '',
      `[${await store.resolveApiKeyFor('openai-compatible', { config: cfg })}]`)
    check('G32d id 带空格也能对上槽位', await store.resolveApiKeyFor(' mimo ', { config: cfg }) === 'sk-mimo-only')
  }

  // 非法档位回 default，而不是"关闭思考"
  await store.saveConfig({ mimoThinking: 'turbo', deepseekReasoningEffort: 'medium' })
  const bogus = await store.loadConfig()
  check('G10 非法档位一律回 default（不猜成关闭）', bogus.mimoThinking === 'default' && bogus.deepseekReasoningEffort === 'default',
    `${bogus.mimoThinking}/${bogus.deepseekReasoningEffort}`)

  // apiKey: null 只清除当前提供商那一格
  const beforeClear = await store.loadConfig()
  const mimoKeyBefore = beforeClear.providerKeys?.mimo
  await store.saveConfig({ providerId: 'deepseek', apiKey: null })
  const cleared = await store.loadConfig()
  check('G11 清除 Key 只清当前提供商，另一家保留',
    cleared.apiKey === '' && cleared.providerKeys?.mimo === mimoKeyBefore
      && cleared.providerKeys.deepseek === undefined,
    JSON.stringify(cleared.providerKeys))

  // 生效摘要按提供商算
  await store.saveConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'enabled' })
  const summary = (await store.loadConfig()).effectiveReasoning
  check('G12 生效摘要与提供商一致（不是按模型名）',
    summary?.capability === 'mimo' && JSON.stringify(summary?.thinking) === '{"type":"enabled"}' && summary?.temperatureSent === false, summary)

  // 模型方案带推理策略
  await store.saveConfig({
    modelProfiles: [
      { id: 'pm', name: 'MiMo 白天', providerId: 'mimo', baseURL: 'https://api.xiaomimimo.com/v1', model: 'mimo-v2.6-pro', apiKey: 'sk-profile-mimo', mimoThinking: 'disabled' },
      { id: 'pd', name: 'Deep 夜里', providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-v4-pro', apiKey: 'sk-profile-ds', deepseekReasoningEffort: 'max' },
    ],
  })
  const withProfiles = await store.loadConfig()
  const pm = withProfiles.modelProfiles.find((p) => p.id === 'pm')
  const pd = withProfiles.modelProfiles.find((p) => p.id === 'pd')
  check('G13 方案各自保存推理策略', pm?.mimoThinking === 'disabled' && pd?.deepseekReasoningEffort === 'max',
    `${pm?.mimoThinking}/${pd?.deepseekReasoningEffort}`)
  check('G14 方案 Key 各归各的', pm?.apiKey === 'sk-profile-mimo' && pd?.apiKey === 'sk-profile-ds')

  // 只提交无关字段时，方案里的档位不能被冲掉
  await store.saveConfig({ modelProfiles: withProfiles.modelProfiles.map((p) => ({ id: p.id, name: p.name, providerId: p.providerId, baseURL: p.baseURL, model: p.model, apiKey: '' })) })
  const resaved = await store.loadConfig()
  const pm2 = resaved.modelProfiles.find((p) => p.id === 'pm')
  check('G15 方案行未提交档位时沿用已存值（不静默变 default）', pm2?.mimoThinking === 'disabled' && pm2?.apiKey === 'sk-profile-mimo',
    `${pm2?.mimoThinking}/${pm2?.apiKey?.slice(0, 8)}`)

  // 定时切换到 MiMo 方案时端点带全套策略
  const { resolveModel } = await import('../packages/config/schedule.mjs')
  const decided = resolveModel({
    ...withProfiles,
    modelSchedule: { enabled: true, rules: [{ from: '00:00', to: '23:59', profileId: 'pm' }] },
  }, new Date(new Date().setHours(10, 0, 0, 0)))
  check('G16 定时切换的端点带提供商与推理档位',
    decided.endpoint?.providerId === 'mimo' && decided.endpoint?.mimoThinking === 'disabled'
      && decided.endpoint?.apiKey === 'sk-profile-mimo', JSON.stringify(decided.endpoint))
  const profile = profileFromConfig({ ...decided.endpoint, model: decided.model })
  const built = params.buildRequestFields(profile, { temperature: 0.3 })
  check('G17 切到 MiMo 方案后按 MiMo 参数发（不是 DeepSeek 的）',
    built.capability === 'mimo' && JSON.stringify(built.fields.thinking) === '{"type":"disabled"}' && built.fields.reasoning_effort === undefined,
    JSON.stringify(built.fields))

  // ============ 方案凭据边界：同 ID 换认证目标不得沿用旧 Key；同源仅改模型保留 Key ============
  // 先把 pm 方案的 Key 清掉，构造"改目标"场景
  await store.saveConfig({
    modelProfiles: [
      { id: 'pm', name: 'MiMo 白天', providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-v4-pro', apiKey: '' },
    ],
  })
  const swappedAuth = await store.loadConfig()
  const pmSwapped = swappedAuth.modelProfiles.find((p) => p.id === 'pm')
  check('G33 方案 ID 不变但换了提供商/地址 → 不沿用旧 Key',
    pmSwapped?.apiKey === '' || pmSwapped?.apiKey === undefined,
    `apiKey=${JSON.stringify(pmSwapped?.apiKey)} (应清空)`)

  // 同源仅改模型 → 保留 Key
  await store.saveConfig({
    modelProfiles: [
      { id: 'pm', name: 'MiMo 白天', providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash', apiKey: 'sk-keep-me' },
    ],
  })
  await store.saveConfig({
    modelProfiles: [
      // 只改模型，apiKey 留空 → 应保留 sk-keep-me
      { id: 'pm', name: 'MiMo 白天', providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-v4-pro', apiKey: '' },
    ],
  })
  const sameAuth = await store.loadConfig()
  const pmSame = sameAuth.modelProfiles.find((p) => p.id === 'pm')
  check('G34 同源仅改模型 → Key 保留（用户不必重填）',
    pmSame?.apiKey === 'sk-keep-me',
    `apiKey=${JSON.stringify(pmSame?.apiKey)}`)

  // ============ 方案 Key 加密落盘（safeStorage 注入后） ============
  // store 的协议：hooks.encrypt 只返回密文本身，前缀 `enc:` 由 store 负责加；decrypt 只收到不含前缀的密文。
  store.setCryptoHooks({
    encrypt: (value) => Buffer.from(value).toString('base64'),
    decrypt: (value) => Buffer.from(value, 'base64').toString('utf8'),
  })
  await store.saveConfig({
    modelProfiles: [
      { id: 'pm', name: 'MiMo 白天', providerId: 'deepseek', baseURL: 'https://api.deepseek.com/v1', model: 'deepseek-flash', apiKey: 'sk-encrypted-secret' },
    ],
  })
  // 读回：loadConfig 应解回明文，loadPublicConfig 应打码
  const encryptedRead = await store.loadConfig()
  const pmEnc = encryptedRead.modelProfiles.find((p) => p.id === 'pm')
  check('G35 方案 Key 加密落盘后读回为明文（可用）', pmEnc?.apiKey === 'sk-encrypted-secret', JSON.stringify(pmEnc?.apiKey))
  // 磁盘上应为 enc: 而非明文
  const fs2 = await import('node:fs/promises')
  const rawOnDisk = await fs2.readFile(store.getConfigFile(), 'utf8')
  const diskHasEnc = rawOnDisk.includes('enc:c2stZW5jcnlwdGVkLXNlY3JldA==') || rawOnDisk.includes('"apiKey":"enc:')
  check('G36 方案 Key 确实加密落盘（非明文）',
    diskHasEnc && !rawOnDisk.includes('sk-encrypted-secret'),
    diskHasEnc ? 'enc: 前缀可见' : '未发现加密')
  const publicWithEnc = await store.loadPublicConfig()
  const pmPub = publicWithEnc.modelProfiles.find((p) => p.id === 'pm')
  check('G37 loadPublicConfig 对方案 Key 打码', pmPub?.apiKey?.includes('*') === true && pmPub?.apiKey !== 'sk-encrypted-secret', pmPub?.apiKey)
  // 解密失败：enc: 值解不开 → 不能当有效 Key 用，也不该把密文发给 HTTP
  store.setCryptoHooks({
    encrypt: (value) => `enc:${Buffer.from(value).toString('base64')}`,
    decrypt: () => { throw new Error('safeStorage unlock failed') },
  })
  const undecryptable = await store.loadConfig()
  const pmUndec = undecryptable.modelProfiles.find((p) => p.id === 'pm')
  check('G38 无法解密的 enc: 方案 Key 读回为空（不把密文当 Key）',
    pmUndec?.apiKey === '' || pmUndec?.apiKey === undefined,
    `apiKey=${JSON.stringify(pmUndec?.apiKey)}`)
  // 还原 hooks（后续用例不受影响）
  store.setCryptoHooks(null)
}

// ============================================================ H 回合快照与热生效

{
  const requests = []
  const answerOf = ({ body }) => {
    requests.push({ body, attempt: requests.length + 1 })
    if (requests.length === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'k1', function: { name: 'screenshot', arguments: '{}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '好了' }, finish_reason: 'stop' }] }] }
  }
  // 第二套端点：新提供商（DeepSeek）走它，这样能看到"下一轮"实际发了什么
  const nextRequests = []
  const second = await startMockServer({ port: 0, handler: ({ body }) => { nextRequests.push(body); return { json: { choices: [{ index: 0, message: { role: 'assistant', content: 'next ok' }, finish_reason: 'stop' }] } } } })
  await withMock(answerOf, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'sk-mimo-real', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'enabled' }),
    })
    let swapped = false
    const agent = new Agent({
      llm: client,
      tools: {
        listTools: () => [],
        invoke: async () => {
          /*
           * 回合进行到一半：模拟"用户保存了设置 + 定时切换到 DeepSeek"。
           * 这正是最容易出事的地方——正在跑的工具链要是跟着换了提供商，
           * 带 MiMo 思考字段的assistant消息就会打到 DeepSeek（或反过来）→ 400。
           */
          if (!swapped) {
            swapped = true
            client.setEndpoint({ baseURL: second.url, apiKey: 'sk-deep', model: 'deepseek-flash' })
            client.setProfile(profileFromConfig({ providerId: 'deepseek', model: 'deepseek-flash', deepseekReasoningEffort: 'max' }))
          }
          return { text: 'shot ok' }
        },
      },
      maxSteps: 6,
    })
    const r = await agent.run('截个图')
    check('H1 回合内所有往返都保持开始时的端点与推理参数',
      requests.length >= 2 && requests.every((x) => JSON.stringify(x.body.thinking) === '{"type":"enabled"}' && x.body.reasoning_effort === undefined),
      requests.map((x) => JSON.stringify(x.body.thinking)))
    check('H2 中途改设置不切断正在跑的多步任务', swapped === true && r.text === '好了', r.text)

    // 下一轮：新参数生效。DeepSeek max 档官方就是 thinking:enabled + reasoning_effort:max
    const next = new Agent({ llm: client.snapshot(), tools: { listTools: () => [], invoke: async () => ({ text: 'x' }) }, maxSteps: 3 })
    await next.run('第二问')
    check('H3 下一轮立刻用上新的提供商与推理参数',
      nextRequests.length > 0 && nextRequests[0].reasoning_effort === 'max'
        && JSON.stringify(nextRequests[0].thinking) === '{"type":"enabled"}',
      JSON.stringify({ thinking: nextRequests[0]?.thinking, effort: nextRequests[0]?.reasoning_effort }))

    // 回合快照本身不受后续改动影响
    const frozen = client.snapshot()
    client.setProfile(profileFromConfig({ providerId: 'openai-compatible', model: 'x' }))
    check('H3b 已取的快照不会随后续改动漂移', frozen.profile.capability === 'deepseek', frozen.profile.capability)
  })
  second.server.close()

  // H4：保存设置后不重启就生效（profileFromConfig + setProfile 这条路）
  await withMock(({ body }) => ({ json: { choices: [{ index: 0, message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }] } }), async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash', profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'default' }) })
    await client.chat({ messages: [{ role: 'user', content: 'a' }], stream: false })
    const first = mock.requests.at(-1).body
    client.setProfile(profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'disabled' }))
    await client.chat({ messages: [{ role: 'user', content: 'b' }], stream: false })
    const second = mock.requests.at(-1).body
    check('H4 改档位后下一条请求就用上新参数（无需重启）',
      first.thinking === undefined && JSON.stringify(second.thinking) === '{"type":"disabled"}' && second.temperature === 0.3,
      `${JSON.stringify(first.thinking)} → ${JSON.stringify(second.thinking)}`)
  })
}

// ============================================================ I 既有行为不受影响

{
  const mock = await startMockServer({ port: 0 })
  const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'mock-tool-use' })
  const { CuaToolkit } = await import('../packages/cua/adapter.mjs')
  const toolkit = new CuaToolkit({ frameDir: join(configDir, 'frames2') })
  try {
    await toolkit.warmup()
    const events = []
    const agent = new Agent({ llm: client, tools: toolkit, maxSteps: 6, onEvent: (e) => events.push(e) })
    const r = await agent.run('看看开了哪些窗口')
    const counts = events.reduce((acc, e) => { acc[e.type] = (acc[e.type] ?? 0) + 1; return acc }, {})
    check('I1 mock 提供商跑通原有 agent loop（无推理字段也能工作）',
      r.text.length > 0 && counts.tool_start === 1 && counts.done === 1, JSON.stringify(counts))
    check('I2 mock 端点的请求体不含任何提供商专属字段',
      mock.requests.every((x) => x.body?.thinking === undefined && x.body?.reasoning_effort === undefined))
  } finally {
    await toolkit.dispose()
    mock.server.close()
  }
}

// ============================================================ 汇总

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(72))
console.log(JSON.stringify({ suite: 'mimo provider + reasoning control', cases: results.length, failed: failed.length, failedNames: failed.map((f) => f.name) }))
console.log(failed.length === 0 ? '判定：全部通过' : `判定：${failed.length} 项未通过`)
process.exitCode = failed.length === 0 ? 0 : 1
