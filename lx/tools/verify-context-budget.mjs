/**
 * 上下文预算验收：**证明压缩真的在 90% 触发**，而不是只检查代码里有阈值。
 *
 * 为什么必须这样测：旧实现用的是"最多 10 条消息 / 3,000 字符 / 40,000 字符 / 12 条"，
 * 这些数字与模型真实容量无关，任何"读一眼代码里的常量"的测试都会通过，
 * 而用户实际遇到的仍是"才用了 4% 上下文就开始忘事"。
 *
 * 这里的做法是：
 *  1. 用 Mock 端点跑**真实 Agent 回合**，把每一次请求体都记下来；
 *  2. 用**实际发出的 messages** 反算 token（与实现同一套估算器），
 *     证明"未达阈值时请求里的历史一条没少"、"达到阈值后确实开始丢历史"；
 *  3. 压缩后逐条校验协议完整性：tool_calls 与 tool 结果必须成对、
 *     没有孤立 tool、reasoning_content 要么整条在要么整条不在（绝不截断后继续配对）。
 *
 * 全程不联网、不用真实 Key：Mock 服务在 127.0.0.1 上，配置目录是临时目录。
 *
 * 用法：node tools/verify-context-budget.mjs
 */
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const configDir = await mkdtemp(join(tmpdir(), 'cua-context-'))
process.env.CUA_VA_CONFIG_DIR = configDir
delete process.env.CUA_VA_API_KEY
delete process.env.TAVILY_API_KEY

const { startMockServer } = await import('./mock-openai.mjs')
const { Agent } = await import('../packages/agent/loop.mjs')
const { LLMClient, profileFromConfig } = await import('../packages/agent/llm.mjs')
const context = await import('../packages/agent/context.mjs')
const store = await import('../packages/config/store.mjs')
const registry = await import('../packages/config/registry.mjs')

const results = []
function check(name, pass, detail = '') {
  results.push({ name, pass: pass === true, detail })
  const mark = pass === true ? '✓' : '✗'
  console.log(`${mark} ${name}${detail === '' ? '' : `\t${typeof detail === 'string' ? detail : JSON.stringify(detail)}`}`)
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/** 起一个 Mock，回调里能拿到每次请求体 */
async function withMock(handler, run) {
  const mock = await startMockServer({ port: 0, handler })
  try {
    return await run(mock)
  } finally {
    await sleep(20)
    mock.server.close()
  }
}

// ============================================================ 0 默认值与策略

{
  const def = registry.defaultConfig()
  check('K1 默认上下文窗口 1,000,000 tokens', def.contextWindow === 1_000_000, def.contextWindow)
  check('K2 默认压缩触发点 90%', def.contextCompressPercent === 90, def.contextCompressPercent)

  const policy = context.resolveContextPolicy({})
  check('K3 默认触发阈值 = 900,000 tokens',
    policy.contextWindow === 1_000_000 && policy.compressAtTokens === 900_000,
    `${policy.contextWindow}/${policy.compressAtTokens}`)
  check('K4 预留输出空间（输入硬上限 < 窗口）',
    policy.hardLimitTokens < policy.contextWindow && policy.hardLimitTokens > 900_000,
    `${policy.hardLimitTokens} (reserve=${policy.outputReserve})`)
  check('K5 压缩目标低于触发点（压完能继续跑几步）',
    policy.targetTokens < policy.compressAtTokens, `${policy.targetTokens} < ${policy.compressAtTokens}`)

  // 模型有更小窗口时按模型实际容量算
  const small = context.resolveContextPolicy({ contextWindow: 1_000_000, modelContextWindow: 128_000 })
  check('K6 模型窗口更小时按模型算（128K → 触发 115.2K）',
    small.contextWindow === 128_000 && small.compressAtTokens === 115_200, JSON.stringify(small))
  const userSmaller = context.resolveContextPolicy({ contextWindow: 200_000, modelContextWindow: 1_000_000 })
  check('K7 用户设了更小窗口时以用户为准（元数据不能放宽）',
    userSmaller.contextWindow === 200_000, userSmaller.contextWindow)
  const custom = context.resolveContextPolicy({ contextWindow: 100_000, compressPercent: 50 })
  check('K8 百分比可自定义（100K @ 50% → 50K 触发）',
    custom.compressAtTokens === 50_000, custom.compressAtTokens)

  // 注册表里 local 端点声明 32K（可信的明确不同容量）
  const localPolicy = store.resolveContextPolicyFor({ providerId: 'local', contextWindow: 1_000_000, contextCompressPercent: 90 })
  check('K9 本地 llama.cpp 按 32K 实际窗口算 90%',
    localPolicy.contextWindow === 32_768, localPolicy.contextWindow)
  const mimoPolicy = store.resolveContextPolicyFor({ providerId: 'mimo', contextWindow: 1_000_000, contextCompressPercent: 90 })
  check('K10 MiMo 官方端点按 1M 算', mimoPolicy.contextWindow === 1_000_000, mimoPolicy.contextWindow)
  const unknownPolicy = store.resolveContextPolicyFor({ providerId: 'openai-compatible', contextWindow: 1_000_000 })
  check('K11 未知模型默认按 1M 处理', unknownPolicy.contextWindow === 1_000_000, unknownPolicy.contextWindow)

  /*
   * 注册表里的窗口是**默认值**，不是硬上限：用户把 llama-server 起成
   * `--ctx-size 131072` 后，在设置里填真实容量，不该被示例值 32K 覆盖回去。
   */
  const localBigger = store.resolveContextPolicyFor({ providerId: 'local', contextWindow: 131_072, contextCompressPercent: 90 })
  check('K11a 用户明确配置的本地真实窗口不被注册表 32K 覆盖',
    localBigger.contextWindow === 131_072, localBigger.contextWindow)
  // 端点上自报的窗口元数据仍可收紧（那是模型自己的真实容量）
  const metaTightens = store.resolveContextPolicyFor(
    { providerId: 'local', contextWindow: 131_072, contextCompressPercent: 90 },
    { modelContextWindow: 65_536 })
  check('K11b 端点自报的更小窗口仍然用于收紧',
    metaTightens.contextWindow === 65_536, metaTightens.contextWindow)
}

// ============================================================ 1 估算器：字符数 ≠ tokens

{
  const cjk = context.estimateTextTokens('你好世界')
  const ascii = context.estimateTextTokens('hello world')
  check('K12 中文按 ~1 token/字', cjk === 4, cjk)
  check('K13 英文按 ~4 字符/token（不是 1 字符 = 1 token）',
    ascii === 3, ascii)
  const long = '这是一段比较长的中文，用来确认估算器不会把字符数直接当 token 数。'.repeat(20)
  const estimated = context.estimateTextTokens(long)
  check('K14 长文本估算远小于字符数（没把字符数当 token）',
    estimated < long.length && estimated > long.length * 0.5,
    `chars=${long.length} tokens=${estimated}`)

  // 图片按分辨率（瓦片）而不是文件大小
  const imgSmall = context.estimateImageTokens([{ type: 'image_url', image_url: { url: `data:image/png;base64,${'A'.repeat(1000)}` } }])
  const imgWithDims = context.estimateImageTokens([
    { type: 'text', text: '（这是 screenshot 刚返回的图，1280x720）' },
    { type: 'image_url', image_url: { url: `data:image/png;base64,${'A'.repeat(1000)}` } },
  ])
  check('K15 图片按分辨率计价（1280x720 → 多瓦片）',
    imgWithDims > 85 && imgWithDims > imgSmall, `small=${imgSmall} dims=${imgWithDims}`)
  check('K16 图片不会被当成几十万 token（不是按 base64 字符数算）',
    imgWithDims < 2000, imgWithDims)

  // 工具定义计入预算
  const tools = [{ type: 'function', function: { name: 'screenshot', description: '截屏', parameters: { type: 'object', properties: { x: { type: 'number' } } } } }]
  const toolTokens = context.estimateToolsTokens(tools)
  check('K17 工具定义计入 token 预算', toolTokens > 10, toolTokens)
}

// ============================================================ 2 低于 90% 不压缩（真跑回合）

/**
 * 造一段"很长但离 90% 还很远"的历史：默认 1M 窗口下，
 * 30 个用户回合 × 约 2K tokens ≈ 6 万 tokens，只有 6%。
 * 旧实现（10 条消息 / 3,000 字符）在这种历史下会砍到只剩最后两条。
 */
function makeHistory(turns, charsPerTurn) {
  const history = []
  for (let i = 1; i <= turns; i += 1) {
    history.push({ role: 'user', content: `第 ${i} 个问题：${'问'.repeat(charsPerTurn)}` })
    history.push({ role: 'assistant', content: `第 ${i} 个回答：${'答'.repeat(charsPerTurn)}` })
  }
  return history
}

{
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    return { sse: [{ choices: [{ index: 0, delta: { content: '收到' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    const agent = new Agent({ llm: client, tools: { listTools: () => [], invoke: async () => ({ text: '' }) }, maxSteps: 2 })
    const history = makeHistory(30, 2000)
    const before = context.estimateRequestTokens({ messages: history })
    await agent.run('最后再问一句', { history })
    const sent = bodies[0].messages
    // system + 30 轮历史 + 本轮 user
    const historyMessages = sent.filter((m) => m.role !== 'system')
    check('K18 远低于 90%（约 6%）时历史一条不丢',
      historyMessages.length === history.length + 1,
      `sent=${historyMessages.length} expect=${history.length + 1} tokens≈${before}`)
    check('K19 第一条历史仍是最早那一轮（没有被从头部砍掉）',
      historyMessages[0].content === history[0].content,
      String(historyMessages[0]?.content ?? '').slice(0, 20))
    check('K20 本轮 user 在最后（顺序没被打乱）',
      historyMessages.at(-1).content === '最后再问一句',
      historyMessages.at(-1)?.content)
  })
}

// 旧阈值下必然被砍的场景：超过 12 条消息、超过 40,000 字符
{
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    return { sse: [{ choices: [{ index: 0, delta: { content: 'ok' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    const agent = new Agent({ llm: client, tools: { listTools: () => [], invoke: async () => ({ text: '' }) }, maxSteps: 2 })
    // 20 轮 × 3,000 字 = 60,000 字符（旧实现的两道阈值都会触发）
    const history = makeHistory(20, 3000)
    const chars = history.reduce((sum, m) => sum + m.content.length, 0)
    const tokens = context.estimateRequestTokens({ messages: history })
    await agent.run('继续', { history })
    const sent = bodies[0].messages.filter((m) => m.role !== 'system')
    check('K21 超过旧的 3,000 字符 / 40,000 字符 / 12 条阈值也不提前压缩',
      sent.length === history.length + 1,
      `chars=${chars} tokens≈${tokens}（占 1M 的 ${(tokens / 1e6 * 100).toFixed(1)}%）sent=${sent.length}`)
  })
}

// ============================================================ 3 达到 90% 必须真的压缩

{
  const bodies = []
  let call = 0
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    call += 1
    // 每步返回一大段工具结果与思考，让上下文快速膨胀
    if (call <= 3) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { reasoning_content: `第${call}步思考。${'想'.repeat(200)}` } }] },
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: `t${call}`, function: { name: 'scroll', arguments: '{"n":1}' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '完成' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash' }),
    })
    /*
     * 用**小窗口**触发压缩：窗口 8,000 tokens、90% → 7,200 就压。
     * 这样一条 3,000 字的历史（≈3,000 tokens）× 4 轮就够撞线，
     * 又不必真的造 90 万 token 的请求（那要跑几十秒）。
     * 阈值本身来自同一个 resolveContextPolicy，所以"90%"这个比例是被真实验证的。
     */
    const policy = context.resolveContextPolicy({ contextWindow: 8_000, compressPercent: 90 })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [], invoke: async () => ({ text: '滚动结果'.repeat(300) }) },
      maxSteps: 6,
      contextPolicy: policy,
    })
    const history = makeHistory(6, 1200)
    const before = context.estimateRequestTokens({ messages: history })
    const result = await agent.run('把页面滚到底', { history })
    const firstSent = bodies[0].messages
    const lastSent = bodies.at(-1).messages
    const firstTokens = context.estimateRequestTokens({ messages: firstSent })
    const lastTokens = context.estimateRequestTokens({ messages: lastSent })

    check('K22 构造的历史确实超过了触发阈值（否则这条测试没意义）',
      before > policy.compressAtTokens,
      `history≈${before} > trigger=${policy.compressAtTokens}`)
    // 第一次请求就已经把超预算的历史压掉了（历史本身 17 条 → 请求里只剩少数几条）
    check('K23 压缩真的发生了：第一次请求的历史就比传入的短',
      firstSent.length < history.length + 2,
      `${history.length + 2} → ${firstSent.length} msgs`)
    check('K23b 压缩后回到预算内（≤ 触发阈值）',
      firstTokens <= policy.compressAtTokens,
      `${firstTokens} <= ${policy.compressAtTokens}`)
    check('K24 压缩后回到预算内（≤ 触发阈值）',
      lastTokens <= policy.compressAtTokens,
      `${lastTokens} <= ${policy.compressAtTokens}`)
    check('K25 压缩保留了当前用户目标',
      lastSent.some((m) => m.role === 'user' && m.content === '把页面滚到底'),
      JSON.stringify(lastSent.filter((m) => m.role === 'user').map((m) => String(m.content).slice(0, 12))))
    check('K26 压缩保留了 system（自定义提示词不会被压掉）',
      lastSent[0].role === 'system' && lastSent.filter((m) => m.role === 'system').length === 1,
      lastSent[0]?.role)
    check('K27 被折叠的早期目标有摘要，模型不会"忘了原本要干什么"',
      lastSent.some((m) => m.role === 'user' && typeof m.content === 'string' && m.content.includes('已折叠')),
      lastSent.filter((m) => m.role === 'user').map((m) => String(m.content).slice(0, 24)))

    // 协议完整性：整条请求序列里 tool_calls 与 tool 结果必须严格成对
    const problems = []
    for (const [index, message] of lastSent.entries()) {
      if (message.role === 'tool') {
        const declared = new Set()
        for (const prev of lastSent.slice(0, index)) {
          if (prev.role === 'assistant' && Array.isArray(prev.tool_calls)) {
            for (const c of prev.tool_calls) declared.add(String(c.id))
          }
        }
        if (!declared.has(String(message.tool_call_id))) problems.push(`孤立 tool@${index}`)
      }
      if (message.role === 'assistant' && Array.isArray(message.tool_calls)) {
        const later = lastSent.slice(index + 1)
        for (const c of message.tool_calls) {
          if (!later.some((m) => m.role === 'tool' && String(m.tool_call_id) === String(c.id))) {
            problems.push(`缺结果的 tool_calls ${c.id}@${index}`)
          }
        }
      }
    }
    check('K28 压缩后没有孤立 tool / 缺结果的 tool_calls', problems.length === 0, problems.join(','))
    // reasoning 只允许整条在或整条不在
    const truncated = lastSent.filter((m) => m.role === 'assistant' && typeof m.reasoning_content === 'string'
      && m.reasoning_content.endsWith('…'))
    check('K29 reasoning_content 绝不被截断后继续配对', truncated.length === 0, truncated.length)
    check('K30 回合正常收尾（压缩没把任务打断）', result.text.includes('完成'), result.text)
  })
}

// ============================================================ 4 接近硬上限时的处理

{
  const messages = []
  messages.push({ role: 'system', content: '系统提示词' })
  for (let i = 1; i <= 40; i += 1) {
    messages.push({ role: 'user', content: `目标 ${i}：${'做'.repeat(500)}` })
    messages.push({
      role: 'assistant',
      content: null,
      tool_calls: [{ id: `c${i}`, type: 'function', function: { name: 'scroll', arguments: '{"n":1}' } }],
      reasoning_content: `第 ${i} 步的完整思考`.repeat(40),
    })
    messages.push({ role: 'tool', tool_call_id: `c${i}`, content: '结果'.repeat(200) })
  }
  messages.push({ role: 'user', content: '最后的目标' })
  const policy = context.resolveContextPolicy({ contextWindow: 20_000, compressPercent: 90 })
  const before = context.estimateRequestTokens({ messages })
  const outcome = context.compressMessages(messages, policy, { tools: [] })
  const after = context.estimateRequestTokens({ messages })
  check('K31 超过触发点时压缩生效', outcome.compressed === true && after < before, `${before} → ${after}`)
  check('K32 压缩后不超过模型硬上限', after <= policy.hardLimitTokens, `${after} <= ${policy.hardLimitTokens}`)
  check('K33 system 仍在头部', messages[0].role === 'system', messages[0]?.role)
  check('K34 当前用户目标保留', messages.some((m) => m.role === 'user' && m.content === '最后的目标'))

  // 单轮自身就超大：只允许截短用户目标文本，绝不截断 reasoning
  const huge = [
    { role: 'system', content: 'sys' },
    { role: 'user', content: '目标'.repeat(5000) },
    { role: 'assistant', content: null, tool_calls: [{ id: 'z', type: 'function', function: { name: 'x', arguments: '{}' } }], reasoning_content: '思'.repeat(20000) },
    { role: 'tool', tool_call_id: 'z', content: '果'.repeat(20000) },
  ]
  const tiny = context.resolveContextPolicy({ contextWindow: 5_000, compressPercent: 90 })
  context.compressMessages(huge, tiny, { tools: [] })
  const reasoningKept = huge.find((m) => m.role === 'assistant')?.reasoning_content ?? ''
  check('K35 单轮超大时 reasoning 整条保留（不截断）',
    reasoningKept.length === 20000, reasoningKept.length)
  check('K36 单轮超大时工具配对仍完整（tool_calls 与 tool 同时存在）',
    huge.some((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
      === huge.some((m) => m.role === 'tool'),
    JSON.stringify(huge.map((m) => m.role)))
}

// ============================================================ 5 会话历史跨回合

{
  const history = []
  for (let i = 1; i <= 50; i += 1) {
    history.push({ role: 'user', content: `问题 ${i}：${'问'.repeat(300)}` })
    history.push({ role: 'assistant', content: `回答 ${i}：${'答'.repeat(300)}` })
  }
  const policy = context.resolveContextPolicy({ contextWindow: 1_000_000, compressPercent: 90 })
  const kept = context.compressHistory(history, policy)
  check('K37 低于 90% 的跨回合历史原样保留',
    kept.length === history.length, `${kept.length}/${history.length}`)
  check('K38 原历史未被就地修改（返回新数组）',
    history.length === 100 && history[0].content.startsWith('问题 1'),
    `${history.length}`)

  // 真超预算时按完整单元裁剪
  const bigPolicy = context.resolveContextPolicy({ contextWindow: 12_000, compressPercent: 90 })
  const trimmed = context.compressHistory(history, bigPolicy)
  // 折叠摘要是一条额外的 user 消息，所以 user 比 assistant 多一条是预期的
  const userCount = trimmed.filter((m) => m.role === 'user').length
  const assistantCount = trimmed.filter((m) => m.role === 'assistant').length
  check('K39 超预算时按完整单元裁剪（user/assistant 成对，不会只剩半条）',
    trimmed.length < history.length && trimmed.length > 0
      && (userCount === assistantCount || userCount === assistantCount + 1),
    `${history.length} → ${trimmed.length} (user=${userCount} assistant=${assistantCount})`)
  check('K40 裁剪保留的是最近的目标（不是最早的）',
    trimmed.some((m) => m.role === 'user' && String(m.content).startsWith('问题 50')),
    trimmed.filter((m) => m.role === 'user').map((m) => String(m.content).slice(0, 6)))
}

// ============================================================ 6 与 store / 设置联动

{
  await store.saveConfig({ providerId: 'mimo', contextWindow: 250_000, contextCompressPercent: 80 })
  const config = await store.loadConfig()
  check('K41 上下文窗口可保存', config.contextWindow === 250_000, config.contextWindow)
  check('K42 压缩百分比可保存', config.contextCompressPercent === 80, config.contextCompressPercent)
  const policy = store.resolveContextPolicyFor(config)
  check('K43 自定义窗口与百分比一起生效（250K @ 80% → 200K 触发）',
    policy.compressAtTokens === 200_000, policy.compressAtTokens)

  // 保存无关设置不得重置这两个值
  await store.saveConfig({ temperature: 0.5 })
  const after = await store.loadConfig()
  check('K44 保存其它设置不会重置上下文设置',
    after.contextWindow === 250_000 && after.contextCompressPercent === 80,
    `${after.contextWindow}/${after.contextCompressPercent}`)

  // 显式给非法值（0 / 空 / 非数字）→ 回默认 1M / 90%，绝不清成 0
  await store.saveConfig({ contextWindow: 0, contextCompressPercent: 0 })
  const fallback = await store.loadConfig()
  check('K45 显式非法值回退默认（不会把窗口清成 0）',
    fallback.contextWindow === 1_000_000 && fallback.contextCompressPercent === 90,
    `${fallback.contextWindow}/${fallback.contextCompressPercent}`)
  check('K45b 全新配置默认 1M / 90%（不是 0 或 undefined）',
    registry.defaultConfig().contextWindow === 1_000_000 && registry.defaultConfig().contextCompressPercent === 90,
    `${registry.defaultConfig().contextWindow}/${registry.defaultConfig().contextCompressPercent}`)
  await store.saveConfig({ contextCompressPercent: 150 })
  check('K46 百分比上限 99（不允许"永不压缩"）',
    (await store.loadConfig()).contextCompressPercent === 99,
    (await store.loadConfig()).contextCompressPercent)

  // 公开配置要带上这两个字段（设置页回显）
  await store.saveConfig({ contextWindow: 1_000_000, contextCompressPercent: 90 })
  const pub = await store.loadPublicConfig()
  check('K47 设置页能读到上下文设置',
    pub.contextWindow === 1_000_000 && pub.contextCompressPercent === 90,
    `${pub.contextWindow}/${pub.contextCompressPercent}`)
}

// ============================================================ 7 历史清理不影响协议（Agent 层）

{
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    return { sse: [{ choices: [{ index: 0, delta: { content: 'ok' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({
      baseURL: mock.url, apiKey: 'k', model: 'mimo-v2.6-flash',
      profile: profileFromConfig({ providerId: 'mimo', model: 'mimo-v2.6-flash', mimoThinking: 'enabled' }),
    })
    const agent = new Agent({ llm: client, tools: { listTools: () => [], invoke: async () => ({ text: '' }) }, maxSteps: 2 })
    // 完整单元（带配对工具结果 + reasoning）+ 残缺单元（缺结果）
    const history = [
      { role: 'user', content: '完整问' },
      { role: 'assistant', content: '完整答', tool_calls: [{ id: 'a1', type: 'function', function: { name: 'x', arguments: '{}' } }], reasoning_content: '完整的思考' },
      { role: 'tool', tool_call_id: 'a1', content: '完整结果' },
      { role: 'user', content: '残缺问' },
      { role: 'assistant', content: '残缺答', tool_calls: [{ id: 'a2', type: 'function', function: { name: 'y', arguments: '{}' } }], reasoning_content: '残缺思考' },
    ]
    await agent.run('新问题', { history })
    const sent = bodies[0].messages
    check('K48 完整单元的工具链与 reasoning 保留',
      sent.some((m) => m.role === 'assistant' && typeof m.reasoning_content === 'string' && m.reasoning_content === '完整的思考')
        && sent.some((m) => m.role === 'tool' && m.tool_call_id === 'a1'),
      JSON.stringify(sent.map((m) => m.role)))
    check('K49 残缺单元的 tool_calls 被整体剔除（不留孤立调用）',
      !sent.some((m) => m.role === 'assistant' && Array.isArray(m.tool_calls) && m.tool_calls.some((c) => c.id === 'a2')),
      JSON.stringify(sent.filter((m) => m.role === 'assistant').map((m) => m.tool_calls?.map((c) => c.id))))
  })
}

// ============================================================ 8 内部提示 vs 真实用户回合

{
  /*
   * 场景：本轮真实目标 + 之后 loop 生成的内部提示（"工具参数不完整，请重新生成"）。
   * 压缩时内部提示**不能**开启新回合，否则真实目标会被当成"更老的回合"删掉——
   * 用户看到的就是"它忘了我要干什么"。
   */
  const messages = [
    { role: 'system', content: 'sys' },
    { role: 'user', content: '旧任务 A' },
    { role: 'assistant', content: '好'.repeat(3000) },
    { role: 'user', content: '把页面滚到最底部并截图确认' },
    { role: 'assistant', content: null, tool_calls: [{ id: 'a', type: 'function', function: { name: 'scroll', arguments: '{}' } }], reasoning_content: '思'.repeat(2000) },
    { role: 'tool', tool_call_id: 'a', content: '滚好了' },
    { role: 'user', content: '（上一步的工具调用参数不完整，没有执行。请重新给出完整参数。）', internal: true },
    { role: 'assistant', content: null, tool_calls: [{ id: 'b', type: 'function', function: { name: 'scroll', arguments: '{"n":999}' } }], reasoning_content: '思'.repeat(2000) },
    { role: 'tool', tool_call_id: 'b', content: '滚好了' },
  ]
  const units = context.splitInteractionUnits(messages.filter((m) => m.role !== 'system'))
  check('K50 内部提示不开启新交互单元（只有真实用户回合算单元）',
    units.length === 2 && units[1].head.content === '把页面滚到最底部并截图确认',
    units.map((u) => String(u.head.content).slice(0, 16)))

  const policy = context.resolveContextPolicy({ contextWindow: 6_000, compressPercent: 90 })
  const outcome = context.compressMessages(messages, policy, { tools: [] })
  check('K51 压缩发生了（这条测试才有意义）', outcome.compressed === true, outcome.compressed)
  check('K52 真实用户目标没有被内部提示挤掉',
    messages.some((m) => m.role === 'user' && m.content === '把页面滚到最底部并截图确认'),
    messages.filter((m) => m.role === 'user').map((m) => String(m.content).slice(0, 18)))
  check('K53 摘要被标成内部提示（不会伪装成用户新指令）',
    messages.filter((m) => typeof m.content === 'string' && m.content.startsWith('（为控制上下文长度'))
      .every((m) => m.internal === true),
    messages.filter((m) => m.role === 'user').map((m) => m.internal))

  // 旧版本落进历史的内部提示（没有 internal 标记）：按固定措辞也要认出来
  const legacy = [
    { role: 'user', content: '真实目标' },
    { role: 'assistant', content: null, tool_calls: [{ id: 'c', type: 'function', function: { name: 'x', arguments: '{}' } }] },
    { role: 'tool', tool_call_id: 'c', content: 'ok' },
    { role: 'user', content: '（已经用满 16 步，不能再调工具了。用一句话告诉用户。）' },
  ]
  const legacyUnits = context.splitInteractionUnits(legacy)
  check('K54 旧历史里的内部提示（无标记）也按措辞识别',
    legacyUnits.length === 1 && legacyUnits[0].head.content === '真实目标',
    legacyUnits.map((u) => String(u.head.content).slice(0, 14)))

  /*
   * 真正可靠的是显式标记；措辞匹配只是升级过渡。两点都要成立：
   *  - 带标记的消息**无论措辞**都算内部提示；
   *  - 用户自己说的括号句不因为长得像就被吞掉。
   */
  check('K54a 带 internal 标记的消息无论措辞都算内部提示',
    context.isInternalUserMessage({ role: 'user', content: '这条是程序写的，措辞随便', internal: true }) === true
      && context.isInternalUserMessage({ role: 'user', content: '（换了个说法的内部提示）', internal: true }) === true,
    '')
  check('K54b 用户自己说的括号句不会被误判成内部提示',
    context.isInternalUserMessage({ role: 'user', content: '（明天）帮我把报告导出来' }) === false
      && context.isInternalUserMessage({ role: 'user', content: '记住：参数不完整时不要重试' }) === false,
    '')
}

// ============================================================ 9 压完仍超硬上限要如实报告

{
  const messages = [
    { role: 'system', content: 'S'.repeat(2000) },
    { role: 'user', content: '目标' },
    { role: 'assistant', content: null, tool_calls: [{ id: 'z', type: 'function', function: { name: 'x', arguments: '{}' } }] },
    { role: 'tool', tool_call_id: 'z', content: '果'.repeat(100000) },
  ]
  const policy = context.resolveContextPolicy({ contextWindow: 5_000, compressPercent: 90 })
  const outcome = context.compressMessages(messages, policy, { tools: [] })
  const after = context.estimateRequestTokens({ messages })
  check('K55 单条超大工具结果按预算截断（不是固定 1,200 字）',
    outcome.shrunkToolResults === 1 && messages.find((m) => m.role === 'tool').content.length > 500,
    `${messages.find((m) => m.role === 'tool').content.length} 字`)
  check('K56 截断后模型能看出这是截断内容',
    messages.find((m) => m.role === 'tool').content.includes('已按上下文预算截断'),
    messages.find((m) => m.role === 'tool').content.slice(-30))
  check('K57 截断后回到硬上限以内', after <= policy.hardLimitTokens, `${after} <= ${policy.hardLimitTokens}`)
  check('K58 工具配对仍然完整（assistant.tool_calls 与 tool 结果都在）',
    messages.some((m) => m.role === 'assistant' && Array.isArray(m.tool_calls))
      && messages.some((m) => m.role === 'tool'), messages.map((m) => m.role))

  // 真的压不下去（system 本身就把窗口撑满）→ 必须报 overLimit，而不是假装成功
  const impossible = [
    { role: 'system', content: 'S'.repeat(40000) },
    { role: 'user', content: '目标' },
  ]
  const tinyPolicy = context.resolveContextPolicy({ contextWindow: 5_000, compressPercent: 90 })
  const impossibleOutcome = context.compressMessages(impossible, tinyPolicy, { tools: [] })
  check('K59 压完仍超硬上限时明确标记 overLimit（不静默照发）',
    impossibleOutcome.overLimit === true && impossibleOutcome.overLimitTokens > tinyPolicy.hardLimitTokens,
    `${impossibleOutcome.overLimitTokens} > ${tinyPolicy.hardLimitTokens}`)
}

// ============================================================ 10 跨回合：内部提示身份不丢

{
  /*
   * 第一轮生成了内部提示，第二轮带着那段历史继续。压缩必须仍然认得出
   * "这条不是用户说的话"——否则上一轮的真实目标会在第二轮被当成旧回合删掉。
   * 这要求 `internal` 标记跨回合保留在 turn 增量里（但不进请求体）。
   */
  const bodies = []
  await withMock(({ body }) => {
    bodies.push(JSON.parse(JSON.stringify(body)))
    if (bodies.length === 1) {
      return {
        sse: [
          { choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'b1', function: { name: 'scroll', arguments: '{bad' } }] } }] },
          { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
        ],
      }
    }
    return { sse: [{ choices: [{ index: 0, delta: { content: '完成这一步' }, finish_reason: 'stop' }] }] }
  }, async (mock) => {
    const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: profileFromConfig({ providerId: 'openai-compatible', model: 'm' }) })
    const policy = context.resolveContextPolicy({ contextWindow: 20_000, compressPercent: 90 })
    const agent = new Agent({
      llm: client,
      tools: { listTools: () => [{ type: 'function', function: { name: 'scroll', parameters: {} } }], invoke: async () => ({ text: 'ok' }) },
      maxSteps: 4,
      contextPolicy: policy,
    })
    const first = await agent.run('把页面滚到最底部')
    check('K60 第一轮的内部提示带 internal 标记且随 turn 返回',
      first.turn.some((m) => m.internal === true), first.turn.map((m) => m.internal ?? m.role).join(','))
    const before = bodies.length
    await agent.run('再滚一次', { history: first.turn })
    const last = bodies.at(-1)
    check('K61 第二轮真的发了请求（这条测试才有意义）', bodies.length > before, `${bodies.length}`)
    check('K62 第二轮里上一轮的真实用户目标仍在（没被内部提示挤掉）',
      last.messages.some((m) => m.content === '把页面滚到最底部'),
      last.messages.filter((m) => m.role === 'user').map((m) => String(m.content).slice(0, 16)))
    check('K63 internal 标记没有进请求体',
      !last.messages.some((m) => 'internal' in m), '')
  })
}

// ============================================================ 11 硬上限：任何请求都不越过

{
  /*
   * 用户把压缩百分比调到 99% 时，会出现"输入已超过硬上限（窗口 - 输出预留），
   * 但还没到 99% 触发点"的区间。旧实现在这个区间直接跳过压缩与超限判断，
   * 请求带着超过模型容量的输入发出去。
   *
   * 这里用**缩小的窗口**做等比例测试，并且验证**实际发出的请求体**，
   * 而不只看压缩函数的返回值。
   */
  const policies = [90, 99].map((percent) => context.resolveContextPolicy({ contextWindow: 100_000, compressPercent: percent }))
  for (const policy of policies) {
    // 造一段落在 (hardLimit, compressAt] 的历史（只对 99% 存在这种区间）
    let history = []
    for (let n = 1; n <= 80; n += 1) {
      history = []
      for (let i = 0; i < n; i += 1) {
        history.push({ role: 'user', content: `轮${i}：${'做'.repeat(3000)}` })
        history.push({ role: 'assistant', content: `答${i}：${'答'.repeat(3000)}` })
      }
      const size = context.estimateRequestTokens({ messages: [{ role: 'system', content: 'sys' }, ...history] })
      if (size > policy.compressAtTokens) break
      if (size > policy.hardLimitTokens) break
    }
    const bodies = []
    await withMock(({ body }) => {
      bodies.push(JSON.parse(JSON.stringify(body)))
      return { sse: [{ choices: [{ index: 0, delta: { content: '好' }, finish_reason: 'stop' }] }] }
    }, async (mock) => {
      const client = new LLMClient({ baseURL: mock.url, apiKey: 'k', model: 'm', profile: profileFromConfig({ providerId: 'openai-compatible', model: 'm' }) })
      const agent = new Agent({ llm: client, tools: { listTools: () => [], invoke: async () => ({ text: '' }) }, maxSteps: 2, contextPolicy: policy })
      await agent.run('新的问题', { history })
    })
    for (const body of bodies) {
      const sent = context.estimateRequestTokens({ messages: body.messages, tools: body.tools ?? [] })
      check(`H1 ${policy.compressPercent}%：实际发出的请求不超过输入硬上限`,
        sent <= policy.hardLimitTokens, `${sent} <= ${policy.hardLimitTokens}`)
    }
  }
}

// ============================================================ 12 真实用户指令不被静默截断

{
  /*
   * 用户说的话本身就超过窗口容量时，旧实现把它截到前 400 字再照常发出，
   * 模型收到的是被改写过的任务，而且没有任何提示。
   * 现在必须"明确报告装不下"，而不是交给模型一段残缺指令。
   */
  const policy = context.resolveContextPolicy({ contextWindow: 50_000, compressPercent: 90 })
  const messages = [
    { role: 'system', content: 'S' },
    { role: 'user', content: '把'.repeat(48_000) },
  ]
  const outcome = context.compressMessages(messages, policy, { tools: [] })
  const userText = String(messages.find((m) => m.role === 'user').content)
  check('H2 用户输入没有被改写（长度原样）', userText.length === 48_000, userText.length)
  check('H3 装不下时明确标记 overLimit（不静默截断后照发）',
    outcome.overLimit === true, JSON.stringify({ overLimit: outcome.overLimit, truncated: outcome.truncated }))

  // Agent 层：这种回合不能发请求，必须如实失败
  let calls = 0
  const agent = new Agent({
    llm: { profile: { providerId: 'openai-compatible' }, chat: async () => { calls += 1; return { text: 'x', toolCalls: [], finishReason: 'stop' } } },
    tools: { listTools: () => [], invoke: async () => ({ text: '' }) },
    maxSteps: 2,
    contextPolicy: policy,
  })
  const result = await agent.run('把'.repeat(48_000))
  check('H4 Agent 不为装不下的回合发请求', calls === 0, `calls=${calls}`)
  check('H5 装不下的回合标记失败并给出原因',
    result.failed === true && result.failureReason === 'context_overflow', `${result.failed}/${result.failureReason}`)
}

// ============================================================ 13 连续压缩不丢更早的目标

{
  /*
   * 第一次压缩写下的摘要里记着最早的目标。第二次压缩如果把它当成普通 user 回合
   * 一起丢掉，"用户最开始要干什么"就永久消失（只剩最近几轮）。
   */
  const policy = context.resolveContextPolicy({ contextWindow: 3_000, compressPercent: 90 })
  const messages = [{ role: 'system', content: 'sys' }]
  for (let i = 1; i <= 6; i += 1) {
    messages.push({ role: 'user', content: `早期目标${i}：${'做'.repeat(300)}` })
    messages.push({ role: 'assistant', content: `答${i}：${'答'.repeat(300)}` })
  }
  messages.push({ role: 'user', content: '当前目标：导出报告' })
  const first = context.compressMessages(messages, policy, { tools: [] })
  check('H6 第一次压缩发生了', first.compressed === true, first.droppedUnits)

  for (let i = 1; i <= 8; i += 1) {
    messages.push({ role: 'user', content: `后续回合${i}：${'x'.repeat(500)}` })
    messages.push({ role: 'assistant', content: `回${i}：${'y'.repeat(500)}` })
  }
  messages.push({ role: 'user', content: '最新目标' })
  const second = context.compressMessages(messages, policy, { tools: [] })
  check('H7 第二次压缩真的发生了', second.compressed === true, second.droppedUnits)

  const digests = messages.filter((m) => m.internal === true)
  check('H8 只保留一份摘要（不会越压越多）', digests.length === 1, digests.length)
  check('H9 第二次压缩后，最早的目标仍在摘要里（不因分组方式丢失）',
    digests.some((m) => m.content.includes('早期目标1')), digests.map((m) => m.content.slice(0, 60)))
  check('H10 最新目标仍然作为真实用户消息保留',
    messages.some((m) => m.content === '最新目标'), '')
  check('H11 摘要没有被当成用户新指令（带 internal 标记）',
    digests.every((m) => m.internal === true && m.role === 'user'), '')
}

// ============================================================ 汇总

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(72))
console.log(JSON.stringify({ suite: 'context budget (90% of 1M)', cases: results.length, failed: failed.length, failedNames: failed.map((r) => r.name) }))
console.log(failed.length === 0 ? '判定：全部通过' : '判定：存在未通过项')
process.exit(failed.length === 0 ? 0 : 1)
