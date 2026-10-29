/**
 * 判断"助手说完这句之后，还要不要继续等用户"。
 *
 * 为什么单独一次请求而不是让模型在回复里顺手标一个记号：
 *   长回复里模型顾不上做这个判断，实测标记方案**漏判**很严重（该等的时候收了）。
 *   把判断剥离出来单独问，只带最后两轮，输出一个强类型 JSON，判定质量完全不同。
 *
 * 为什么输出的是**置信度**而不是 true/false：
 *   二选一必然有一头错。业界（LiveKit / Vapi）的做法都是把概率映射成"等多久"——
 *   拿不准就多等一会儿，判错也不致命。
 */

export const FOLLOWUP_DEFAULTS = {
  /** "用户还会继续说"的置信度 → 等待窗口 */
  thresholds: { high: 0.7, low: 0.3 },
  windowsMs: { high: 8000, mid: 4000, low: 1500 },
  /** 判定请求的超时：超时就退回启发式，不能因为一个判断把收尾卡住 */
  judgeTimeoutMs: 6000,
}

/** 置信度 → 窗口时长（毫秒） */
export function windowFor(confidence, config = {}) {
  const cfg = { ...FOLLOWUP_DEFAULTS, ...config }
  const value = Number(confidence)
  if (!Number.isFinite(value)) return cfg.windowsMs.mid
  if (value >= cfg.thresholds.high) return cfg.windowsMs.high
  if (value <= cfg.thresholds.low) return cfg.windowsMs.low
  return cfg.windowsMs.mid
}

/**
 * 启发式兜底：判定请求超时/解析失败时用。
 * 只认最明显的信号，宁可少等一点也不要空等八秒。
 */
export function heuristicFollowUp(replyText) {
  const text = String(replyText ?? '').trim()
  if (text === '') return { wait: false, confidence: 0.2, why: '空回复' }
  if (/[？?]\s*$/u.test(text)) return { wait: true, confidence: 0.75, why: '结尾是问句' }
  if (/(要不要|要我|还是|你希望|需要我|可以吗|行吗|好吗|哪个|哪一|是不是)/u.test(text)) {
    return { wait: true, confidence: 0.65, why: '句中有征询' }
  }
  if (/吗[。！!]?$/u.test(text)) return { wait: true, confidence: 0.6, why: '结尾是反问' }
  return { wait: false, confidence: 0.25, why: '已是收尾语气' }
}

/** 从模型输出里抠 JSON（它经常包在 ```json 或多余解释里） */
export function parseJudgeOutput(raw) {
  const text = String(raw ?? '')
  const match = text.match(/\{[\s\S]*?\}/u)
  if (match === null) return null
  try {
    const parsed = JSON.parse(match[0])
    const confidence = Number(parsed.confidence)
    if (!Number.isFinite(confidence)) return null
    return {
      wait: parsed.wait === true || confidence >= 0.5,
      confidence: Math.min(1, Math.max(0, confidence)),
      why: typeof parsed.why === 'string' ? parsed.why.slice(0, 20) : '',
    }
  } catch {
    return null
  }
}

const JUDGE_SYSTEM = `你在判断：助手刚说完这句话之后，用户是不是还需要接着说。
只输出一行 JSON，不要解释、不要代码块：
{"wait":true,"confidence":0.0,"why":"不超过10个字"}

判断依据：
- wait=true：助手在等用户回答、补充、确认；或者话没说完、留了话口
- wait=false：助手已经把事办完并收尾；或者用户在听结果，没有再说的必要
- confidence：你对 wait 的把握，0~1。拿不准就给 0.4~0.6`

/**
 * 调一次模型做判定。失败（网络/超时/解析不了）就退回启发式。
 * @param {{chat:(options:object)=>Promise<any>, userText:string, replyText:string, timeoutMs?:number}} options
 */
export async function judgeFollowUp({ chat, userText, replyText, timeoutMs = FOLLOWUP_DEFAULTS.judgeTimeoutMs }) {
  const fallback = heuristicFollowUp(replyText)

  /*
   * 测试钩子：CUA_VA_JUDGE_FAKE="wait=1,conf=0.9,delay=1500"
   * 真实模型要 2~6 秒且不稳定，验收"晚到的判定会不会重调窗口"不能靠它碰运气。
   */
  const fake = process.env.CUA_VA_JUDGE_FAKE
  if (typeof fake === 'string' && fake !== '') {
    const wait = /wait\s*=\s*1/u.test(fake)
    const confMatch = fake.match(/conf\s*=\s*([\d.]+)/u)
    const delayMatch = fake.match(/delay\s*=\s*(\d+)/u)
    const confidence = confMatch === null ? (wait ? 0.8 : 0.2) : Number(confMatch[1])
    const delay = delayMatch === null ? 0 : Number(delayMatch[1])
    if (delay > 0) await new Promise((resolve) => setTimeout(resolve, delay))
    return { wait, confidence, why: '测试注入', source: 'test-fake' }
  }

  if (typeof chat !== 'function') return { ...fallback, source: 'heuristic' }

  const messages = [
    { role: 'system', content: JUDGE_SYSTEM },
    { role: 'user', content: `用户说：${String(userText).slice(0, 300)}\n助手答：${String(replyText).slice(0, 600)}` },
  ]

  try {
    const result = await withTimeout(
      chat({ messages, temperature: 0, maxTokens: 80, stream: false }),
      timeoutMs,
    )
    const parsed = parseJudgeOutput(result?.text ?? result?.content ?? '')
    if (parsed === null) return { ...fallback, source: 'heuristic-parse' }
    return { ...parsed, source: 'model' }
  } catch (error) {
    // 超时和出错要分开记：前者说明模型慢，后者说明模型/网络有问题，排查方向不同
    const timedOut = /judge timeout/u.test(error.message)
    return { ...fallback, source: timedOut ? 'heuristic-timeout' : 'heuristic-error', error: error.message }
  }
}

function withTimeout(promise, ms) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`judge timeout ${ms}ms`)), ms)
    promise.then((value) => { clearTimeout(timer); resolve(value) }, (error) => { clearTimeout(timer); reject(error) })
  })
}
