/**
 * 连续对话判定验收。
 *
 * 三件事必须对：
 *   1. 置信度 → 等待时长 的映射（判错也不该是硬伤）
 *   2. 判定请求失败时能退回启发式，而不是把收尾卡住
 *   3. 模型输出里夹带解释/代码块时还能把 JSON 抠出来
 *
 * 用法：node tools/verify-followup.mjs
 */
import { windowFor, heuristicFollowUp, parseJudgeOutput, judgeFollowUp, FOLLOWUP_DEFAULTS } from '../packages/agent/followup.mjs'

const results = []
function check(name, pass, detail) {
  results.push({ name, pass, detail })
}

// ---- 置信度 → 窗口
check('高置信 → 8 秒', windowFor(0.9) === 8000, `${windowFor(0.9)}ms`)
check('中置信 → 4 秒', windowFor(0.5) === 4000, `${windowFor(0.5)}ms`)
check('低置信 → 1.5 秒', windowFor(0.05) === 1500, `${windowFor(0.05)}ms`)
check('边界 0.7 → 8 秒', windowFor(0.7) === 8000, `${windowFor(0.7)}ms`)
check('边界 0.3 → 1.5 秒', windowFor(0.3) === 1500, `${windowFor(0.3)}ms`)
check('非法值 → 中间档', windowFor(Number.NaN) === 4000, `${windowFor(Number.NaN)}ms`)
check('阈值可配', windowFor(0.6, { thresholds: { high: 0.5, low: 0.2 } }) === 8000, '')

// ---- 启发式兜底
check('问句 → 等', heuristicFollowUp('你要哪一个？').wait === true, '')
check('征询 → 等', heuristicFollowUp('要我帮你打开吗').wait === true, '')
check('收尾 → 不等', heuristicFollowUp('已经关掉了。').wait === false, '')
check('空回复 → 不等', heuristicFollowUp('').wait === false, '')

// ---- JSON 解析（模型爱加解释和代码块）
check('裸 JSON', parseJudgeOutput('{"wait":true,"confidence":0.8,"why":"在问"}')?.confidence === 0.8, '')
check('代码块里的 JSON', parseJudgeOutput('```json\n{"wait":false,"confidence":0.2}\n```')?.wait === false, '')
check('带解释的 JSON', parseJudgeOutput('我觉得：{"wait":true,"confidence":0.62} 就这样')?.confidence === 0.62, '')
check('缺 confidence → 解析失败', parseJudgeOutput('{"wait":true}') === null, '')
check('不是 JSON → 解析失败', parseJudgeOutput('好的，我觉得需要继续') === null, '')

// ---- 判定调用：成功 / 超时 / 抛错
{
  const ok = await judgeFollowUp({
    chat: async () => ({ text: '{"wait":true,"confidence":0.8,"why":"在等确认"}' }),
    userText: '打开记事本',
    replyText: '要到 C 盘还是 D 盘的那个？',
  })
  check('模型判定被采纳', ok.source === 'model' && ok.wait === true && ok.confidence === 0.8, `${ok.source}/${ok.confidence}`)

  const slow = await judgeFollowUp({
    chat: () => new Promise((resolve) => setTimeout(() => resolve({ text: '{"wait":true,"confidence":0.9}' }), 3000)),
    userText: '随便',
    replyText: '好的，已经弄完了。',
    timeoutMs: 300,
  })
  check('超时退回启发式', slow.source.startsWith('heuristic'), `${slow.source}/${slow.wait}`)

  const broke = await judgeFollowUp({
    chat: async () => { throw new Error('fetch failed') },
    userText: 'x',
    replyText: '需要我继续吗？',
  })
  check('报错退回启发式', broke.source === 'heuristic-error' && broke.wait === true, `${broke.source}/${broke.wait}`)

  const junk = await judgeFollowUp({
    chat: async () => ({ text: '这个嘛…我再想想' }),
    userText: 'x',
    replyText: '都弄好了。',
  })
  check('解析不了退回启发式', junk.source === 'heuristic-parse', junk.source)
}

check('默认配置完整', typeof FOLLOWUP_DEFAULTS.judgeTimeoutMs === 'number' && FOLLOWUP_DEFAULTS.windowsMs.high > FOLLOWUP_DEFAULTS.windowsMs.low, '')

console.log('连续对话判定验收\n' + '='.repeat(66))
for (const row of results) {
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(24, ' ')} ${row.detail ?? ''}`)
}
const failed = results.filter((row) => !row.pass)
console.log('='.repeat(66))
console.log(JSON.stringify({ cases: results.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
