//! 语音确认：把一句识别文本变成「允许 / 拒绝 / 不明确」，并绑定到具体的确认请求。
//!
//! 这是权限门的一部分，取向必须保守（任务书）：
//!  - **否定优先**：`不可以`、`不要`、`不同意`、`别执行` 一律拒绝。
//!    中文里「不可以」包含子串「可以」，先判否定才不会被读成同意；
//!  - 只有**明确的肯定**才允许：`可以`、`允许`、`同意` 等，而且句子里不能
//!    还有别的实义内容（`可以吗`、`可以的话再说` 都不算同意）；
//!  - 含糊、混合、过长、带疑问语气 → 不明确：保持等待（用户还能点击或重说）。
//!
//! 身份规则（防止一条迟到的识别结果批准别的请求）：
//!  - 检查文本之前先核对「这一句绑定的是哪个 `request_id`」与「现在挂起的是哪个」；
//!  - 两者不一致、或已经没有挂起请求（重复答复 / 已超时 / 已关闭）→ 直接忽略。

/// 一句识别文本的意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmIntent {
    Allow,
    Deny,
    /// 含糊/混合/过长/疑问：保持等待，不当作任何答复。
    Unclear,
}

impl ConfirmIntent {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Allow => "允许",
            Self::Deny => "拒绝",
            Self::Unclear => "不明确",
        }
    }
}

/// 否定词。**先判这一组**：只要出现就拒绝，绝不被后面的肯定词覆盖。
const DENY_PHRASES: &[&str] = &[
    "不可以",
    "不能",
    "不行",
    "不要",
    "不用",
    "不同意",
    "不允许",
    "不批准",
    "不好",
    "不需要",
    "别执行",
    "别做",
    "别动",
    "别点",
    "别继续",
    "别打开",
    "别删",
    "别动它",
    "拒绝",
    "取消",
    "算了",
    "停止",
    "停下",
    "no",
    "nope",
    "dont",
    "don't",
    "not",
];

/// 肯定词。
const ALLOW_PHRASES: &[&str] = &[
    "可以",
    "允许",
    "同意",
    "批准",
    "确认",
    "执行",
    "没问题",
    "好的",
    "好",
    "行",
    "是的",
    "是",
    "对",
    "ok",
    "okay",
    "yes",
];

/// 语气词/口头语：去掉之后不影响语义。
const FILLERS: &[&str] = &[
    "嗯", "啊", "哦", "呀", "吧", "呢", "请", "就", "现在", "马上", "你", "我", "它", "这个",
    "那个", "操作", "一个", "了", "的", "一下", "直接", "觉得", "应该", "继续", "去", "来",
];

/// 疑问/条件语气：出现就不算同意。
const QUESTION_MARKERS: &[&str] = &["吗", "么", "?", "？", "是否", "要不要", "能不能", "会不会"];

/// 把否定词包在里面的疑问格式：先判成"不明确"，不要读成拒绝或同意。
const AMBIGUOUS_QUESTION_FORMS: &[&str] = &["能不能", "可不可以", "会不会", "要不要", "是不是"];

/// 称呼开头：这是在叫助手，不是在回答。
const GREETING_PREFIXES: &[&str] = &["你好", "您好", "喂", "嘿", "助手"];

fn normalize(text: &str) -> String {
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
                )
        })
        .collect::<String>()
        .to_lowercase()
}

/// 一句识别文本的意图判定（确定性、可单测）。
pub fn interpret(text: &str) -> ConfirmIntent {
    let plain = normalize(text);
    if plain.is_empty() {
        return ConfirmIntent::Unclear;
    }
    if GREETING_PREFIXES
        .iter()
        .any(|prefix| plain.starts_with(prefix))
    {
        return ConfirmIntent::Unclear;
    }
    // 1) `能不能`/`可不可以`/`要不要` 这类疑问句把否定词包在词里，
    //    先判成不明确：既不能读成同意，也不该读成"用户拒绝了"。
    if AMBIGUOUS_QUESTION_FORMS
        .iter()
        .any(|form| plain.contains(form))
    {
        return ConfirmIntent::Unclear;
    }
    // 2) 否定优先：`不可以` 里的 `可以` 不得变成同意。
    if DENY_PHRASES.iter().any(|phrase| plain.contains(phrase)) {
        return ConfirmIntent::Deny;
    }
    // 2) 疑问/条件语气不算同意（`可以吗`、`能不能`）。
    if QUESTION_MARKERS.iter().any(|marker| plain.contains(marker)) {
        return ConfirmIntent::Unclear;
    }
    // 3) 必须出现肯定词，且去掉肯定词与语气词之后没有实义残留。
    let mut rest = plain.clone();
    let mut saw_allow = false;
    let mut changed = true;
    while changed {
        changed = false;
        for phrase in ALLOW_PHRASES {
            if rest.contains(phrase) {
                saw_allow = true;
                rest = rest.replace(phrase, "");
                changed = true;
            }
        }
        for filler in FILLERS {
            if rest.contains(filler) {
                rest = rest.replace(filler, "");
                changed = true;
            }
        }
    }
    if !saw_allow {
        return ConfirmIntent::Unclear;
    }
    if rest.is_empty() {
        ConfirmIntent::Allow
    } else {
        // 还有别的实义内容：可能是在谈条件，不当作同意。
        ConfirmIntent::Unclear
    }
}

/// 语音答复的处置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// 结算这一次请求：允许。
    Allow(u64),
    /// 结算这一次请求：拒绝。
    Deny(u64),
    /// 不结算（忽略），附给用户看的原因。
    Ignore(String),
}

/// 把一句识别结果绑定到挂起的确认请求上。
///
/// `pending`：当前挂起的 `request_id`（`None` = 没有挂起请求）。
/// `bound`：这句语音**开始录的时候**绑定的 `request_id`（0 = 不是确认答复）。
///
/// 三条不批准"别的请求"的规则：
///  - `bound == 0`：这一句不是确认答复（打断词/普通指令），完全不参与；
///  - 没有挂起请求：重复答复、已超时、已点按钮关闭——一律忽略；
///  - `bound != pending`：这一句属于**另一个**（已结束的）请求，忽略。
pub fn resolve(pending: Option<u64>, bound: u64, text: &str) -> ConfirmDecision {
    if bound == 0 {
        return ConfirmDecision::Ignore("这一句不是确认答复".to_string());
    }
    let Some(pending) = pending else {
        return ConfirmDecision::Ignore("确认请求已经结束".to_string());
    };
    if bound != pending {
        return ConfirmDecision::Ignore("这一句属于已结束的确认请求".to_string());
    }
    match interpret(text) {
        ConfirmIntent::Allow => ConfirmDecision::Allow(pending),
        ConfirmIntent::Deny => ConfirmDecision::Deny(pending),
        ConfirmIntent::Unclear => {
            ConfirmDecision::Ignore("没听清是允许还是拒绝，可以再说一次或点按钮".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **红线**：`不可以` 绝不能被当成 `可以`。
    #[test]
    fn negations_never_become_consent() {
        for text in [
            "不可以",
            "不可以。",
            "不要",
            "不同意",
            "别执行",
            "不允许",
            "不能这样",
            "不行",
            "我不同意",
            "不要执行这个操作",
            "算了",
            "拒绝",
            "不可以吧",
            "可以不要吗",
        ] {
            assert_eq!(
                interpret(text),
                ConfirmIntent::Deny,
                "{text:?} 必须被判为拒绝"
            );
        }
    }

    #[test]
    fn explicit_consent_is_allowed() {
        for text in [
            "可以",
            "可以。",
            "允许",
            "同意",
            "我同意",
            "嗯，可以",
            "好的",
            "可以执行",
            "批准",
            "确认",
            "ok",
            "yes",
        ] {
            assert_eq!(
                interpret(text),
                ConfirmIntent::Allow,
                "{text:?} 应当被判为允许"
            );
        }
    }

    /// 含糊、混合、疑问、过长都不是同意——必须保持等待。
    #[test]
    fn ambiguous_input_keeps_waiting() {
        for text in [
            "",
            "嗯",
            "啊这",
            "可以吗",
            "能不能",
            "你好",
            "你好，可以吗",
            "可以的话我再看看别的窗口然后再说",
            "帮我打开浏览器",
            "这个操作有点危险吧",
        ] {
            assert_eq!(
                interpret(text),
                ConfirmIntent::Unclear,
                "{text:?} 不应当被当成明确答复"
            );
        }
    }

    /// 绑定规则：过期 / 重复 / 别的请求的答复都不能批准当前请求。
    #[test]
    fn decisions_are_bound_to_the_pending_request_id() {
        // 正常：id 对得上，结算这一次。
        assert_eq!(resolve(Some(5), 5, "可以"), ConfirmDecision::Allow(5));
        assert_eq!(resolve(Some(5), 5, "不可以"), ConfirmDecision::Deny(5));
        // 重复答复：第一次结算后挂起就没了，第二次必须被忽略。
        assert!(matches!(
            resolve(None, 5, "可以"),
            ConfirmDecision::Ignore(_)
        ));
        // 旧请求的迟到答复不能批准新请求。
        assert!(matches!(
            resolve(Some(6), 5, "可以"),
            ConfirmDecision::Ignore(_)
        ));
        // 不是确认答复的语音（打断词/普通指令）完全不参与。
        assert!(matches!(
            resolve(Some(6), 0, "可以"),
            ConfirmDecision::Ignore(_)
        ));
        // 含糊文本不结算（保持等待点击/重说）。
        assert!(matches!(
            resolve(Some(6), 6, "嗯"),
            ConfirmDecision::Ignore(_)
        ));
    }

    #[test]
    fn intent_labels_are_readable() {
        assert_eq!(ConfirmIntent::Allow.label(), "允许");
        assert_eq!(ConfirmIntent::Deny.label(), "拒绝");
        assert_eq!(ConfirmIntent::Unclear.label(), "不明确");
    }
}
