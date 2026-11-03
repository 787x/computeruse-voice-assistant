//! 语音：输入（PR6）与输出（PR7）。
//!
//! 输入侧的模块边界（扁平，不为架构而架构）：
//!  - [`pcm`]：纯函数音频处理（下混、整型转换、重采样、电平）
//!  - [`capture`]：麦克风采集（cpal / WASAPI）与有界队列
//!  - [`kws`]：唤醒词（sherpa-onnx Zipformer）
//!  - [`vad`]：噪声底跟踪 + 端点状态机
//!  - [`asr`]：whisper.cpp 常驻模型 + 工作线程
//!  - [`settings`]：语音配置与路径解析
//!  - [`coordinator`]：把以上串成一个有状态的语音会话
//!
//! 输出侧（PR7）：
//!  - [`tts`]：合成后端（MiMo 云端 / 本地 ONNX），产出带格式与身份的音频块
//!  - [`playback`]：有界缓冲 + 原生输出，"播完"以缓冲排空为准
//!  - [`speech`]：一次播报的编排（句子顺序、状态机、取消、字幕）
//!  - [`bargein`]：打断词的文本判定与回声防护（纯函数）
//!  - [`confirm`]：语音确认的意图判定与请求绑定（纯函数）
//!
//! 本模块**不**直接操作界面：状态变化通过 [`coordinator::VoiceEvent`] /
//! [`speech::SpeechEvent`] 交给 `ui::worker`，由它决定显示与是否提交给 Agent。

pub mod asr;
pub mod bargein;
pub mod capture;
pub mod confirm;
pub mod coordinator;
pub mod kws;
pub mod pcm;
pub mod playback;
pub mod settings;
pub mod speech;
pub mod tts;
pub mod vad;

pub use coordinator::{
    ActiveSession, CaptureInfoForUi, Coordinator, CoordinatorConfig, ListenMode, ListenPurpose,
    RejectReason, SharedSink, VoiceEvent, VoiceEventKind, VoiceHandle, VoiceInstance, VoiceState,
    VoiceTurn,
};
pub use settings::{AsrLanguage, VoicePaths, VoiceSettings};
pub use speech::{SpeechEvent, SpeechRuntime};
pub use tts::{TtsBackend, TtsSettings};
