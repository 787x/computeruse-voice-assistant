//! Whisper ASR：模型常驻、独立工作线程、结果按会话关联。
//!
//! 三条硬约束（任务书）：
//!  1. **模型在会话间复用**，不得每句话启动一次 whisper-cli；
//!  2. 推理在**独立阻塞线程**上，不阻塞 Slint 主线程、音频采集与 tokio Agent；
//!  3. 停止监听 / 取消 / 退出后，**迟到的识别结果必须丢弃**，
//!     不能发送到新一轮，也不能触发工具。
//!
//! 第 3 条有两道闸：协调器用「实例号 + 会话号」核对结果归属（正确性），
//! 同时把 whisper 的 abort 回调接到取消标记上（尽快停下，属于优化）。
//! 即使 abort 没被底层及时轮询，结果返回时仍会被归属核对丢弃。
//!
//! 后端选择：**用户请求**与**实际后端**是两件事。`ReadyInfo` 只回报实际
//! 初始化成功的后端；请求了 GPU 但 CUDA 不可用时降级到同一模型的 CPU 并
//! 带上原因，两者都失败则报错——不会出现「显示就绪但其实没有可用后端」。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self as std_mpsc, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// **用户请求**是否用 GPU。实际后端由 [`ReadyInfo`] 如实回报。
    pub use_gpu: bool,
    /// 提示词：中文场景给简体偏好 + 标点，减少 Whisper 混出繁体。
    pub initial_prompt: String,
}

/// 本构建是否带 CUDA 后端。
///
/// `cuda` 是默认 feature，但可以关掉（`--no-default-features`）。
/// 关掉时即使设置里要求 GPU 也只能走 CPU：界面会明确显示原因。
pub const fn gpu_compiled_in() -> bool {
    cfg!(feature = "cuda")
}

/// 实际用于推理的后端。**不是**用户期望值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrBackend {
    Cpu,
    Cuda,
}

impl AsrBackend {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Cuda => "CUDA",
        }
    }

    /// 给界面用的稳定状态码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
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
            // 这里保存的是「用户请求」，不在这里偷偷按 feature 改值：
            // 后端决策与「实际后端」的回报集中在 `load_context`。
            use_gpu: settings.use_gpu,
            initial_prompt: default_initial_prompt(settings.language),
        }
    }
}

/// 后端尝试计划（纯逻辑，可确定性测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendPlan {
    /// 依次尝试的后端。
    pub order: Vec<AsrBackend>,
    /// 为什么没有尝试 GPU（CPU-only 构建 / 没有可用设备）；请求 GPU 时才有值。
    pub gpu_unavailable: Option<String>,
}

/// 决定尝试哪些后端。
///
/// 关键点：**只有本构建带 CUDA 且确实有可用 GPU 设备时才去请求 GPU**。
/// whisper.cpp 在请求 GPU 但找不到设备时不会让初始化失败，而是静默用 CPU；
/// 那样「初始化成功」就不能证明在用 GPU。所以先用 ggml 的设备枚举确认。
pub fn plan_backends(request_gpu: bool, compiled: bool, gpu_devices: usize) -> BackendPlan {
    if !request_gpu {
        return BackendPlan {
            order: vec![AsrBackend::Cpu],
            gpu_unavailable: None,
        };
    }
    if !compiled {
        return BackendPlan {
            order: vec![AsrBackend::Cpu],
            gpu_unavailable: Some("本构建未启用 CUDA".to_string()),
        };
    }
    if gpu_devices == 0 {
        return BackendPlan {
            order: vec![AsrBackend::Cpu],
            gpu_unavailable: Some("没有检测到可用的 CUDA 设备".to_string()),
        };
    }
    BackendPlan {
        order: vec![AsrBackend::Cuda, AsrBackend::Cpu],
        gpu_unavailable: None,
    }
}

/// 按计划依次尝试后端的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSelection {
    /// 实际初始化成功的后端。
    pub backend: AsrBackend,
    /// 前面尝试失败的原因（降级到 CPU 时给用户看）。
    pub fallback_reason: Option<String>,
}

/// 依次尝试后端，返回第一个成功的；全部失败时把每次的原因拼成一条错误。
///
/// 抽成独立函数是为了能确定性地测「GPU 初始化失败 → CPU 接管」这条路，
/// 而不需要在没有 GPU 的机器上真的制造一次 CUDA 故障。
pub fn select_backend(
    plan: &[AsrBackend],
    mut attempt: impl FnMut(AsrBackend) -> Result<(), String>,
) -> Result<BackendSelection, String> {
    let mut failures: Vec<String> = Vec::new();
    for backend in plan {
        match attempt(*backend) {
            Ok(()) => {
                return Ok(BackendSelection {
                    backend: *backend,
                    fallback_reason: (!failures.is_empty()).then(|| failures.join("；")),
                })
            }
            Err(reason) => failures.push(format!("{}：{reason}", backend.label())),
        }
    }
    Err(failures.join("；"))
}

/// ggml 实际能看到的 GPU 设备数。
///
/// 为什么需要它，为什么只能用 `unsafe`：whisper.cpp 在 `use_gpu = true` 但找不到
/// 设备时**不会**让初始化失败，而是静默退回 CPU 并只打一行日志。若只看
/// 「初始化成功」就把后端报成 CUDA，等于把用户期望当成事实——这正是要避免的。
/// whisper-rs 没有包装 `ggml_backend_dev_*`，所以这里直接调用同一份静态链接
/// 进来的查询函数：
///
///  - 替代方案 A：信任初始化成功 → 会产生假的「CUDA」显示；
///  - 替代方案 B：抓 whisper 的日志钩子 → 全局日志副作用 + 脆弱的字符串匹配；
///  - 替代方案 C：开子进程跑 `nvidia-smi` → 多一个外部依赖，且它看到设备
///    不等于 ggml/whisper 能看到可用设备。
///
/// 安全前提：只做设备枚举，不分配、不持有指针、不修改状态；索引严格来自
/// `ggml_backend_dev_count()`。若上游改名/删除这些符号，会在**链接期**失败
/// （构建错误），不会变成运行期的未定义行为。
pub fn gpu_device_count() -> usize {
    use whisper_rs_sys as sys;
    // SAFETY: 见函数文档。`ggml_backend_dev_get` 在索引小于设备数时返回有效指针
    // （或空指针，已判空）；设备指针只在本次调用内读取。
    unsafe {
        let count = sys::ggml_backend_dev_count();
        let mut gpus = 0;
        for index in 0..count {
            let device = sys::ggml_backend_dev_get(index);
            if device.is_null() {
                continue;
            }
            let kind = sys::ggml_backend_dev_type(device);
            if kind == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                || kind == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
            {
                gpus += 1;
            }
        }
        gpus
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

/// 一次推理的取消标记（whisper 的 abort 回调会轮询它）。
///
/// 注意：abort 是**优化**，不是正确性依赖。底层可能在整个推理结束前才轮询，
/// 也可能已经产出了半句文本；结果是否可用仍由协调器的归属核对决定。
#[derive(Debug, Clone, Default)]
pub struct InferenceCancel(Arc<AtomicBool>);

impl InferenceCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

enum Request {
    Transcribe {
        session_id: u64,
        samples: Vec<f32>,
        /// 让正在跑的推理尽快停下（取消/关麦时置位）。
        cancel: InferenceCancel,
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
                let (context, ready) = match load_context(&config) {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(ready));
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
                // 线程在报告就绪前崩了。不要无限等：它要么已经结束，要么
                // 卡在底层初始化里，界面上如实报错比堵住更安全。
                join_bounded(join, DROP_JOIN_BUDGET);
                Err(AsrError::Load("ASR 线程在报告就绪前退出了".to_string()))
            }
        }
    }

    /// 提交一段音频。结果通过 `reply` 回到调用方线程。
    pub fn submit(
        &self,
        session_id: u64,
        samples: Vec<f32>,
        cancel: InferenceCancel,
        reply: ReplySink,
    ) -> Result<(), AsrError> {
        self.tx
            .send(Request::Transcribe {
                session_id,
                samples,
                cancel,
                reply,
            })
            .map_err(|_| AsrError::Stopped)
    }

    /// 已完成的最后一个会话序号（测试与诊断用）。
    pub fn last_completed(&self) -> u64 {
        self.last_completed.load(Ordering::Relaxed)
    }

    /// 停止线程并释放模型。
    ///
    /// `Shutdown` 会排在正在跑的推理之后；这里**有上限**地等待（见
    /// [`join_bounded`]），超时就把线程分离。已经发生的推理无法从外部强杀，
    /// 但结果不会再提交（协调器按归属丢弃），模型也会在推理结束时释放。
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            join_bounded(join, SHUTDOWN_JOIN_BUDGET);
        }
    }
}

/// 显式停止时最多等多久（覆盖一次正常推理的收尾）。
const SHUTDOWN_JOIN_BUDGET: Duration = Duration::from_secs(3);
/// Drop 时最多等多久：关麦/退出不该因为一句推理卡住界面。
const DROP_JOIN_BUDGET: Duration = Duration::from_millis(300);

/// 有上限地 join 一个线程：超时就分离（进程退出时回收）。
///
/// `std::thread::JoinHandle` 没有超时接口，这里用 `is_finished()` 轮询。
/// 返回 `true` 表示线程已经结束并被 join。
fn join_bounded(join: std::thread::JoinHandle<()>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while !join.is_finished() {
        if Instant::now() >= deadline {
            // 分离：线程会在推理结束后自行退出并释放模型/上下文。
            // 不无限等待，避免关麦或退出卡住工作线程。
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = join.join();
    true
}

impl Drop for AsrWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            // 正常情况（推理已结束或 abort 生效）会很快退出；
            // 否则分离线程，由它自己收尾——这里绝不无限等待。
            join_bounded(join, DROP_JOIN_BUDGET);
        }
    }
}

/// 线程就绪时回报的**实际**后端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyInfo {
    /// 真正初始化成功的后端。
    pub backend: AsrBackend,
    /// 降级原因：请求了 GPU 但 CUDA 不可用（构建未启用 / 无设备 / 初始化失败）。
    pub fallback_reason: Option<String>,
}

/// 加载模型并确定实际后端：先按计划尝试，失败再降级。
///
/// 模型文件本身不可用（不存在/损坏）时**不降级**——那是真正的错误，
/// 不是「GPU 不可用」，必须如实报出来。
fn load_context(config: &AsrConfig) -> Result<(WhisperContext, ReadyInfo), AsrError> {
    if !config.model_path.is_file() {
        return Err(AsrError::Load(format!(
            "模型文件不存在：{}",
            config.model_path.display()
        )));
    }

    // 只有真的要请求 GPU 时才去探测设备（CPU-only 构建里这次调用也不会发生）。
    let gpu_devices = if config.use_gpu && gpu_compiled_in() {
        gpu_device_count()
    } else {
        0
    };
    let plan = plan_backends(config.use_gpu, gpu_compiled_in(), gpu_devices);
    let mut loaded: Option<WhisperContext> = None;
    let selection = select_backend(&plan.order, |backend| {
        match open_context(&config.model_path, backend) {
            Ok(context) => {
                loaded = Some(context);
                Ok(())
            }
            Err(reason) => Err(reason),
        }
    })
    .map_err(|reason| {
        AsrError::Load(format!(
            "无法加载 Whisper 模型 {}：{reason}",
            config.model_path.display()
        ))
    })?;

    let Some(context) = loaded else {
        // select_backend 成功时必然已经写入上下文；这里只是不假设。
        return Err(AsrError::Load(format!(
            "无法加载 Whisper 模型 {}：没有可用的推理后端",
            config.model_path.display()
        )));
    };

    // 只有实际在用 CPU 时才带上降级原因；用上 GPU 时不解释「为什么不是 GPU」。
    let fallback_reason = if selection.backend == AsrBackend::Cpu {
        selection.fallback_reason.or(plan.gpu_unavailable)
    } else {
        None
    };
    Ok((
        context,
        ReadyInfo {
            backend: selection.backend,
            fallback_reason,
        },
    ))
}

/// 按指定后端打开 whisper 上下文。
fn open_context(path: &Path, backend: AsrBackend) -> Result<WhisperContext, String> {
    let mut params = WhisperContextParameters::default();
    params.use_gpu(backend == AsrBackend::Cuda);
    WhisperContext::new_with_params(path, params).map_err(|error| match error {
        WhisperError::InitError => {
            "初始化失败（模型损坏/格式不兼容，或缺少 CUDA 运行库）".to_string()
        }
        other => format!("{other:?}"),
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
                cancel,
                reply,
            } => {
                // 排队期间就被取消：连推理都不启动，直接回报停止。
                let result = if cancel.is_cancelled() {
                    Err(AsrError::Stopped)
                } else {
                    transcribe_controlled(&context, config, session_id, &samples, Some(&cancel))
                };
                last_completed.store(session_id, Ordering::Relaxed);
                reply(result);
            }
        }
    }
}

/// 同步执行一次识别。供离线测试与基准共用（不带取消）。
pub fn transcribe(
    context: &WhisperContext,
    config: &AsrConfig,
    session_id: u64,
    samples: &[f32],
) -> Result<Transcript, AsrError> {
    transcribe_controlled(context, config, session_id, samples, None)
}

/// 同步执行一次识别，可选地接入取消标记。
///
/// `cancel` 会被装到 whisper 的 abort 回调上：底层在解码步之间轮询它，
/// 置位后尽快结束。这是**优化**——被取消时可能仍返回半句文本，
/// 调用方必须再用会话归属核对一次。
fn transcribe_controlled(
    context: &WhisperContext,
    config: &AsrConfig,
    session_id: u64,
    samples: &[f32],
    cancel: Option<&InferenceCancel>,
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
    // 取消/关麦时尽快停下：whisper 会在解码步之间轮询这个回调。
    // 它不能强杀线程，也不保证立即返回——所以结果仍然要按归属核对。
    //
    // 这里必须传 **boxed trait object**，不能传裸闭包：whisper-rs 0.16 的
    // `set_abort_callback_safe` 把 user_data 还原成 `*mut F`（F 是泛型参数），
    // 而实际存进去的是 `Box<dyn FnMut() -> bool>`。传裸闭包时 F 是闭包类型，
    // 布局对不上，回调会读到垃圾值并随机中止推理（实测表现为每次识别都以
    // `GenericError(-6)` 失败）。传 `Box<dyn FnMut() -> bool>` 时两者一致。
    if let Some(cancel) = cancel.cloned() {
        let abort: Box<dyn FnMut() -> bool> = Box::new(move || cancel.is_cancelled());
        params.set_abort_callback_safe(abort);
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
        .map_err(|error| error.to_string())?;
    let mut out = resampler.push(&mono).map_err(|error| error.to_string())?;
    out.extend(resampler.flush().map_err(|error| error.to_string())?);
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

    /// 后端计划：四种组合都必须给出「实际可尝试」的顺序，而不是用户期望。
    #[test]
    fn backend_plan_matches_availability() {
        // 用户没要求 GPU：只走 CPU，也不需要解释。
        let plan = plan_backends(false, true, 2);
        assert_eq!(plan.order, vec![AsrBackend::Cpu]);
        assert_eq!(plan.gpu_unavailable, None);

        // CPU-only 构建：即使要求 GPU 也只走 CPU，并说明原因。
        let plan = plan_backends(true, false, 0);
        assert_eq!(plan.order, vec![AsrBackend::Cpu]);
        assert!(plan.gpu_unavailable.unwrap().contains("CUDA"));

        // 构建带 CUDA 但没有可用设备：不请求 GPU（whisper 会静默退回 CPU）。
        let plan = plan_backends(true, true, 0);
        assert_eq!(plan.order, vec![AsrBackend::Cpu]);
        assert!(plan.gpu_unavailable.unwrap().contains("设备"));

        // 有可用设备：先 CUDA，CPU 作为回退。
        let plan = plan_backends(true, true, 1);
        assert_eq!(plan.order, vec![AsrBackend::Cuda, AsrBackend::Cpu]);
        assert_eq!(plan.gpu_unavailable, None);
    }

    /// GPU 初始化失败 → 同一模型的 CPU 接管，并带上原因。
    #[test]
    fn gpu_failure_falls_back_to_cpu_with_reason() {
        let mut tried = Vec::new();
        let selection = select_backend(&[AsrBackend::Cuda, AsrBackend::Cpu], |backend| {
            tried.push(backend);
            match backend {
                AsrBackend::Cuda => Err("CUDA 初始化失败".to_string()),
                AsrBackend::Cpu => Ok(()),
            }
        })
        .expect("CPU 成功后必须返回选择结果");
        assert_eq!(tried, vec![AsrBackend::Cuda, AsrBackend::Cpu]);
        assert_eq!(selection.backend, AsrBackend::Cpu);
        assert!(selection.fallback_reason.unwrap().contains("CUDA"));
    }

    /// 两者都失败：如实报错，不能假装就绪。
    #[test]
    fn both_backends_failing_is_an_error() {
        let error = select_backend(&[AsrBackend::Cuda, AsrBackend::Cpu], |backend| {
            Err(format!("{} 不可用", backend.label()))
        })
        .unwrap_err();
        assert!(error.contains("CUDA") && error.contains("CPU"), "{error}");
    }

    /// 直接用上 GPU 时不需要「降级原因」。
    #[test]
    fn gpu_success_has_no_fallback_reason() {
        let selection = select_backend(&[AsrBackend::Cuda], |_| Ok(())).expect("CUDA 成功");
        assert_eq!(selection.backend, AsrBackend::Cuda);
        assert_eq!(selection.fallback_reason, None);
    }

    /// 模型文件不存在：立刻报错，不能出现「线程起来了但其实没模型」。
    /// 这不是「GPU 不可用」，不该被后端回退掩盖。
    #[test]
    fn missing_model_is_an_explicit_error() {
        let config = AsrConfig {
            model_path: std::path::PathBuf::from("definitely-not-here/ggml-missing.bin"),
            language: AsrLanguage::Chinese,
            threads: 2,
            use_gpu: true,
            initial_prompt: String::new(),
        };
        let message = match AsrWorker::start(config) {
            Ok(_) => panic!("模型文件不存在时必须报错，不能假装就绪"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("模型文件不存在"), "{message}");
    }

    /// CPU-only 构建里 ggml 不该报出任何 GPU 设备（否则会误以为能上 GPU）。
    #[test]
    fn gpu_device_probe_matches_build() {
        if gpu_compiled_in() {
            // 带 CUDA 的构建：这台机器可能有、也可能没有可用 GPU，只打印实测值，
            // 不把「本机有显卡」写成测试断言。
            eprintln!(
                "本构建带 CUDA，ggml 可见 GPU 设备数：{}",
                gpu_device_count()
            );
            return;
        }
        assert_eq!(gpu_device_count(), 0, "CPU-only 构建不应看到 GPU 设备");
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
