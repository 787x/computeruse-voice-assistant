/**
 * 配置界面（R9）：本地网页，配置模型提供商与模型。
 *
 * 为什么先做网页而不是 Electron：
 *   P0/P1 阶段还没有壳子，先用一个独立小服务把"配置数据层 + 校验 + 测试连接"
 *   跑通并可视化；P2 建 Electron 后，这套 HTTP API 原样复用，只换承载容器。
 *
 * 启动：node tools/config-ui.mjs  → http://127.0.0.1:18777
 */
import { createServer } from 'node:http'
import { readFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { loadPublicConfig, loadConfig, saveConfig, resolveApiKeyFor, resolveSearchKey, listProviders, CONFIG_FILE } from '../packages/config/store.mjs'
import { getProvider } from '../packages/config/registry.mjs'
import { LLMClient, profileFromConfig } from '../packages/agent/llm.mjs'
import { WebSearch } from '../packages/tools/web-search.mjs'

const HERE = dirname(fileURLToPath(import.meta.url))
const PORT = Number(process.env.CONFIG_UI_PORT ?? 18777)
const JSON_TYPE = 'application/json; charset=utf-8'

const server = createServer(async (req, res) => {
  try {
    const url = new URL(req.url ?? '/', `http://127.0.0.1:${PORT}`)

    if (req.method === 'GET' && (url.pathname === '/' || url.pathname === '/index.html')) {
      const html = await readFile(join(HERE, 'config-ui.html'), 'utf8')
      return send(res, 200, 'text/html; charset=utf-8', html)
    }

    if (req.method === 'GET' && url.pathname === '/api/state') {
      const config = await loadPublicConfig()
      return send(res, 200, JSON_TYPE, JSON.stringify({
        config,
        providers: listProviders(),
        configFile: CONFIG_FILE,
      }))
    }

    if (req.method === 'POST' && url.pathname === '/api/config') {
      const body = await readJson(req)
      try {
        const saved = await saveConfig(body)
        return send(res, 200, JSON_TYPE, JSON.stringify({ ok: true, config: saved }))
      } catch (error) {
        return send(res, 400, JSON_TYPE, JSON.stringify({ ok: false, error: error.message }))
      }
    }

    if (req.method === 'POST' && url.pathname === '/api/test') {
      const body = await readJson(req)
      const stored = await loadPublicConfig()
      const targetProviderId = body.providerId ?? stored.providerId
      // 选了别家就用别家自己那份 Key：拿当前凭据去打陌生端点只会得到假的 401
      const apiKey = typeof body.apiKey === 'string' && body.apiKey !== '' && !body.apiKey.includes('*')
        ? body.apiKey
        : await resolveApiKeyFor(targetProviderId, { config: await loadConfig() })
      const baseURL = body.baseURL || getProvider(targetProviderId)?.baseURL || stored.baseURL
      const model = body.model || stored.model

      const provider = getProvider(targetProviderId)
      if (provider?.apiKeyRequired === true && !apiKey) {
        return send(res, 200, JSON_TYPE, JSON.stringify({ ok: false, error: '该提供商需要 API Key，但当前未配置' }))
      }

      /*
       * 和实际请求走同一套提供商档案：测试路径多发一个该提供商不支持的参数，
       * 就会出现"测着是红的、其实能用"的假失败。
       */
      const client = new LLMClient({
        baseURL, apiKey, model, timeoutMs: 20000,
        profile: profileFromConfig({ ...stored, providerId: targetProviderId, baseURL, model }),
      })
      const checks = {}
      try {
        const listed = await client.listModels()
        // listModels 返回的是整个账号目录（含 ASR/TTS）；这里只留能聊天的，和设置页一致
        checks.models = listed.models.filter((item) => item.chat).map((item) => item.id)
        checks.modelsExcluded = listed.models.filter((item) => !item.chat).map((item) => item.id)
      } catch (error) {
        checks.modelsError = error.message
      }
      try {
        const started = Date.now()
        const reply = await client.chat({ messages: [{ role: 'user', content: '回复 ok 两个字母即可' }], stream: false })
        checks.chat = { ok: true, ms: Date.now() - started, text: reply.text.slice(0, 80) }
      } catch (error) {
        checks.chat = { ok: false, error: error.message }
      }
      const ok = checks.chat?.ok === true
      return send(res, 200, JSON_TYPE, JSON.stringify({ ok, checks }))
    }

    if (req.method === 'POST' && url.pathname === '/api/test-search') {
      const body = await readJson(req)
      const key = typeof body.apiKey === 'string' && body.apiKey !== '' && !body.apiKey.includes('*')
        ? body.apiKey
        : await resolveSearchKey()
      if (!key) {
        return send(res, 200, JSON_TYPE, JSON.stringify({ ok: false, error: '还没有配置搜索 Key' }))
      }
      const search = new WebSearch({ apiKey: key })
      try {
        const started = Date.now()
        const result = await search.invoke({ query: String(body.query ?? '上海今天天气') })
        return send(res, 200, JSON_TYPE, JSON.stringify({
          ok: true,
          ms: Date.now() - started,
          count: result.meta.count,
          text: result.text.slice(0, 800),
        }))
      } catch (error) {
        return send(res, 200, JSON_TYPE, JSON.stringify({ ok: false, error: error.message }))
      }
    }

    send(res, 404, JSON_TYPE, JSON.stringify({ error: 'not found' }))
  } catch (error) {
    send(res, 500, JSON_TYPE, JSON.stringify({ error: error.message }))
  }
})

server.listen(PORT, '127.0.0.1', () => {
  console.log(`配置界面: http://127.0.0.1:${PORT}`)
  console.log(`配置文件: ${CONFIG_FILE}`)
})

function send(res, status, contentType, body) {
  res.writeHead(status, { 'content-type': contentType, 'cache-control': 'no-store' })
  res.end(body)
}

function readJson(req) {
  return new Promise((resolve, reject) => {
    let raw = ''
    req.on('data', (chunk) => { raw += chunk })
    req.on('end', () => {
      try { resolve(JSON.parse(raw || '{}')) } catch (error) { reject(error) }
    })
    req.on('error', reject)
  })
}
