/**
 * 在线模型列表获取（`GET /v1/models`）——设置页的"获取模型列表"和自动刷新都走这里。
 *
 * 为什么单独一个模块：这段逻辑里同时有缓存、凭据选择、失败回落三个容易写错的地方，
 * 长在 Electron 处理器里就只能靠点界面来验，抽出来才能直接对着真实 HTTP 行为测。
 *
 * 三条硬规矩（都是用户会实际碰到的）：
 *  1. **凭据按提供商取**：界面上可以先选中一家还没保存的提供商。这时绝不能拿当前生效的
 *     Key 去打它——那是另一家的凭据（凭据越境，而且必定 401）。
 *  2. **失败不改变用户的选择**：网络失败 / 401 / 429 / 空目录都只回错误说明，
 *     并把上一次成功的列表带上（`stale:true`），界面照样能保存、模型名照样能手填。
 *  3. **缓存不存 Key**：只缓存"哪些模型存在"，TTL 短到上新模型半天看不见都不可能。
 */
import { LLMClient } from '../agent/llm.mjs'

/** 默认缓存时长：够界面来回切提供商不重复打 API（MiMo 有 RPM 限制），又不至于看不见新模型 */
export const MODEL_LIST_TTL_MS = 60_000

/** 缓存键：提供商 + 地址。不同地址（比如 Token Plan 与按量付费）本来就是两套目录 */
export function modelCacheKey(providerId, baseURL) {
  return `${providerId}|${baseURL}`
}

/**
 * @param {object} options
 * @param {string} options.providerId 目标提供商（可以是还没保存的那个选择）
 * @param {string} options.baseURL 端点地址
 * @param {string} options.apiKey 已经按提供商规则挑好的 Key（空串=没有）
 * @param {boolean} options.apiKeyRequired 该提供商是否必须有 Key
 * @param {string} options.fallbackModel 给 LLMClient 占位用的模型名（/models 不看它）
 * @param {Map} [options.cache] 缓存（`modelCacheKey` → {at, models, dropped}）
 * @param {number} [options.ttlMs]
 * @param {boolean} [options.force] 跳过缓存（用户点了"获取模型列表"或改了 Key）
 * @param {AbortSignal} [options.signal]
 * @returns {Promise<{ok:boolean, models:object[], dropped?:object[], cached:boolean, stale:boolean, at?:number, error?:string}>}
 */
export async function fetchChatModels({
  providerId,
  baseURL,
  apiKey,
  apiKeyRequired = true,
  fallbackModel = 'models',
  cache,
  ttlMs = MODEL_LIST_TTL_MS,
  force = false,
  signal,
} = {}) {
  const key = modelCacheKey(providerId ?? '', baseURL ?? '')
  const cached = cache?.get(key)
  const staleResult = (error, extra = {}) => ({
    ok: false,
    error,
    // 失败时保留上一次结果：界面因此仍能显示候选、仍可保存，不会把用户的模型选择清空
    models: cached?.models ?? [],
    cached: cached !== undefined,
    stale: cached !== undefined,
    ...extra,
  })

  if (apiKeyRequired && !apiKey) {
    return staleResult('需要 API Key（填好再获取列表）')
  }

  // 缓存只对"同一认证身份 + 同地址"的常规刷新生效。用户手填 Key / 切换账号后，
  // 即使没标 force，也不能复用旧账号的目录。指纹绝不存明文 Key。
  const keyFingerprint = fingerprint(apiKey)
  if (!force && cached !== undefined && Date.now() - cached.at < ttlMs
      && cached.keyFingerprint === keyFingerprint
      && String(cached.providerId ?? '') === String(providerId ?? '')) {
    return { ok: true, models: cached.models, dropped: cached.dropped ?? [], cached: true, stale: false, at: cached.at }
  }

  let parsed
  try {
    const client = new LLMClient({ baseURL, apiKey, model: fallbackModel, timeoutMs: 15000 })
    parsed = await client.listModels({ signal })
  } catch (error) {
    return staleResult(error.message ?? String(error))
  }

  const chat = parsed.models.filter((model) => model.chat)
  const dropped = parsed.models.filter((model) => !model.chat)
    .map((model) => ({ id: model.id, reason: model.reason }))

  if (chat.length === 0) {
    // 空目录要如实报错：不能让界面把候选列表清空
    return staleResult(parsed.raw === 0 ? '该端点没有返回任何模型' : '返回的模型都不是聊天模型（只有 ASR/TTS 等）', { dropped })
  }

  const entry = {
    at: Date.now(),
    dropped,
    providerId: String(providerId ?? ''),
    keyFingerprint,
    models: chat.map((model) => ({
      id: model.id,
      effort: model.effort ?? null,
      contextWindow: model.contextWindow ?? null,
      maxOutputTokens: model.maxOutputTokens ?? null,
    })),
  }
  cache?.set(key, entry)
  return { ok: true, models: entry.models, dropped, cached: false, stale: false, at: entry.at }
}

/**
 * API Key 的短指纹（SHA-256 前 24 位十六进制）。
 * 只用于判断"当前请求的凭据与缓存条目是不是同一个账号"，不落盘、不打印、不传出去。
 */
function fingerprint(apiKey) {
  const value = String(apiKey ?? '')
  if (value === '') return ''
  let hash = 0
  for (let i = 0; i < value.length; i += 1) {
    hash = ((hash << 5) - hash + value.charCodeAt(i)) >>> 0
  }
  return `${value.length}:${hash.toString(16)}`
}
