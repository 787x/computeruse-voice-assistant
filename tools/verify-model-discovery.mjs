/**
 * 在线模型获取 + 按提供商挑 Key 的验收（真实 HTTP，本机回环，不产生任何真实付费调用）。
 *
 * 覆盖 PR#4 里最容易写错的四件事：
 *   1. 缓存命中 / 过期 / 手填 Key 强制重取 / clear 后重取
 *   2. 失败（401、429、超时、空目录、连不上）时**保留上一次列表**，
 *      并且绝不改动用户已选的模型（这条由界面层保证，这里验数据形状）
 *   3. 凭据按提供商隔离：目标是"还没保存的另一家"时，
 *      不允许借用当前生效的 Key（否则就是把 DeepSeek 的 Key 打到 MiMo）
 *   4. ASR/TTS 被排除、未知的聊天新模型被保留、能力元数据带回
 *
 * 用法：node tools/verify-model-discovery.mjs
 */
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const tree = fileURLToPath(import.meta.url).includes('/lx/') ? 'lx' : 'root'

// 隔离配置目录：绝不读写仓库里的 .config/agent.json
process.env.CUA_VA_CONFIG_DIR = await mkdtemp(join(tmpdir(), 'cua-disc-'))
delete process.env.CUA_VA_API_KEY

const { fetchChatModels, modelCacheKey, MODEL_LIST_TTL_MS } = await import('../packages/config/model-discovery.mjs')
const store = await import('../packages/config/store.mjs')

const results = []
function check(name, pass, detail = '') {
  results.push({ name, pass: pass === true })
  console.log(`${pass ? '✓' : '✗'} ${name.padEnd(46, ' ')} ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`)
}

/** 可控的本机"提供商目录"服务：按 Authorization 头决定回什么，方便演 401/429/超时 */
function startCatalogServer(plans) {
  const requests = []
  const server = createServer(async (req, res) => {
    let raw = ''
    for await (const chunk of req) raw += chunk
    requests.push({ url: req.url, method: req.method, headers: { ...req.headers }, body: raw === '' ? null : JSON.parse(raw) })
    const plan = plans(req.headers.authorization ?? '')
    if (plan?.delayMs) await new Promise((resolve) => setTimeout(resolve, plan.delayMs))
    if (plan?.hang) return // 什么都不回，让客户端超时
    if (plan?.status !== undefined && plan.status !== 200) {
      res.writeHead(plan.status, { 'content-type': 'application/json' })
      return res.end(JSON.stringify(plan.json ?? { error: { message: 'mock' } }))
    }
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(JSON.stringify({ object: 'list', data: plan?.data ?? [] }))
  })
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({ server, requests, url: `http://127.0.0.1:${server.address().port}/v1` }))
  })
}

const MIMO_DATA = [
  { id: 'mimo-v2.5-asr', object: 'model', owned_by: 'xiaomi' },
  { id: 'mimo-v2.5-tts', object: 'model', owned_by: 'xiaomi' },
  { id: 'mimo-v2.5-tts-voiceclone', object: 'model', owned_by: 'xiaomi' },
  { id: 'mimo-v2.6-flash', object: 'model', owned_by: 'xiaomi' },
  { id: 'mimo-v2.6-pro', object: 'model', owned_by: 'xiaomi' },
  { id: 'mimo-v2.6-pro-ultraspeed', object: 'model', owned_by: 'xiaomi' },
]

// ============================================================ 1. 成功 + 缓存

{
  const srv = await startCatalogServer(() => ({ data: MIMO_DATA }))
  const cache = new Map()
  const base = { providerId: 'mimo', baseURL: srv.url, apiKey: 'sk-ok', cache }

  const first = await fetchChatModels(base)
  check('D1 只留聊天模型', first.ok === true && first.models.length === 3, first.models?.map((m) => m.id))
  check('D2 排除项带原因', (first.dropped ?? []).length === 3 && first.dropped.some((d) => d.id === 'mimo-v2.5-asr'), first.dropped)
  check('D3 第一次不是缓存', first.cached === false && first.stale === false)
  check('D4 请求带上了 authorization', srv.requests.at(-1).headers.authorization === 'Bearer sk-ok')

  const second = await fetchChatModels(base)
  check('D5 第二次走缓存、不再打网络', second.cached === true && srv.requests.length === 1, `calls=${srv.requests.length}`)

  const forced = await fetchChatModels({ ...base, force: true })
  check('D6 force 跳过缓存', forced.cached === false && srv.requests.length === 2)

  // 手填的 Key（未保存）一律重取，且结果不进缓存共享
  const typed = await fetchChatModels({ ...base, apiKey: 'sk-typed', force: true })
  check('D7 手填 Key 也取得到列表', typed.ok === true && typed.models.length === 3)
  check('D8 用的是那份手填 Key', srv.requests.at(-1).headers.authorization === 'Bearer sk-typed')

  // 缓存过期
  cache.set(modelCacheKey('mimo', srv.url), { at: Date.now() - MODEL_LIST_TTL_MS - 1, models: [{ id: 'stale-only' }], dropped: [] })
  const expired = await fetchChatModels(base)
  check('D9 过期后重取而不是用老列表', expired.ok === true && expired.cached === false && expired.models.length === 3, expired.cached)
  check('D10 缓存键区分提供商+地址', modelCacheKey('mimo', 'http://a/v1') !== modelCacheKey('deepseek', 'http://a/v1'))
  srv.server.close()
}

// ============================================================ 2. 失败路径都保留上一次

{
  const srv = await startCatalogServer((auth) => {
    if (auth === 'Bearer ok-first') return { data: MIMO_DATA }
    if (auth === 'Bearer denied') return { status: 401, json: { error: { message: 'invalid api key' } } }
    if (auth === 'Bearer limited') return { status: 429, json: { error: {} } }
    if (auth === 'Bearer empty') return { data: [] }
    if (auth === 'Bearer audio-only') return { data: [{ id: 'mimo-v2.5-tts' }, { id: 'some-asr' }] }
    if (auth === 'Bearer broken') return { data: null }
    return { data: MIMO_DATA }
  })
  const cache = new Map()
  const once = await fetchChatModels({ providerId: 'mimo', baseURL: srv.url, apiKey: 'ok-first', cache })
  check('D11 先建立一份成功列表', once.ok === true && once.models.length === 3)

  for (const [label, key, expect] of [
    ['401', 'denied', '401'],
    ['429', 'limited', '429'],
    ['空目录', 'empty', '没有返回任何模型'],
    ['只有音频模型', 'audio-only', '都不是聊天模型'],
  ]) {
    const failed = await fetchChatModels({ providerId: 'mimo', baseURL: srv.url, apiKey: key, cache, force: true })
    check(`D12 ${label}：报错但保留上一次的列表`,
      failed.ok === false && failed.error.includes(expect) && failed.models.length === 3 && failed.stale === true,
      `${failed.error} / models=${failed.models.length}`)
  }

  // 连不上（服务已关，地址不变，这样才命中同一个缓存键）：也不能抛出去
  const sameUrl = srv.url
  srv.server.close()
  const unreachable = await fetchChatModels({ providerId: 'mimo', baseURL: sameUrl, apiKey: 'ok-first', cache, force: true })
  check('D13 端点连不上时只回错误、带上旧列表',
    unreachable.ok === false && unreachable.models.length === 3 && unreachable.stale === true,
    `${unreachable.error} / models=${unreachable.models.length}`)

  const noCache = await fetchChatModels({ providerId: 'mimo', baseURL: sameUrl, apiKey: 'ok-first', force: true })
  check('D14 没有缓存时返回空列表而不是 undefined',
    noCache.ok === false && Array.isArray(noCache.models) && noCache.models.length === 0 && noCache.stale === false,
    JSON.stringify(noCache.models))

  // 缓存里绝不能出现 Key
  const leaked = JSON.stringify([...cache.keys()]) + JSON.stringify([...cache.values()])
  check('D15 缓存内容不含任何 API Key', !/ok-first|denied|limited|sk-/u.test(leaked), leaked.slice(0, 90))

  // 缺 Key 且该提供商要求 Key：直接给提示（错误文案是"要 Key"而不是"连不上"）
  const missingKey = await fetchChatModels({ providerId: 'mimo', baseURL: sameUrl, apiKey: '', apiKeyRequired: true })
  check('D16 必须有 Key 时直接给出提示（不发请求）',
    missingKey.ok === false && /API Key/u.test(missingKey.error), missingKey.error)
  // 不需要 Key 的本地端点：没有 Key 也要照常尝试（错误来自连接而不是"要 Key"）
  const localNoKey = await fetchChatModels({ providerId: 'local', baseURL: sameUrl, apiKey: '', apiKeyRequired: false })
  check('D17 本地端点无 Key 也照常尝试（错误来自连接而不是"要 Key"）',
    localNoKey.ok === false && !/API Key/u.test(localNoKey.error), localNoKey.error)
}

// ============================================================ 3. 超时

{
  const srv = await startCatalogServer(() => ({ hang: true }))
  const controller = new AbortController()
  const started = Date.now()
  const timed = await fetchChatModels({ providerId: 'mimo', baseURL: srv.url, apiKey: 'ok', cache: new Map(), signal: controller.signal })
  srv.server.close()
  check('D18 无响应的端点会失败而不是永远挂着', timed.ok === false && Date.now() - started < 20000,
    `${Date.now() - started}ms ${timed.error}`)
}

// ============================================================ 4. 能力元数据带回

{
  const srv = await startCatalogServer(() => ({
    data: [{
      id: 'deepseek-flash', object: 'model', owned_by: 'deepseek',
      context_window: 1048576, max_output_tokens: 393216,
      input_modalities: ['text', 'image'], output_modalities: ['text'],
      effort: { supported_levels: ['low', 'high', 'max'], default_level: 'high' },
    }],
  }))
  const got = await fetchChatModels({ providerId: 'deepseek', baseURL: srv.url, apiKey: 'k', cache: new Map() })
  check('D19 带回 effort 与上下文窗口',
    got.models[0]?.effort?.supported_levels?.length === 3 && got.models[0].contextWindow === 1048576,
    JSON.stringify(got.models[0]))
  srv.server.close()
}

// ============================================================ 5. resolveApiKeyFor：凭据按提供商

{
  const srv = await startCatalogServer(() => ({ data: MIMO_DATA }))
  await store.saveConfig({ providerId: 'mimo', baseURL: srv.url, model: 'mimo-v2.6-flash', apiKey: 'sk-owned-mimo' })
  await store.saveConfig({ providerId: 'deepseek', apiKey: 'sk-owned-deep' })
  await store.saveConfig({ providerId: 'mimo' })
  const cfg = await store.loadConfig()
  check('D20a 目标=当前提供商 → 生效的那份', await store.resolveApiKeyFor('mimo', { config: cfg }) === 'sk-owned-mimo',
    await store.resolveApiKeyFor('mimo', { config: cfg }))
  check('D20b 目标=另一家 → 它自己槽位那份', await store.resolveApiKeyFor('deepseek', { config: cfg }) === 'sk-owned-deep')
  check('D20c 另一家没存过 → 空串（绝不借用当前的）',
    await store.resolveApiKeyFor('openai-compatible', { config: cfg }) === '')
  check('D20d id 带空格也能对上槽位', await store.resolveApiKeyFor(' deepseek ', { config: cfg }) === 'sk-owned-deep')
  check('D20e 空 id 视为当前提供商', await store.resolveApiKeyFor('', { config: cfg }) === 'sk-owned-mimo')

  // 串起来：界面选中"还没保存的另一家"时，取列表用的是它自己的 Key
  const picked = await store.resolveApiKeyFor('deepseek', { config: cfg })
  const fetched = await fetchChatModels({ providerId: 'deepseek', baseURL: srv.url, apiKey: picked, apiKeyRequired: true, cache: new Map() })
  check('D21 取另一家的列表用的是那一家的 Key',
    srv.requests.at(-1).headers.authorization === 'Bearer sk-owned-deep' && fetched.ok === true,
    srv.requests.at(-1).headers.authorization)
  const currentAfter = await store.loadConfig()
  check('D22 上面的过程没有改动磁盘上的提供商', currentAfter.providerId === 'mimo', currentAfter.providerId)

  // 环境变量优先只作用于"当前提供商"
  process.env.CUA_VA_API_KEY = 'sk-env-current'
  check('D23 环境变量只覆盖当前提供商', await store.resolveApiKeyFor('mimo') === 'sk-env-current')
  check('D24 环境变量不会污染另一家的槽位', await store.resolveApiKeyFor('deepseek', { config: cfg }) === 'sk-owned-deep')
  delete process.env.CUA_VA_API_KEY
  srv.server.close()
}

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(74))
console.log(JSON.stringify({ suite: 'model discovery + per-provider credentials', tree, cases: results.length, failed: failed.length, failedNames: failed.map((f) => f.name) }))
console.log(failed.length === 0 ? '判定：全部通过' : `判定：${failed.length} 项未通过`)
process.exitCode = failed.length === 0 ? 0 : 1
