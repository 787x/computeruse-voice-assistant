//! 语音合成（PR7）：统一的 TTS 接口 + 两个正式后端。
//!
//! 模块边界：
//!  - [`settings`]：落盘配置（向后兼容、默认不联网）
//!  - [`mimo`]：MiMo-V2.5-TTS（标准预置音色，流式 PCM16/24k）
//!  - [`local`]：本地 ONNX（sherpa-onnx `OfflineTts` / Kokoro 中英多语言）
//!
//! 这一层**不碰播放**：它只产出带身份与格式的音频块（[`SpeechChunk`]），
//! 由 [`crate::voice::playback`] 转成设备格式并输出。这样"合成"与"出声"
//! 的生命周期可以分别测试——"HTTP 结束"不等于"声音播完"。
//!
//! 身份规则：每个音频块都带 `speech_id`（一次播报）与 `turn`（Agent 回合）。
//! 新回合/新播报一开始就换号，旧号迟到的音频在网络层被丢弃，不会串到新回合。

pub mod local;
pub mod mimo;
pub mod settings;

use crate::cancel::CancelToken;
use crate::secret::SecretStore;

pub use settings::{TtsBackend, TtsSettings};

/// 一次播报的身份（单调递增）。
pub type SpeechId = u64;
/// Agent 回合身份（由工作线程分配，用于把音频与回合对上）。
pub type TurnId = u64;

/// 播报文本的长度上限。
///
/// 超长回复整篇朗读会让人等上好几分钟，而且缓冲与内存压力都变大；
/// 这里**明确截断并告知**，不假装读完了。
pub const MAX_SPEECH_CHARS: usize = 400;
/// 单句长度上限（过长会让"首音延迟"变得不可控）。
pub const MAX_SENTENCE_CHARS: usize = 120;

/// 少于这个字数的碎片不单独成句（并进下一句），避免"好。"这种一声一顿。
pub const MIN_OWN_SENTENCE_CHARS: usize = 3;

/// 音频样本格式。当前只有 PCM16LE 一种（MiMo 官方与本地模型都输出 f32/PCM16）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Pcm16Le,
}

impl AudioFormat {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pcm16Le => "pcm16",
        }
    }
}

/// 一个带完整格式描述与身份的音频块。
///
/// `samples` 已经由后端解码成**单声道 f32**（`[-1, 1]`），但仍然带上
/// `sample_rate` / `channels` / `format`：播放层据此决定要不要重采样，
/// 测试也据此断言"我们按 24k 单声道解释 PCM16"。
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechChunk {
    pub speech_id: SpeechId,
    pub turn: TurnId,
    /// 第几句（从 0 开始，同一个 `speech_id` 内递增）。
    pub sentence: usize,
    /// 源采样率（MiMo 固定 24k；本地模型由模型决定）。
    pub sample_rate: u32,
    /// 源声道数（两个后端都是单声道）。
    pub channels: u16,
    pub format: AudioFormat,
    pub samples: Vec<f32>,
}

/// 一次合成请求（一句）。
#[derive(Debug, Clone)]
pub struct SynthRequest {
    pub speech_id: SpeechId,
    pub turn: TurnId,
    pub sentence: usize,
    pub text: String,
    /// 简短风格指令（只有 MiMo 用；空 = 不发送）。
    pub style: String,
    /// 取消令牌：置位后后端应尽快停止（网络请求/推理都停下）。
    pub cancel: CancelToken,
}

/// 合成过程中后端发给编排层的事件。
#[derive(Debug, Clone)]
pub enum TtsEvent {
    /// 一段有效音频（可以立刻进播放缓冲）。
    Chunk(SpeechChunk),
    /// 这一句合成完成（不代表声音已经播完）。
    Finished,
    /// 这一句失败（**只影响播报**，不影响 Agent 回合与工具权限）。
    Failed(String),
}

/// 后端返回的音频流。
pub struct SpeechStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<TtsEvent>,
}

impl SpeechStream {
    pub fn new(rx: tokio::sync::mpsc::UnboundedReceiver<TtsEvent>) -> Self {
        Self { rx }
    }

    pub async fn recv(&mut self) -> Option<TtsEvent> {
        self.rx.recv().await
    }

    /// 供测试：同步取一条（非阻塞）。
    pub fn try_recv(&mut self) -> Option<TtsEvent> {
        self.rx.try_recv().ok()
    }
}

/// 合成后端。
///
/// 实现必须做到：
///  - 第一批有效音频**立刻**交付（不能等整段合成结束）；
///  - 取消后尽快停止并结束 `SpeechStream`（不阻塞调用方）；
///  - 错误信息里**不出现** Key、原始 Authorization 或整段音频。
pub trait TtsEngine: Send + Sync {
    /// 给界面看的后端描述（不含密钥）。
    fn label(&self) -> String;

    /// 启动一句合成。
    fn start(&self, request: SynthRequest) -> Result<SpeechStream, String>;

    /// 预热（后台调用，失败只记日志）。
    ///
    /// 本地模型 + CUDA 的**首次**合成要付 cuDNN/显存的初始化代价（实测 9 秒），
    /// 预热把这段时间挪到"用户还在配置界面"的时候，第一次播报才不用干等。
    /// 云端后端不需要预热（会白花额度），保持默认空实现。
    fn warm_up(&self) {}
}

/// 一段待播报文本的切分结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechPlan {
    pub sentences: Vec<String>,
    /// 因为超过 [`MAX_SPEECH_CHARS`] 被丢弃的字数（0 = 完整播报）。
    pub dropped_chars: usize,
}

impl SpeechPlan {
    pub fn is_empty(&self) -> bool {
        self.sentences.is_empty()
    }
}

/// 给播报用的文本规整：去掉 Markdown 标记与链接，只保留可朗读的正文。
///
/// 刻意**不做**的事：不改写数字/单位（中英混读需要原样交给模型），
/// 不删正文里的英文（Kokoro 中英混合模型就是为这种句子准备的）。
pub fn normalize_for_speech(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // 链接 `[文字](地址)`：只读文字。
            '[' => {
                let mut label = String::new();
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == ']' {
                        closed = true;
                        break;
                    }
                    label.push(inner);
                }
                if closed && chars.peek() == Some(&'(') {
                    // 丢掉括号里的地址。注意要先把左括号吃掉，否则下面的
                    // 层级判断会把整个括号算成"还有一层"，一路吞到文本结尾。
                    chars.next();
                    let mut depth = 0;
                    for inner in chars.by_ref() {
                        if inner == '(' {
                            depth += 1;
                        } else if inner == ')' {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        }
                    }
                    out.push_str(&label);
                } else {
                    out.push_str(&label);
                }
            }
            // 行内代码/强调/标题符号：去掉符号，保留文字。
            '`' | '*' | '#' | '_' | '~' => {}
            // 行首列表符号与引用符号。
            '-' | '>' if out.ends_with('\n') || out.is_empty() => {}
            other => out.push(other),
        }
    }
    // URL 直接去掉：读出来是噪音。
    let mut cleaned = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(index) = rest.find("http://").or_else(|| rest.find("https://")) {
        cleaned.push_str(&rest[..index]);
        let tail = &rest[index..];
        let end = tail
            .find(|ch: char| ch.is_whitespace() || ch == '，' || ch == '。' || ch == '）')
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    cleaned.push_str(rest);
    // 折叠空白（保留换行：换行是句子边界）。
    let mut folded = String::with_capacity(cleaned.len());
    let mut last_space = false;
    for ch in cleaned.chars() {
        if ch == '\n' {
            folded.push('\n');
            last_space = false;
        } else if ch.is_whitespace() {
            if !last_space {
                folded.push(' ');
                last_space = true;
            }
        } else {
            folded.push(ch);
            last_space = false;
        }
    }
    folded.trim().to_string()
}

/// 把一段可见回复切成按顺序播报的句子。
///
/// 规则（确定性，便于测试与"不重复、不跳句"的验收）：
///  1. 先按段落（换行）与句末标点切；
///  2. 仍然过长的片段再按逗号/顿号切开；
///  3. 很短（< 6 字）的碎片并入下一句，避免一顿一顿；
///  4. 单句超 [`MAX_SENTENCE_CHARS`] 硬切；
///  5. 总长超 [`MAX_SPEECH_CHARS`] 在句子边界截断，并如实记录丢了多少字。
pub fn prepare_speech(text: &str) -> SpeechPlan {
    let normalized = normalize_for_speech(text);
    if normalized.is_empty() {
        return SpeechPlan {
            sentences: Vec::new(),
            dropped_chars: 0,
        };
    }

    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in normalized.chars() {
        match ch {
            '\n' => {
                if !current.trim().is_empty() {
                    pieces.push(current.trim().to_string());
                }
                current.clear();
            }
            '。' | '！' | '？' | '!' | '?' | '；' | ';' => {
                if !current.trim().is_empty() {
                    pieces.push(current.trim().to_string());
                }
                current.clear();
            }
            other => current.push(other),
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current.trim().to_string());
    }

    // 仍然过长的片段再按逗号切开。
    let mut refined: Vec<String> = Vec::new();
    for piece in pieces {
        if piece.chars().count() <= MAX_SENTENCE_CHARS {
            refined.push(piece);
            continue;
        }
        let mut buffer = String::new();
        for ch in piece.chars() {
            buffer.push(ch);
            let at_pause = matches!(ch, '，' | '、' | ',' | '：' | ':');
            if at_pause && buffer.chars().count() >= 24 {
                refined.push(buffer.trim().to_string());
                buffer.clear();
            }
        }
        if !buffer.trim().is_empty() {
            refined.push(buffer.trim().to_string());
        }
    }

    // 硬切超长句。
    let mut hard: Vec<String> = Vec::new();
    for piece in refined {
        let chars: Vec<char> = piece.chars().collect();
        for window in chars.chunks(MAX_SENTENCE_CHARS) {
            let text: String = window.iter().collect();
            if !text.trim().is_empty() {
                hard.push(text.trim().to_string());
            }
        }
    }

    // 极短碎片（"好。"、"嗯。"）并入**下一句**：单字句合成出来只有一声，
    // 单独排队反而更碎。2 字以上的完整短句（"已完成。"）保留自己的节奏。
    // 末尾没得并的碎片再并回上一句，避免它自己成为一句。
    let mut merged: Vec<String> = Vec::new();
    let mut carry: Option<String> = None;
    for piece in hard {
        let joined = match carry.take() {
            Some(previous) => format!("{previous}{piece}"),
            None => piece,
        };
        if joined.chars().count() < MIN_OWN_SENTENCE_CHARS {
            carry = Some(joined);
        } else {
            merged.push(joined);
        }
    }
    if let Some(last) = carry {
        match merged.last_mut() {
            Some(previous)
                if previous.chars().count() + last.chars().count() <= MAX_SENTENCE_CHARS =>
            {
                previous.push_str(&last);
            }
            _ => merged.push(last),
        }
    }

    let mut sentences: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for (index, sentence) in merged.iter().enumerate() {
        let length = sentence.chars().count();
        if used + length > MAX_SPEECH_CHARS && !sentences.is_empty() {
            dropped += length;
            // 剩下的句子全都不播，一并计入丢弃字数。
            for rest in merged.iter().skip(index + 1) {
                dropped += rest.chars().count();
            }
            break;
        }
        used += length;
        sentences.push(sentence.clone());
    }

    SpeechPlan {
        sentences,
        dropped_chars: dropped,
    }
}

/// 建立正式 TTS 后端（只这两种）。
///
/// 云端模式**只**使用 MiMo 账户自己的 Key：绝不借用当前对话提供商
/// （DeepSeek / 自定义端点）的 Key，也绝不把 Key 发到自定义地址。
pub fn build_engine(
    settings: &TtsSettings,
    secret: &SecretStore,
) -> Result<Box<dyn TtsEngine>, String> {
    match settings.active_backend() {
        TtsBackend::Off => Err("语音播报未开启".to_string()),
        TtsBackend::Mimo => {
            let key = secret
                .load(mimo::KEY_PROVIDER)
                .map_err(|error| format!("读取 MiMo 账户 Key 失败：{error}"))?
                .unwrap_or_default();
            if key.trim().is_empty() {
                return Err(
                    "MiMo 云端播报需要 MiMo 账户的 Key：请在设置的「提供商 = mimo」下保存 Key"
                        .to_string(),
                );
            }
            Ok(Box::new(mimo::MimoTts::new(settings, key)?))
        }
        TtsBackend::Local => Ok(Box::new(local::LocalTts::new(settings)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_split_keeps_order_and_drops_markdown() {
        let plan = prepare_speech(
            "**已完成**。我打开了「记事本」，并输入了 *hello 世界*。\n\n\
             参考 [文档](https://example.com/doc) 里的说明。",
        );
        assert_eq!(plan.dropped_chars, 0);
        assert_eq!(plan.sentences.len(), 3, "{:?}", plan.sentences);
        assert_eq!(plan.sentences[0], "已完成");
        assert!(plan.sentences[1].contains("hello 世界"));
        assert!(plan.sentences[2].contains("文档"));
        assert!(!plan.sentences.join("").contains('*'));
        assert!(!plan.sentences.join("").contains("https://"));
    }

    #[test]
    fn short_fragments_merge_into_next_sentence() {
        let plan = prepare_speech("好。接下来我会打开设置面板并调整字体大小。");
        // "好" 只有 1 个字，必须并进下一句，而不是单独合成一顿。
        assert_eq!(plan.sentences.len(), 1, "{:?}", plan.sentences);
        assert!(plan.sentences[0].starts_with("好"));
    }

    #[test]
    fn long_text_is_truncated_at_sentence_boundary_and_reported() {
        let sentence = "这是一段用于测试的长句子，它会重复很多次用来超过上限。";
        let text = sentence.repeat(30);
        let plan = prepare_speech(&text);
        let total: usize = plan.sentences.iter().map(|s| s.chars().count()).sum();
        assert!(total <= MAX_SPEECH_CHARS);
        assert!(plan.dropped_chars > 0, "截断必须如实记录丢了多长");
        assert!(plan.sentences.iter().all(|s| !s.is_empty()));
    }

    #[test]
    fn very_long_single_sentence_is_hard_split() {
        let text = "啊".repeat(500);
        let plan = prepare_speech(&text);
        assert!(plan
            .sentences
            .iter()
            .all(|s| s.chars().count() <= MAX_SENTENCE_CHARS));
        assert!(plan.sentences.len() >= 2);
    }

    #[test]
    fn empty_and_whitespace_only_produce_no_speech() {
        assert!(prepare_speech("").is_empty());
        assert!(prepare_speech("   \n\n  ").is_empty());
        assert!(prepare_speech("```").is_empty());
    }

    #[test]
    fn numbers_and_mixed_languages_are_preserved() {
        let plan = prepare_speech("内存占用 1.5 GB，CPU 占用 12%。");
        let joined = plan.sentences.join("|");
        assert!(joined.contains("1.5 GB"), "{joined}");
        assert!(joined.contains("12%"), "{joined}");
    }

    #[test]
    fn mimo_engine_requires_its_own_account_key() {
        let settings = TtsSettings {
            enabled: true,
            backend: TtsBackend::Mimo,
            ..TtsSettings::default()
        };
        // 用临时目录里的空 SecretStore：没有 MiMo Key 时必须明确拒绝，
        // 而不是悄悄借用当前对话提供商的 Key。
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join("tts-engine-key");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let secret = SecretStore::new(&root);
        let error = match build_engine(&settings, &secret) {
            Ok(_) => panic!("没有 MiMo Key 时不应该创建引擎"),
            Err(error) => error,
        };
        assert!(error.contains("MiMo"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
