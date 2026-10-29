/**
 * 打断检测（barge-in）——**按关键词**，不按音量。
 *
 * 为什么不用能量检测：第一版是"比底噪高 13dB 且持续 300ms 就算打断"，
 * 结果是宿舍里咳嗽一声、室友笑一下都能让助手闭嘴。用户的原话是
 * 「要不然有点声就被打断了」——这就说明能量法在开放环境里不成立。
 *
 * 为什么用 ASR 而不是 KWS 找关键词：
 *   实测把「等一下」喂给项目里的 KWS 模型（wenetspeech 3.3M），
 *   8 种音素写法 × 12 组阈值全部 0 命中——这个 KWS 对"训练集外的短词"能力很弱。
 *   而同一个 ASR 模型对同一段音频给出的结果就是「等一下」，一字不差。
 *   所以这里用 ASR 的 partial 文本做匹配，代价是延迟高一点，换来真的能听见。
 *
 * 两条闸门同时满足才算打断：
 *   1. 电平确实高于底噪（有人在说话，不是识别器对静音的幻觉）
 *   2. 识别文本里出现了约定的词（等一下 / 停一下 / 别说了 …）
 */

/** 约定的打断词。改这里即可扩词，注意别加太短的（"停"容易误命中）。 */
export const BARGE_WORDS = ['等一下', '等一等', '等会儿', '停一下', '别说了', '先停下']

/** 电平闸门：比底噪高多少 dB 才认为"确实有人在说话" */
export const BARGE_LEVEL_DB = 8

/** 去掉标点与空格，避免"等 一下。""等一下，"这类识别结果漏匹配 */
function normalize(text) {
  return String(text ?? '').replace(/[\s，。！？、；：,.!?;:]/gu, '')
}

/**
 * 在识别文本里找打断词。
 * @param {string} text ASR 的 partial/final
 * @param {string[]} words 约定词表
 * @returns {string|null} 命中的词，没有则 null
 */
export function matchBargeWord(text, words = BARGE_WORDS) {
  const plain = normalize(text)
  if (plain === '') return null
  for (const word of words) {
    if (plain.includes(normalize(word))) return word
  }
  return null
}

/**
 * 从识别文本里删掉打断词。
 *
 * 打断之后那段音频还在同一句话里——用户说「等一下」的尾巴会继续被识别，
 * 于是下一轮的"指令"变成「等一下等一下」（实测）。不清理就会拿它去问模型，
 * 白白多跑一轮，还会让用户看到莫名其妙的自言自语。
 */
export function stripBargeWords(text, words = BARGE_WORDS) {
  let out = String(text ?? '')
  for (const word of words) {
    out = out.split(word).join('')
  }
  return out.replace(/[\s，。！？、；：,.!?;:]/gu, '').trim()
}

/**
 * 组合判定：电平够 + 文本命中。
 *
 * @param {{text:string, levelDb:number, floorDb:number, words?:string[], minLevelDb?:number}} sample
 * @returns {{hit:boolean, word:string|null, reason:string}}
 */
export function judgeBargeIn({ text, levelDb, floorDb, words = BARGE_WORDS, minLevelDb = BARGE_LEVEL_DB }) {
  const word = matchBargeWord(text, words)
  if (word === null) return { hit: false, word: null, reason: 'no-word' }
  if (levelDb - floorDb < minLevelDb) {
    // 文本里有词但声音太小：多半是识别器在噪声上的幻觉，宁可不打断
    return { hit: false, word, reason: 'too-quiet' }
  }
  return { hit: true, word, reason: 'ok' }
}
