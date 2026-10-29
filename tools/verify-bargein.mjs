/**
 * 打断判定的验收（纯逻辑，不需要麦克风/扬声器）。
 *
 * 这一层要钉死的是"**什么时候不该打断**"：
 * 用户的原话是「要不然有点声就被打断了」——所以下面的反例比正例更重要。
 *
 * 用法：node tools/verify-bargein.mjs
 */
import { judgeBargeIn, matchBargeWord, BARGE_WORDS } from '../packages/voice/bargein.mjs'

const FLOOR = -56 // 本机底噪实测中位数
const LOUD = FLOOR + 22

const cases = [
  // —— 正例：说了约定词，且音量正常
  { name: '等一下（正常音量）', text: '等一下', levelDb: LOUD, expect: true },
  { name: '等一下（带标点）', text: '等一下，先别说了', levelDb: LOUD, expect: true },
  { name: '等一 下（识别出空格）', text: '等一 下', levelDb: LOUD, expect: true },
  { name: '停一下', text: '停一下', levelDb: LOUD, expect: true },
  { name: '别说了', text: '别说了', levelDb: LOUD, expect: true },
  { name: '等会儿吧', text: '等会儿吧', levelDb: LOUD, expect: true },
  { name: '句中包含（我等一下再弄）', text: '我等一下再弄', levelDb: LOUD, expect: true },

  // —— 反例：这就是用户抱怨的场景，一个字都不能打断
  { name: '咳嗽/嗯（无词）', text: '嗯', levelDb: LOUD, expect: false },
  { name: '旁人聊天（无词）', text: '今天天气不错，我们出去走走', levelDb: LOUD, expect: false },
  { name: '笑声', text: '哈哈', levelDb: FLOOR + 30, expect: false },
  { name: '有词但声音太小（识别幻觉）', text: '等一下', levelDb: FLOOR + 3, expect: false },
  { name: '空文本', text: '', levelDb: LOUD, expect: false },
  { name: '只有标点', text: '，。', levelDb: LOUD, expect: false },
  { name: '近似词（等一等车）不算吗', text: '等一等车', levelDb: LOUD, expect: true, note: '子串命中是可接受的宽化' },
  { name: '无关指令（打开浏览器）', text: '帮我把浏览器打开', levelDb: LOUD, expect: false },
]

const rows = cases.map((item) => {
  const verdict = judgeBargeIn({ text: item.text, levelDb: item.levelDb, floorDb: FLOOR })
  return { ...item, pass: verdict.hit === item.expect, verdict }
})

// 词表本身的自检：太短的词会把日常对话全打断
const tooShort = BARGE_WORDS.filter((word) => [...word].length < 3)
rows.push({
  name: '词表里没有过短的词（<3 字）',
  pass: tooShort.length === 0,
  verdict: { hit: tooShort.length === 0, word: null, reason: tooShort.join(',') },
})

// matchBargeWord 单独验一遍归一化
const normOk = matchBargeWord('好，等 一下。', BARGE_WORDS) === '等一下'
rows.push({ name: 'matchBargeWord 去标点/空格', pass: normOk, verdict: { hit: normOk, word: null, reason: '' } })

console.log('打断判定验收\n' + '='.repeat(70))
for (const row of rows) {
  const detail = row.verdict.hit ? `命中「${row.verdict.word ?? '?'}」` : (row.verdict.reason ?? '')
  console.log(`${row.pass ? '✓' : '✗'} ${row.name.padEnd(34, ' ')} 期望 ${row.expect === undefined ? '-' : (row.expect ? '打断' : '不打断')} → ${detail}`)
}
const failed = rows.filter((r) => !r.pass)
console.log('='.repeat(70))
console.log(JSON.stringify({ cases: rows.length, failed: failed.length, verdict: failed.length === 0 ? 'pass' : 'fail' }))
if (failed.length > 0) process.exitCode = 1
