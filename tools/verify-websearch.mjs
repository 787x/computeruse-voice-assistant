/**
 * 联网搜索验收（本地假接口，不打真实网络、不花额度）。
 *
 * 验四件事：
 *   1. 请求体对不对（Tavily 的字段名写错了不会报错，只会静默搜不到）
 *   2. 返回文本给模型看够不够用（要点在前、来源在后、摘要截断）
 *   3. 同查询 5 分钟内只打一次接口（缓存）
 *   4. 没有 key / 接口报错 时的失败姿势（要能被模型读懂）
 *
 * 用法：node tools/verify-websearch.mjs
 */
import { createServer } from 'node:http'

import { WebSearch, formatForModel, WEB_SEARCH_TOOL } from '../packages/tools/web-search.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

// —— 假 Tavily
let hits = 0
let lastBody = null
const server = createServer((req, res) => {
  let raw = ''
  req.on('data', (chunk) => { raw += chunk })
  req.on('end', () => {
    hits += 1
    try { lastBody = JSON.parse(raw) } catch { lastBody = null }
    if (req.url === '/unauthorized') {
      res.writeHead(401, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ detail: 'invalid api key' }))
      return
    }
    if (req.url === '/broken') {
      res.writeHead(500, { 'content-type': 'text/plain' })
      res.end('upstream exploded')
      return
    }
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(JSON.stringify({
      answer: '北京今天多云转晴，18~27 度。',
      results: [
        { title: '北京天气', url: 'https://example.com/weather', content: '今天多云转晴，最高 27 度，最低 18 度，风力 2 级。'.repeat(30) },
        { title: '穿衣建议', url: 'https://example.com/tips', content: '早晚偏凉，建议带一件外套。' },
      ],
    }))
  })
})

await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
const { port } = server.address()
const endpoint = `http://127.0.0.1:${port}/search`

// 1. 正常搜索
const search = new WebSearch({ apiKey: 'test-key', endpoint, cacheMs: 60000 })
const first = await search.invoke({ query: '北京今天天气' })
check('请求体：字段名正确', lastBody?.api_key === 'test-key'
  && lastBody?.query === '北京今天天气'
  && lastBody?.include_answer === true
  && lastBody?.search_depth === 'basic', JSON.stringify(Object.keys(lastBody ?? {})))
check('返回文本：要点在前', first.text.includes('要点：北京今天多云转晴'), first.text.split('\n')[1] ?? '')
check('返回文本：带来源与链接', first.text.includes('1. 北京天气 — https://example.com/weather'), '')
check('返回文本：摘要被截断', /…$/mu.test(first.text) && first.text.length < 900, `${first.text.length} 字`)
check('meta：非缓存首次调用', first.meta.cached === false && first.meta.count === 2, JSON.stringify(first.meta))

// 2. 缓存：同查询第二次不再打接口
const before = hits
const second = await search.invoke({ query: '北京今天天气' })
check('缓存：第二次命中且不打接口', second.meta.cached === true && hits === before && second.text === first.text, `hits=${hits}`)

// 3. 没有 key：工具应当"不存在"
const noKey = new WebSearch({ apiKey: '', endpoint })
let disabledError = ''
try { await noKey.invoke({ query: 'x' }) } catch (error) { disabledError = error.message }
check('无 key：enabled=false 且调用报错', noKey.enabled === false && /没有配置/u.test(disabledError), disabledError)

// 4. 接口报错：错误信息要能读懂
const bad = new WebSearch({ apiKey: 'test-key', endpoint: `http://127.0.0.1:${port}/unauthorized` })
let authError = ''
try { await bad.invoke({ query: 'x' }) } catch (error) { authError = error.message }
check('401：错误信息带状态码', /401/u.test(authError), authError)

const broken = new WebSearch({ apiKey: 'test-key', endpoint: `http://127.0.0.1:${port}/broken` })
let brokenError = ''
try { await broken.invoke({ query: 'x' }) } catch (error) { brokenError = error.message }
check('500：错误信息带状态码', /500/u.test(brokenError), brokenError)

// 5. 空结果与格式边界
const empty = formatForModel({ query: '一个搜不到的词', results: [] })
check('空结果：明确说没搜到', empty.includes('（没有搜到结果）'), empty.split('\n').at(-1))

// 6. 工具 schema 是 OpenAI function 形状
check('工具 schema 形状正确', WEB_SEARCH_TOOL.type === 'function'
  && WEB_SEARCH_TOOL.function.name === 'web_search'
  && WEB_SEARCH_TOOL.function.parameters.required.includes('query'), WEB_SEARCH_TOOL.function.name)

server.close()

console.log('联网搜索验收（本地假接口）\n' + '='.repeat(66))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(30, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((r) => !r.pass)
console.log('='.repeat(66))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
