/**
 * 主程序（apps/shell/main.mjs）对"模型响应失败"的处理验收。
 *
 * 为什么单独测这一层：Agent 早已会把失败的回合标成 `failed`，但主程序从来不看——
 * 用户听到半句话之后界面就显示"好了"，下一轮还会基于半截回答去猜意图。
 * 这一层的行为（字幕、状态、要不要做追问判定、要不要报"没说完"）只有把真实的
 * runCommand 拿出来跑才看得见，静态检查证明不了。
 *
 * 做法和 verify-turn-cancellation 一样：从 main.mjs 里切出 runCommand 与
 * handleAgentEvent 的原文，在 vm 里配好最小桩件执行。
 *
 * 用法：node tools/verify-turn-failure-shell.mjs
 */
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'

const source = (await readFile(new URL('../apps/shell/main.mjs', import.meta.url), 'utf8')).replace(/\r\n/g, '\n')
const fn = (name) => {
  const at = source.indexOf(name)
  assert.ok(at >= 0, name)
  return source.slice(at, source.indexOf('\n}', at) + 2)
}

const results = []
function check(name, pass, detail = '') {
  results.push({ name, pass: pass === true, detail })
  console.log(`${pass === true ? '✓' : '✗'} ${name}${detail === '' ? '' : `\t${detail}`}`)
}

/** 从 runCommand 的事件回放里直接驱动 handleAgentEvent（模拟真实的流式事件序列） */
async function runTurnWithEvents(agentResult, events) {
  const ctx = vm.createContext({
    AbortController, Date, process, console, Promise, JSON, Number, String,
    setTimeout: () => 0, clearTimeout: () => {},
  })
  vm.runInContext(`
globalThis.__ui = []
globalThis.__speech = []
globalThis.__judged = 0
let turnSeq = 0, activeTurn = null, latestTurn = null, hideTimer = null, commandTimer = null
let commandActive = false, wantContinue = false, followUpJudgement = null, followUpJudgePromise = null
let asr = null, lastCommandText = '', lastSentPartial = '', lastUtterance = '', deniedInTurn = {}
let turnHistoryCounts = [], sessionHistory = [], userDismissedHud = false, listening = true
let agentRuntime = null, lastReplyText = '', lastAssistantText = '', followUpWindowStart = 0
let followUpConfig = { thresholds: { high: 0.7, low: 0.3 }, windowsMs: { high: 0, mid: 0, low: 0 }, judgeTimeoutMs: 1000 }
function isOfflineError() { return false }
function closeConfirmCard() {}
function stopSpeaking() {}
function hideHud() { __ui.push('hide') }
function log() {}
function sendSubtitle(x) { __ui.push(Object.assign({ kind: 'subtitle' }, x)) }
function setState(x) { __ui.push({ kind: 'state', value: x }) }
function enqueueSpeech(t) { __speech.push(t) }
function parseOfflineIntent() { return null }
function isOfflineNow() { return false }
function takeSentences(turn) {
  const parts = turn.buffer.split(/(?<=[。！？!?\\n])/)
  turn.buffer = parts.pop() ?? ''
  return parts.map((p) => p.trim()).filter(Boolean)
}
function flushTurn(turn) { const rest = turn.buffer.trim(); turn.buffer = ''; if (rest !== '') speakLine(turn, rest) }
function speakLine(turn, sentence) { __ui.push({ kind: 'speak', text: sentence }); __speech.push(sentence) }
function rememberTurn() {}
function judgeFollowUp() { __judged += 1; return Promise.resolve({ wait: false, confidence: 0.5, source: 'stub' }) }
function heuristicFollowUp() { return { wait: false, confidence: 0.5 } }
function windowFor() { return 0 }
function scheduleHide() {}
function playingAudio() { return false }
function flushPendingUiAction() {}
let ttsPumping = false, ttsQueue = [], speechBuffer = '', continuousMode = true
let currentState = 'hidden', confirmResolver = null, heardAny = false, commandStartAt = 0
${fn('async function afterTurnSpeechSettled()')}
${fn('async function startFollowUpIfWanted()')}
${fn('async function resolveFollowUpJudgement()')}
async function refreshSearchKey() {}
async function syncAgentEndpointIfChanged() {}
async function ensureAgent() {
  return {
    agent: {
      run: async (_text, options) => {
        // 按真实顺序回放事件，然后交回最终结果
        for (const event of globalThis.__events) options.onEvent(event)
        return globalThis.__agentResult
      },
    },
  }
}
${fn('function cancelActiveTurn(reason)')}
${fn('function isCurrentTurn(turn)')}
${fn('function handleAgentEvent(event, turn = activeTurn)')}
${fn('async function runCommand(text)')}
`, ctx)
  vm.runInContext('globalThis.__agentResult = ' + JSON.stringify(agentResult), ctx)
  vm.runInContext('globalThis.__events = ' + JSON.stringify(events), ctx)
  await vm.runInContext("runCommand('问一下')", ctx)
  /*
   * 播报结束后的收尾必经之路：runCommand 的 finally 会调它（ttsQueue 空时也会由
   * pumpSpeech 调）。这里显式跑一次**真实实现**，才能验证"失败提示播完之后
   * 会不会又被改成「好了」/开待命窗口"——把 afterTurnSpeechSettled 换成空函数
   * 恰好会把这个问题藏起来。
   */
  await vm.runInContext('afterTurnSpeechSettled()', ctx)
  const ui = JSON.parse(vm.runInContext('JSON.stringify(__ui)', ctx))
  return {
    ui,
    speech: JSON.parse(vm.runInContext('JSON.stringify(__speech)', ctx)),
    judged: vm.runInContext('__judged', ctx),
    states: ui.filter((e) => e.kind === 'state').map((e) => e.value),
    hints: ui.filter((e) => e.kind === 'subtitle' && typeof e.hint === 'string').map((e) => e.hint),
    finals: ui.filter((e) => e.kind === 'subtitle' && typeof e.final === 'string').map((e) => e.final),
  }
}

// ============================================================ 1 说到一半失败

{
  const outcome = await runTurnWithEvents(
    { text: '我看看这个', steps: [], turn: [], failed: true, failureReason: 'stream_error', llm: null },
    [
      { type: 'text_delta', text: '我看看这个。' },
      { type: 'stream_error', text: '我看看这个', finishReason: 'incomplete' },
      { type: 'done', text: '我看看这个', failed: true },
    ],
  )
  check('P1 失败回合落到 error 状态（不显示"好了"）',
    outcome.states.includes('error') && !outcome.states.includes('done'), outcome.states.join(','))
  check('P2 已经说出口的话照常播报（不吞掉、不重播）',
    outcome.speech.some((line) => line.includes('我看看这个')), JSON.stringify(outcome.speech))
  check('P3 明确告诉用户回答没说完', outcome.hints.some((h) => h.includes('没有完整完成'))
    || outcome.speech.some((l) => l.includes('没有说完')), JSON.stringify({ hints: outcome.hints, speech: outcome.speech }))
  check('P4 失败回合不做追问判定（不基于半截回答猜意图）', outcome.judged === 0, outcome.judged)
  /*
   * 失败提示播完之后（afterTurnSpeechSettled 走真实实现），不能再冒出
   * "好了" + 待命窗口——那等于把失败当成正常收尾，而用户刚被告知没说完。
   */
  check('P4b 失败提示播完后不会改回「好了」（不误报正常完成）',
    !outcome.states.includes('done'), outcome.states.join(','))
  check('P4c 失败提示播完后不开待命窗口（不会再问"想继续说"）',
    !outcome.hints.some((h) => h.includes('想继续说')), JSON.stringify(outcome.hints))
}

// ============================================================ 2 一个字都没出来

{
  const outcome = await runTurnWithEvents(
    { text: '（模型响应异常，未能完成回答）', steps: [], turn: [], failed: true, failureReason: 'stream_error', llm: null },
    [{ type: 'stream_error', text: '', finishReason: 'incomplete' }, { type: 'done', text: '', failed: true }],
  )
  check('P5 没有任何输出时给出明确的失败说明（不是静默收场）',
    outcome.finals.some((t) => t.includes('没有正常回答')) || outcome.speech.some((t) => t.includes('没有正常回答')),
    JSON.stringify({ finals: outcome.finals, speech: outcome.speech }))
  check('P6 同样落到 error 状态', outcome.states.includes('error'), outcome.states.join(','))
  check('P7 同样不做追问判定', outcome.judged === 0, outcome.judged)
}

// ============================================================ 3 正常完成不受影响

{
  const outcome = await runTurnWithEvents(
    { text: '好的，都办完了。', steps: [], turn: [], failed: false, llm: null },
    [
      { type: 'text_delta', text: '好的，都办完了。' },
      { type: 'assistant', text: '好的，都办完了。', step: 1 },
      { type: 'done', text: '好的，都办完了。', failed: false },
    ],
  )
  check('P8 正常完成仍是 speaking 状态（没有误报失败）',
    outcome.states.includes('speaking') && !outcome.states.includes('error'), outcome.states.join(','))
  check('P9 正常完成仍然做追问判定（连续对话体验不变）', outcome.judged === 1, outcome.judged)
  check('P10 正常完成不追加"没说完"之类的话',
    !outcome.speech.some((l) => l.includes('没有说完')), JSON.stringify(outcome.speech))
}

// ============================================================ 3b 失败不会沿用上一轮回复做判定

{
  /*
   * 上一轮正常回答里带了个问句（"要我给你打开吗？"），如果失败轮还拿 lastReplyText
   * 去做启发式，就会得出"应该继续等"的结论——用户刚被告知"没说完"，
   * 紧接着又看到一个"想继续说就直接说"的待命窗口。
   */
  const outcome = await runTurnWithEvents(
    { text: '我看看这个', steps: [], turn: [], failed: true, failureReason: 'stream_error', llm: null },
    [
      { type: 'text_delta', text: '我看看这个。' },
      { type: 'stream_error', text: '我看看这个', finishReason: 'incomplete' },
      { type: 'done', text: '我看看这个', failed: true },
    ],
  )
  check('P13 失败轮不会基于上一轮的问句开待命窗口',
    !outcome.hints.some((h) => h.includes('想继续说')), JSON.stringify(outcome.hints))
}

// ============================================================ 4 用户主动打断

{
  /*
   * 打断走 AbortController：Agent.run 抛 AbortError，runCommand 应当直接返回，
   * 既不改状态成 error，也不报"模型故障"。
   */
  const ctx = vm.createContext({
    AbortController, Date, process, console, Promise, JSON, Number, String,
    setTimeout: () => 0, clearTimeout: () => {},
  })
  vm.runInContext(`
globalThis.__ui = []
let turnSeq = 0, activeTurn = null, latestTurn = null, hideTimer = null, commandTimer = null
let commandActive = false, wantContinue = false, followUpJudgement = null, followUpJudgePromise = null
let asr = null, lastCommandText = '', lastSentPartial = '', lastUtterance = '', deniedInTurn = {}
let turnHistoryCounts = [], sessionHistory = [], userDismissedHud = false, listening = false
let agentRuntime = null, lastReplyText = '', lastAssistantText = '', followUpWindowStart = 0
let followUpConfig = { thresholds: {}, windowsMs: {}, judgeTimeoutMs: 1000 }
function isOfflineError() { return false }
function closeConfirmCard() {}
function stopSpeaking() {}
function hideHud() { __ui.push({ kind: 'hide' }) }
function log() {}
function sendSubtitle(x) { __ui.push(Object.assign({ kind: 'subtitle' }, x)) }
function setState(x) { __ui.push({ kind: 'state', value: x }) }
function enqueueSpeech() {}
function parseOfflineIntent() { return null }
function isOfflineNow() { return false }
function takeSentences() { return [] }
function flushTurn() {}
function speakLine() {}
function rememberTurn() {}
function judgeFollowUp() { return Promise.resolve({ wait: false, confidence: 0.5 }) }
function heuristicFollowUp() { return { wait: false, confidence: 0.5 } }
function windowFor() { return 0 }
function afterTurnSpeechSettled() {}
function scheduleHide() {}
async function refreshSearchKey() {}
async function syncAgentEndpointIfChanged() {}
async function ensureAgent() {
  return { agent: { run: async () => { const e = new Error('已中断'); e.name = 'AbortError'; throw e } } }
}
${fn('function cancelActiveTurn(reason)')}
${fn('function isCurrentTurn(turn)')}
${fn('function handleAgentEvent(event, turn = activeTurn)')}
${fn('async function runCommand(text)')}
`, ctx)
  await vm.runInContext("runCommand('问一下')", ctx)
  const ui = JSON.parse(vm.runInContext('JSON.stringify(__ui)', ctx))
  const states = ui.filter((e) => e.kind === 'state').map((e) => e.value)
  check('P11 用户打断不把状态改成 error（不误报模型故障）',
    !states.includes('error'), states.join(','))
  check('P12 用户打断不报"没有完整完成"',
    !ui.some((e) => e.kind === 'subtitle' && String(e.hint ?? e.final ?? '').includes('没有完整完成')),
    JSON.stringify(ui.filter((e) => e.kind === 'subtitle')))
}

// ============================================================ 汇总

const failed = results.filter((r) => !r.pass)
console.log('='.repeat(72))
console.log(JSON.stringify({ suite: 'shell turn failure feedback', cases: results.length, failed: failed.length, failedNames: failed.map((r) => r.name) }))
console.log(failed.length === 0 ? '判定：全部通过' : '判定：存在未通过项')
process.exit(failed.length === 0 ? 0 : 1)
