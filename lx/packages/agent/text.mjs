/**
 * 播报文本清洗。
 *
 * 为什么要独立一个模块：这是纯函数，但要被主进程和验收脚本同时用到。
 * 留在 main.mjs 里就只能靠"从源码里抠函数"来测——那种测试一改格式就废。
 */

/**
 * 把模型输出里"不该念出来"的东西清掉。
 *
 * 提示词已经要求只输出自然语言，但模型偶尔还是会把结构化字段
 * （{"wait":true,"confidence":0.9}）、代码块、[[继续]] 一起吐出来。
 * 那些是给程序看的，念出来就是噪音——实测用户直接在字幕里看到了。
 */
export function cleanLine(text) {
  const stripped = String(text ?? '')
    // 代码块整体删掉
    .replace(/```[\s\S]*?```/gu, '')
    // 带内部字段名的 JSON 对象
    .replace(/\{[^{}]*"(?:wait|confidence|why|reply|await_user|expect)"[^{}]*\}/gu, '')
    // 裸的 key: value 片段
    .replace(/["']?(?:wait|confidence|why|await_user)["']?\s*[:：]\s*[^,，。\s}]+[，,]?/gu, '')
    // 空的大括号
    .replace(/\{\s*\}/gu, '')
    .replace(/\[\[继续\]\]/gu, '')
    .replace(/^\s*[-*•]\s*/u, '')
    .replace(/^\s*\d+[.、)]\s*/u, '')
    .replace(/\*\*/g, '')
    .replace(/[`*_]/g, '')
    // 删掉结构化字段后常留下"好的， 马上做"这种空悬的逗号/空格
    .replace(/[，,]\s*(?=[，,。！？、])/gu, '')
    .replace(/([，,。！？、])\s+/gu, '$1')
    .replace(/\s{2,}/gu, ' ')
    .replace(/\s+([，,。！？、])/gu, '$1')
    .trim()

  // 清理后只剩标点/括号的行没有意义，直接丢掉
  const meaningful = stripped.replace(/[\s,，。！？、：:；;()（）[\]{}"'“”]/gu, '')
  return meaningful === '' ? '' : stripped
}
