//! 语音打断：按**词语 + 有声判断**判定，不按音量。
//!
//! 旧版实现里已经踩过的坑（`packages/voice/bargein.mjs` 的注释）：
//!  - 只用能量阈值 → 宿舍里咳嗽一声、室友笑一下都能让助手闭嘴；
//!  - 用 PR6 的 Zipformer KWS 找「等一下」→ 8 种音素写法 × 12 组阈值全部 0 命中
//!    （这个 KWS 对训练集外的短词能力很弱），而 ASR 对同一段音频一字不差。
//!
//! 所以这里的判定顺序是：
//!  1. **必须有真实识别结果**（`Final`，不是电平），文本里出现约定词；
//!  2. 这句语音必须真的"有声"（VAD 的有声时长门槛，由协调器负责）；
//!  3. **回声防护**：如果识别到的内容正是我们自己正在播的那句话的一部分，
//!     判定为本机回声，不当成打断。
//!
//! 第 3 条是必须的：助手播报「如果说等一下，我就停下来」时，麦克风会把
//! 「等一下」原样收回来。没有这一层，朗读含有打断词的回复就会自我打断。
//!
//! 明确不做的事：不承诺任意音响设备上的完美全双工 AEC。扬声器外放时
//! 本机声音仍可能被识别成别的词，这里只保证「不因为自己朗读的打断词而打断」。

/// 约定的打断词。
///
/// 刻意不加单字（「停」「别」）：实测这类词在连续语音里误命中率很高。
pub const BARGE_WORDS: &[&str] = &[
    "等一下",
    "等一等",
    "等会儿",
    "先停下",
    "停一下",
    "别说了",
    "不要说了",
    "先别说话",
];

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BargeVerdict {
    pub hit: bool,
    /// 命中的词（没有则为 `None`）。
    pub word: Option<String>,
    pub reason: BargeReason,
}

/// 判定依据（给日志与测试用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BargeReason {
    /// 命中且不是回声。
    Matched,
    /// 文本里没有打断词。
    NoWord,
    /// 命中了，但这正是我们自己在播的内容（回声）。
    Echo,
}

/// 去掉标点与空格，避免「等 一下。」「等一下，」这类识别结果漏匹配。
pub fn normalize(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '，' | '。'
                        | '！'
                        | '？'
                        | '、'
                        | '；'
                        | '：'
                        | ','
                        | '.'
                        | '!'
                        | '?'
                        | ';'
                        | ':'
                        | '"'
                        | '\''
                        | '“'
                        | '”'
                        | '‘'
                        | '’'
                        | '…'
                        | '·'
                )
        })
        .collect()
}

/// 在识别文本里找打断词。返回命中的词。
pub fn match_barge_word(text: &str) -> Option<String> {
    match_barge_word_in(text, BARGE_WORDS)
}

/// 指定词表的版本（测试可以注入自定义词表）。
pub fn match_barge_word_in(text: &str, words: &[&str]) -> Option<String> {
    let plain = normalize(text);
    if plain.is_empty() {
        return None;
    }
    for word in words {
        let needle = normalize(word);
        if !needle.is_empty() && plain.contains(&needle) {
            return Some((*word).to_string());
        }
    }
    None
}

/// 从识别文本里删掉打断词，返回剩下的内容。
///
/// 打断之后那段音频还会继续被识别（用户说完"等一下"往往还会接着说），
/// 剩下的内容这里只用于判断"是不是还说了别的事"，**不会**自动当成新指令。
pub fn strip_barge_words(text: &str) -> String {
    let mut out = text.to_string();
    for word in BARGE_WORDS {
        out = out.replace(word, "");
    }
    normalize(&out)
}

/// 是不是本机播报的回声。
///
/// 判据很保守：识别到的内容被正在/刚播过的某一句包含（或反过来包含），
/// 就认为是我们自己的声音。代价是"我们自己读到某句含打断词的话时，
/// 用户同时说同一句"可能被漏判——这比"朗读时自我打断"安全得多。
pub fn is_probably_echo(heard: &str, spoken_recent: &[String]) -> bool {
    let heard = normalize(heard);
    if heard.is_empty() {
        return false;
    }
    for spoken in spoken_recent {
        let spoken = normalize(spoken);
        if spoken.is_empty() {
            continue;
        }
        if spoken.contains(&heard) || heard.contains(&spoken) {
            return true;
        }
    }
    false
}

/// 综合判定：文本命中 + 非回声。
///
/// 有声判断（VAD 的有声时长）由协调器在此之前把关：这一句必须先通过
/// 端点检测才会被送去识别，本身就已经是"有人说了话"。
pub fn judge(heard: &str, spoken_recent: &[String]) -> BargeVerdict {
    let Some(word) = match_barge_word(heard) else {
        return BargeVerdict {
            hit: false,
            word: None,
            reason: BargeReason::NoWord,
        };
    };
    if is_probably_echo(heard, spoken_recent) {
        return BargeVerdict {
            hit: false,
            word: Some(word),
            reason: BargeReason::Echo,
        };
    }
    BargeVerdict {
        hit: true,
        word: Some(word),
        reason: BargeReason::Matched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_common_phrases_with_punctuation() {
        for text in [
            "等一下",
            "等一下。",
            "等 一下，你先别说了",
            "停一下！",
            "别说了",
            "等会儿再说",
        ] {
            assert!(
                match_barge_word(text).is_some(),
                "{text:?} 应当被识别为打断"
            );
        }
    }

    #[test]
    fn normal_sentences_do_not_trigger() {
        for text in ["打开浏览器", "现在几点了", "谢谢", ""] {
            assert!(match_barge_word(text).is_none(), "{text:?} 不应当触发打断");
        }
    }

    /// **关键**：助手自己朗读出「等一下」时不能自我打断。
    #[test]
    fn own_speech_is_not_treated_as_barge_in() {
        let spoken = vec!["如果说等一下，我就会停下来".to_string()];
        let verdict = judge("等一下", &spoken);
        assert!(!verdict.hit, "本机回声不能触发打断");
        assert_eq!(verdict.reason, BargeReason::Echo);
        assert_eq!(verdict.word.as_deref(), Some("等一下"));

        // 识别结果只是播报内容的一部分时，同样按回声处理。
        let verdict = judge("如果说等一下", &spoken);
        assert!(!verdict.hit);
        assert_eq!(verdict.reason, BargeReason::Echo);
    }

    #[test]
    fn user_phrase_still_triggers_while_other_text_is_spoken() {
        let spoken = vec!["我来看看当前窗口列表".to_string()];
        let verdict = judge("等一下", &spoken);
        assert!(verdict.hit);
        assert_eq!(verdict.reason, BargeReason::Matched);
        assert_eq!(verdict.word.as_deref(), Some("等一下"));
    }

    #[test]
    fn empty_spoken_history_never_suppresses() {
        let verdict = judge("停一下", &[]);
        assert!(verdict.hit);
    }

    #[test]
    fn stripping_removes_the_word_but_keeps_the_rest() {
        assert_eq!(strip_barge_words("等一下，帮我查天气"), "帮我查天气");
        assert_eq!(strip_barge_words("等一下"), "");
    }
}
