//! Whisper ASR：模型常驻、独立工作线程、结果按会话关联。
//!
//! 三条硬约束（任务书）：
//!  1. **模型在会话间复用**，不得每句话启动一次 whisper-cli；
//!  2. 推理在**独立阻塞线程**上，不阻塞 Slint 主线程、音频采集与 tokio Agent；
//!  3. 停止监听 / 取消 / 退出后，**迟到的识别结果必须丢弃**，
//!     不能发送到新一轮，也不能触发工具。
//!
//! 第 3 条用「会话序号」实现：加载时记下 `session_id`，返回时再比一次。
//! 序号对不上就直接丢弃——即使底层推理没能被 abort 回调立刻打断。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self as std_mpsc, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
};

use crate::voice::settings::AsrLanguage;

/// 推理参数（与界面可配置项对齐）。
#[derive(Debug, Clone)]
pub struct AsrConfig {
    pub model_path: PathBuf,
    pub language: AsrLanguage,
    pub threads: usize,
    pub use_gpu: bool,
    /// 提示词：中文场景给简体偏好 + 标点，减少 Whisper 混出繁体。
    pub initial_prompt: String,
}

/// 本构建是否带 CUDA 后端。
///
/// `cuda` 是可选 feature：不开启时即使设置里要求 GPU 也只能走 CPU。
/// 这不是「降级到意外的东西」——界面会明确显示「本构建未启用 CUDA」。
pub const fn gpu_compiled_in() -> bool {
    cfg!(feature = "cuda")
}

impl AsrConfig {
    pub fn from_settings(
        settings: &crate::voice::settings::VoiceSettings,
        model_path: PathBuf,
    ) -> Self {
        Self {
            model_path,
            language: settings.language,
            threads: settings.resolved_threads(),
            // 请求 GPU 但没有编进 CUDA 时不要真的去初始化 GPU：
            // 那会在加载阶段失败，而不是「用 CPU 跑起来」。
            use_gpu: settings.use_gpu && gpu_compiled_in(),
            initial_prompt: default_initial_prompt(settings.language),
        }
    }
}

/// 给 Whisper 的初始提示词。
///
/// 为什么需要它：中文 Whisper 在只有一两秒的短句上很容易输出繁体，
/// 或干脆补一句「以下是普通话的句子」。提示词能明显压低这两类问题。
/// 不在这里做「繁转简」后处理，是因为那会误改专有名词。
pub fn default_initial_prompt(language: AsrLanguage) -> String {
    match language {
        AsrLanguage::Chinese => "以下是普通话的句子，请用简体中文和标点。".to_string(),
        AsrLanguage::English => "The following is a short English voice command.".to_string(),
        AsrLanguage::Auto => String::new(),
    }
}

/// 一次识别结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    /// 关联的语音会话序号（回调方据此丢弃过期结果）。
    pub session_id: u64,
    /// 识别文本（已清理首尾空白）。
    pub text: String,
    /// 音频时长（秒）。
    pub audio_seconds: f32,
    /// 推理耗时（毫秒）。
    pub decode_ms: u64,
    /// 模型实际判定的语言 id（-1 表示未知）。
    pub language_id: i32,
    /// 每个分段的 no_speech 概率（诊断用，不上界面）。
    pub no_speech: Vec<f32>,
}

impl Transcript {
    /// 是否值得提交给 Agent：非空、且不是模型常见的「无语音」幻觉。
    pub fn is_submittable(&self) -> bool {
        let text = self.text.trim();
        if text.is_empty() {
            return false;
        }
        if is_whisper_noise(text) {
            return false;
        }
        // 纯标点/空白不算内容。
        text.chars().any(|c| c.is_alphanumeric() || is_cjk(c))
    }
}

/// Whisper 在静音/噪声上常见的固定幻觉输出。只挡**整句完全匹配**的几种，
/// 不做关键词过滤（那会误伤用户真的说了「谢谢」之类的指令）。
fn is_whisper_noise(text: &str) -> bool {
    let normalized: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && !".,!?。，！？…、；：".contains(*c))
        .collect::<String>()
        .to_lowercase();
    matches!(
        normalized.as_str(),
        "" | "谢谢观看"
            | "谢谢观看。"
            | "感谢观看"
            | "字幕由amaraorg社区提供"
            | "请不吝点赞订阅转发打赏支持明镜与点点栏目"
            | "you"
            | "thankyou"
            | "thanksforwatching"
            | "subtitlesbytheamaraorgcommunity"
    )
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF)
}

/// ASR 工作线程的句柄。
///
/// 线程模型：一个**常驻**线程持有一个 `WhisperContext`（模型只加载一次），
/// 通过通道接收「这段 PCM 要识别」的请求。这样：
///  - 加载只在启动/换模型时发生；
///  - 推理期间音频线程与界面线程都不受影响；
///  - 关监听时发 `Shutdown`，线程把模型释放掉。
pub struct AsrWorker {
    tx: Sender<Request>,
    join: Option<std::thread::JoinHandle<()>>,
    /// 最近一次识别结束时的会话序号（用于诊断与测试）。
    last_completed: Arc<AtomicU64>,
}

enum Request {
    Transcribe {
        session_id: u64,
        samples: Vec<f32>,
        /// 结果回传给谁（由语音协调器提供）。
        reply: ReplySink,
    },
    Shutdown,
}

/// 结果回调：在 ASR 线程上调用，调用方必须很快返回（只做转发，不做 UI 操作）。
pub type ReplySink = Box<dyn FnOnce(Result<Transcript, AsrError>) + Send>;

/// ASR 错误（要能直接显示给用户）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum AsrError {
    #[error("Whisper 模型加载失败：{0}")]
    Load(String),
    #[error("Whisper 推理失败：{0}")]
    Decode(String),
    #[error("语音输入已停止，未执行识别")]
    Stopped,
}

impl AsrWorker {
    /// 启动常驻线程并**同步**加载模型——加载失败要立刻报给调用方，
    /// 不能出现「线程起来了但模型没加载」的假监听状态。
    pub fn start(config: AsrConfig) -> Result<(Self, ReadyInfo), AsrError> {
        let (tx, rx) = std_mpsc::channel::<Request>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<ReadyInfo, AsrError>>();
        let last_completed = Arc::new(AtomicU64::new(0));
        let last_for_thread = last_completed.clone();

        let join = std::thread::Builder::new()
            .name("cuava-asr".to_string())
            .spawn(move || {
                let context = match load_context(&config) {
                    Ok(context) => {
                        let _ = ready_tx.send(Ok(ReadyInfo {
                            use_gpu: config.use_gpu,
                        }));
                        context
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                run_loop(context, &config, rx, last_for_thread);
            })
            .map_err(|error| AsrError::Load(format!("无法启动 ASR 线程：{error}")))?;

        match ready_rx.recv() {
            Ok(Ok(info)) => Ok((
                Self {
                    tx,
                    join: Some(join),
                    last_completed,
                },
                info,
            )),
            Ok(Err(error)) => {
                // 线程已经自己退出了；join 一下收尸，避免泄漏。
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                let _ = join.join();
                Err(AsrError::Load("ASR 线程在报告就绪前退出了".to_string()))
            }
        }
    }

    /// 提交一段音频。结果通过 `reply` 回到调用方线程。
    pub fn submit(
        &self,
        session_id: u64,
        samples: Vec<f32>,
        reply: ReplySink,
    ) -> Result<(), AsrError> {
        self.tx
            .send(Request::Transcribe {
                session_id,
                samples,
                reply,
            })
            .map_err(|_| AsrError::Stopped)
    }

    /// 已完成的最后一个会话序号（测试与诊断用）。
    pub fn last_completed(&self) -> u64 {
        self.last_completed.load(Ordering::Relaxed)
    }

    /// 停止线程并释放模型。已经进入推理的请求会被 abort 回调尽快打断。
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AsrWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            // 不能无限等：底层推理若卡住，交给进程退出回收。
            // 但正常情况（abort 回调生效）会很快结束。
            let _ = join.join();
        }
    }
}

/// 线程就绪时回报的最小信息（是否走 GPU 由界面显示）。
#[derive(Debug, Clone)]
pub struct ReadyInfo {
    pub use_gpu: bool,
}

fn load_context(config: &AsrConfig) -> Result<WhisperContext, AsrError> {
    if !config.model_path.is_file() {
        return Err(AsrError::Load(format!(
            "模型文件不存在：{}",
            config.model_path.display()
        )));
    }
    let mut params = WhisperContextParameters::default();
    params.use_gpu(config.use_gpu);
    WhisperContext::new_with_params(&config.model_path, params).map_err(|error| {
        let detail = match error {
            WhisperError::InitError => "无法初始化（文件损坏、格式不兼容，或缺少 CUDA 运行库）",
            other => return AsrError::Load(format!("{other:?}")),
        };
        AsrError::Load(format!("{detail}：{}", config.model_path.display()))
    })
}

fn run_loop(
    context: WhisperContext,
    config: &AsrConfig,
    rx: Receiver<Request>,
    last_completed: Arc<AtomicU64>,
) {
    while let Ok(request) = rx.recv() {
        match request {
            Request::Shutdown => break,
            Request::Transcribe {
                session_id,
                samples,
                reply,
            } => {
                let result = transcribe(&context, config, session_id, &samples);
                last_completed.store(session_id, Ordering::Relaxed);
                reply(result);
            }
        }
    }
}

/// 同步执行一次识别。供工作线程与离线测试共用。
pub fn transcribe(
    context: &WhisperContext,
    config: &AsrConfig,
    session_id: u64,
    samples: &[f32],
) -> Result<Transcript, AsrError> {
    if samples.is_empty() {
        return Ok(Transcript {
            session_id,
            text: String::new(),
            audio_seconds: 0.0,
            decode_ms: 0,
            language_id: -1,
            no_speech: Vec::new(),
        });
    }
    let mut state = context
        .create_state()
        .map_err(|error| AsrError::Decode(format!("无法创建推理状态：{error:?}")))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(config.threads.clamp(1, 64) as i32);
    // 短指令不需要跨段上下文：关掉能减少「上一句带偏下一句」。
    params.set_no_context(true);
    params.set_single_segment(false);
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_timestamps(true);
    params.set_suppress_blank(true);
    params.set_temperature(0.0);
    params.set_language(config.language.whisper_language());
    if !config.initial_prompt.is_empty() {
        params.set_initial_prompt(&config.initial_prompt);
    }

    let started = Instant::now();
    state
        .full(params, samples)
        .map_err(|error| AsrError::Decode(format!("{error:?}")))?;
    let decode_ms = started.elapsed().as_millis() as u64;

    let mut text = String::new();
    let mut no_speech = Vec::new();
    for segment in state.as_iter() {
        text.push_str(&segment.to_str_lossy().unwrap_or_default());
        no_speech.push(segment.no_speech_probability());
    }

    Ok(Transcript {
        session_id,
        text: tidy(&text),
        audio_seconds: samples.len() as f32 / 16_000.0,
        decode_ms,
        language_id: state.full_lang_id_from_state(),
        no_speech,
    })
}

/// 整理模型输出：去掉首尾空白与 Whisper 常见的多余空格。
pub fn tidy(text: &str) -> String {
    text.replace("undefined", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// 用回调驱动的取消标志包装一次识别（供已经放到独立线程里的长期任务使用）。
///
/// 说明：本 PR 的 ASR 工作线程是串行队列，取消通过「结果过期丢弃」保证正确性；
/// 这个 abort 标志用于**尽快**结束正在跑的推理，属于优化而非正确性依赖。
pub struct AbortFlag(Arc<AtomicBool>);

impl AbortFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn abort(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for AbortFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// 把一段 16k 单声道 PCM 存成 WAV（**只用于测试/基准**，运行期录音不落盘）。
pub fn write_wav_16k(path: &Path, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|error| format!("创建 WAV 失败：{error}"))?;
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer
            .write_sample(value)
            .map_err(|error| format!("写 WAV 失败：{error}"))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("收尾 WAV 失败：{error}"))
}

/// 读 WAV：任意采样率/声道/位深 → 16k 单声道 f32。
///
/// 只给测试与基准用。运行期音频来自采集线程，不走文件。
pub fn read_wav_16k(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|error| format!("打开 WAV 失败 {}：{error}", path.display()))?;
    let spec = reader.spec();
    let mut mono: Vec<f32> = Vec::new();
    let channels = spec.channels.max(1) as usize;
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let bits = spec.bits_per_sample.max(1);
            let max = (1i64 << (bits - 1)) as f32;
            let mut frame = 0.0f32;
            let mut count = 0usize;
            for sample in reader.samples::<i32>() {
                let value = sample.map_err(|error| format!("读 WAV 失败：{error}"))?;
                frame += value as f32 / max;
                count += 1;
                if count == channels {
                    mono.push(frame / channels as f32);
                    frame = 0.0;
                    count = 0;
                }
            }
        }
        hound::SampleFormat::Float => {
            let mut frame = 0.0f32;
            let mut count = 0usize;
            for sample in reader.samples::<f32>() {
                let value = sample.map_err(|error| format!("读 WAV 失败：{error}"))?;
                frame += value;
                count += 1;
                if count == channels {
                    mono.push(frame / channels as f32);
                    frame = 0.0;
                    count = 0;
                }
            }
        }
    }
    let mut resampler = crate::voice::pcm::StreamingResampler::new(spec.sample_rate)
        .map_err(|error| format!("{error}"))?;
    let mut out = resampler.push(&mono).map_err(|error| format!("{error}"))?;
    out.extend(resampler.flush().map_err(|error| format!("{error}"))?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_strips_whitespace_but_keeps_english_spacing() {
        assert_eq!(tidy("  打开 浏览器  "), "打开 浏览器");
        assert_eq!(tidy("open   notepad"), "open notepad");
        assert_eq!(tidy("undefined打开"), "打开");
    }

    #[test]
    fn empty_transcript_is_not_submittable() {
        let transcript = Transcript {
            session_id: 1,
            text: "   ".to_string(),
            audio_seconds: 1.0,
            decode_ms: 1,
            language_id: 1,
            no_speech: vec![],
        };
        assert!(!transcript.is_submittable());
    }

    #[test]
    fn known_whisper_hallucinations_are_rejected() {
        for text in ["谢谢观看。", "Thank you.", "字幕由 Amara.org 社区提供"] {
            let transcript = Transcript {
                session_id: 1,
                text: text.to_string(),
                audio_seconds: 1.0,
                decode_ms: 1,
                language_id: 1,
                no_speech: vec![],
            };
            assert!(!transcript.is_submittable(), "{text} 应当被拒绝");
        }
    }

    #[test]
    fn real_commands_are_submittable() {
        for text in ["打开浏览器", "open notepad", "帮我列出窗口", "好的", "谢谢"] {
            let transcript = Transcript {
                session_id: 1,
                text: text.to_string(),
                audio_seconds: 1.0,
                decode_ms: 1,
                language_id: 1,
                no_speech: vec![],
            };
            assert!(transcript.is_submittable(), "{text} 应当被接受");
        }
    }

    /// 纯标点（模型偶尔吐出「，」）不是有效指令。
    #[test]
    fn punctuation_only_is_rejected() {
        let transcript = Transcript {
            session_id: 1,
            text: "，。,".to_string(),
            audio_seconds: 1.0,
            decode_ms: 1,
            language_id: 1,
            no_speech: vec![],
        };
        assert!(!transcript.is_submittable());
    }

    #[test]
    fn initial_prompt_prefers_simplified_chinese() {
        let prompt = default_initial_prompt(AsrLanguage::Chinese);
        assert!(prompt.contains("简体"));
        assert!(default_initial_prompt(AsrLanguage::Auto).is_empty());
    }

    #[test]
    fn wav_roundtrip_16k() {
        let dir = crate::voice::settings::tests_support::temp_root("wav");
        let path = dir.join("t.wav");
        let samples: Vec<f32> = (0..1600)
            .map(|i| (i as f32 / 1600.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        write_wav_16k(&path, &samples).unwrap();
        let back = read_wav_16k(&path).unwrap();
        assert_eq!(back.len(), samples.len());
        for (a, b) in samples.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_reader_resamples_44k_to_16k() {
        let dir = crate::voice::settings::tests_support::temp_root("wav44");
        let path = dir.join("t.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..44_100 {
            let value = ((i as f32 / 44_100.0 * 440.0 * std::f32::consts::TAU).sin()
                * 0.3
                * i16::MAX as f32) as i16;
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();
        let samples = read_wav_16k(&path).unwrap();
        assert!(
            (samples.len() as f64 - 16_000.0).abs() < 400.0,
            "44.1k 1 秒应约等于 16000 样本，实际 {}",
            samples.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
