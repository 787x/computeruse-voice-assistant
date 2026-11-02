//! 唤醒词：sherpa-onnx 的 Zipformer KWS。
//!
//! 与旧版 `packages/voice/kws.mjs` 行为对齐，并保留了三条经实测的规则：
//!
//!  1. **喂未经过 AGC 的原始 16k PCM**。旧版实测（`tools/verify-wake-runtime.mjs`）
//!     同一段「小爱同学」直接喂能命中，经 AGC 放大后再喂就漏检——增益把波形削顶了。
//!     所以这条管线里**没有** AGC，KWS 拿到的就是采集线程重采样后的原始样本。
//!  2. **开口前重置一次关键词流**。sherpa 的关键词流在长时间静音后命中率不稳定，
//!     旧版实测「开麦后第一次唤醒失败、手动关开一次才好」就是这个原因。
//!  3. **每 3 秒静音重置一次**，避免流里堆积几十秒静音。
//!
//! 模型加载失败**必须如实报错**，不能进入「看起来在监听」的状态。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};

/// 默认 KWS 参数，与旧版 `KWS_DEFAULTS` 一致。
pub const KEYWORDS_SCORE: f32 = 1.0;
pub const KEYWORDS_THRESHOLD: f32 = 0.25;
pub const NUM_TRAILING_BLANKS: i32 = 2;

/// KWS 模型目录里的固定文件名（int8 权重：体积小、CPU 快）。
pub const ENCODER_FILE: &str = "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
pub const DECODER_FILE: &str = "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
pub const JOINER_FILE: &str = "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
pub const TOKENS_FILE: &str = "tokens.txt";

/// 长时间静音后重置关键词流的阈值。
pub const SILENT_RESET: Duration = Duration::from_secs(3);
/// 判定「刚才还是静音」的阈值：语音起点发生在它之后就该重置。
pub const SPEECH_ONSET_GAP: Duration = Duration::from_millis(300);

/// KWS 构造失败的原因（要能直接显示给用户）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum KwsError {
    #[error("KWS 模型目录不完整：{0}")]
    MissingModel(String),
    #[error("关键词文件不可用：{0}")]
    Keywords(String),
    #[error("KWS 模型加载失败：{0}")]
    Load(String),
}

/// 命中的关键词（就是关键词文件里 `@` 后的展示名）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeHit {
    pub keyword: String,
}

/// 关键词文件内容签名：用「大小 + 修改时间」判断是否需要重建。
///
/// 只在开麦边界比较一次，不在每个音频块上读文件（旧版同样如此）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeywordSignature {
    pub len: u64,
    pub modified: Option<std::time::SystemTime>,
}

impl KeywordSignature {
    pub fn read(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(meta) => Self {
                len: meta.len(),
                modified: meta.modified().ok(),
            },
            Err(_) => Self::default(),
        }
    }
}

/// 流式唤醒检测器。**不是** `Send` 之外的线程问题：它自己持有 stream，
/// 只能在一个线程里用（音频线程）。
pub struct Kws {
    spotter: KeywordSpotter,
    stream: OnlineStream,
    keywords_file: std::path::PathBuf,
    signature: KeywordSignature,
    /// 从上次重置到现在，静音了多久。
    silent_for: Duration,
    /// 上一块的结束时刻，用于把「块」换算成时间。
    last_block_at: Instant,
    /// 命中计数（诊断用）。
    hits: Arc<AtomicU64>,
    labels: Vec<String>,
}

impl std::fmt::Debug for Kws {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Kws")
            .field("keywords_file", &self.keywords_file)
            .field("labels", &self.labels)
            .finish_non_exhaustive()
    }
}

impl Kws {
    /// 创建检测器。`model_dir` 必须包含四个模型文件。
    pub fn new(model_dir: &Path, keywords_file: &Path, threads: usize) -> Result<Self, KwsError> {
        for (name, label) in [
            (ENCODER_FILE, "encoder"),
            (DECODER_FILE, "decoder"),
            (JOINER_FILE, "joiner"),
            (TOKENS_FILE, "tokens"),
        ] {
            if !model_dir.join(name).is_file() {
                return Err(KwsError::MissingModel(format!(
                    "{} 缺少 {label} 文件 {name}",
                    model_dir.display()
                )));
            }
        }
        if !keywords_file.is_file() {
            return Err(KwsError::Keywords(format!(
                "文件不存在：{}",
                keywords_file.display()
            )));
        }

        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer.encoder =
            Some(model_dir.join(ENCODER_FILE).display().to_string());
        config.model_config.transducer.decoder =
            Some(model_dir.join(DECODER_FILE).display().to_string());
        config.model_config.transducer.joiner =
            Some(model_dir.join(JOINER_FILE).display().to_string());
        config.model_config.tokens = Some(model_dir.join(TOKENS_FILE).display().to_string());
        config.model_config.num_threads = threads.clamp(1, 8) as i32;
        config.model_config.provider = Some("cpu".to_string());
        config.keywords_file = Some(keywords_file.display().to_string());
        config.keywords_score = KEYWORDS_SCORE;
        config.keywords_threshold = KEYWORDS_THRESHOLD;
        config.num_trailing_blanks = NUM_TRAILING_BLANKS;

        let spotter = KeywordSpotter::create(&config).ok_or_else(|| {
            KwsError::Load(format!(
                "sherpa-onnx 无法创建关键词检测器（模型目录 {}，关键词 {}）",
                model_dir.display(),
                keywords_file.display()
            ))
        })?;
        let stream = spotter.create_stream();
        let signature = KeywordSignature::read(keywords_file);
        let labels = crate::voice::settings::keyword_labels(keywords_file);

        Ok(Self {
            spotter,
            stream,
            keywords_file: keywords_file.to_path_buf(),
            signature,
            silent_for: SILENT_RESET,
            last_block_at: Instant::now(),
            hits: Arc::new(AtomicU64::new(0)),
            labels,
        })
    }

    pub fn keywords_file(&self) -> &Path {
        &self.keywords_file
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// 关键词文件在磁盘上是否已经变化（开麦边界比较，不在音频块上比较）。
    pub fn keywords_changed(&self) -> bool {
        KeywordSignature::read(&self.keywords_file) != self.signature
    }

    pub fn signature(&self) -> &KeywordSignature {
        &self.signature
    }

    /// 重置关键词流（清空解码状态，但保留模型）。
    pub fn reset(&mut self) {
        self.spotter.reset(&self.stream);
        self.silent_for = Duration::ZERO;
    }

    /// 喂一块 16k 单声道**原始**样本（未经增益），返回命中的关键词。
    ///
    /// `speech_active` 是 VAD 对同一块的判定，用于决定要不要在「开口前」重置流：
    ///  - 从静音进入说话：重置一次（保证每次唤醒都从干净的流开始）；
    ///  - 持续静音超过 3 秒：重置一次（避免堆积静音）。
    pub fn accept(&mut self, samples: &[f32], speech_active: bool) -> Option<WakeHit> {
        if samples.is_empty() {
            return None;
        }
        // 用调用方的块长度换算时间：音频块通常 10~100ms。
        let now = Instant::now();
        let block = now.saturating_duration_since(self.last_block_at);
        self.last_block_at = now;
        // 第一块或异常长间隔时按块长估算，避免把「程序卡了一下」算成静音很久。
        let elapsed = if block > Duration::from_secs(1) {
            Duration::from_secs_f32(samples.len() as f32 / 16_000.0)
        } else {
            block
        };

        if speech_active {
            if self.silent_for >= SPEECH_ONSET_GAP {
                // 规则 2：开口前重置。
                self.spotter.reset(&self.stream);
            }
            self.silent_for = Duration::ZERO;
        } else {
            self.silent_for += elapsed;
            if self.silent_for >= SILENT_RESET {
                // 规则 3：长期静音定期重置。
                self.spotter.reset(&self.stream);
                self.silent_for = Duration::ZERO;
            }
        }

        self.stream.accept_waveform(16_000, samples);
        let mut hit = None;
        // guard 防止异常模型让循环停不下来（旧版同样有 guard）。
        let mut guard = 0;
        while self.spotter.is_ready(&self.stream) && guard < 64 {
            self.spotter.decode(&self.stream);
            guard += 1;
        }
        if let Some(result) = self.spotter.get_result(&self.stream) {
            let keyword = result.keyword.trim().to_string();
            if !keyword.is_empty() && keyword != "undefined" {
                hit = Some(WakeHit { keyword });
            }
        }
        if hit.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
            // 命中后重置：下一次唤醒必须重新完整说出关键词。
            self.spotter.reset(&self.stream);
            self.silent_for = Duration::ZERO;
        }
        hit
    }

    /// 收尾：告诉流「没有更多音频」，把剩余帧解码完。
    ///
    /// 离线测试（喂 WAV）必须调用，否则最后一段的命中不会吐出。
    pub fn finish(&mut self) -> Option<WakeHit> {
        self.stream.input_finished();
        let mut hit = None;
        let mut guard = 0;
        while self.spotter.is_ready(&self.stream) && guard < 1024 {
            self.spotter.decode(&self.stream);
            guard += 1;
        }
        if let Some(result) = self.spotter.get_result(&self.stream) {
            let keyword = result.keyword.trim().to_string();
            if !keyword.is_empty() && keyword != "undefined" {
                hit = Some(WakeHit { keyword });
            }
        }
        hit
    }
}

/// KWS 的可用性状态（给界面与日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KwsState {
    Ready { labels: Vec<String> },
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不依赖模型就能确定的规则：签名比较、缺文件报错。
    #[test]
    fn signature_changes_with_file() {
        let dir = crate::voice::settings::tests_support::temp_root("kws-sig");
        let file = dir.join("kw.txt");
        std::fs::write(&file, "a @A\n").unwrap();
        let first = KeywordSignature::read(&file);
        std::fs::write(&file, "a @A\nb @B\n").unwrap();
        let second = KeywordSignature::read(&file);
        assert_ne!(first, second, "文件内容/大小变了签名必须变");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_are_reported_not_panicked() {
        let dir = crate::voice::settings::tests_support::temp_root("kws-missing");
        let error = Kws::new(&dir, &dir.join("kw.txt"), 2).unwrap_err();
        match error {
            KwsError::MissingModel(message) => assert!(message.contains("encoder")),
            other => panic!("期望 MissingModel，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_keywords_file_is_reported() {
        let dir = crate::voice::settings::tests_support::temp_root("kws-nokw");
        for name in [ENCODER_FILE, DECODER_FILE, JOINER_FILE, TOKENS_FILE] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let error = Kws::new(&dir, &dir.join("kw.txt"), 2).unwrap_err();
        assert!(matches!(error, KwsError::Keywords(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
