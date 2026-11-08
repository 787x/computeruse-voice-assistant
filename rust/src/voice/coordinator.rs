//! 语音会话协调器：把采集、KWS、VAD、ASR 串成一个有明确状态的会话。
//!
//! 状态机（任务书要求可合理合并，这里保留五个对用户有意义的状态）：
//!
//! ```text
//! Off ──启用──> Loading ──成功──> Listening ──唤醒──> Capturing ──端点──> Transcribing
//!  ^                 │                ^                  │                    │
//!  │                 └──失败──> Error │<── 超时/短噪声 ───┘                    │
//!  └────────────── 关闭 ──────────────┴──────────── 完成/失败/取消 <──────────┘
//! ```
//!
//! 不变量：
//!  - **同一时刻只有一个可提交的语音会话**：每次进入 Capturing 分配新的会话号；
//!    ASR 返回时若归属对不上，结果直接丢弃。
//!  - 每一句语音的身份是 `(instance, session)`：`instance` 是**麦克风实例号**
//!    （每次「打开麦克风」都是新实例），`session` 在实例内自增。
//!    只靠会话号不行——每次启动都从 1 开始，关麦后立刻重开时，
//!    旧实例的 `session = 1` 会冒充新实例的 `session = 1`。
//!  - 任何结束路径（有效 Final / 空转录 / 推理错误 / 太短 / 溢出 / 取消）
//!    都必须让状态回到 `Listening`，不能永久卡在 `Transcribing`。
//!  - 模型加载失败不会伪装成「正在监听」：状态直接进 `Error`，
//!    并且文本 Agent 完全不受影响。
//!
//! 这个模块不直接操作 Slint：状态变化通过 `VoiceEvent` 发给 worker，
//! 由 worker 决定怎么显示。这样音频线程可以在没有界面的情况下确定性地测试。

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::voice::asr::{
    AsrBackend, AsrConfig, AsrError, AsrWorker, InferenceCancel, ReadyInfo as AsrReady, Transcript,
};
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

/// 这一轮聆听的**目的**。
///
/// PR6 只有一种聆听方式：必须听到唤醒词才开始录指令。PR7 需要两种"直通"聆听：
///  - 播报期间听打断词（用户说「等一下」不该先说唤醒词）；
///  - 权限确认期间听「允许 / 拒绝」。
///
/// 直通模式**只在明确的时刻**打开（正在播报 / 有挂起的确认请求），
/// 其它时候仍然要求唤醒词——否则等于让麦克风一直把环境语音送去识别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenPurpose {
    /// 常规：必须听到唤醒词。
    #[default]
    Wake,
    /// 用户点击麦克风后的一次直通指令。
    Manual,
    Followup,
    /// 播报期间听打断短语。
    BargeIn,
    /// 权限确认期间听「允许 / 拒绝」。
    Confirm,
}

impl ListenPurpose {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Manual => "manual",
            Self::Followup => "followup",
            Self::BargeIn => "barge-in",
            Self::Confirm => "confirm",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Wake => "唤醒词",
            Self::Manual => "点击识别",
            Self::Followup => "可以继续说",
            Self::BargeIn => "打断聆听",
            Self::Confirm => "语音确认",
        }
    }

    /// 是否**不要求**唤醒词。
    pub fn is_direct(&self) -> bool {
        !matches!(self, Self::Wake)
    }
}

/// 聆听模式：工作线程写、音频线程读。
///
/// 用共享状态而不是通道：模式是"当前值"，音频线程每个音频块都读一次，
/// 通道会让"切回唤醒词模式"这种操作在队列里延迟生效。
///
/// `request_id` 只在 [`ListenPurpose::Confirm`] 时非零，用来把这一轮识别
/// **绑定到具体的确认请求**上：请求结束后迟到的识别结果不能被当成
/// 下一个请求的答复。
#[derive(Clone, Debug, Default)]
pub struct ListenMode(
    Arc<Mutex<(ListenPurpose, u64)>>,
    Arc<Mutex<Option<FollowupAdmission>>>,
);
#[derive(Clone, Debug)]
struct FollowupAdmission {
    generation: u64,
    deadline: Instant,
    started: Option<Instant>,
}

impl ListenMode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, purpose: ListenPurpose, request_id: u64) {
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = (purpose, request_id);
    }

    pub fn get(&self) -> (ListenPurpose, u64) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn followup_deadline(&self, generation: u64, deadline: Instant) {
        let mut state = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if state.as_ref().is_none_or(|s| s.generation != generation) {
            *state = Some(FollowupAdmission {
                generation,
                deadline,
                started: None,
            });
        }
    }
    pub fn admit_followup(&self, generation: u64, now: Instant) -> bool {
        let mut state = self.1.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = state
            .as_mut()
            .filter(|s| s.generation == generation && now < s.deadline && s.started.is_none())
        else {
            return false;
        };
        s.started = Some(now);
        true
    }
    pub fn followup_started(&self, generation: u64) -> bool {
        self.1
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|s| {
                s.generation == generation && s.started.is_some_and(|start| start < s.deadline)
            })
    }
    pub fn clear_followup(&self) {
        *self.1.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub fn purpose(&self) -> ListenPurpose {
        self.get().0
    }
}

/// 麦克风实例号：每次「打开麦克风」分配一个新的。
///
/// 用来区分**不同**的采集实例：关麦后立刻重开时，旧线程还可能吐出
/// `Ready` / `StateChanged` / `Final` / `Rejected`，这些事件绝不能影响新实例。
pub type VoiceInstance = u64;

/// 一句语音的全局身份：实例号 + 实例内自增会话号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoiceTurn {
    pub instance: VoiceInstance,
    pub session: u64,
}

/// 当前可提交的语音会话（同一麦克风实例内跨线程共享）。
///
/// 由协调器写入（开始/结束一句），worker 可以清空它来取消当前这句。
/// 所有清除操作都做**归属核对**：旧回合的迟到完成事件不能清掉新回合。
///
/// **一份 `ActiveSession` 只属于一个麦克风实例**：`clear()` 是无条件的，
/// 若两个时代的实例共享同一份，旧线程的 `shutdown()` / `fail()` 收尾就会把
/// 新实例正在识别的那一句抹掉（表现为新实例的 Final 被当成过期丢弃）。
/// 调用方每次 `spawn()` 必须传一份新的；见 `ui::worker::VoiceRoute::open`。
#[derive(Clone, Debug, Default)]
pub struct ActiveSession(Arc<Mutex<Option<VoiceTurn>>>, Arc<Mutex<Option<VoiceTurn>>>);

impl ActiveSession {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<VoiceTurn>> {
        // 这里的临界区只有赋值/比较，不会 panic；即使中毒也照常可用。
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 开始一句语音。
    pub fn begin(&self, turn: VoiceTurn) {
        *self.lock() = Some(turn);
    }

    /// 取消当前会话（关麦 / 用户取消）。
    pub fn clear(&self) {
        *self.lock() = None;
    }

    pub fn request_finish(&self, turn: VoiceTurn) {
        if self.is_current(turn) {
            *self.1.lock().unwrap_or_else(|e| e.into_inner()) = Some(turn);
        }
    }
    fn take_finish(&self) -> Option<VoiceTurn> {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn current(&self) -> Option<VoiceTurn> {
        *self.lock()
    }

    pub fn is_current(&self, turn: VoiceTurn) -> bool {
        *self.lock() == Some(turn)
    }

    /// 只在仍是这一句时清除；返回是否真的清了。
    pub fn clear_if_current(&self, turn: VoiceTurn) -> bool {
        let mut guard = self.lock();
        if *guard == Some(turn) {
            *guard = None;
            true
        } else {
            false
        }
    }
}

/// 协调器发给 worker 的事件。
#[derive(Debug, Clone)]
pub struct VoiceEvent {
    /// 产生事件的麦克风实例。worker 据此丢弃旧实例的迟到事件。
    pub instance: VoiceInstance,
    pub kind: VoiceEventKind,
}

/// 事件内容。
#[derive(Debug, Clone)]
pub enum VoiceEventKind {
    StateChanged(VoiceState),
    /// 唤醒词命中，开始录音。
    Woke {
        turn: VoiceTurn,
        keyword: String,
    },
    /// 直通聆听开始（打断词 / 语音确认），没有唤醒词。
    DirectStarted {
        turn: VoiceTurn,
        purpose: ListenPurpose,
        /// 绑定的确认请求 id（`ListenPurpose::BargeIn` 时为 0）。
        request_id: u64,
    },
    /// 识别出的最终文本（**只有它会提交给 Agent**）。
    ///
    /// `purpose` 是**开始录这一句时**的聆听目的（不是现在的模式）：
    /// 模式可能在识别期间变化，结果必须按"当时为什么听"来处置。
    Final {
        turn: VoiceTurn,
        text: String,
        audio_seconds: f32,
        decode_ms: u64,
        purpose: ListenPurpose,
        /// 这一句绑定到的确认请求 id（`Confirm` 之外为 0）。
        request_id: u64,
    },
    /// 一次语音会话结束但没有可提交文本（附原因，给用户看）。
    Rejected {
        turn: VoiceTurn,
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

/// 「这一次识别已经结束」的回报（由 ASR 线程写入，音频线程消费）。
///
/// 必须带回合身份：只用一个 bool 的话，上一句**迟到**的完成事件会把
/// 新一轮从 `Transcribing` 提前拉回 `Listening`。
#[derive(Clone, Debug, Default)]
struct AsrCompletion(Arc<Mutex<Option<VoiceTurn>>>);

impl AsrCompletion {
    fn report(&self, turn: VoiceTurn) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(turn);
    }

    fn take(&self) -> Option<VoiceTurn> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// 结果回调：在 ASR 线程上调用，调用方必须很快返回（只做转发，不做 UI 操作）。
pub type ReplySink = Box<dyn FnOnce(Result<Transcript, AsrError>) + Send>;

/// 提交一段音频去识别（生产实现是 [`AsrWorker`]，测试里是可注入的假实现）。
pub trait AsrSubmitter: Send {
    fn submit(
        &self,
        turn: VoiceTurn,
        samples: Vec<f32>,
        cancel: InferenceCancel,
        reply: ReplySink,
    ) -> Result<(), AsrError>;
}

impl AsrSubmitter for AsrWorker {
    fn submit(
        &self,
        turn: VoiceTurn,
        samples: Vec<f32>,
        cancel: InferenceCancel,
        reply: ReplySink,
    ) -> Result<(), AsrError> {
        AsrWorker::submit(self, turn.session, samples, cancel, reply)
    }
}

/// 正在识别的这一句。
struct Inflight {
    turn: VoiceTurn,
    cancel: InferenceCancel,
}

/// 一句语音的状态机（不持有设备与模型）。
///
/// 抽出来的理由很具体：任务要求用**确定性**测试验证「迟到结果、空转录、
/// 推理错误、取消、旧实例事件」这些生命周期行为，而它们不应该依赖
/// 真麦克风、真 GPU 或真人录音。
pub struct SessionMachine {
    instance: VoiceInstance,
    sink: SharedSink,
    active: ActiveSession,
    asr: Box<dyn AsrSubmitter>,
    min_speech_ms: u32,
    /// 正在录的这一句。
    session: Option<VoiceTurn>,
    /// 这一句的聆听目的与绑定的确认请求 id（开始录时快照，之后不再变）。
    purpose: ListenPurpose,
    request_id: u64,
    /// 已提交、等结果的那一句。
    inflight: Option<Inflight>,
    completion: AsrCompletion,
    next_session: u64,
    state: VoiceState,
    dropped: u64,
}

impl SessionMachine {
    pub fn new(
        instance: VoiceInstance,
        sink: SharedSink,
        active: ActiveSession,
        asr: Box<dyn AsrSubmitter>,
        min_speech_ms: u32,
    ) -> Self {
        Self {
            instance,
            sink,
            active,
            asr,
            min_speech_ms,
            session: None,
            purpose: ListenPurpose::Wake,
            request_id: 0,
            inflight: None,
            completion: AsrCompletion::default(),
            next_session: 0,
            state: VoiceState::Loading,
            dropped: 0,
        }
    }

    /// 当前这一句的聆听目的（诊断/测试用）。
    pub fn purpose(&self) -> ListenPurpose {
        self.purpose
    }

    pub fn state(&self) -> &VoiceState {
        &self.state
    }

    /// 已丢弃的会话数（诊断）。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 是否需要 VAD/KWS（识别期间不吃音频，避免在解码时积累下一句）。
    pub fn wants_audio(&self) -> bool {
        matches!(self.state, VoiceState::Listening | VoiceState::Capturing)
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub fn is_capturing(&self) -> bool {
        self.state == VoiceState::Capturing
    }

    /// 模型与设备就绪：进入监听。
    pub fn start_listening(&mut self) {
        self.set_state(VoiceState::Listening);
    }

    /// 关闭麦克风：取消一切在途结果，并如实进入 Off。
    pub fn shutdown(&mut self) {
        if let Some(inflight) = self.inflight.take() {
            inflight.cancel.cancel();
        }
        self.session = None;
        self.active.clear();
        self.completion.take();
        self.set_state(VoiceState::Off);
    }

    /// 不可恢复的错误（设备断开、音频处理失败）。
    pub fn fail(&mut self, message: String) {
        if let Some(inflight) = self.inflight.take() {
            inflight.cancel.cancel();
        }
        self.session = None;
        self.active.clear();
        self.set_state(VoiceState::Error(message));
    }

    /// 唤醒：开始录一句（常规路径，必须已经听到唤醒词）。
    pub fn begin(&mut self, keyword: String) {
        let Some(turn) = self.open_session(ListenPurpose::Wake, 0) else {
            return;
        };
        self.emit(VoiceEventKind::Woke { turn, keyword });
    }

    /// 直通聆听：不要求唤醒词，直接开始录一句。
    ///
    /// 只在「正在播报（听打断词）」与「有挂起的权限确认（听允许/拒绝）」时使用。
    /// 返回 `false` 表示当前状态不允许开始（不在 Listening / 已有一句在录）。
    pub fn begin_direct(&mut self, purpose: ListenPurpose, request_id: u64) -> bool {
        if !purpose.is_direct() {
            return false;
        }
        let Some(turn) = self.open_session(purpose, request_id) else {
            return false;
        };
        self.emit(VoiceEventKind::DirectStarted {
            turn,
            purpose,
            request_id,
        });
        true
    }

    /// 分配会话号、登记归属并进入 Capturing。状态不对时返回 `None`。
    fn open_session(&mut self, purpose: ListenPurpose, request_id: u64) -> Option<VoiceTurn> {
        if self.state != VoiceState::Listening {
            return None;
        }
        self.next_session += 1;
        let turn = VoiceTurn {
            instance: self.instance,
            session: self.next_session,
        };
        self.session = Some(turn);
        self.purpose = purpose;
        self.request_id = if purpose.is_direct() { request_id } else { 0 };
        self.active.begin(turn);
        self.set_state(VoiceState::Capturing);
        Some(turn)
    }

    /// 端点到了：太短就丢弃，否则交给 ASR。
    pub fn finish(&mut self, voiced_seconds: f32, samples: Vec<f32>) {
        let Some(turn) = self.session.take() else {
            return;
        };
        // 录制期间被取消（关麦/用户取消）：音频不再提交。
        if !self.active.is_current(turn) {
            self.resume();
            return;
        }
        let min_speech = self.min_speech_ms as f32 / 1000.0;
        if samples.is_empty() || voiced_seconds < min_speech {
            self.reject_turn(turn, RejectReason::TooShort);
            return;
        }

        let cancel = InferenceCancel::new();
        let reply = self.reply_sink(turn, self.purpose, self.request_id);
        self.inflight = Some(Inflight {
            turn,
            cancel: cancel.clone(),
        });
        match self.asr.submit(turn, samples, cancel, reply) {
            Ok(()) => self.set_state(VoiceState::Transcribing),
            Err(error) => {
                self.inflight = None;
                self.reject_turn(turn, RejectReason::Failed(error.to_string()));
            }
        }
    }

    /// 丢弃当前正在录的这一句（无语音超时等）。
    pub fn reject(&mut self, reason: RejectReason) {
        let Some(turn) = self.session.take() else {
            return;
        };
        self.reject_turn(turn, reason);
    }

    /// 采集溢出：只影响正在录的这一句；已经在识别的音频不受影响。
    pub fn overflow(&mut self) {
        self.emit(VoiceEventKind::Overflow);
        if let Some(turn) = self.session.take() {
            self.reject_turn(turn, RejectReason::Overflow);
        }
    }

    /// 每个 tick 调一次：处理「识别完成」与「外部取消」。
    ///
    /// 返回 `true` 表示刚回到监听（调用方应重置 VAD 与 KWS 状态）。
    pub fn poll(&mut self) -> bool {
        // 1) 识别结束（成功或失败）：ASR 线程已经回报了这一句的归属。
        if let Some(completed) = self.completion.take() {
            if self.inflight.as_ref().map(|inflight| inflight.turn) == Some(completed) {
                self.inflight = None;
                self.resume();
                return true;
            }
            // 旧回合的迟到完成事件：丢掉，不影响当前回合。
        }

        // 2) 识别期间被取消（关麦/用户取消）：立刻让底层推理停下。
        let cancelled = self
            .inflight
            .as_ref()
            .is_some_and(|inflight| !self.active.is_current(inflight.turn));
        if cancelled {
            if let Some(inflight) = self.inflight.take() {
                inflight.cancel.cancel();
            }
            self.resume();
            return true;
        }

        // 3) 录制期间被取消。
        let stale = self
            .session
            .is_some_and(|turn| !self.active.is_current(turn));
        if stale {
            self.session = None;
            self.resume();
            return true;
        }
        false
    }

    fn reject_turn(&mut self, turn: VoiceTurn, reason: RejectReason) {
        self.dropped += 1;
        self.active.clear_if_current(turn);
        self.emit(VoiceEventKind::Rejected { turn, reason });
        self.resume();
    }

    /// 回到监听：只有确实处在会话状态时才切换（避免覆盖 Off/Error）。
    fn resume(&mut self) {
        if matches!(self.state, VoiceState::Capturing | VoiceState::Transcribing) {
            self.set_state(VoiceState::Listening);
        }
    }

    fn reply_sink(&self, turn: VoiceTurn, purpose: ListenPurpose, request_id: u64) -> ReplySink {
        let sink = self.sink.clone();
        let active = self.active.clone();
        let completion = self.completion.clone();
        let instance = self.instance;
        Box::new(move |result| {
            // 归属核对：已取消/关麦/被新会话取代 → 结果直接丢弃。
            if active.is_current(turn) {
                let kind = match result {
                    Ok(transcript) if transcript.is_submittable() => VoiceEventKind::Final {
                        turn,
                        text: transcript.text,
                        audio_seconds: transcript.audio_seconds,
                        decode_ms: transcript.decode_ms,
                        purpose,
                        request_id,
                    },
                    Ok(_) => VoiceEventKind::Rejected {
                        turn,
                        reason: RejectReason::EmptyTranscript,
                    },
                    Err(error) => VoiceEventKind::Rejected {
                        turn,
                        reason: RejectReason::Failed(error.to_string()),
                    },
                };
                sink.emit(VoiceEvent { instance, kind });
            }
            // **上报之后**才让接收方失效：接收端要按这次结果决定是否提交。
            active.clear_if_current(turn);
            completion.report(turn);
        })
    }

    fn emit(&self, kind: VoiceEventKind) {
        self.sink.emit(VoiceEvent {
            instance: self.instance,
            kind,
        });
    }

    fn set_state(&mut self, state: VoiceState) {
        if self.state == state {
            return;
        }
        self.state = state.clone();
        self.emit(VoiceEventKind::StateChanged(state));
    }
}

/// 协调器（跑在音频线程上）。
pub struct Coordinator {
    machine: SessionMachine,
    capture: Capture,
    receiver: Receiver<CapturedChunk>,
    frontend: FrontEnd,
    kws: Kws,
    /// VAD 一直运行：空闲时用来给 KWS 提供「现在是不是说话起点」。
    detector: EndpointDetector,
    last_level_at: Option<Instant>,
    /// 聆听模式（唤醒词 / 打断 / 语音确认），由工作线程设置。
    mode: ListenMode,
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
        instance: VoiceInstance,
        mode: ListenMode,
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
            backend: asr_ready.backend,
            fallback_reason: asr_ready.fallback_reason.clone(),
        };

        let machine = SessionMachine::new(
            instance,
            sink,
            active,
            Box::new(asr),
            config.voice.min_speech_ms,
        );

        let coordinator = Self {
            detector: EndpointDetector::new(
                config.voice.endpoints(),
                crate::voice::pcm::TARGET_SAMPLE_RATE,
            ),
            capture,
            receiver,
            frontend,
            kws,
            machine,
            last_level_at: None,
            mode,
            stop,
        };
        Ok((coordinator, ui, asr_ready))
    }

    pub fn state(&self) -> &VoiceState {
        self.machine.state()
    }

    /// 主循环：拿采集数据 → 重采样 → VAD → KWS → 端点后提交 ASR。
    ///
    /// 返回 `Err` 表示采集/解码出了不可恢复的问题（设备断开等）。
    pub fn run(mut self) -> Result<(), String> {
        self.machine.start_listening();
        loop {
            if self.stop.load(std::sync::atomic::Ordering::SeqCst) {
                self.machine.shutdown();
                self.capture.stop();
                return Ok(());
            }
            if self.machine.poll() {
                // 回到监听：识别期间积压的 VAD/KWS 状态不能串到下一句。
                self.detector.discard();
                self.kws.reset();
            }

            if let Some(turn) = self.machine.active.take_finish() {
                if self.machine.session == Some(turn)
                    && self.machine.active.is_current(turn)
                    && self.machine.is_capturing()
                    && !self.handle_overflow()
                {
                    // 只取本次观察到的队列快照，不等待未来音频。
                    let queued: Vec<_> = self.receiver.try_iter().collect();
                    for chunk in queued {
                        let samples = self.frontend.process(&chunk.samples)?;
                        let events = self.detector.push(&samples);
                        for event in events {
                            self.handle_vad_event(event);
                        }
                        if !self.machine.is_capturing() {
                            break;
                        }
                    }
                    if self.machine.is_capturing() {
                        let tail = self.frontend.resampler.flush()?;
                        let events = self.detector.push(&tail);
                        for event in events {
                            self.handle_vad_event(event);
                        }
                    }
                    self.detector.finish_now();
                    self.finish_session();
                }
            }
            let chunk = match self.receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => chunk,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // 即使没有音频，也要检查设备侧是否报错。
                    if let Some(error) = self.capture.shared().take_error() {
                        let message = format!("麦克风错误：{error}");
                        self.machine.fail(message.clone());
                        self.capture.stop();
                        return Err(message);
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // 采集端已释放（关闭麦克风）。
                    self.machine.shutdown();
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
                    self.machine.fail(message.clone());
                    self.capture.stop();
                    return Err(message);
                }
            };
            if samples.is_empty() {
                continue;
            }

            if let Some(level) = self.level_event(&samples) {
                self.machine.emit(VoiceEventKind::Level {
                    level_db: level.0,
                    floor_db: level.1,
                });
            }

            // 识别期间不吃音频：Whisper 没有流式输出，这段音频也不会被用到。
            // 继续喂只会让 VAD 在解码时积累下一句，回到监听时状态错位。
            if !self.machine.wants_audio() {
                continue;
            }

            let events = self.detector.push(&samples);
            let speech_started = events
                .iter()
                .any(|event| matches!(event, VadEvent::SpeechStarted));

            /*
             * 空闲时才开始新的一句。两种入口：
             *  - 常规：**必须**听到唤醒词（KWS），会话期间 KWS 不参与（避免自己打断自己）；
             *  - 直通（打断聆听 / 语音确认）：VAD 的说话起点就是开始，
             *    否则用户得先喊唤醒词才能让助手停下来说话。
             *
             * 直通模式只在「正在播报」或「有挂起的确认」时打开（见 worker）。
             */
            if !self.machine.has_session() {
                let (purpose, request_id) = self.mode.get();
                if purpose.is_direct() {
                    if speech_started
                        && self.machine.state() == &VoiceState::Listening
                        && (purpose != ListenPurpose::Followup
                            || self.mode.admit_followup(request_id, Instant::now()))
                        && self.machine.begin_direct(purpose, request_id)
                    {
                        // 从说话起点重新计一句：起点之前那段静音/回声不算进这一句。
                        self.detector.start_utterance();
                        self.kws.reset();
                        // 这一块里的 VAD 事件属于开头，不能立刻当成端点。
                        continue;
                    }
                } else if let Some(hit) = self.kws.accept(&samples, speech_started) {
                    // 从唤醒这一刻重新开始计一句：唤醒词本身那段音频不进指令。
                    self.detector.start_utterance();
                    self.kws.reset();
                    self.machine.begin(hit.keyword);
                    // 这一块里的 VAD 事件属于唤醒前，不能算到新一句上
                    // （否则上一声「端点」会立刻把刚唤醒的会话判成太短）。
                    continue;
                }
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
        self.detector.discard();
        self.kws.reset();
        self.machine.overflow();
        true
    }

    fn handle_vad_event(&mut self, event: VadEvent) {
        match event {
            VadEvent::Waiting | VadEvent::SpeechContinuing | VadEvent::SpeechStarted => {}
            VadEvent::UtteranceEnded { reason } => match reason {
                EndReason::NoSpeechTimeout => self.machine.reject(RejectReason::NoSpeech),
                EndReason::Silence | EndReason::MaxDuration => self.finish_session(),
            },
        }
    }

    fn finish_session(&mut self) {
        if !self.machine.is_capturing() {
            return;
        }
        // 「太短」按**有声**时长判，而不是总长度：静音收尾与补白不算语音。
        let voiced = self.detector.last_voiced_seconds();
        let samples = self.detector.take_utterance();
        self.machine.finish(voiced, samples);
    }

    /// 电平事件（已节流）。
    fn level_event(&mut self, samples: &[f32]) -> Option<(f32, f32)> {
        let now = Instant::now();
        let due = self
            .last_level_at
            .map(|last| now.duration_since(last) >= Duration::from_millis(100))
            .unwrap_or(true);
        if !due {
            return None;
        }
        self.last_level_at = Some(now);
        Some((crate::voice::pcm::rms_db(samples), self.detector.floor_db()))
    }

    /// 已丢弃的会话数（诊断）。
    pub fn dropped(&self) -> u64 {
        self.machine.dropped()
    }
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
    /// 实际用于推理的后端（由 ASR 线程回报，不是用户期望值）。
    pub backend: AsrBackend,
    /// 降到 CPU 的原因（构建未启用 CUDA / 没有可用设备 / CUDA 初始化失败）；
    /// 成功用 GPU 或本来就用 CPU 时为 `None`。
    pub fallback_reason: Option<String>,
}

/// 对外暴露「启动语音」的统一入口：在工作线程里起一个新的音频线程。
///
/// 返回值里的 `handle` 用来停止：置位停止标记 → 上层 drop `Capture`
/// （`Coordinator` 被 move 进线程，drop 时释放设备与模型）。
pub struct VoiceHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    /// 本实例的麦克风实例号（事件归属据此区分）。
    pub instance: VoiceInstance,
}

/// 停止时最多等音频线程多久。
///
/// 主循环 20ms 一轮，正常收尾很快；ASR 线程靠 abort 回调尽快结束推理，
/// 最坏情况由 [`crate::voice::asr`] 里的有界 join 兜底。
///
/// 为什么允许在工作线程里等一小会儿：关麦后立刻重开时，如果旧线程还握着
/// 麦克风/GPU，就会同时存在两个实例。等待有上限，且 Slint 界面线程不受影响。
const STOP_JOIN_BUDGET: Duration = Duration::from_millis(700);

impl VoiceHandle {
    /// 请求停止，并有上限地等待收尾。
    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let Some(join) = self.join.take() else {
            return;
        };
        let deadline = Instant::now() + STOP_JOIN_BUDGET;
        while !join.is_finished() {
            if Instant::now() >= deadline {
                // 分离：线程在下一个检查点自行退出并释放设备与模型。
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = join.join();
    }

    pub fn is_running(&self) -> bool {
        self.join
            .as_ref()
            .map(|join| !join.is_finished())
            .unwrap_or(false)
    }

    /// 供测试：构造一个没有后台线程的句柄。
    ///
    /// 用来确定性地验证「关麦是否真的置位停止标记」，不必打开真实麦克风。
    #[cfg(test)]
    pub fn for_test(stop: Arc<std::sync::atomic::AtomicBool>, instance: VoiceInstance) -> Self {
        Self {
            stop,
            join: None,
            instance,
        }
    }
}

impl Drop for VoiceHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动语音线程。模型加载与设备打开在**新线程**上做，
/// 结果通过 `sink` 回传（加载可能要几百毫秒，不能堵住 worker 主循环）。
///
/// `active` 必须是一个**新的** [`ActiveSession`]：它只属于这个实例，
/// 旧实例的线程继续持有自己的那份，收尾时清不到这里。
pub fn spawn(
    config: CoordinatorConfig,
    sink: SharedSink,
    active: ActiveSession,
    stop: Arc<std::sync::atomic::AtomicBool>,
    instance: VoiceInstance,
    mode: ListenMode,
) -> Result<VoiceHandle, String> {
    // 设备与模型必须在同一个线程里创建并持有：`cpal::Stream` 不是 `Send`。
    //
    // 这里**不等待**加载完成：加载要几百毫秒，堵住 worker 会让界面按钮没反应。
    // 结果通过 `sink` 以状态事件回传（Loading → Listening / Error），
    // 失败时状态就是 `Error`，不会出现「显示在监听但模型没加载」。
    let sink_loading = sink.clone();
    let sink_fail = sink.clone();
    let stop_thread = stop.clone();
    let join = std::thread::Builder::new()
        .name(format!("cuava-voice-{instance}"))
        .spawn(move || {
            sink_loading.emit(VoiceEvent {
                instance,
                kind: VoiceEventKind::StateChanged(VoiceState::Loading),
            });
            match Coordinator::start(
                config,
                sink_loading.clone(),
                active,
                stop_thread,
                instance,
                mode,
            ) {
                Ok((coordinator, info, _asr_ready)) => {
                    sink_loading.emit(VoiceEvent {
                        instance,
                        kind: VoiceEventKind::Ready(Box::new(info)),
                    });
                    if let Err(error) = coordinator.run() {
                        // 运行期错误已经由协调器以 `StateChanged(Error)` 上报；
                        // 这里只留一条诊断日志，不重复制造界面事件。
                        eprintln!("[voice] 语音线程结束：{error}");
                    }
                }
                Err(error) => {
                    sink_fail.emit(VoiceEvent {
                        instance,
                        kind: VoiceEventKind::StateChanged(VoiceState::Error(error)),
                    });
                }
            }
        })
        .map_err(|error| format!("无法启动语音线程：{error}"))?;

    Ok(VoiceHandle {
        stop,
        join: Some(join),
        instance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可注入的假 ASR：不加载模型，把「这一句要识别」记下来，
    /// 由测试决定什么时候、以什么结果回调。
    struct FakeAsr {
        submitted: Arc<Mutex<Vec<VoiceTurn>>>,
        fail_submit: bool,
        /// 提交时立刻回调的结果（模拟极快的推理）。
        immediate: Option<Result<String, String>>,
    }

    impl FakeAsr {
        fn new() -> Self {
            Self {
                submitted: Arc::new(Mutex::new(Vec::new())),
                fail_submit: false,
                immediate: None,
            }
        }
    }

    impl AsrSubmitter for FakeAsr {
        fn submit(
            &self,
            turn: VoiceTurn,
            _samples: Vec<f32>,
            cancel: InferenceCancel,
            reply: ReplySink,
        ) -> Result<(), AsrError> {
            if self.fail_submit {
                return Err(AsrError::Stopped);
            }
            self.submitted.lock().unwrap().push(turn);
            if let Some(result) = &self.immediate {
                let result = match result {
                    Ok(text) => Ok(Transcript {
                        session_id: turn.session,
                        text: text.clone(),
                        audio_seconds: 1.0,
                        decode_ms: 5,
                        language_id: 1,
                        no_speech: vec![],
                    }),
                    Err(error) => Err(AsrError::Decode(error.clone())),
                };
                assert!(!cancel.is_cancelled(), "提交时不该已经被取消");
                reply(result);
            }
            Ok(())
        }
    }

    /// 记录事件，供断言使用。
    #[derive(Clone, Default)]
    struct Recorder {
        events: Arc<Mutex<Vec<VoiceEventKind>>>,
    }

    impl Recorder {
        fn sink(&self) -> SharedSink {
            let events = self.events.clone();
            SharedSink::new(move |event| events.lock().unwrap().push(event.kind))
        }

        fn kinds(&self) -> Vec<VoiceEventKind> {
            self.events.lock().unwrap().clone()
        }

        fn finals(&self) -> Vec<(VoiceTurn, String)> {
            self.kinds()
                .into_iter()
                .filter_map(|kind| match kind {
                    VoiceEventKind::Final { turn, text, .. } => Some((turn, text)),
                    _ => None,
                })
                .collect()
        }

        fn rejects(&self) -> Vec<(VoiceTurn, RejectReason)> {
            self.kinds()
                .into_iter()
                .filter_map(|kind| match kind {
                    VoiceEventKind::Rejected { turn, reason } => Some((turn, reason)),
                    _ => None,
                })
                .collect()
        }
    }

    /// 一个手动控制的 ASR：把 reply 存起来，由测试决定何时回调。
    struct DeferredAsr {
        pending: Arc<Mutex<Vec<(VoiceTurn, ReplySink)>>>,
    }

    impl AsrSubmitter for DeferredAsr {
        fn submit(
            &self,
            turn: VoiceTurn,
            _samples: Vec<f32>,
            _cancel: InferenceCancel,
            reply: ReplySink,
        ) -> Result<(), AsrError> {
            self.pending.lock().unwrap().push((turn, reply));
            Ok(())
        }
    }

    fn transcript(text: &str, turn: VoiceTurn) -> Result<Transcript, AsrError> {
        Ok(Transcript {
            session_id: turn.session,
            text: text.to_string(),
            audio_seconds: 1.0,
            decode_ms: 5,
            language_id: 1,
            no_speech: vec![],
        })
    }

    const AUDIO: [f32; 4] = [0.1, -0.1, 0.2, -0.2];

    fn machine_with<A: AsrSubmitter + 'static>(
        instance: VoiceInstance,
        recorder: &Recorder,
        active: &ActiveSession,
        asr: A,
    ) -> SessionMachine {
        SessionMachine::new(
            instance,
            recorder.sink(),
            active.clone(),
            Box::new(asr),
            250,
        )
    }

    #[test]
    fn manual_request_is_bound_to_turn_and_submission_is_once() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr::new();
        let submitted = asr.submitted.clone();
        let mut machine = machine_with(7, &recorder, &active, asr);
        machine.start_listening();
        machine.begin("wake".into());
        let old = active.current().unwrap();
        active.request_finish(old);
        active.request_finish(old);
        assert_eq!(active.take_finish(), Some(old));
        assert_eq!(active.take_finish(), None);
        machine.finish(1.0, AUDIO.to_vec());
        machine.finish(1.0, AUDIO.to_vec());
        assert_eq!(submitted.lock().unwrap().len(), 1);
        active.clear();
        machine.poll();
        machine.begin("next".into());
        let next = active.current().unwrap();
        assert_ne!(old, next);
        active.request_finish(old);
        assert_eq!(active.take_finish(), None);
        assert!(active.is_current(next));
    }

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

    /// 迟到的识别结果：归属对不上时必须被判定为过期。
    #[test]
    fn stale_results_are_detected() {
        let active = ActiveSession::new();
        let turn = VoiceTurn {
            instance: 1,
            session: 1,
        };
        assert!(!active.is_current(turn), "没有进行中的会话时一律过期");

        active.begin(turn);
        assert!(active.is_current(turn));
        assert!(!active.is_current(VoiceTurn {
            instance: 1,
            session: 2
        }));
        // 同一个会话号、不同实例（关麦后立刻重开）也必须过期。
        assert!(!active.is_current(VoiceTurn {
            instance: 2,
            session: 1
        }));

        active.clear();
        assert!(!active.is_current(turn));
    }

    /// 旧回合的迟到完成事件不能清掉新回合。
    #[test]
    fn clearing_is_owner_checked() {
        let active = ActiveSession::new();
        let old = VoiceTurn {
            instance: 1,
            session: 1,
        };
        let new = VoiceTurn {
            instance: 2,
            session: 1,
        };
        active.begin(new);
        assert!(!active.clear_if_current(old), "旧回合不能清掉新回合");
        assert!(active.is_current(new));
        assert!(active.clear_if_current(new));
        assert!(active.current().is_none());
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

    /// 有效 Final：提交一次，并回到监听（不能卡在 Transcribing）。
    #[test]
    fn valid_final_submits_once_and_resumes_listening() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let turn = VoiceTurn {
            instance: 7,
            session: 1,
        };
        let asr = FakeAsr {
            immediate: Some(Ok("打开浏览器".to_string())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(7, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱同学".to_string());
        assert_eq!(machine.state(), &VoiceState::Capturing);
        machine.finish(1.0, AUDIO.to_vec());
        assert_eq!(machine.state(), &VoiceState::Transcribing);
        assert!(machine.poll(), "识别结束后必须回到监听");
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert_eq!(
            recorder.finals(),
            vec![(turn, "打开浏览器".to_string())],
            "有效结果恰好提交一次"
        );
        assert!(active.current().is_none(), "结果被消费后会话要清空");

        // 同一个结果不会再次被接受（poll 不会重复发事件）。
        assert!(!machine.poll());
        assert_eq!(recorder.finals().len(), 1);
    }

    #[test]
    fn manual_click_recognizes_without_wake_and_keeps_its_purpose() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("列出窗口".into())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(12, &recorder, &active, asr);
        machine.start_listening();
        assert!(machine.begin_direct(ListenPurpose::Manual, 0));
        machine.finish(1.0, AUDIO.to_vec());
        assert!(machine.poll());
        assert_eq!(recorder.finals().len(), 1);
        assert!(recorder.kinds().iter().any(|event| matches!(event, VoiceEventKind::Final { purpose: ListenPurpose::Manual, text, .. } if text == "列出窗口")));
        assert!(!recorder
            .kinds()
            .iter()
            .any(|event| matches!(event, VoiceEventKind::Woke { .. })));
        assert!(!machine.poll());
    }

    /// 直通聆听（打断词）：不需要唤醒词，Final 必须带上当时的目的。
    #[test]
    fn direct_listening_does_not_require_wake_word() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("等一下".to_string())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(9, &recorder, &active, asr);
        machine.start_listening();
        assert!(machine.begin_direct(ListenPurpose::BargeIn, 0));
        assert_eq!(machine.state(), &VoiceState::Capturing);
        assert_eq!(machine.purpose(), ListenPurpose::BargeIn);
        machine.finish(1.0, AUDIO.to_vec());
        assert!(machine.poll());
        assert_eq!(machine.state(), &VoiceState::Listening);

        let finals: Vec<(ListenPurpose, u64)> = recorder
            .kinds()
            .into_iter()
            .filter_map(|kind| match kind {
                VoiceEventKind::Final {
                    purpose,
                    request_id,
                    ..
                } => Some((purpose, request_id)),
                _ => None,
            })
            .collect();
        assert_eq!(finals, vec![(ListenPurpose::BargeIn, 0)]);
        // 直通开始事件也要发给界面（否则提示还停在「正在监听唤醒词」）。
        assert!(recorder.kinds().iter().any(|kind| matches!(
            kind,
            VoiceEventKind::DirectStarted {
                purpose: ListenPurpose::BargeIn,
                ..
            }
        )));
    }

    #[test]
    fn followup_session_carries_generation_through_asr() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("继续".into())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(11, &recorder, &active, asr);
        machine.start_listening();
        assert!(machine.begin_direct(ListenPurpose::Followup, 73));
        machine.finish(1.0, AUDIO.to_vec());
        assert!(machine.poll());
        assert!(recorder.kinds().iter().any(|e| matches!(
            e,
            VoiceEventKind::Final {
                purpose: ListenPurpose::Followup,
                request_id: 73,
                ..
            }
        )));
    }

    /// 语音确认：请求 id 原样带回，worker 据此拒绝过期答复。
    #[test]
    fn confirm_session_carries_the_request_id() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("不可以".to_string())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(11, &recorder, &active, asr);
        machine.start_listening();
        assert!(machine.begin_direct(ListenPurpose::Confirm, 42));
        machine.finish(1.0, AUDIO.to_vec());
        assert!(machine.poll());

        let finals: Vec<(ListenPurpose, u64)> = recorder
            .kinds()
            .into_iter()
            .filter_map(|kind| match kind {
                VoiceEventKind::Final {
                    purpose,
                    request_id,
                    ..
                } => Some((purpose, request_id)),
                _ => None,
            })
            .collect();
        assert_eq!(finals, vec![(ListenPurpose::Confirm, 42)]);
    }

    /// 直通入口不能被用来绕过唤醒词之外的约束：`Wake` 只能走 `begin`。
    #[test]
    fn direct_entry_rejects_wake_purpose() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let mut machine = machine_with(5, &recorder, &active, FakeAsr::new());
        machine.start_listening();
        assert!(!machine.begin_direct(ListenPurpose::Wake, 0));
        assert_eq!(machine.state(), &VoiceState::Listening);
        // 不在 Listening 时也不能开始。
        machine.shutdown();
        assert!(!machine.begin_direct(ListenPurpose::BargeIn, 0));
    }

    /// 聆听模式是共享的当前值（工作线程写、音频线程读）。
    #[test]
    fn listen_mode_roundtrip() {
        let mode = ListenMode::new();
        assert_eq!(mode.get(), (ListenPurpose::Wake, 0));
        mode.set(ListenPurpose::Confirm, 7);
        assert_eq!(mode.get(), (ListenPurpose::Confirm, 7));
        assert!(mode.purpose().is_direct());
        mode.set(ListenPurpose::Wake, 0);
        assert!(!mode.purpose().is_direct());
    }

    /// 空转录：走 Rejected，并**恢复监听**（这是原来会卡死的路径）。
    #[test]
    fn empty_transcript_returns_to_listening() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("谢谢观看。".to_string())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(3, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        assert!(machine.poll());
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert!(recorder.finals().is_empty());
        assert_eq!(
            recorder.rejects(),
            vec![(
                VoiceTurn {
                    instance: 3,
                    session: 1
                },
                RejectReason::EmptyTranscript
            )]
        );
    }

    /// 推理错误：同样必须回到监听，而不是永远「正在识别」。
    #[test]
    fn asr_error_returns_to_listening() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Err("CUDA OOM".to_string())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(4, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        assert!(machine.poll());
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert!(recorder.finals().is_empty());
        let rejects = recorder.rejects();
        assert_eq!(rejects.len(), 1);
        assert!(matches!(rejects[0].1, RejectReason::Failed(_)));
    }

    /// 太短不提交，直接回到监听。
    #[test]
    fn too_short_is_rejected_without_submit() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr::new();
        let submitted = asr.submitted.clone();
        let mut machine = machine_with(1, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(0.1, AUDIO.to_vec());
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert!(submitted.lock().unwrap().is_empty(), "太短不该提交给 ASR");
        assert_eq!(recorder.rejects().len(), 1);
        assert_eq!(recorder.rejects()[0].1, RejectReason::TooShort);
    }

    /// 提交失败（ASR 线程已停）：报错并回到监听。
    #[test]
    fn submit_failure_returns_to_listening() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            fail_submit: true,
            ..FakeAsr::new()
        };
        let mut machine = machine_with(1, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert!(recorder.finals().is_empty());
        assert!(matches!(recorder.rejects()[0].1, RejectReason::Failed(_)));
    }

    /// **取消**：录制/识别中被取消后，迟到的结果不得提交，状态回到监听。
    #[test]
    fn cancelled_turn_drops_late_result() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = DeferredAsr {
            pending: Arc::new(Mutex::new(Vec::new())),
        };
        let pending = asr.pending.clone();
        let mut machine = machine_with(5, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        assert_eq!(machine.state(), &VoiceState::Transcribing);

        // 用户取消（等价于关麦）：清空当前会话 → 协调器让推理停下并回到监听。
        active.clear();
        assert!(machine.poll(), "取消后要回到监听");
        assert_eq!(machine.state(), &VoiceState::Listening);

        // 迟到的识别结果：归属对不上，绝不能提交。
        let (turn, reply) = pending.lock().unwrap().pop().unwrap();
        reply(transcript("打开浏览器", turn));
        assert!(recorder.finals().is_empty(), "取消后不得提交");
        assert!(!machine.poll(), "迟到的完成事件不该再推动状态");
        assert_eq!(machine.state(), &VoiceState::Listening);
    }

    /// 录制中取消（还没提交）：这一句直接作废。
    #[test]
    fn cancel_while_capturing_discards_the_utterance() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr::new();
        let submitted = asr.submitted.clone();
        let mut machine = machine_with(1, &recorder, &active, asr);

        machine.start_listening();
        machine.begin("小爱".to_string());
        active.clear();
        assert!(machine.poll());
        assert_eq!(machine.state(), &VoiceState::Listening);
        // 取消之后再走端点：没有会话可提交。
        machine.finish(1.0, AUDIO.to_vec());
        assert!(submitted.lock().unwrap().is_empty());
        assert!(recorder.finals().is_empty());
    }

    /// 上一句迟到的完成事件不能把**新的**这一句从 Transcribing 拉回监听。
    ///
    /// 真实时序：第一句被取消时推理还在跑（关麦不必等 GPU 算完），用户马上
    /// 说了第二句；第一句的推理此时才结束。若只用 bool 记「识别结束了」，
    /// 第二句会被提前判成结束，然后它的结果又因为没人接受而被丢弃。
    #[test]
    fn late_completion_from_previous_turn_does_not_end_the_new_turn() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let pending = Arc::new(Mutex::new(Vec::new()));
        let mut machine = machine_with(
            1,
            &recorder,
            &active,
            DeferredAsr {
                pending: pending.clone(),
            },
        );

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        // 用户取消第一句（关麦/取消输入）：立刻让推理停下并回到监听。
        active.clear();
        assert!(machine.poll());
        assert_eq!(machine.state(), &VoiceState::Listening);

        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        assert_eq!(machine.state(), &VoiceState::Transcribing);

        // 第一句的推理现在才结束：不能提交，也不能结束第二句。
        let (first, first_reply) = {
            let mut guard = pending.lock().unwrap();
            (guard[0].0, guard.remove(0).1)
        };
        first_reply(transcript("上一句", first));
        assert!(recorder.finals().is_empty(), "旧回合结果不得提交");
        assert!(!machine.poll(), "旧回合的完成事件不能结束新的一句");
        assert_eq!(machine.state(), &VoiceState::Transcribing);

        // 第二句正常结束：提交一次并回到监听。
        let (second, second_reply) = pending.lock().unwrap().pop().unwrap();
        second_reply(transcript("新一句", second));
        assert!(machine.poll());
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert_eq!(recorder.finals(), vec![(second, "新一句".to_string())]);
    }

    /// 溢出只丢弃正在录的那一句，并且可以继续工作。
    #[test]
    fn overflow_drops_current_capture_and_resumes() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let mut machine = machine_with(1, &recorder, &active, FakeAsr::new());

        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.overflow();
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert_eq!(machine.dropped(), 1);
        assert!(recorder
            .kinds()
            .iter()
            .any(|kind| matches!(kind, VoiceEventKind::Overflow)));
        assert_eq!(recorder.rejects()[0].1, RejectReason::Overflow);
    }

    /// 空文本不会变成 Final。
    #[test]
    fn blank_transcript_is_rejected() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("   ".to_string())),
            ..FakeAsr::new()
        };
        let mut machine = machine_with(1, &recorder, &active, asr);
        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        machine.poll();
        assert!(recorder.finals().is_empty());
        assert_eq!(recorder.rejects()[0].1, RejectReason::EmptyTranscript);
    }

    /// 每次唤醒的会话号在实例内唯一；不同实例的会话号可以重复，但身份不同。
    #[test]
    fn session_ids_are_unique_within_an_instance() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let mut machine = machine_with(9, &recorder, &active, FakeAsr::new());

        machine.start_listening();
        machine.begin("小爱".to_string());
        let first = active.current().unwrap();
        machine.reject(RejectReason::NoSpeech);
        machine.begin("小爱".to_string());
        let second = active.current().unwrap();

        assert_eq!(first.instance, 9);
        assert_eq!(second.instance, 9);
        assert_ne!(first.session, second.session, "实例内会话号必须递增");
        assert_ne!(first, second);
    }

    /// 唤醒后一直没说话：不提交，回到监听。
    #[test]
    fn no_speech_rejects_and_resumes() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let mut machine = machine_with(1, &recorder, &active, FakeAsr::new());
        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.reject(RejectReason::NoSpeech);
        assert_eq!(machine.state(), &VoiceState::Listening);
        assert_eq!(recorder.rejects()[0].1, RejectReason::NoSpeech);
    }

    /// 关闭麦克风：状态如实变 Off，且会话作废。
    #[test]
    fn shutdown_sets_off_and_clears_session() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let mut machine = machine_with(1, &recorder, &active, FakeAsr::new());
        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.shutdown();
        assert_eq!(machine.state(), &VoiceState::Off);
        assert!(active.current().is_none());
        assert!(!machine.wants_audio());
    }

    /// 统计用：确认 `dropped` 不会把正常提交也算进去。
    #[test]
    fn dropped_counts_only_rejected_sessions() {
        let recorder = Recorder::default();
        let active = ActiveSession::new();
        let asr = FakeAsr {
            immediate: Some(Ok("打开记事本".to_string())),
            ..FakeAsr::new()
        };
        let submitted = asr.submitted.clone();
        let mut machine = machine_with(1, &recorder, &active, asr);
        machine.start_listening();
        machine.begin("小爱".to_string());
        machine.finish(1.0, AUDIO.to_vec());
        machine.poll();
        assert_eq!(machine.dropped(), 0, "正常提交不该计入丢弃");
        assert_eq!(submitted.lock().unwrap().len(), 1);
        assert_eq!(
            recorder
                .kinds()
                .iter()
                .filter(|kind| matches!(kind, VoiceEventKind::Woke { .. }))
                .count(),
            1
        );
    }
}
