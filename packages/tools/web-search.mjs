/**
 * 联网搜索（默认 Tavily）。
 *
 * 为什么需要它：走"打开浏览器→地址栏打字→截图看结果"查一次实时信息要 10~20 秒，
 * 而且页面一改就废。查天气、查股价、查新闻这类问题，一次 API 调用 1~2 秒就够了。
 *
 * Tavily 是给 AI 用的搜索接口：它自己会抓正文并给一段 `answer`，
 * 所以返回给模型的东西可以很短——语音助手不需要十条链接，需要一句能念的话。
 */
import { createHash } from 'node:crypto'

export const SEARCH_DEFAULTS = {
  endpoint: 'https://api.tavily.com/search',
  maxResults: 5,
  /** 单条摘要截断长度：语音场景下模型用不到整篇文章 */
  snippetChars: 320,
  timeoutMs: 8000,
  /** 同一句话连续追问时别反复打接口（5 分钟内同查询命中缓存） */
  cacheMs: 5 * 60 * 1000,
  searchDepth: 'basic',
}

/** 给模型看的工具描述：写清楚"什么时候该用"，比写清楚参数更重要 */
export const WEB_SEARCH_TOOL = {
  type: 'function',
  function: {
    name: 'web_search',
    description: '联网搜索实时信息（天气、新闻、价格、今天发生的事、你不确定的事实）。'
      + '比打开浏览器快得多，查这类内容优先用它。一次一个问题，不要拿它查本地文件或屏幕内容。',
    parameters: {
      type: 'object',
      properties: {
        query: { type: 'string', description: '搜索词，用最关键的几个词，不要写整句话' },
        max_results: { type: 'number', description: '要几条结果，默认 5，最多 8' },
      },
      required: ['query'],
      additionalProperties: false,
    },
  },
}

export class WebSearch {
  #apiKey
  #options
  #cache = new Map()
  #fetchImpl

  constructor({ apiKey = '', fetchImpl = globalThis.fetch, ...options } = {}) {
    this.#apiKey = apiKey
    this.#options = { ...SEARCH_DEFAULTS, ...options }
    this.#fetchImpl = fetchImpl
  }

  /** 没有 key 就当作"这个工具不存在"，而不是让模型调用后再失败 */
  get enabled() { return typeof this.#apiKey === 'string' && this.#apiKey.trim() !== '' }

  /**
   * 运行期换 key（设置页保存后不用重启）。
   * 顺手清缓存：换了 key 还拿旧结果，排查起来会很困惑。
   */
  setApiKey(key) {
    const next = typeof key === 'string' ? key.trim() : ''
    if (next === this.#apiKey) return false
    this.#apiKey = next
    this.#cache.clear()
    return true
  }

  get options() { return this.#options }

  /**
   * 搜一次。
   * @param {{query?:string, max_results?:number}} args
   * @returns {Promise<{text:string, meta:{ms:number, cached:boolean, count:number}, data:object}>}
   */
  async invoke(args = {}) {
    const query = String(args.query ?? '').trim()
    if (query === '') throw new Error('web_search 需要 query')
    if (!this.enabled) throw new Error('没有配置搜索 API Key')

    const maxResults = Math.min(8, Math.max(1, Number(args.max_results ?? this.#options.maxResults) || this.#options.maxResults))
    const key = createHash('sha1').update(`${query}|${maxResults}`).digest('hex')
    const cached = this.#cache.get(key)
    if (cached !== undefined && Date.now() - cached.at < this.#options.cacheMs) {
      return { text: cached.text, meta: { ms: 0, cached: true, count: cached.count }, data: cached.data }
    }

    const started = Date.now()
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), this.#options.timeoutMs)
    let response
    try {
      response = await this.#fetchImpl(this.#options.endpoint, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          api_key: this.#apiKey,
          query,
          search_depth: this.#options.searchDepth,
          include_answer: true,
          max_results: maxResults,
        }),
        signal: controller.signal,
      })
    } catch (error) {
      clearTimeout(timer)
      if (error?.name === 'AbortError') throw new Error(`搜索超时（${this.#options.timeoutMs}ms）`)
      throw new Error(`搜索请求失败：${error.message}`)
    } finally {
      clearTimeout(timer)
    }

    if (!response.ok) {
      const body = await response.text().catch(() => '')
      throw new Error(`搜索接口 ${response.status}：${body.slice(0, 160)}`)
    }

    const payload = await response.json()
    const results = Array.isArray(payload.results) ? payload.results : []
    const text = formatForModel({ query, answer: payload.answer, results, snippetChars: this.#options.snippetChars })
    const ms = Date.now() - started

    this.#cache.set(key, { at: Date.now(), text, count: results.length, data: payload })
    if (this.#cache.size > 32) this.#cache.delete(this.#cache.keys().next().value)

    return { text, meta: { ms, cached: false, count: results.length }, data: payload }
  }
}

/**
 * 把接口结果压成模型能直接用的文本。
 *
 * 故意不返回 JSON：模型读表格化文本比读 JSON 稳，而且长链接把它拖进幻觉的概率更低。
 * 有 answer 就把它放最前面——语音场景要的就是这一句。
 */
export function formatForModel({ query, answer, results = [], snippetChars = 320 }) {
  const lines = [`查询：${query}`]
  const brief = typeof answer === 'string' ? answer.trim() : ''
  if (brief !== '') lines.push(`要点：${brief}`)
  if (results.length === 0) {
    lines.push('（没有搜到结果）')
    return lines.join('\n')
  }
  lines.push('来源：')
  results.slice(0, 8).forEach((item, index) => {
    const title = String(item?.title ?? '').trim() || '(无标题)'
    const url = String(item?.url ?? '').trim()
    const content = String(item?.content ?? '').replace(/\s+/gu, ' ').trim()
    const snippet = content.length > snippetChars ? `${content.slice(0, snippetChars)}…` : content
    lines.push(`${index + 1}. ${title}${url === '' ? '' : ` — ${url}`}`)
    if (snippet !== '') lines.push(`   ${snippet}`)
  })
  return lines.join('\n')
}
