//! 语音输入（PR6）：采集 → 唤醒词 → 端点 → Whisper 转录。
//!
//! 模块边界（扁平，不为架构而架构）：
//!  - [`pcm`]：纯函数音频处理（下混、整型转换、重采样、电平）
//!  - [`capture`]：麦克风采集（cpal / WASAPI）与有界队列
//!  - [`kws`]：唤醒词（sherpa-onnx Zipformer）
//!  - [`vad`]：噪声底跟踪 + 端点状态机
//!  - [`asr`]：whisper.cpp 常驻模型 + 工作线程
//!  - [`settings`]：语音配置与路径解析
//!  - [`coordinator`]：把以上串成一个有状态的语音会话
//!
//! 本模块**不**直接操作界面：状态变化通过 [`coordinator::VoiceEvent`] 交给
//! `ui::worker`，由它决定显示与是否提交给 Agent。
//! 也**不**做语音合成、播报、打断与语音确认——那些属于 PR7。

pub mod asr;
pub mod capture;
pub mod coordinator;
pub mod kws;
pub mod pcm;
pub mod settings;
pub mod vad;

pub use coordinator::{
    ActiveSession, CaptureInfoForUi, Coordinator, CoordinatorConfig, RejectReason, SharedSink,
    VoiceEvent, VoiceEventKind, VoiceHandle, VoiceInstance, VoiceState, VoiceTurn,
};
pub use settings::{AsrLanguage, VoicePaths, VoiceSettings};
