//! 语音会话协调器：把采集、KWS、VAD、ASR 串成一个有明确状态的会话。
//!
//! 状态机（任务书要求可合理合并，这里保留五个对用户有意义的状态）：
//!
//! ```text
//! Off ──启用──> Loading ──成功──> Listening ──唤醒──> Capturing ──端点──> Transcribing
//!  ^                 │                ^                  │                    │
//!  │                 └──失败──> Error │<── 超时/短噪声 ───┘                    │
//!  └────────────── 关闭 ──────────────┴──────────── 完成/失败 <────────────────┘
//! ```
//!
//! 不变量：
//!  - **同一时刻只有一个可提交的语音会话**：每次进入 Capturing 分配新的
//!    `session_id`；ASR 返回时若 `active_session` 已不是它，结果直接丢弃。
//!  - 唤醒误触发、纯静音、短噪声、超时、空转录一律**不提交**。
//!  - 模型加载失败不会伪装成「正在监听」：状态直接进 `Error`，
//!    并且文本 Agent 完全不受影响。
//!
//! 这个模块不直接操作 Slint：状态变化通过 `VoiceEvent` 发给 worker，
//! 由 worker 决定怎么显示。这样音频线程可以在没有界面的情况下确定性地测试。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::voice::asr::{AsrConfig, AsrError, AsrWorker, ReadyInfo as AsrReady, Transcript};
use crate::voice::capture::{Capture, CaptureError, CapturedChunk, FrontEnd};
use crate::voice::kws::Kws;
use crate::voice::settings::{Resolved, VoicePaths, VoiceSettings};
use crate::voice::vad::{EndReason, EndpointDetector, VadEvent};

/// 对用户有意义的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceState {
    Off,
    /// 正在加载模型。
    Loading,
    /// 监听中（等唤醒词）。
    Listening,
    /// 已唤醒，正在录一句指令。
    Capturing,
    /// 端点已到，正在识别。
    Transcribing,
    /// 出错（模型缺失、设备断开等），文本模式仍可用。
    Error(String),
}

impl VoiceState {
    pub fn label(&self) -> String {
        match self {
            Self::Off => "麦克风已关闭".to_string(),
            Self::Loading => "正在加载语音模型…".to_string(),
            Self::Listening => "正在监听唤醒词".to_string(),
            Self::Capturing => "我在听，请说".to_string(),
            Self::Transcribing => "正在识别…".to_string(),
            Self::Error(message) => format!("语音不可用：{message}"),
        }
    }

    /// 状态码（给 Slint 用；Slint 侧不直接持有 Rust 枚举）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Loading => "loading",
            Self::Listening => "listening",
            Self::Capturing => "capturing",
            Self::Transcribing => "transcribing",
            Self::Error(_) => "error",
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Off | Self::Error(_))
    }
}

/// 协调器发给 worker 的事件。
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    StateChanged(VoiceState),
    /// 唤醒词命中，开始录音。
    Woke {
        keyword: String,
    },
    /// 识别出的最终文本（**只有它会提交给 Agent**）。
    Final {
        session_id: u64,
        text: String,
        audio_seconds: f32,
        decode_ms: u64,
    },
    /// 一次语音会话结束但没有可提交文本（附原因，给用户看）。
    Rejected {
        session_id: u64,
        reason: RejectReason,
    },
    /// 模型与设备都就绪（附采集信息）。
    Ready(Box<CaptureInfoForUi>),
    /// 电平/底噪（界面显示用，已节流）。
    Level {
        level_db: f32,
        floor_db: f32,
    },
    /// 采集溢出：当前句已丢弃。
    Overflow,
}

/// 不提交的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// 唤醒后一直没说话。
    NoSpeech,
    /// 语音太短（短噪声/误触发）。
    TooShort,
    /// 转录为空或只有噪声幻觉。
    EmptyTranscript,
    /// 识别失败。
    Failed(String),
    /// 会话已被取消/过期（不再提交）。
    Stale,
    /// 采集溢出，这一句不完整。
    Overflow,
}

impl RejectReason {
    pub fn message(&self) -> String {
        match self {
            Self::NoSpeech => "没听到说话".to_string(),
            Self::TooShort => "声音太短，已忽略".to_string(),
            Self::EmptyTranscript => "没听清，再说一次？".to_string(),
            Self::Failed(error) => format!("识别失败：{error}"),
            Self::Stale => "这次语音已取消".to_string(),
            Self::Overflow => "音频积压，这一句已丢弃".to_string(),
        }
    }
}

/// 可克隆的事件发送端（worker 用通道实现，ASR 线程也持有同一份）。
#[derive(Clone)]
pub struct SharedSink(Arc<dyn Fn(VoiceEvent) + Send + Sync + 'static>);

impl SharedSink {
    pub fn new(send: impl Fn(VoiceEvent) + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    pub fn emit(&self, event: VoiceEvent) {
        (self.0)(event);
    }
}

/// 协调器配置（从 `VoiceSettings` 与解析好的路径构造）。
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    pub voice: VoiceSettings,
    pub kws_model_dir: PathBuf,
    pub keywords_file: PathBuf,
    pub whisper_model: PathBuf,
}

impl CoordinatorConfig {
    /// 解析路径并校验；模型缺失时返回**可读的原因**（全部列出，不只第一条）。
    pub fn resolve(voice: &VoiceSettings, paths: &VoicePaths) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let kws = paths.resolve_kws_model(&voice.kws_model_dir);
        let keywords = match paths.resolve_keywords(&voice.keywords_file) {
            Ok(resolved) => resolved,
            Err(error) => {
                errors.push(error);
                Resolved::Missing(String::new())
            }
        };
        let whisper = paths.resolve_whisper_model(&voice.whisper_model);
        for resolved in [&kws, &keywords, &whisper] {
            if let Some(message) = resolved.error() {
                if !message.is_empty() {
                    errors.push(message.to_string());
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self {
            voice: voice.clone(),
            kws_model_dir: kws.path().expect("已校验").to_path_buf(),
            keywords_file: keywords.path().expect("已校验").to_path_buf(),
            whisper_model: whisper.path().expect("已校验").to_path_buf(),
        })
    }
}

/// 当前可提交的会话序号。0 = 没有进行中的会话。
///
/// 由协调器与 ASR 回调用同一份原子量共享：ASR 返回时先比一次，
/// 对不上就不提交——即使底层推理没能被立刻打断。
pub type ActiveSession = Arc<AtomicU64>;

/// 识别结果是否仍属于「当前会话」。
///
/// 这一条是「迟到的结果不得触发工具」的中心判据：
/// 取消、关麦、新唤醒都会把 `active` 改成别的值（或 0），
/// 于是旧结果在返回时对不上，被直接丢弃。
pub fn is_current_session(active: &ActiveSession, session_id: u64) -> bool {
    session_id != 0 && active.load(Ordering::SeqCst) == session_id
}

/// 当前录制的这一句。
struct CaptureSession {
    session_id: u64,
}

/// 协调器（跑在音频线程上）。
pub struct Coordinator {
    config: CoordinatorConfig,
    sink: SharedSink,
    capture: Capture,
    receiver: Receiver<CapturedChunk>,
    frontend: FrontEnd,
    kws: Kws,
    asr: AsrWorker,
    /// VAD 一直运行：空闲时用来给 KWS 提供「现在是不是说话起点」。
    detector: EndpointDetector,
    session: Option<CaptureSession>,
    active: ActiveSession,
    next_session: AtomicU64,
    last_level_at: Option<Instant>,
    state: VoiceState,
    dropped: u64,
    /// ASR 线程报告「这一次识别已经结束」（成功或失败）。
    ///
    /// **不能**在结果回调里直接把 `active` 清零：回调是在 ASR 线程上跑的，
    /// 而工作线程是之后才在命令通道里处理这条事件的。先清零会让工作线程
    /// 的「这次结果是否仍属于当前会话」判断永远为假，所有语音指令都会被丢弃。
    asr_done: Arc<std::sync::atomic::AtomicBool>,
    /// worker 要求停止时置位；音频线程在每个循环检查。
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Coordinator {
    /// 启动采集 + 加载 KWS 与 ASR。
    ///
    /// 任一模型加载失败都返回 `Err`，由调用方切换到 `Error`：
    /// **不会**出现「采集在跑、模型其实没加载」的假监听。
    pub fn start(
        config: CoordinatorConfig,
        sink: SharedSink,
        active: ActiveSession,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(Self, CaptureInfoForUi, AsrReady), String> {
        let mut capture =
            Capture::start(&config.voice.input_device).map_err(|error| match error {
                CaptureError::NoDevice => "没有可用的麦克风输入设备".to_string(),
                other => other.to_string(),
            })?;
        let info = capture.info().clone();
        let receiver = capture
            .take_receiver()
            .ok_or_else(|| "采集通道初始化失败".to_string())?;
        let frontend = FrontEnd::new(info.clone()).map_err(|error| {
            format!(
                "麦克风“{}”的采样率 {} Hz 不受支持：{error}",
                info.device_name, info.sample_rate
            )
        })?;

        let kws = Kws::new(
            &config.kws_model_dir,
            &config.keywords_file,
            config.voice.resolved_threads(),
        )
        .map_err(|error| error.to_string())?;

        let asr_config = AsrConfig::from_settings(&config.voice, config.whisper_model.clone());
        let (asr, asr_ready) = AsrWorker::start(asr_config).map_err(|error| match error {
            AsrError::Load(message) => message,
            other => other.to_string(),
        })?;

        let labels = kws.labels().to_vec();
        let ui = CaptureInfoForUi {
            device_name: info.device_name.clone(),
            sample_rate: info.sample_rate,
            resampled: !frontend.resampler.is_passthrough(),
            wake_labels: labels,
            keywords_file: config.keywords_file.clone(),
            whisper_model: config.whisper_model.clone(),
            gpu_requested: config.voice.use_gpu,
            gpu_compiled: crate::voice::asr::gpu_compiled_in(),
            gpu_ready: asr_ready.use_gpu,
        };

        let coordinator = Self {
            detector: EndpointDetector::new(
                config.voice.endpoints(),
                crate::voice::pcm::TARGET_SAMPLE_RATE,
            ),
            config,
            sink,
            capture,
            receiver,
            frontend,
            kws,
            asr,
            session: None,
            active,
            next_session: AtomicU64::new(1),
            last_level_at: None,
            state: VoiceState::Loading,
            dropped: 0,
            asr_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stop,
        };
        Ok((coordinator, ui, asr_ready))
    }

    pub fn state(&self) -> &VoiceState {
        &self.state
    }

    /// 主循环：拿采集数据 → 重采样 → VAD → KWS → 端点后提交 ASR。
    ///
    /// 返回 `Err` 表示采集/解码出了不可恢复的问题（设备断开等）。
    pub fn run(mut self) -> Result<(), String> {
        self.set_state(VoiceState::Listening);
        loop {
            if self.stop.load(Ordering::SeqCst) {
                self.set_state(VoiceState::Off);
                self.capture.stop();
                return Ok(());
            }
            // 识别完成（或失败）后回到监听：ASR 回调会把 active 清零。
            if self.state == VoiceState::Transcribing && self.active.load(Ordering::SeqCst) == 0 {
                self.set_state(VoiceState::Listening);
            }

            let chunk = match self.receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => chunk,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // 即使没有音频，也要检查设备侧是否报错。
                    if let Some(error) = self.capture.shared().take_error() {
                        let message = format!("麦克风错误：{error}");
                        self.set_state(VoiceState::Error(message.clone()));
                        self.capture.stop();
                        return Err(message);
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // 采集端已释放（关闭麦克风）。
                    self.set_state(VoiceState::Off);
                    return Ok(());
                }
            };

            if self.handle_overflow() {
                continue;
            }

            let samples = match self.frontend.process(&chunk.samples) {
                Ok(samples) => samples,
                Err(error) => {
                    let message = format!("音频处理失败：{error}");
                    self.set_state(VoiceState::Error(message.clone()));
                    self.capture.stop();
                    return Err(message);
                }
            };
            if samples.is_empty() {
                continue;
            }

            let events = self.detector.push(&samples);
            let speech_started = events
                .iter()
                .any(|event| matches!(event, VadEvent::SpeechStarted));

            // 空闲时才听唤醒词；会话期间 KWS 不参与（避免自己打断自己）。
            if self.session.is_none() {
                if let Some(hit) = self.kws.accept(&samples, speech_started) {
                    self.begin_session(hit.keyword);
                }
            }

            if let Some(level) = self.level_event(&samples) {
                self.sink.emit(level);
            }

            for event in events {
                self.handle_vad_event(event);
            }
        }
    }

    /// 溢出的音频不能当成完整句子：丢弃当前会话并如实上报。
    /// 返回 `true` 表示已经丢弃，调用方应跳过这一块。
    fn handle_overflow(&mut self) -> bool {
        let overflow = self.capture.shared().take_overflow_frames();
        if overflow == 0 {
            return false;
        }
        self.dropped += 1;
        let had_session = self.session.is_some();
        let session_id = self
            .session
            .as_ref()
            .map(|session| session.session_id)
            .unwrap_or(0);
        self.session = None;
        self.active.store(0, Ordering::SeqCst);
        self.detector.discard();
        self.kws.reset();
        self.sink.emit(VoiceEvent::Overflow);
        if had_session {
            self.sink.emit(VoiceEvent::Rejected {
                session_id,
                reason: RejectReason::Overflow,
            });
        }
        // 只有在等待/录音时才回到监听；正在识别时不要覆盖状态。
        if !matches!(self.state, VoiceState::Transcribing) {
            self.set_state(VoiceState::Listening);
        }
        true
    }

    fn begin_session(&mut self, keyword: String) {
        let session_id = self.next_session.fetch_add(1, Ordering::SeqCst);
        // 从唤醒这一刻重新开始计一句：唤醒词本身的那段音频不进指令。
        self.detector.start_utterance();
        self.kws.reset();
        self.session = Some(CaptureSession { session_id });
        self.active.store(session_id, Ordering::SeqCst);
        self.sink.emit(VoiceEvent::Woke { keyword });
        self.set_state(VoiceState::Capturing);
    }

    fn handle_vad_event(&mut self, event: VadEvent) {
        match event {
            VadEvent::Waiting | VadEvent::SpeechContinuing | VadEvent::SpeechStarted => {}
            VadEvent::UtteranceEnded { reason } => match reason {
                EndReason::NoSpeechTimeout => {
                    if let Some(session) = self.session.take() {
                        self.dropped += 1;
                        self.active.store(0, Ordering::SeqCst);
                        self.sink.emit(VoiceEvent::Rejected {
                            session_id: session.session_id,
                            reason: RejectReason::NoSpeech,
                        });
                        self.set_state(VoiceState::Listening);
                    }
                }
                EndReason::Silence | EndReason::MaxDuration => self.finish_session(),
            },
        }
    }

    fn finish_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        // 「太短」按**有声**时长判，而不是总长度：静音收尾与补白不算语音。
        let voiced = self.detector.last_voiced_seconds();
        let samples = self.detector.take_utterance();
        let min_speech = self.config.voice.min_speech_ms as f32 / 1000.0;

        // 太短：短噪声/误触发，不提交，也不去打扰 ASR。
        if voiced < min_speech {
            self.dropped += 1;
            self.active.store(0, Ordering::SeqCst);
            self.sink.emit(VoiceEvent::Rejected {
                session_id: session.session_id,
                reason: RejectReason::TooShort,
            });
            self.set_state(VoiceState::Listening);
            return;
        }

        self.set_state(VoiceState::Transcribing);
        let sink = self.sink.clone();
        let active = self.active.clone();
        let asr_done = self.asr_done.clone();
        let session_id = session.session_id;
        let reply: crate::voice::asr::ReplySink = Box::new(move |result| {
            // 过期检查：会话已被取消/关闭，或已经开了新一轮 → 结果直接丢弃。
            if !is_current_session(&active, session_id) {
                asr_done.store(true, Ordering::SeqCst);
                return;
            }
            match result {
                Ok(transcript) => deliver(&sink, transcript),
                Err(error) => sink.emit(VoiceEvent::Rejected {
                    session_id,
                    reason: RejectReason::Failed(error.to_string()),
                }),
            }
            // 这里**不清空** `active`：工作线程随后还要用它判断结果是否有效，
            // 由工作线程接受后再清零。清洁时机错了会吞掉每一条语音指令。
            asr_done.store(true, Ordering::SeqCst);
        });

        if let Err(error) = self.asr.submit(session_id, samples, reply) {
            self.active.store(0, Ordering::SeqCst);
            self.sink.emit(VoiceEvent::Rejected {
                session_id,
                reason: RejectReason::Failed(error.to_string()),
            });
            self.set_state(VoiceState::Listening);
        }
    }

    fn level_event(&mut self, samples: &[f32]) -> Option<VoiceEvent> {
        let now = Instant::now();
        let due = self
            .last_level_at
            .map(|last| now.duration_since(last) >= Duration::from_millis(100))
            .unwrap_or(true);
        if !due {
            return None;
        }
        self.last_level_at = Some(now);
        Some(VoiceEvent::Level {
            level_db: crate::voice::pcm::rms_db(samples),
            floor_db: self.detector.floor_db(),
        })
    }

    fn set_state(&mut self, state: VoiceState) {
        if self.state == state {
            return;
        }
        self.state = state.clone();
        self.sink.emit(VoiceEvent::StateChanged(state));
    }

    /// 已丢弃的会话数（诊断）。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// 把识别结果按「是否可提交」转换成事件。
fn deliver(sink: &SharedSink, transcript: Transcript) {
    if !transcript.is_submittable() {
        sink.emit(VoiceEvent::Rejected {
            session_id: transcript.session_id,
            reason: RejectReason::EmptyTranscript,
        });
        return;
    }
    sink.emit(VoiceEvent::Final {
        session_id: transcript.session_id,
        text: transcript.text,
        audio_seconds: transcript.audio_seconds,
        decode_ms: transcript.decode_ms,
    });
}

/// 启动成功后给界面看的采集信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureInfoForUi {
    pub device_name: String,
    pub sample_rate: u32,
    pub resampled: bool,
    pub wake_labels: Vec<String>,
    pub keywords_file: PathBuf,
    pub whisper_model: PathBuf,
    /// 设置里是否要求 GPU。
    pub gpu_requested: bool,
    /// 本构建是否编译了 CUDA 后端。
    pub gpu_compiled: bool,
    /// 模型实际是否在用 GPU（由 ASR 线程回报）。
    pub gpu_ready: bool,
}

/// 对外暴露「启动语音」的统一入口：在工作线程里起一个新的音频线程。
///
/// 返回值里的 `handle` 用来停止：置位停止标记 → 上层 drop `Capture`
/// （`Coordinator` 被 move 进线程，drop 时释放设备与模型）。
pub struct VoiceHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    /// 加载完成前为 `None`；拿到后由 `VoiceEvent::Ready` 通知界面。
    pub capture: Option<CaptureInfoForUi>,
}

impl VoiceHandle {
    /// 请求停止，并**不让界面等模型加载**。
    ///
    /// 线程可能正卡在模型加载里（几百毫秒到几秒），join 会把这期间的用户操作
    /// 全部堵住。这里只置位停止标记并放弃 join 句柄：线程会在下一个检查点
    /// 退出，退出时 drop `Coordinator` 自动释放设备与模型。
    /// 已经结束的线程仍然 join，避免留下僵尸句柄。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            if join.is_finished() {
                let _ = join.join();
            }
            // 否则分离：线程负责自己收尾（run 循环与 Drop 都会看到 stop 标记）。
        }
    }

    pub fn is_running(&self) -> bool {
        self.join
            .as_ref()
            .map(|join| !join.is_finished())
            .unwrap_or(false)
    }
}

impl Drop for VoiceHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动语音线程。模型加载与设备打开在**新线程**上做，
/// 结果通过 `on_state` 回传（加载可能要几百毫秒，不能堵住 worker 主循环）。
pub fn spawn(
    config: CoordinatorConfig,
    sink: SharedSink,
    active: ActiveSession,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Result<VoiceHandle, String> {
    // 设备与模型必须在同一个线程里创建并持有：`cpal::Stream` 不是 `Send`。
    //
    // 这里**不等待**加载完成：加载要几百毫秒，堵住 worker 会让界面按钮没反应。
    // 结果通过 `sink` 以状态事件回传（Loading → Listening / Error），
    // 失败时状态就是 `Error`，不会出现「显示在监听但模型没加载」。
    let sink_ready = sink.clone();
    let sink_fail = sink.clone();
    let stop_thread = stop.clone();
    let join = std::thread::Builder::new()
        .name("cuava-voice".to_string())
        .spawn(move || {
            sink_ready.emit(VoiceEvent::StateChanged(VoiceState::Loading));
            match Coordinator::start(config, sink_ready.clone(), active, stop_thread) {
                Ok((coordinator, info, _asr_ready)) => {
                    sink_ready.emit(VoiceEvent::Ready(Box::new(info)));
                    if let Err(error) = coordinator.run() {
                        sink_ready.emit(VoiceEvent::StateChanged(VoiceState::Error(error)));
                    }
                }
                Err(error) => {
                    sink_fail.emit(VoiceEvent::StateChanged(VoiceState::Error(error)));
                }
            }
        })
        .map_err(|error| format!("无法启动语音线程：{error}"))?;

    Ok(VoiceHandle {
        stop,
        join: Some(join),
        capture: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_labels_are_user_readable() {
        assert!(VoiceState::Off.label().contains("关闭"));
        assert!(VoiceState::Listening.label().contains("唤醒"));
        assert!(VoiceState::Capturing.label().contains("请说"));
        assert!(VoiceState::Error("模型缺失".into())
            .label()
            .contains("模型缺失"));
        assert_eq!(VoiceState::Capturing.code(), "capturing");
    }

    /// 迟到的识别结果：会话号对不上时必须被判定为过期。
    #[test]
    fn stale_results_are_detected() {
        let active: ActiveSession = Arc::new(AtomicU64::new(0));
        assert!(
            !is_current_session(&active, 1),
            "没有进行中的会话时一律过期"
        );

        active.store(7, Ordering::SeqCst);
        assert!(is_current_session(&active, 7));
        assert!(!is_current_session(&active, 6), "旧会话的结果必须过期");
        assert!(!is_current_session(&active, 8), "未来会话的结果也不能接受");

        // 取消/关麦：清零之后，之前的会话号立刻失效。
        active.store(0, Ordering::SeqCst);
        assert!(!is_current_session(&active, 7));
    }

    #[test]
    fn reject_reasons_have_messages() {
        for reason in [
            RejectReason::NoSpeech,
            RejectReason::TooShort,
            RejectReason::EmptyTranscript,
            RejectReason::Failed("x".into()),
            RejectReason::Stale,
            RejectReason::Overflow,
        ] {
            assert!(!reason.message().is_empty());
        }
    }

    /// 非空转录才会变成 `Final`；空/噪声转录必须走 `Rejected`。
    #[test]
    fn deliver_gates_on_submittable_text() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            SharedSink::new(move |event| events.lock().unwrap().push(event))
        };

        deliver(
            &sink,
            Transcript {
                session_id: 1,
                text: "打开浏览器".to_string(),
                audio_seconds: 1.0,
                decode_ms: 10,
                language_id: 1,
                no_speech: vec![],
            },
        );
        deliver(
            &sink,
            Transcript {
                session_id: 2,
                text: "谢谢观看。".to_string(),
                audio_seconds: 1.0,
                decode_ms: 10,
                language_id: 1,
                no_speech: vec![],
            },
        );

        let events = events.lock().unwrap();
        assert!(matches!(events[0], VoiceEvent::Final { session_id: 1, .. }));
        assert!(matches!(
            events[1],
            VoiceEvent::Rejected {
                session_id: 2,
                reason: RejectReason::EmptyTranscript
            }
        ));
    }
}
