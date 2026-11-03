//! 后台工作线程：持有 Agent、Computer Use 工具与配置，驱动一条回合。
//!
//! 这里只做"把界面动作翻译成 Agent 调用、再把结果翻译成界面状态"，
//! 具体的协议、权限、预算逻辑都在各自模块里，便于单独测试。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::agent::{Agent, AgentConfig, AgentEvent, ConfirmationPrompt, TurnOutcome};
use crate::cancel::CancelToken;
use crate::config::{self, AppPaths, PermissionMode, Settings};
use crate::cua::bridge::{BridgeOptions, PwshBridge};
use crate::cua::tools::CuaToolkit;
use crate::llm::{ChatMessage, LlmClient};
use crate::permissions::{
    ConfirmAnswer, ConfirmFuture, ConfirmRequest, Confirmer, PermissionState,
};
use crate::prompt;
use crate::secret::SecretStore;
use crate::voice::{
    asr::AsrBackend,
    bargein,
    confirm::{self, ConfirmDecision},
    coordinator::{
        self, ActiveSession, CoordinatorConfig, ListenMode, ListenPurpose, SharedSink, VoiceHandle,
    },
    playback::PlaybackState,
    settings::VoicePaths,
    speech::{SpeechEvent, SpeechRuntime},
    tts::{SpeechId, TtsBackend, TtsSettings, TurnId},
    CaptureInfoForUi, RejectReason, VoiceEvent, VoiceEventKind, VoiceInstance, VoiceState,
    VoiceTurn,
};
use crate::ChatEntry;
use crate::ToolCard;

use crate::AppWindow;

/// 界面发给工作线程的命令。
#[derive(Debug)]
pub enum UiCommand {
    /// 用户发出一条指令。
    Send(String),
    /// 取消当前回合（同时停止这一回合的播报）。
    Cancel,
    /// 只停止播报：不取消 Agent 回合，不结算任何权限请求。
    StopSpeaking,
    /// 试音：合成一句固定的话（验证后端/设备，不产生 Agent 回合）。
    SpeakTest,
    /// **一次保存**：模型端点 + 语音播报一起提交（界面上的「保存」只发这一条）。
    ///
    /// 异步执行：调用方不等结果，工作线程完成后回填界面。
    /// 没有回执通道是刻意的——在 Slint 主线程上等待会卡住整个界面。
    /// `generation` 是提交时取的代次：回填时只有仍是最新的一代才允许改界面。
    SaveAll {
        request: SaveAllRequest,
        generation: u64,
    },
    /// 关闭设置面板（取消）：丢弃界面上的未保存修改，重新按已保存配置回填。
    ///
    /// `generation` 用来作废在途的保存：取消之后，工作线程会丢弃排队中/过期的
    /// 保存请求，不会"用户点了取消却还是偷偷存了一份旧表单"。
    CancelSettings { generation: u64 },
    /// 清除**指定提供商**的 Key（界面当前选中的那一家）。
    ///
    /// 必须带 provider_id：设置页可以切到别家而不保存，
    /// 那时"清除"应该作用于界面显示的那家，而不是已保存配置里的那家。
    ClearApiKey(String),
    /// 切换提供商（界面据此回填地址与模型）。
    ProviderChanged(i32),
    /// 打开设置：按当前已保存的配置回填界面。
    OpenSettings,
    /// 启用/关闭麦克风监听（界面开关）。
    SetVoiceEnabled(bool),
    /// 语音线程发回的事件（内部消息，不是用户动作）。
    Voice(VoiceEvent),
    /// 播报层发回的事件（状态/字幕/提示）。
    Speech(SpeechEvent),
    /// 退出。
    Quit,
}

/// 提供商下拉框的顺序（与 UI 里的数组一致）。
const PROVIDER_IDS: &[&str] = &["mimo", "deepseek", "openai-compatible"];
const PERMISSION_MODES: &[PermissionMode] = &[
    PermissionMode::Standard,
    PermissionMode::Conservative,
    PermissionMode::Permissive,
];

fn provider_id_for_index(index: i32) -> &'static str {
    // 越界一律落到"自定义端点"，而不是悄悄用某个官方提供商：
    // 用错端点会把请求打到用户没配 Key 的官方地址上。
    usize::try_from(index)
        .ok()
        .and_then(|index| PROVIDER_IDS.get(index).copied())
        .unwrap_or("openai-compatible")
}

fn permission_mode_for_index(index: i32) -> PermissionMode {
    usize::try_from(index)
        .ok()
        .and_then(|index| PERMISSION_MODES.get(index).copied())
        .unwrap_or(PermissionMode::Standard)
}

/// 启动界面与工作线程（阻塞直到窗口关闭）。
pub fn run_app() -> Result<(), Box<dyn std::error::Error>> {
    let window = AppWindow::new()?;
    let paths = AppPaths::resolve();
    let secret = SecretStore::new(paths.secret_file());

    /*
     * 初次加载配置。
     *
     * 文件损坏时**不**回退默认配置：那会让程序用一个用户从没配过的端点发请求，
     * 并在下次保存时把那份（也许只是暂时读不了的）真实配置覆盖掉。
     * 这里如实报错、停在只读状态，也不做旧 Key 迁移——
     * 迁移需要"配置里明确记录的提供商"，配置读不出来就没有这个依据。
     */
    let mut settings = match config::load_settings(&paths) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("[config] {error}");
            seed_ui_with_load_error(&window, &paths, &error.to_string());
            window.run()?;
            return Ok(());
        }
    };
    // 旧版单 Key 数据：只迁移到配置里**明确记录**的提供商，不猜测、不删除。
    // 迁移失败要让用户看得见——它是"旧 Key 还在不在"的唯一线索。
    let mut migration_error: Option<String> = None;
    match secret.migrate_legacy(&settings.provider_id) {
        Ok(true) => eprintln!(
            "[secret] 已把旧版单 Key 迁移到提供商 {}",
            settings.provider_id
        ),
        Ok(false) => {}
        Err(error) => {
            // 迁移失败不影响启动（用户仍可手动重填 Key），但必须让他知道：
            // 否则旧 Key "看起来消失了"，而实际上还能重试。
            eprintln!("[secret] 旧 Key 迁移失败：{error}");
            migration_error = Some(error.to_string());
        }
    }
    config::apply_env_overrides(&mut settings);
    let has_key = secret.has(&settings.provider_id);
    seed_ui(&window, &settings, has_key);
    if let Some(error) = migration_error {
        window.set_status(format!("旧 Key 迁移失败：{error}").into());
    }

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();

    // 确认器由界面持有：设置页/确认框/退出都要和它交互。
    let confirmer = Arc::new(UiConfirmer::new(window.as_weak()));

    // 设置保存的代次（单调递增）。只有最新一代的回填才允许改界面，
    // 这样超时/取消/更晚完成的旧保存都不会覆盖用户的新编辑。
    let save_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // ---- 界面 → 工作线程 ----
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_send(move |text| {
            let tx = tx.clone();
            let value = text.to_string();
            let weak = weak.clone();
            // 立刻把用户这句话放进对话区（不必等工作线程回报）。
            if let Some(window) = weak.upgrade() {
                push_entry(&window, "你", &value, "", "#e8f0fe");
                window.set_busy(true);
                window.set_status("正在思考…".into());
                window.set_draft("".into());
            }
            let _ = tx.send(UiCommand::Send(value));
        });
    }
    {
        let tx = command_tx.clone();
        window.on_cancel_turn(move || {
            let _ = tx.send(UiCommand::Cancel);
        });
    }
    {
        /*
         * 设置页必须显示**当前**已保存的配置，而不是启动时的快照。
         *
         * 之前这里捕获了 `run_app` 里的 `settings` 副本，保存之后再次打开仍显示旧值。
         * 现在改为向工作线程请求"最新设置"，由工作线程回填——
         * 工作线程是唯一持有权威配置的地方。
         */
        let tx = command_tx.clone();
        window.on_open_settings(move || {
            let _ = tx.send(UiCommand::OpenSettings);
        });
    }
    {
        /*
         * **一次保存**：模型端点与语音播报一起提交。
         *
         * 这里**不**等待结果：落盘 + 重建 TTS 引擎可能耗时（尤其本地模型），
         * 在 Slint 主线程上 `recv_timeout` 会把整个界面卡住。改为"发出去就返回"，
         * 由工作线程完成后回填界面（成功关面板 / 失败就地显示原因）。
         * 保存期间界面置 `settings-saving`，按钮禁用，避免重复提交。
         *
         * 每个请求带一个自增的 `generation`：工作线程回填时只有**最新**的那一代
         * 才允许改界面。这样"超时/取消/更晚完成的旧请求"不会覆盖用户已经编辑
         * 的新状态；取消之后排队的旧请求也不会被静默执行。
         */
        let tx = command_tx.clone();
        let weak = window.as_weak();
        let save_generation = save_generation.clone();
        window.on_save_all(
            move |provider_index,
                  base_url,
                  model,
                  api_key,
                  permission_index,
                  tts_enabled,
                  tts_backend_index,
                  mimo_voice,
                  mimo_style,
                  local_dir,
                  local_sid,
                  local_speed,
                  speak_mode| {
                let request = SaveAllRequest {
                    provider_index,
                    base_url: base_url.to_string(),
                    model: model.to_string(),
                    api_key: api_key.to_string(),
                    permission_index,
                    tts_enabled,
                    tts_backend_index,
                    mimo_voice: mimo_voice.to_string(),
                    mimo_style: mimo_style.to_string(),
                    local_dir: local_dir.to_string(),
                    local_sid: local_sid.to_string(),
                    local_speed: local_speed.to_string(),
                    speak_mode,
                };

                // 取一个新的代次；工作线程只有拿它回填才算"最新"。
                let generation = save_generation.fetch_add(1, Ordering::SeqCst) + 1;
                if tx
                    .send(UiCommand::SaveAll {
                        request,
                        generation,
                    })
                    .is_err()
                {
                    // 后台线程没了：立刻收掉"保存中"，并如实说明没保存。
                    if let Some(window) = weak.upgrade() {
                        window.set_settings_saving(false);
                        window.set_settings_error("后台线程已退出，设置没有保存".into());
                    }
                }
            },
        );
    }
    {
        /*
         * 取消：先作废**所有**在途保存（把代次推到更大值），再让工作线程
         * 丢弃还没开始执行的旧请求。这样"用户已经取消"之后不会再有
         * 一份排队中的旧保存被静默执行。
         */
        let tx = command_tx.clone();
        let save_generation = save_generation.clone();
        window.on_cancel_settings(move || {
            let generation = save_generation.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = tx.send(UiCommand::CancelSettings { generation });
        });
    }
    {
        let tx = command_tx.clone();
        window.on_clear_api_key(move |provider_index| {
            // 界面传的是下拉框里的索引，这里立刻翻译成 provider id：
            // 命令在通道里排队期间用户可能又改了选择，先固定住目标。
            let provider_id = provider_id_for_index(provider_index).to_string();
            let _ = tx.send(UiCommand::ClearApiKey(provider_id));
        });
    }
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        let paths_for_key = paths.clone();
        window.on_provider_changed(move |index| {
            let _ = tx.send(UiCommand::ProviderChanged(index));
            let provider_id = provider_id_for_index(index).to_string();
            // "该提供商有没有 Key" 要跟着下拉框变，否则界面会对着
            // 一家没有 Key 的提供商显示"已保存"。这里只探测文件存在性，不读明文。
            let has_key = SecretStore::new(paths_for_key.secret_file()).has(&provider_id);
            if let Some(window) = weak.upgrade() {
                // 同步回填该提供商的默认地址/模型（用户仍可手改）。
                if let Some(provider) = config::get_provider(&provider_id) {
                    window.set_base_url(provider.base_url.into());
                    if !provider.default_model.is_empty() {
                        window.set_model_name(provider.default_model.into());
                    }
                }
                window.set_has_key(has_key);
            }
        });
    }
    {
        /*
         * 确认答复必须带请求 id：重复点击、上一次确认的迟到点击都会被丢弃，
         * 不会让"下一次工具"白捡一次授权。
         */
        let confirmer = confirmer.clone();
        window.on_confirm_decision(move |request_id, allow| {
            let answer = if allow {
                ConfirmAnswer::allow_click()
            } else {
                ConfirmAnswer::deny_click()
            };
            // 只有与当前挂起请求匹配的答复才会被接受。
            confirmer.submit(request_id.max(0) as u64, answer);
        });
    }
    {
        let tx = command_tx.clone();
        window.on_set_voice_enabled(move |enabled| {
            // 立即反映开关状态；真正的启动/停止由工作线程完成，失败会回报原因。
            let _ = tx.send(UiCommand::SetVoiceEnabled(enabled));
        });
    }
    {
        let tx = command_tx.clone();
        window.on_stop_speaking(move || {
            let _ = tx.send(UiCommand::StopSpeaking);
        });
    }
    {
        let tx = command_tx.clone();
        window.on_test_voice(move || {
            let _ = tx.send(UiCommand::SpeakTest);
        });
    }
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        let confirmer = confirmer.clone();
        window.on_quit_app(move || {
            if let Some(window) = weak.upgrade() {
                window.set_confirm_open(false);
            }
            // 让挂起中的确认立刻结束（拒绝），不留一个还能点的框。
            confirmer.abort_pending();
            let _ = tx.send(UiCommand::Quit);
        });
    }
    // ---- 工作线程 ----
    let weak = window.as_weak();
    std::thread::Builder::new()
        .name("cuava-worker".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let message = format!("无法启动后台线程：{error}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            window.set_status(message.into());
                        }
                    });
                    return;
                }
            };
            let weak_for_exit = weak.clone();
            runtime.block_on(run_worker(
                weak,
                paths,
                secret,
                settings,
                command_rx,
                command_tx,
                confirmer,
                save_generation,
            ));
            /*
             * 工作线程结束 = 待处理回合已被取消、PowerShell 子进程已释放。
             * 只有到这一刻才真正关窗口并结束事件循环——
             * 这样"退出"不会在后台还留着子进程时先把界面关掉。
             */
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_exit.upgrade() {
                    let _ = window.hide();
                }
                let _ = slint::quit_event_loop();
            });
        })?;

    window.run()?;
    Ok(())
}

/// 把初始配置同步到界面控件。
fn seed_ui(window: &AppWindow, settings: &Settings, has_key: bool) {
    let provider_index = PROVIDER_IDS
        .iter()
        .position(|id| *id == settings.provider_id)
        .unwrap_or(2) as i32;
    window.set_provider_index(provider_index);
    window.set_base_url(settings.base_url.clone().into());
    window.set_model_name(settings.model.clone().into());
    window.set_has_key(has_key);
    let permission_index = PERMISSION_MODES
        .iter()
        .position(|mode| *mode == settings.permission_mode)
        .unwrap_or(0) as i32;
    window.set_permission_index(permission_index);
    window.set_endpoint_text(config::describe_endpoint(settings, has_key).into());
    window.set_entries(ModelRc::new(VecModel::from(Vec::<ChatEntry>::new())));
    window.set_tools(ModelRc::new(VecModel::from(Vec::<ToolCard>::new())));
}

/// 配置读不出来时的界面：把错误原文显示出来，**不伪造一份默认配置**。
///
/// 刻意什么都不预填（地址/模型留空、不显示端点摘要）：
/// 填上默认值会让用户以为"配置就是这个"，然后在保存时把它当真写回去，
/// 把原本那份（也许只是暂时读不了的）配置覆盖掉。
fn seed_ui_with_load_error(window: &AppWindow, paths: &AppPaths, message: &str) {
    window.set_entries(ModelRc::new(VecModel::from(Vec::<ChatEntry>::new())));
    window.set_tools(ModelRc::new(VecModel::from(Vec::<ToolCard>::new())));
    window.set_busy(false);
    window.set_provider_index(0);
    window.set_base_url("".into());
    window.set_model_name("".into());
    window.set_api_key_input("".into());
    window.set_has_key(false);
    window.set_settings_open(false);
    window.set_confirm_open(false);
    window.set_endpoint_text("配置未加载".into());
    window.set_context_text(format!("配置文件位置：{}", paths.settings_file().display()).into());
    window.set_status(format!("配置无法读取：{message}").into());
    push_entry(
        window,
        "系统",
        &format!(
            "配置文件读不出来，程序已停止加载配置。\n\n{message}\n\n\
             为了不覆盖你的设置，程序不会自动使用默认配置，也不会迁移旧版 Key。\n\
             修复或删除上面那个文件后重启即可。"
        ),
        "",
        "#fdeeee",
    );
}

/// 把 `#rrggbb` 解析成颜色；解析失败退回中性灰（绝不因为配色问题让界面崩掉）。
fn parse_hex_color(value: &str) -> slint::Color {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        ) {
            return slint::Color::from_rgb_u8(r, g, b);
        }
    }
    slint::Color::from_rgb_u8(255, 255, 255)
}

/// `push_entry` 的弱引用版本：工作线程里拿不到 `&AppWindow` 时用。
fn push_entry_via_weak(
    weak: &slint::Weak<AppWindow>,
    role: &str,
    text: &str,
    detail: &str,
    tint: &str,
) {
    let role = role.to_string();
    let text = text.to_string();
    let detail = detail.to_string();
    let tint = tint.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            push_entry(&window, &role, &text, &detail, &tint);
        }
    });
}

fn push_entry(window: &AppWindow, role: &str, text: &str, detail: &str, tint: &str) {
    let model = window.get_entries();
    let mut items: Vec<ChatEntry> = model.iter().collect();
    items.push(ChatEntry {
        role: role.into(),
        text: text.into(),
        detail: detail.into(),
        tint: parse_hex_color(tint),
    });
    // 只保留最近 500 条，避免长时间运行无限增长。
    if items.len() > 500 {
        items.drain(0..items.len() - 500);
    }
    window.set_entries(ModelRc::new(VecModel::from(items)));
}

fn update_tool(window: &AppWindow, name: &str, state: &str, detail: &str, tint: &str) {
    let model = window.get_tools();
    let mut items: Vec<ToolCard> = model.iter().collect();
    let color = parse_hex_color(tint);
    if let Some(existing) = items
        .iter_mut()
        .rev()
        .find(|card| card.name.as_str() == name)
    {
        existing.state = state.into();
        existing.detail = detail.into();
        existing.tint = color;
    } else {
        items.push(ToolCard {
            name: name.into(),
            state: state.into(),
            detail: detail.into(),
            tint: color,
        });
    }
    if items.len() > 200 {
        items.drain(0..items.len() - 200);
    }
    window.set_tools(ModelRc::new(VecModel::from(items)));
}

/// 一次待确认的请求。
struct PendingConfirm {
    request_id: u64,
    /// **一次性**回复通道：只有当前这次确认能收到它自己的答复。
    /// 用一次性通道而不是共享接收器，是为了让"上一次确认的重复点击"
    /// 根本没有地方可去——它发不到下一次确认上。
    reply: tokio::sync::oneshot::Sender<ConfirmAnswer>,
}

/// 把确认请求发到界面并等待用户点击。
///
/// 三条必须成立的规则（都是审查里发现的真实缺陷）：
///  1. **请求关联**：每次确认有自己的 id 与一次性回复通道；界面的答复必须带同一个 id
///     才会被接受，重复点击、过期答复、上一次的迟到点击都无法影响下一次授权。
///  2. **只结算一次**：一次性通道拿到值就消费掉，第二次发送必然失败。
///  3. **可中止**：取消令牌、退出、超时都会结束等待，并把确认框关掉。
struct UiConfirmer {
    weak: slint::Weak<AppWindow>,
    /// 当前挂起的确认（最多一个：权限门是串行询问的）。
    pending: Arc<std::sync::Mutex<Option<PendingConfirm>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// 当前挂起的 `request_id`（0 = 没有）。
    ///
    /// 与 `pending` 的互斥锁分开保存，是为了让**音频线程侧**（工作线程的
    /// 每秒若干次的聆听模式同步）读取时不需要碰确认通道的锁。
    pending_id: Arc<std::sync::atomic::AtomicU64>,
    /// 挂起状态变化时唤醒工作线程：否则等待点击确认期间没有命令进来，
    /// 语音确认模式永远进不去。
    changed: Arc<tokio::sync::Notify>,
    /// 用户多久不点就自动拒绝——避免"确认框一直开着，回合永远不结束"。
    timeout: std::time::Duration,
}

impl UiConfirmer {
    fn new(weak: slint::Weak<AppWindow>) -> Self {
        Self {
            weak,
            pending: Arc::new(std::sync::Mutex::new(None)),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            pending_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            changed: Arc::new(tokio::sync::Notify::new()),
            timeout: std::time::Duration::from_secs(120),
        }
    }

    /// 当前挂起的确认请求 id（`None` = 没有）。
    ///
    /// **语音确认只在这之后才允许进入**：没有挂起请求时说话不能结算任何东西。
    fn pending_request(&self) -> Option<u64> {
        match self.pending_id.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            id => Some(id),
        }
    }

    /// 挂起状态变化的通知句柄（工作线程用它唤醒自己的 select）。
    fn changes(&self) -> Arc<tokio::sync::Notify> {
        self.changed.clone()
    }

    fn publish_pending(&self, request_id: u64) {
        self.pending_id
            .store(request_id, std::sync::atomic::Ordering::SeqCst);
        // `notify_waiters` 只唤醒**已经**在等的人；调用方会在每个循环开头
        // 重新读一次 `pending_request()`，所以不会因为错过通知而卡住。
        self.changed.notify_waiters();
    }

    /// 立即中止当前挂起的确认（退出时用）。
    ///
    /// 用"拒绝"结算而不是直接丢掉通道：等待方拿到的是一份明确的拒绝，
    /// 工具不会执行，确认框也不会留到下一次。
    fn abort_pending(&self) {
        let Ok(mut guard) = self.pending.lock() else {
            return;
        };
        if let Some(pending) = guard.take() {
            let _ = pending.reply.send(ConfirmAnswer::deny_click());
        }
        drop(guard);
        self.publish_pending(0);
    }

    /// 在接受界面发来的一次答复。**只有** id 与当前挂起请求一致的才生效。
    ///
    /// 返回 `true` 表示确实结算了一次确认；返回 `false` 表示这次点击
    /// 已经被丢弃（重复点击、过期确认、没有挂起请求）。
    fn submit(&self, request_id: u64, answer: ConfirmAnswer) -> bool {
        let Ok(mut guard) = self.pending.lock() else {
            return false;
        };
        let Some(pending) = guard.as_ref() else {
            return false;
        };
        if pending.request_id != request_id {
            // 这条答复属于另一次（多半已经结束的）确认，丢弃。
            return false;
        }
        let Some(pending) = guard.take() else {
            return false;
        };
        // 一次性通道：发送成功才算这次确认被消费。
        let accepted = pending.reply.send(answer).is_ok();
        drop(guard);
        if accepted {
            clear_pending_id(&self.pending_id, &self.changed, request_id);
        }
        accepted
    }
}

/// 只清掉**匹配的**挂起 id（不误伤已经开始的下一次确认）。
fn clear_pending_id(
    pending_id: &Arc<std::sync::atomic::AtomicU64>,
    changed: &Arc<tokio::sync::Notify>,
    request_id: u64,
) {
    if request_id != 0
        && pending_id
            .compare_exchange(
                request_id,
                0,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    {
        changed.notify_waiters();
    }
}

impl Confirmer for UiConfirmer {
    fn confirm<'a>(
        &'a self,
        request: ConfirmRequest,
        cancel: &'a crate::cancel::CancelToken,
    ) -> ConfirmFuture<'a> {
        let weak = self.weak.clone();
        let pending = self.pending.clone();
        let pending_id = self.pending_id.clone();
        let changed = self.changed.clone();
        let request_id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let timeout = self.timeout;
        Box::pin(async move {
            let prompt = ConfirmationPrompt {
                tool: request.tool.clone(),
                capability: request.capability.to_string(),
                label: request.label.to_string(),
                reason: request.reason.clone(),
                args_summary: request.args_summary.clone(),
            };
            let args_text = serde_json::to_string(&prompt.args_summary).unwrap_or_default();

            let (reply, wait_reply) = tokio::sync::oneshot::channel();
            // 安装到槽位：界面点击时要能按 id 找到它。
            match pending.lock() {
                Ok(mut guard) => {
                    // 上一次没被清掉的（异常路径）直接作废，绝不让它残留。
                    *guard = Some(PendingConfirm { request_id, reply });
                }
                Err(_) => return ConfirmAnswer::deny_click(),
            }
            // 对外公布"现在挂起的是哪一次"：语音确认据此绑定请求 id，
            // 工作线程据此切换到语音确认聆听模式。
            pending_id.store(request_id, std::sync::atomic::Ordering::SeqCst);
            changed.notify_waiters();

            let weak_for_ui = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_ui.upgrade() {
                    window.set_confirm_tool(prompt.tool.clone().into());
                    window.set_confirm_label(prompt.label.clone().into());
                    window.set_confirm_reason(prompt.reason.clone().into());
                    window.set_confirm_args(args_text.clone().into());
                    // 界面把 id 存起来，点按钮时原样回传，避免"上一次的点击"混进来。
                    window.set_confirm_request_id(request_id as i32);
                    window.set_confirm_open(true);
                    window.set_status("等待你确认…".into());
                }
            });

            let answer = tokio::select! {
                answer = wait_reply => answer.unwrap_or_else(|_| ConfirmAnswer::deny_click()),
                _ = cancel.cancelled() => ConfirmAnswer::deny_click(),
                _ = tokio::time::sleep(timeout) => ConfirmAnswer::deny_click(),
            };

            // 无论哪种结局，都把槽位清掉并关闭确认框：
            // 不留下任何还能被下一次点击喂到的残留。
            if let Ok(mut guard) = pending.lock() {
                if guard
                    .as_ref()
                    .map(|entry| entry.request_id == request_id)
                    .unwrap_or(false)
                {
                    *guard = None;
                }
            }
            // 只有仍是这一次时才对外宣布"没有挂起请求"
            // （避免把紧接着出现的下一次确认也一起清掉）。
            clear_pending_id(&pending_id, &changed, request_id);
            let weak_for_close = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_close.upgrade() {
                    window.set_confirm_open(false);
                    window.set_confirm_request_id(0);
                }
            });
            answer
        })
    }
}

/// 工作线程主循环。
///
/// 返回后调用方会关闭窗口——所以这个函数**必须**保证：任何退出路径上，
/// 挂起的回合、确认框和 PowerShell 子进程都已收敛。
#[allow(clippy::too_many_arguments)]
async fn run_worker(
    weak: slint::Weak<AppWindow>,
    paths: AppPaths,
    secret: SecretStore,
    mut settings: Settings,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    command_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    confirmer: Arc<UiConfirmer>,
    save_generation: Arc<std::sync::atomic::AtomicU64>,
) {
    // Computer Use 后端：懒启动（第一次真正调用时才拉起 PowerShell）。
    let script = BridgeOptions::resolve_default_script();
    let bridge = match script {
        Some(path) => {
            set_status(&weak, "Computer Use 后端：按需启动");
            Some(PwshBridge::new(BridgeOptions::new(path)))
        }
        None => {
            set_status(
                &weak,
                "找不到 cua.ps1；Computer Use 工具将不可用（可用 CUAVA_CUA_SCRIPT 指定）",
            );
            None
        }
    };

    let mut toolkit: Option<Arc<CuaToolkit>> =
        bridge.map(|bridge| Arc::new(CuaToolkit::new(bridge, Some(paths.frames_dir()))));
    // 会话历史由回合任务持有并送回：回合在工作线程之外跑，主循环仍要能响应取消。
    let mut history: Vec<ChatMessage> = Vec::new();
    let mut turn_cancel: Option<CancelToken> = None;
    // 回合身份：每个 Agent 回合一个，播报据此判断"这段音频属于哪一轮"。
    let mut next_turn_id: TurnId = 0;

    // 语音线程发事件回工作线程：复用同一条命令通道，主循环天然串行处理。
    // 实例号/会话号的归属核对也在 `VoiceRuntime` 里，旧实例的迟到事件不会生效。
    let mut voice = VoiceRuntime::new();

    /*
     * 播报层（PR7）：引擎与设备都懒加载，事件同样回到这条命令通道。
     *
     * 用一条独立的转发任务而不是让播报层直接持有 `UiCommand` 发送端：
     * 播报层不依赖界面类型，可以脱离 Slint 单独测试。
     */
    let (speech_tx, mut speech_rx) = tokio::sync::mpsc::unbounded_channel::<SpeechEvent>();
    let mut speech = SpeechRuntime::new(speech_tx);
    // 界面侧的播报身份：迟到的旧播报事件不能覆盖新播报。
    let mut speech_view = SpeechView::default();
    {
        let forward = command_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = speech_rx.recv().await {
                if forward.send(UiCommand::Speech(event)).is_err() {
                    break;
                }
            }
        });
    }
    // 启用过的配置在启动时就尝试建立引擎：缺 Key / 缺模型时**只禁用播报**，
    // 界面显示原因，文本与 CUA 完全不受影响。
    if let Err(error) = speech.apply_settings(&settings.tts, &secret) {
        eprintln!("[tts] 播报不可用：{error}");
    }

    /*
     * 已保存的启用偏好在启动时**不自动开麦**：
     * 任务书要求「新安装启动时不意外开麦」，主人不在电脑前时更不该开。
     * 偏好只在用户点了开关之后才写回。
     */
    seed_voice_ui(&weak, &settings);
    seed_tts_ui(&weak, &settings, &speech);

    loop {
        // 每一轮都按"当前是否在播报 / 是否有挂起的确认"重新决定聆听模式：
        // 只有播报期间与等待确认期间才允许不喊唤醒词。
        sync_listen_mode(&mut voice, &speech, &confirmer);
        let Some(command) = commands.recv().await else {
            break;
        };
        match command {
            UiCommand::Quit => {
                // 取消当前回合，让它在下一个检查点结束；子进程由 break 之后的
                // dispose 负责清理。主循环不会再接受任何新命令。
                if let Some(token) = turn_cancel.take() {
                    token.cancel();
                }
                confirmer.abort_pending();
                // 退出先停声音再关麦：不留一段没人管的音频在网络/设备上跑。
                speech.shutdown();
                voice.disable(&weak);
                break;
            }
            UiCommand::Cancel => {
                if let Some(token) = turn_cancel.take() {
                    token.cancel();
                    set_status(&weak, "正在中断…");
                }
                // 取消任务 = 取消 Agent **并且**停掉本回合的声音。
                speech.stop();
                // 取消也作用于「正在录/正在识别的那一句」：之后返回的结果不提交。
                // 已经开始的 Agent 回合仍按原有取消语义处理。
                voice.cancel_pending();
            }
            UiCommand::StopSpeaking => {
                // **只停声音**：不取消回合、不结算权限请求、不关麦克风。
                if speech.is_speaking() {
                    speech.stop();
                    set_status(&weak, "已停止播报（任务继续）");
                } else {
                    set_status(&weak, "当前没有在播报");
                }
            }
            UiCommand::SpeakTest => handle_speak_test(&weak, &mut speech),
            UiCommand::SaveAll {
                request,
                generation,
            } => {
                /*
                 * 过期请求直接丢弃：用户已经取消、或又提交了更新的一次保存。
                 * 不落盘、不改界面——否则"取消之后旧表单仍被静默保存"。
                 */
                if !generation_is_current(&save_generation, generation) {
                    return_current_saving_state(&weak);
                } else {
                    let _ = handle_save_all_ui(
                        &weak,
                        &paths,
                        &secret,
                        &mut settings,
                        &mut speech,
                        &request,
                        generation,
                        &save_generation,
                    );
                }
            }
            UiCommand::CancelSettings { generation } => {
                // 取消 = 丢弃界面上的未保存修改，按已保存配置重新回填并关闭面板。
                if generation_is_current(&save_generation, generation) {
                    publish_tts_ui(&weak, &settings.tts, &speech);
                    let has_key = secret.has(&settings.provider_id);
                    apply_settings_to_ui(&weak, &settings, has_key);
                    close_settings(&weak);
                }
            }
            UiCommand::Speech(event) => {
                apply_speech_event(&mut speech_view, &weak, &event);
            }
            UiCommand::OpenSettings => {
                // 权威配置在工作线程：每次打开设置都按当前值回填，不用启动快照。
                let has_key = secret.has(&settings.provider_id);
                apply_settings_to_ui(&weak, &settings, has_key);
            }
            UiCommand::ProviderChanged(_index) => {
                // 地址/模型由界面侧回填；这里不需要动作。
            }
            UiCommand::ClearApiKey(provider_id) => {
                match secret.clear(&provider_id) {
                    Ok(()) => {
                        /*
                         * 设置页的 `has-key` 表示"**设置页当前选中**的提供商有没有 Key"。
                         *
                         * 用户点"清除"时，界面选中的就是 `provider_id`（命令携带的就是它），
                         * 所以状态要按它算——不能用 `settings.provider_id`：
                         * 那是**已保存**的那家，用户可能刚切走还没保存，两者不是一回事。
                         *
                         * 主界面的端点摘要反映的是**实际生效**的配置，
                         * 所以它继续按 `settings` 算，不跟着未保存的选择走。
                         */
                        let view = view_after_clear(&secret, &settings, &provider_id);
                        let weak_ui = weak.clone();
                        let provider_id_ui = provider_id.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = weak_ui.upgrade() {
                                window.set_has_key(view.selected_has_key);
                                window.set_endpoint_text(view.endpoint_summary.into());
                                window.set_status(format!("已清除 {provider_id_ui} 的 Key").into());
                            }
                        });
                    }
                    Err(error) => set_status(&weak, &format!("清除 Key 失败：{error}")),
                }
            }
            UiCommand::Send(input) => {
                // 新一轮开始：上一轮的声音立刻停下，绝不接在旧音频后面。
                speech.stop();
                next_turn_id += 1;
                match start_turn(
                    &weak,
                    &paths,
                    &secret,
                    &settings,
                    &toolkit,
                    &confirmer,
                    &mut history,
                    &mut turn_cancel,
                    input,
                    next_turn_id,
                ) {
                    TurnStart::Started(turn) => {
                        // 等待结束，但**同时**继续处理取消/退出/设置命令。
                        match wait_turn(
                            &mut commands,
                            turn,
                            &weak,
                            &mut turn_cancel,
                            &paths,
                            &secret,
                            &mut settings,
                            &confirmer,
                            &mut voice,
                            &mut speech,
                            &mut speech_view,
                            &command_tx,
                            &save_generation,
                            TurnSource::Text,
                        )
                        .await
                        {
                            TurnWait::Finished(returned) => history = returned,
                            TurnWait::Quit => break,
                        }
                    }
                    TurnStart::Rejected(status) => set_status(&weak, &status),
                }
            }
            UiCommand::SetVoiceEnabled(enabled) => {
                apply_voice_enabled(
                    &weak,
                    &paths,
                    &mut settings,
                    &mut voice,
                    &command_tx,
                    enabled,
                );
            }
            UiCommand::Voice(event) => {
                let prompt = handle_voice_event(
                    &weak,
                    &mut voice,
                    &confirmer,
                    &mut speech,
                    event,
                    turn_cancel.is_some(),
                );
                if let Some(prompt) = prompt {
                    // 语音指令也是一轮新的回合：先把上一轮的声音停掉。
                    speech.stop();
                    next_turn_id += 1;
                    let turn = commit_voice_text(
                        &weak,
                        prompt.text,
                        prompt.audio_seconds,
                        prompt.decode_ms,
                        &mut turn_cancel,
                        &toolkit,
                        &confirmer,
                        &mut history,
                        &settings,
                        &paths,
                        &secret,
                        next_turn_id,
                    );
                    if let Some(turn) = turn {
                        // 与文本输入完全相同的等待方式：取消/退出/设置照常处理。
                        match wait_turn(
                            &mut commands,
                            turn,
                            &weak,
                            &mut turn_cancel,
                            &paths,
                            &secret,
                            &mut settings,
                            &confirmer,
                            &mut voice,
                            &mut speech,
                            &mut speech_view,
                            &command_tx,
                            &save_generation,
                            TurnSource::Voice,
                        )
                        .await
                        {
                            TurnWait::Finished(returned) => history = returned,
                            TurnWait::Quit => break,
                        }
                    }
                }
            }
        }
    }

    // 退出前：停播报、关麦克风、取消回合、清掉确认、释放子进程。
    speech.shutdown();
    voice.disable(&weak);
    confirmer.abort_pending();
    if let Some(token) = turn_cancel.take() {
        token.cancel();
    }
    if let Some(toolkit) = toolkit.take() {
        toolkit.bridge().dispose().await;
    }
}

// ---------------------------------------------------------------- 语音

/// 当前语音实例与待提交会话的路由。
///
/// 纯状态，不持有线程和界面：这样「旧实例的迟到事件不得影响新实例」这类
/// 规则可以脱离真实麦克风做确定性测试。
#[derive(Debug, Default)]
struct VoiceRoute {
    instance: Option<VoiceInstance>,
    pending: Option<VoiceTurn>,
    next_instance: VoiceInstance,
    /// 当前实例自己的活动会话。
    ///
    /// 每开一次麦克风换一份新的：旧实例的线程（`shutdown` / `fail` / 迟到的
    /// ASR 回调）只持有**旧的那份**，清不到新实例正在识别的这一句。
    active: ActiveSession,
}

impl VoiceRoute {
    /// 打开麦克风：分配新的实例号与**新的**活动会话。
    ///
    /// 旧实例的迟到事件从这里开始不再有效；它的收尾动作也再碰不到新实例。
    fn open(&mut self) -> VoiceInstance {
        self.next_instance += 1;
        self.instance = Some(self.next_instance);
        self.pending = None;
        self.active = ActiveSession::new();
        self.next_instance
    }

    /// 关闭麦克风：当前实例作废（旧线程可以继续收尾，但不再代表“当前”）。
    fn close(&mut self) {
        self.instance = None;
        self.pending = None;
        // 只清**这一份**：让本实例的在途结果立刻过期（ASR 回调的归属核对会失败，
        // 音频线程也会据此置位 abort）。其它时代的实例各自持有自己的会话。
        self.active.clear();
    }

    /// 交给新实例的活动会话（内部共享同一份状态）。
    fn active_session(&self) -> ActiveSession {
        self.active.clone()
    }

    /// 让当前实例的在途输入过期（用户取消这一句）。
    fn clear_active(&self) {
        self.active.clear();
    }

    fn is_current(&self, instance: VoiceInstance) -> bool {
        self.instance == Some(instance)
    }

    /// 唤醒：记下这一句（只有当前实例才认）。
    fn bind(&mut self, instance: VoiceInstance, turn: VoiceTurn) -> bool {
        if !self.is_current(instance) {
            return false;
        }
        self.pending = Some(turn);
        true
    }

    /// 取走可提交的这一句：归属必须对得上，而且只取一次。
    fn take(&mut self, instance: VoiceInstance, turn: VoiceTurn) -> Option<VoiceTurn> {
        if !self.is_current(instance) || self.pending != Some(turn) {
            return None;
        }
        self.pending = None;
        Some(turn)
    }

    /// 这一句结束了（被拒绝/太短/空转录），只清归属匹配的那一句。
    fn release(&mut self, instance: VoiceInstance, turn: VoiceTurn) {
        if self.is_current(instance) && self.pending == Some(turn) {
            self.pending = None;
        }
    }

    /// 取消当前输入；返回被取消的那一句。
    fn cancel(&mut self) -> Option<VoiceTurn> {
        self.pending.take()
    }
}

/// 一次可提交的语音结果。
struct VoicePrompt {
    text: String,
    audio_seconds: f32,
    decode_ms: u64,
}

/// 一条语音事件处理完之后，工作线程该做什么。
enum VoiceOutcome {
    /// 只更新了界面状态。
    None,
    /// 可提交的指令（与文本输入走同一个 Agent 入口）。
    Prompt(VoicePrompt),
    /// 播报期间的识别结果：可能是打断词（是否真打断由工作线程结合回声判定）。
    BargeIn { heard: String },
    /// 权限确认期间的识别结果，已绑定到具体请求 id。
    Confirm { request_id: u64, text: String },
}

impl VoiceOutcome {
    /// 只有"要启动一个 Agent 回合"的那一种才算可提交。
    #[cfg(test)]
    fn into_prompt(self) -> Option<VoicePrompt> {
        match self {
            VoiceOutcome::Prompt(prompt) => Some(prompt),
            _ => None,
        }
    }
}

/// 语音运行时：线程句柄 + 实例路由（含当前实例自己的活动会话）。
struct VoiceRuntime {
    handle: Option<VoiceHandle>,
    route: VoiceRoute,
    /// 聆听模式（唤醒词 / 打断 / 语音确认）：音频线程读取。
    mode: ListenMode,
}

impl VoiceRuntime {
    fn new() -> Self {
        Self {
            handle: None,
            route: VoiceRoute::default(),
            mode: ListenMode::new(),
        }
    }

    /// 设置聆听模式（关麦时也会记下来，重开麦后沿用当前值）。
    fn set_listen_mode(&self, purpose: ListenPurpose, request_id: u64) {
        self.mode.set(purpose, request_id);
    }

    /// 打开麦克风。
    ///
    /// 模型加载与设备打开全在新线程上：加载要几百毫秒到几秒，不能堵住这里。
    /// 失败会以 `StateChanged(Error)` 回到主循环，界面如实显示，
    /// **文本 Agent 完全不受影响**。
    fn enable(
        &mut self,
        weak: &slint::Weak<AppWindow>,
        paths: &AppPaths,
        settings: &Settings,
        command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    ) {
        if self
            .handle
            .as_ref()
            .map(|handle| handle.is_running())
            .unwrap_or(false)
        {
            set_status(weak, "麦克风已经在监听");
            return;
        }
        // 上一个实例（如果还有句柄）到此彻底作废：新实例不该继承任何旧会话。
        self.handle = None;
        self.route.close();

        let voice_paths = VoicePaths::resolve(paths);
        let config = match CoordinatorConfig::resolve(&settings.voice, &voice_paths) {
            Ok(config) => config,
            Err(errors) => {
                // 模型缺失：明确提示，并且**不进入监听状态**。
                let message = errors.join("\n\n");
                set_voice_state(weak, &VoiceState::Error("缺少语音模型".to_string()));
                push_entry_via_weak(weak, "系统", &message, "", "#fdeeee");
                set_status(weak, "语音模型不完整；文本模式仍然可用");
                return;
            }
        };

        let tx = command_tx.clone();
        let sink = SharedSink::new(move |event| {
            let _ = tx.send(UiCommand::Voice(event));
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let instance = self.route.open();
        let active = self.route.active_session();
        match coordinator::spawn(config, sink, active, stop, instance, self.mode.clone()) {
            Ok(handle) => {
                self.handle = Some(handle);
                set_voice_state(weak, &VoiceState::Loading);
                set_status(weak, "正在启动麦克风…");
            }
            Err(error) => {
                self.route.close();
                set_voice_state(weak, &VoiceState::Error(error.clone()));
                set_status(weak, &format!("无法启动语音：{error}"));
            }
        }
    }

    /// 关闭麦克风：真的停止采集、让在途结果过期，并如实进入 Off。
    ///
    /// 与「偏好保存」无关：即使 `settings.json` 写失败，这里也必须真的关掉。
    fn disable(&mut self, weak: &slint::Weak<AppWindow>) {
        // 先让还在识别的结果过期，再停线程：迟到的文本不会落到新实例上。
        self.route.close();
        if let Some(mut handle) = self.handle.take() {
            handle.stop();
        }
        set_voice_state(weak, &VoiceState::Off);
    }

    /// 取消当前这一句（不关麦）：之后返回的识别结果不再提交。
    fn cancel_pending(&mut self) {
        self.route.cancel();
        self.route.clear_active();
    }

    /// 处理一条语音事件。
    ///
    /// `agent_busy` 为真时（有回合在跑、含等待点击确认）**只更新状态，
    /// 不启动第二回合，也不排队延后执行**。
    fn on_event(
        &mut self,
        weak: &slint::Weak<AppWindow>,
        event: VoiceEvent,
        agent_busy: bool,
    ) -> VoiceOutcome {
        // 旧实例的迟到事件：不改界面状态、不动新会话、不触发任何工具。
        if !self.route.is_current(event.instance) {
            return VoiceOutcome::None;
        }
        match event.kind {
            VoiceEventKind::StateChanged(state) => {
                set_voice_state(weak, &state);
                set_status(weak, &state.label());
                if !state.is_active() {
                    // 线程已结束或即将结束：释放句柄，但保留界面上的原因。
                    self.route.close();
                    if let Some(mut handle) = self.handle.take() {
                        handle.stop();
                    }
                }
                VoiceOutcome::None
            }
            VoiceEventKind::Ready(info) => {
                apply_capture_info(weak, &info);
                VoiceOutcome::None
            }
            VoiceEventKind::Woke { turn, keyword } => {
                self.route.bind(event.instance, turn);
                set_voice_state(weak, &VoiceState::Capturing);
                set_status(weak, &format!("已唤醒（{keyword}），请说指令"));
                VoiceOutcome::None
            }
            VoiceEventKind::DirectStarted {
                turn,
                purpose,
                request_id,
            } => {
                self.route.bind(event.instance, turn);
                set_voice_state(weak, &VoiceState::Capturing);
                match purpose {
                    ListenPurpose::BargeIn => {
                        set_status(weak, "正在听打断词（说「等一下」可以停止播报）")
                    }
                    ListenPurpose::Confirm => {
                        let _ = request_id;
                        set_status(weak, "请说「允许」或「拒绝」（也可以直接点按钮）")
                    }
                    ListenPurpose::Wake => {}
                }
                VoiceOutcome::None
            }
            VoiceEventKind::Level { level_db, floor_db } => {
                apply_level(weak, level_db, floor_db);
                VoiceOutcome::None
            }
            VoiceEventKind::Rejected { turn, reason } => {
                self.route.release(event.instance, turn);
                // 空转录/太短属于「没听清」，给一句可读的提示；
                // 不需要弹窗，也不能触发工具。直通聆听时保持安静更合适。
                if self.mode.purpose().is_direct() {
                    return VoiceOutcome::None;
                }
                if matches!(
                    reason,
                    RejectReason::EmptyTranscript | RejectReason::TooShort
                ) {
                    set_status(weak, "没听清，再说一次？");
                } else {
                    set_status(weak, &reason.message());
                }
                VoiceOutcome::None
            }
            VoiceEventKind::Overflow => {
                set_status(weak, "音频积压，已丢弃这一句");
                VoiceOutcome::None
            }
            VoiceEventKind::Final {
                turn,
                text,
                audio_seconds,
                decode_ms,
                purpose,
                request_id,
            } => {
                // 归属核对 + 一次性消费：过期/重复的结果绝不提交。
                if self.route.take(event.instance, turn).is_none() {
                    if !purpose.is_direct() {
                        set_status(weak, "这次语音已取消");
                    }
                    return VoiceOutcome::None;
                }
                set_transcript(weak, &text);
                match purpose {
                    ListenPurpose::Wake => {
                        if agent_busy {
                            set_status(weak, "上一轮还在跑；这句话没有执行");
                            return VoiceOutcome::None;
                        }
                        VoiceOutcome::Prompt(VoicePrompt {
                            text,
                            audio_seconds,
                            decode_ms,
                        })
                    }
                    // 打断词**不会**被当成新指令送进 Agent：只用于停止播报。
                    ListenPurpose::BargeIn => VoiceOutcome::BargeIn { heard: text },
                    ListenPurpose::Confirm => VoiceOutcome::Confirm { request_id, text },
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 播报

/// 试音用的固定文本与"回合身份"（不产生 Agent 回合，也不进会话历史）。
const TEST_SPEECH_TURN: TurnId = 0;
const TEST_SPEECH_TEXT: &str = "语音播报已就绪。这是一次试音，数字 123，mixed English 也能读。";

/// 一次回合的输入来源：决定这条回复要不要自动播报。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnSource {
    Text,
    Voice,
}

impl TurnSource {
    fn is_voice(&self) -> bool {
        matches!(self, Self::Voice)
    }
}

/// 播报设置下拉框的顺序（与界面数组一致）。
const TTS_BACKENDS: &[TtsBackend] = &[TtsBackend::Off, TtsBackend::Mimo, TtsBackend::Local];

fn tts_backend_for_index(index: i32) -> TtsBackend {
    usize::try_from(index)
        .ok()
        .and_then(|index| TTS_BACKENDS.get(index).copied())
        .unwrap_or(TtsBackend::Off)
}

fn tts_backend_index(backend: TtsBackend) -> i32 {
    TTS_BACKENDS
        .iter()
        .position(|candidate| *candidate == backend)
        .unwrap_or(0) as i32
}

/// 按"当前在播报 / 有挂起的确认"决定聆听模式。
///
/// 这是"什么时候可以不喊唤醒词"的**唯一**开关：
///  - 有挂起的 `ask` → 听「允许 / 拒绝」，并绑定该请求 id；
///  - 正在播报 → 听打断词；
///  - 其它时候 → 必须唤醒词（与 PR6 一致）。
fn sync_listen_mode(voice: &mut VoiceRuntime, speech: &SpeechRuntime, confirmer: &UiConfirmer) {
    let desired = if let Some(request_id) = confirmer.pending_request() {
        (ListenPurpose::Confirm, request_id)
    } else if speech.is_speaking() {
        (ListenPurpose::BargeIn, 0)
    } else {
        (ListenPurpose::Wake, 0)
    };
    if voice.mode.get() != desired {
        voice.set_listen_mode(desired.0, desired.1);
    }
}

/// 处理一条语音事件：只更新状态 / 提交指令 / 停止播报 / 结算语音确认。
///
/// 返回 `Some(prompt)` 表示**要启动一个 Agent 回合**（只有常规唤醒路径会这样）。
fn handle_voice_event(
    weak: &slint::Weak<AppWindow>,
    voice: &mut VoiceRuntime,
    confirmer: &Arc<UiConfirmer>,
    speech: &mut SpeechRuntime,
    event: VoiceEvent,
    agent_busy: bool,
) -> Option<VoicePrompt> {
    match voice.on_event(weak, event, agent_busy) {
        VoiceOutcome::None => None,
        VoiceOutcome::Prompt(prompt) => Some(prompt),
        VoiceOutcome::BargeIn { heard } => {
            /*
             * 打断判定：识别文本命中打断词，且不是我们自己正在播的内容。
             *
             * 旧版实测的教训：只靠能量阈值会被咳嗽/笑声打断；
             * 这里的"有声判断"由 VAD 的有声时长与端点检测把关（能进到这里
             * 就说明真的有人说了话），词语判断由 ASR 文本负责。
             */
            let verdict = bargein::judge(&heard, &speech.recent_sentences());
            match verdict.reason {
                bargein::BargeReason::Matched => {
                    speech.stop();
                    set_status(
                        weak,
                        &format!("已停止播报（听到「{}」）", verdict.word.unwrap_or_default()),
                    );
                }
                bargein::BargeReason::Echo => {
                    // 自己读出来的内容被麦克风收回来：静默忽略，不能自我打断。
                }
                bargein::BargeReason::NoWord => {
                    set_status(
                        weak,
                        &format!("听到「{heard}」，没有打断词；说「等一下」可以停止播报"),
                    );
                }
            }
            None
        }
        VoiceOutcome::Confirm { request_id, text } => {
            settle_voice_confirmation(weak, confirmer, request_id, &text);
            None
        }
    }
}

/// 用一句识别文本结算挂起的权限确认。返回是否真的结算了。
///
/// 绑定规则由 [`confirm::resolve`] 保证：过期请求、重复答复、旧麦克风实例
/// 的结果都不会批准别的请求。
fn settle_voice_confirmation(
    weak: &slint::Weak<AppWindow>,
    confirmer: &Arc<UiConfirmer>,
    bound_request: u64,
    text: &str,
) -> bool {
    match confirm::resolve(confirmer.pending_request(), bound_request, text) {
        ConfirmDecision::Allow(id) => {
            if confirmer.submit(id, ConfirmAnswer::allow_voice()) {
                set_status(weak, "已用语音允许这一次操作");
                true
            } else {
                set_status(weak, "这次确认已经结束");
                false
            }
        }
        ConfirmDecision::Deny(id) => {
            if confirmer.submit(id, ConfirmAnswer::deny_voice("用户用语音拒绝了这一次操作"))
            {
                set_status(weak, "已用语音拒绝这一次操作");
                true
            } else {
                set_status(weak, "这次确认已经结束");
                false
            }
        }
        ConfirmDecision::Ignore(reason) => {
            set_status(weak, &reason);
            false
        }
    }
}

/// 回合正常完成后播报可见回复。
///
/// 只播 `TurnOutcome::text`（经过 Agent 汇总的**用户可见**文本）：
/// 思考内容、工具 JSON、内部提示都不在这里，也不可能从这里漏出去。
fn maybe_speak(
    weak: &slint::Weak<AppWindow>,
    speech: &mut SpeechRuntime,
    settings: &Settings,
    outcome: &TurnOutcome,
    turn_id: TurnId,
    source: TurnSource,
) {
    if outcome.cancelled || outcome.failed {
        // 失败/中断的回合不播报：把"已中断"读出来只会让人更困惑。
        return;
    }
    if !settings.tts.speaks(source.is_voice()) {
        return;
    }
    let text = outcome.text.trim();
    if text.is_empty() {
        return;
    }
    if let Err(error) = speech.speak(turn_id, text) {
        // 播报不可用只影响声音：文本已经在界面上，工具结果也已经落定。
        set_speech_unavailable(weak, &error);
    }
}

fn set_speech_unavailable(weak: &slint::Weak<AppWindow>, reason: &str) {
    let reason = reason.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_speech_state("unavailable".into());
            window.set_speech_detail(format!("播报不可用：{reason}").into());
        }
    });
}

/// 界面侧的播报身份：**只有当前有效播报**的事件可以改状态、字幕和提示。///
/// 复用播报层已有的 `speech_id`（单调递增，一次播报一个），不另造 generation：
///  - 更新的 `speech_id` 自动接管（新播报开始）；
///  - 比当前更旧的 `speech_id` 一律忽略——A 停止后立刻开始 B 时，A 迟到的
///    `Stopped` / `Finished` / `Failed` / 字幕清空都不能覆盖 B。
#[derive(Debug, Default)]
struct SpeechView {
    /// 当前（或最近一次）播报的身份；`None` = 还没有播报。
    current: Option<SpeechId>,
}

impl SpeechView {
    /// 这条事件是否属于当前有效播报；新播报会在这里接管界面。
    fn accepts(&mut self, speech_id: SpeechId) -> bool {
        match self.current {
            Some(current) if speech_id < current => false,
            Some(current) if speech_id == current => true,
            _ => {
                self.current = Some(speech_id);
                true
            }
        }
    }

    #[cfg(test)]
    fn current(&self) -> Option<SpeechId> {
        self.current
    }
}

/// 播报事件 → 界面（状态、字幕、提示）。
fn apply_speech_event(view: &mut SpeechView, weak: &slint::Weak<AppWindow>, event: &SpeechEvent) {
    let speech_id = match event {
        SpeechEvent::State { speech_id, .. }
        | SpeechEvent::Subtitle { speech_id, .. }
        | SpeechEvent::Notice { speech_id, .. } => *speech_id,
    };
    if !view.accepts(speech_id) {
        // 旧播报的迟到事件：不改状态、不改字幕、也不弹提示。
        return;
    }
    match event {
        SpeechEvent::State { state, .. } => {
            let code = state.code().to_string();
            let label = state.label();
            let label_for_loop = label.clone();
            let active = state.is_active();
            let failed = matches!(state, PlaybackState::Failed(_));
            let weak_ui = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_ui.upgrade() {
                    window.set_speech_state(code.into());
                    window.set_speech_detail(label_for_loop.into());
                    window.set_speech_active(active);
                    if !active {
                        window.set_speech_subtitle("".into());
                    }
                }
            });
            if failed {
                set_status(weak, &label);
            }
        }
        SpeechEvent::Subtitle { text, .. } => {
            let text = text.to_string();
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_speech_subtitle(text.into());
                }
            });
        }
        SpeechEvent::Notice { text, .. } => {
            set_status(weak, text);
            let detail = text.clone();
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_speech_detail(detail.into());
                }
            });
        }
    }
}

/// 界面上「保存」一次提交的全部内容（模型端点 + 语音播报）。
///
/// 为什么合并成一个请求：原来是两条独立命令，各自落盘、各自关面板，
/// 于是「模型设置写失败但播报设置写成功」会把面板关掉，用户既看不到错误、
/// 也不知道哪一半生效了。现在由 [`apply_save_all`] 当作**一个事务**处理。
#[derive(Debug, Clone)]
pub struct SaveAllRequest {
    pub provider_index: i32,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub permission_index: i32,
    pub tts_enabled: bool,
    pub tts_backend_index: i32,
    /// 界面上的文本原样传入：解析失败要**如实报错**，不能静默改成默认值。
    pub local_sid: String,
    pub local_speed: String,
    pub mimo_voice: String,
    pub mimo_style: String,
    pub local_dir: String,
    pub speak_mode: i32,
}

impl SaveAllRequest {
    /// 音色编号/语速是文本输入：解析失败返回可读错误（保留用户输入）。
    fn parse_local_numbers(&self) -> Result<(i32, f32), String> {
        let sid_text = self.local_sid.trim();
        let sid = if sid_text.is_empty() {
            0
        } else {
            sid_text
                .parse::<i32>()
                .map_err(|_| format!("音色编号必须是整数，收到「{sid_text}」"))?
        };
        let speed_text = self.local_speed.trim();
        let speed = if speed_text.is_empty() {
            1.0
        } else {
            speed_text
                .parse::<f32>()
                .map_err(|_| format!("语速必须是数字（0.5~2.0），收到「{speed_text}」"))?
        };
        Ok((sid, speed))
    }
}

/// 把界面上的「自动播报」选项翻译成两个布尔开关。
fn speak_flags(mode: i32) -> (bool, bool) {
    match mode {
        1 => (true, false),
        2 => (false, true),
        3 => (false, false),
        // 0 与越界都按「语音与文本」处理（与界面数组第一项一致）。
        _ => (true, true),
    }
}

/// 一次保存的**结果分类**（对应任务书第 4 条）。
///
/// 刻意做成枚举而不是几个布尔：界面要区分"没保存"和"保存了但播报暂不可用"，
/// 两者对用户的含义完全不同（前者要保留草稿继续改，后者可以关面板）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// 配置已写入，一切正常。
    Saved,
    /// 配置已写入，但播报当前不可用（缺 Key / 缺模型 / 设备问题）。
    ///
    /// **算保存成功**：用户的设置确实存下来了，只是引擎建不起来。
    SavedSpeechUnavailable(String),
    /// 配置已写入，但 Key 没有写进去（只有该提供商的 Key 受影响）。
    SavedKeyFailed(String),
    /// 配置**没有**写入：校验失败或落盘失败。界面必须保留草稿。
    NotSaved(String),
}

impl SaveOutcome {
    /// 配置是否已经落盘（决定界面要不要保留草稿）。
    pub fn config_written(&self) -> bool {
        !matches!(self, Self::NotSaved(_))
    }

    /// 是否可以在成功后关闭面板。
    pub fn can_close_panel(&self) -> bool {
        self.config_written()
    }

    /// 给用户看的一句话（状态栏与面板内提示共用）。
    pub fn message(&self) -> String {
        match self {
            Self::Saved => "设置已保存".to_string(),
            Self::SavedSpeechUnavailable(detail) => {
                format!("设置已保存，但播报暂不可用：{detail}")
            }
            Self::SavedKeyFailed(detail) => {
                format!("设置已保存，但 Key 没有写入：{detail}")
            }
            Self::NotSaved(detail) => detail.clone(),
        }
    }
}

/// **一次保存的全部逻辑**（模型端点 + 语音播报）。
///
/// 事务规则（对应任务书第 4 条）：
///  1. 先在内存里把两份配置都算出来并**一起校验**；任何一项不合法就整体失败，
///     此时磁盘和内存都没被改过，用户输入原样留在界面上；
///  2. 校验通过后**只写一次盘**（端点与播报本来就在同一份 settings.json 里）；
///  3. 写盘成功后再写 Key（只写到目标提供商名下，留空 = 不动）；
///  4. 最后重建 TTS 引擎。
///
/// 引擎建不起来（缺 Key / 缺模型）**不算保存失败**：配置本身合法，
/// 只是当前不可用——如实显示为警告，不把用户的选择回滚掉。
pub fn apply_save_all(
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    speech: &mut SpeechRuntime,
    request: &SaveAllRequest,
) -> SaveOutcome {
    // ---- 1) 组装候选配置（还没碰磁盘） ----
    let (sid, speed) = match request.parse_local_numbers() {
        Ok(values) => values,
        Err(error) => return SaveOutcome::NotSaved(error),
    };
    let (speak_voice, speak_text) = speak_flags(request.speak_mode);

    let previous = settings.clone();
    let mut next = settings.clone();
    next.provider_id = provider_id_for_index(request.provider_index).to_string();
    next.base_url = request.base_url.clone();
    next.model = request.model.clone();
    next.permission_mode = permission_mode_for_index(request.permission_index);
    next.tts.enabled = request.tts_enabled;
    next.tts.backend = tts_backend_for_index(request.tts_backend_index);
    next.tts.mimo_voice = request.mimo_voice.clone();
    next.tts.mimo_style = request.mimo_style.clone();
    next.tts.local_model_dir = request.local_dir.clone();
    next.tts.local_sid = sid;
    next.tts.local_speed = speed;
    next.tts.speak_voice_turns = speak_voice;
    next.tts.speak_text_turns = speak_text;

    // ---- 2) 整体校验（端点 + 播报一起） ----
    let next = match next.normalized() {
        Ok(next) => next,
        Err(error) => return SaveOutcome::NotSaved(error),
    };

    // ---- 3) 只写一次盘 ----
    if let Err(error) = config::save_settings(paths, &next) {
        return SaveOutcome::NotSaved(format!("写入配置文件失败：{error}"));
    }

    // 写盘成功那一刻起，配置就是"已保存"了。后面无论 Key 或引擎出什么问题，
    // 都不能再报成"没有保存"——否则用户会以为改动丢了，然后重复保存。
    *settings = next;

    // ---- 4) 再写 Key（留空 = 不动该提供商的 Key） ----
    let key_provider = settings.provider_id.clone();
    let new_key = request.api_key.trim().to_string();
    let key_error = if new_key.is_empty() {
        None
    } else {
        secret.save(&key_provider, &new_key).err()
    };

    /*
     * ---- 5) 只在**播报配置真的变了**时才重建引擎 ----
     *
     * 只改模型端点 / 推理 / 权限时，TTS 的有效配置与相关凭据都没变：
     * 重建会 `stop()` 掉正在播的内容，还会让本地模型重新加载 + 预热（数秒）。
     * 这里先判断"是否需要"，避免完全不必要的打断。
     */
    // 只有"给当前启用的 MiMo 后端换了新 Key"才算凭据变化。
    let key_changed_for_active_backend = key_error.is_none()
        && !new_key.is_empty()
        && settings.tts.active_backend() == TtsBackend::Mimo;
    let tts_changed =
        tts_runtime_config_changed(&previous, settings, key_changed_for_active_backend);
    let speech_warning = if tts_changed {
        rebuild_speech(settings, secret, speech)
    } else {
        // 配置没变：保持当前引擎与正在播放的音频不动，只同步界面描述。
        None
    };

    match (key_error, speech_warning) {
        // Key 没写进去要单独说清楚：普通配置已保存，只有 Key 没存。
        (Some(error), warning) => {
            let mut detail = error.to_string();
            if let Some(warning) = warning {
                detail.push('；');
                detail.push_str(&warning);
            }
            SaveOutcome::SavedKeyFailed(detail)
        }
        (None, Some(warning)) => SaveOutcome::SavedSpeechUnavailable(warning),
        (None, None) => SaveOutcome::Saved,
    }
}

/// 这次保存是否真的改变了**播报运行时**的配置。
///
/// 只有这些变化才值得重建引擎（会 `stop()` 当前播报、重载本地模型并重新预热）：
///  - TTS 开关 / 后端；
///  - MiMo 的模型/音色/风格，本地模型的目录/音色编号/语速/提供者；
///  - 输出设备与超时。
///
/// **不需要**重建的：模型端点、推理参数、权限档位，以及"自动播报"两个开关
/// （它们只决定"要不要播"，不影响引擎本身）。
///
/// 另外，给**当前启用的云端后端**写入了新 Key 时，凭据变了也必须重建。
fn tts_runtime_config_changed(
    previous: &Settings,
    next: &Settings,
    key_changed_for_active_backend: bool,
) -> bool {
    let a = &previous.tts;
    let b = &next.tts;
    a.enabled != b.enabled
        || a.backend != b.backend
        || a.mimo_model != b.mimo_model
        || a.mimo_voice != b.mimo_voice
        || a.mimo_style != b.mimo_style
        || a.local_model_dir != b.local_model_dir
        || a.local_sid != b.local_sid
        || (a.local_speed - b.local_speed).abs() > f32::EPSILON
        || a.local_provider != b.local_provider
        || a.output_device != b.output_device
        || a.timeout_ms != b.timeout_ms
        || key_changed_for_active_backend
}

/// 按当前（已保存的）播报配置重建引擎。
///
/// 返回 `None` = 一切正常；`Some(原因)` = 配置已保存但引擎建不起来
/// （缺 Key / 缺模型 / 设备不可用）——这**不是**保存失败。
///
/// 必须用调用方传进来的 `SecretStore`（同一个数据目录），否则会在错误的
/// 目录里找 Key，误报"播报不可用"。
fn rebuild_speech(
    settings: &Settings,
    secret: &SecretStore,
    speech: &mut SpeechRuntime,
) -> Option<String> {
    speech.apply_settings(&settings.tts, secret).err()
}

/// 启动时把已保存的播报偏好同步到界面（**不**自动试音）。
fn seed_tts_ui(weak: &slint::Weak<AppWindow>, settings: &Settings, speech: &SpeechRuntime) {
    publish_tts_ui(weak, &settings.tts, speech);
    if let Some(error) = speech.engine_error() {
        set_speech_unavailable(weak, error);
    }
}

/// 把**已保存并生效**的播报配置推给界面。
///
/// 这里同时写入两组值：
///  - `tts-*`：设置页里可编辑的选择（保存或打开设置时用已保存值回填）；
///  - `tts-active-*`：当前真正生效的那一份配置（试音据此启用/说明）。
///
/// 两者在用户改动但未保存时会不一致，这正是这里要区分的地方。
fn publish_tts_ui(weak: &slint::Weak<AppWindow>, tts: &TtsSettings, speech: &SpeechRuntime) {
    let backend_index = tts_backend_index(tts.backend);
    let enabled = tts.enabled;
    let mimo_voice = tts.mimo_voice.clone();
    let mimo_style = tts.mimo_style.clone();
    let local_dir = tts.local_model_dir.clone();
    let local_sid = tts.local_sid.to_string();
    let local_speed = format!("{:.1}", tts.local_speed);
    let speak_mode = speak_mode_code(tts.speak_voice_turns, tts.speak_text_turns);
    let available = speech.is_available();
    let detail = speech
        .engine_label()
        .or_else(|| speech.engine_error().map(str::to_string))
        .unwrap_or_else(|| "未启用播报".to_string());
    // 「当前生效」的描述：引擎标签优先，否则说明为什么不可用。
    let active_label = if tts.active_backend() == crate::voice::tts::TtsBackend::Off {
        "关闭（不播报）".to_string()
    } else {
        speech
            .engine_label()
            .or_else(|| speech.engine_error().map(str::to_string))
            .unwrap_or_else(|| "未启用播报".to_string())
    };
    let active_ready =
        speech.is_available() && tts.active_backend() != crate::voice::tts::TtsBackend::Off;
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_tts_enabled(enabled);
            window.set_tts_backend_index(backend_index);
            window.set_tts_mimo_voice(mimo_voice.into());
            window.set_tts_mimo_style(mimo_style.into());
            window.set_tts_local_dir(local_dir.into());
            window.set_tts_local_sid(local_sid.into());
            window.set_tts_local_speed(local_speed.into());
            window.set_tts_speak_mode(speak_mode);
            window.set_tts_backend_ready(available);
            window.set_speech_detail(detail.into());
            window.set_tts_active_backend_index(backend_index);
            window.set_tts_active_label(active_label.into());
            window.set_tts_active_ready(active_ready);
        }
    });
}

/// 两个"是否自动播报"的开关 → 界面上的单一选项（0=都播，1=只语音，2=只文本，3=都不）。
fn speak_mode_code(voice: bool, text: bool) -> i32 {
    match (voice, text) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

/// 应用一次语音开关：**先操作设备，再谈持久化**。
///
/// 空闲与执行中走同一条路径：偏好保存失败只影响持久化，不妨碍本次开/关。
fn apply_voice_enabled(
    weak: &slint::Weak<AppWindow>,
    paths: &AppPaths,
    settings: &mut Settings,
    voice: &mut VoiceRuntime,
    command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    enabled: bool,
) {
    let mut next = settings.clone();
    next.voice.enabled = enabled;
    match config::save_settings(paths, &next) {
        Ok(()) => *settings = next,
        Err(error) => {
            // 偏好没存下来不影响本次启停，但必须如实告诉用户。
            set_status(weak, &format!("语音偏好保存失败：{error}"));
        }
    }
    if enabled {
        voice.enable(weak, paths, settings, command_tx);
    } else {
        voice.disable(weak);
    }
}

/// 语音最终文本 → 现有 Agent 入口（与文本发送同一条路径）。
///
/// 返回 `Some(turn)` 表示回合已启动，调用方负责等待；`None` 表示没有启动
/// （空文本，或 Agent 忙/等待确认/正在取消）。
#[allow(clippy::too_many_arguments)]
fn commit_voice_text(
    weak: &slint::Weak<AppWindow>,
    text: String,
    audio_seconds: f32,
    decode_ms: u64,
    turn_cancel: &mut Option<CancelToken>,
    toolkit: &Option<Arc<CuaToolkit>>,
    confirmer: &Arc<UiConfirmer>,
    history: &mut Vec<ChatMessage>,
    settings: &Settings,
    paths: &AppPaths,
    secret: &SecretStore,
    turn_id: TurnId,
) -> Option<RunningTurn> {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    // 先把结果放进对话区：用户看得见「我听到的是这句」。
    push_entry_via_weak(
        weak,
        "你（语音）",
        &trimmed,
        &format!("{audio_seconds:.1}s 音频 · 识别 {decode_ms} ms"),
        "#e8f0fe",
    );
    match start_turn(
        weak,
        paths,
        secret,
        settings,
        toolkit,
        confirmer,
        history,
        turn_cancel,
        trimmed,
        turn_id,
    ) {
        TurnStart::Started(turn) => {
            set_busy(weak, true);
            set_status(weak, "正在思考…");
            Some(turn)
        }
        TurnStart::Rejected(status) => {
            // Agent 忙 / 等待点击确认 / 正在取消：**不启动第二回合**，
            // 也不把这句话排队延后执行——明确告诉用户这句话没有执行。
            set_status(weak, &format!("{status}；这句话没有执行"));
            None
        }
    }
}

/// 把最终识别文字显示出来（只显示 Final；Preview 从不进入这里）。
fn set_transcript(weak: &slint::Weak<AppWindow>, text: &str) {
    let text = text.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_voice_transcript(text.into());
        }
    });
}

/// 语音状态 → 界面。开关状态与状态码分开：开关反映「用户想不想听」，
/// 状态反映「现在实际在做什么」。
fn set_voice_state(weak: &slint::Weak<AppWindow>, state: &VoiceState) {
    let code = state.code().to_string();
    // 只有真正可工作的状态才算「开」：Error 时麦克风并没有在跑，
    // 按钮要回到「打开麦克风」，让用户可以重试。
    let enabled = state.is_active();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_voice_state(code.into());
            window.set_voice_enabled(enabled);
        }
    });
}

/// 后端说明：只描述**实际**选择，不把用户期望当成事实。
fn describe_backend(info: &CaptureInfoForUi) -> String {
    match info.backend {
        AsrBackend::Cuda => "Whisper CUDA".to_string(),
        AsrBackend::Cpu => match &info.fallback_reason {
            Some(reason) => format!("Whisper CPU（{reason}）"),
            None => "Whisper CPU".to_string(),
        },
    }
}

/// 采集信息就绪：显示设备与实际采样率（用户能据此判断「是不是选错麦」）。
fn apply_capture_info(weak: &slint::Weak<AppWindow>, info: &CaptureInfoForUi) {
    let text = format!(
        "{} · {} Hz{} · 唤醒：{} · 模型：{}",
        info.device_name,
        info.sample_rate,
        if info.resampled {
            "（已重采样到 16k）"
        } else {
            ""
        },
        if info.wake_labels.is_empty() {
            "（未配置）".to_string()
        } else {
            info.wake_labels.join("/")
        },
        describe_backend(info)
    );
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_voice_device(text.into());
        }
    });
}

/// 电平显示：只给一个粗略的相对电平，避免刷屏。
fn apply_level(weak: &slint::Weak<AppWindow>, level_db: f32, floor_db: f32) {
    let text = format!("电平 {level_db:.0} dBFS / 底噪 {floor_db:.0} dB");
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_voice_level(text.into());
        }
    });
}

/// 启动时把已保存的语音偏好同步到界面（**不**自动开麦）。
fn seed_voice_ui(weak: &slint::Weak<AppWindow>, settings: &Settings) {
    let weak = weak.clone();
    let enabled = settings.voice.enabled;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            // 偏好为 true 时仍不自动开麦：界面开关显示为关，
            // 由用户点一下才真正启动（见 run_worker 里的说明）。
            window.set_voice_enabled(false);
            window.set_voice_state("off".into());
            if enabled {
                window.set_voice_device("上次退出时麦克风是开着的，点「打开麦克风」继续".into());
            }
        }
    });
}

/// 正在跑的一轮：任务句柄 + 回合身份。
///
/// 回合身份是播报层"这段音频属于哪个 Agent 回合"的依据：新回合一开始就换号，
/// 旧回合迟到的音频不会串到新回合里。
struct RunningTurn {
    task: tokio::task::JoinHandle<(TurnOutcome, Vec<ChatMessage>, Settings)>,
    turn_id: TurnId,
}

/// 一次回合的启动结果。
enum TurnStart {
    Started(RunningTurn),
    /// 没启动（上一轮还在跑 / 后端不可用 / 端点无效），附给用户看的原因。
    Rejected(String),
}

/// 启动一个 Agent 回合。文本输入与语音最终文本都**只**经由这里，
/// 所以两条路径的权限、取消与并发约束完全一致。
#[allow(clippy::too_many_arguments)]
fn start_turn(
    weak: &slint::Weak<AppWindow>,
    _paths: &AppPaths,
    secret: &SecretStore,
    settings: &Settings,
    toolkit: &Option<Arc<CuaToolkit>>,
    confirmer: &Arc<UiConfirmer>,
    history: &mut Vec<ChatMessage>,
    turn_cancel: &mut Option<CancelToken>,
    input: String,
    turn_id: TurnId,
) -> TurnStart {
    /*
     * 并发约束：上一轮还在跑（含等待点击确认）时**不启动第二回合**。
     * 语音文本也走这一条——不能因为「是语音说的」就插队或排队延后执行。
     */
    if turn_cancel.is_some() {
        return TurnStart::Rejected("上一轮还在跑，先取消再发新指令".to_string());
    }
    let Some(toolkit) = toolkit.clone() else {
        set_busy(weak, false);
        return TurnStart::Rejected("Computer Use 后端不可用，无法执行指令".to_string());
    };

    // 端点快照：整轮用同一套「地址 + Key + 模型 + 推理档位」。
    // **按提供商取 Key**：当前提供商没有就拿空串，绝不借用别家的。
    let api_key = secret
        .load(&settings.provider_id)
        .unwrap_or(None)
        .unwrap_or_default();
    let profile = settings.profile();
    let echo_reasoning = profile.capability.requires_reasoning_echo();
    let llm = match LlmClient::new(profile, api_key, settings.temperature, settings.timeout_ms) {
        Ok(client) => client,
        Err(error) => {
            set_busy(weak, false);
            return TurnStart::Rejected(format!("端点配置无效：{error}"));
        }
    };
    let model_context_window = crate::config::get_provider(&settings.provider_id)
        .and_then(|provider| provider.context_window);
    let policy = crate::context::resolve_context_policy(
        Some(settings.context_window),
        Some(settings.context_compress_percent),
        model_context_window,
    );
    let agent_config = AgentConfig {
        max_steps: settings.max_steps,
        context_policy: policy,
        system_prompt: prompt::resolve_system_prompt(settings.system_prompt.as_deref()),
        echo_reasoning,
    };
    let permissions = PermissionState::new(settings.permission_mode);
    let mut agent = Agent::new(llm, toolkit, agent_config, permissions);

    let token = CancelToken::new();
    // 记下本回合的令牌，让"取消"能作用于正在跑的这一轮。
    *turn_cancel = Some(token.clone());

    /*
     * 回合必须跑在**独立任务**里，不能在这里直接 await。
     *
     * 直接 await 会把整个工作线程占住，`Cancel` / `Quit` 命令排在它后面
     * 根本轮不到处理——用户点了"取消"却没有任何反应，直到模型自己跑完。
     * 放到 spawn 里之后，主循环随时能拿到命令，取消也就能立刻生效。
     */
    let weak_events = weak.clone();
    let settings_snapshot = settings.clone();
    let turn_confirmer = confirmer.clone();
    let turn_history = std::mem::take(history);
    let turn = tokio::spawn(async move {
        let emit = move |event: AgentEvent| {
            apply_event(&weak_events, event);
        };
        let mut turn_history = turn_history;
        let outcome = agent
            .run(&input, &mut turn_history, &token, &turn_confirmer, &emit)
            .await;
        (outcome, turn_history, settings_snapshot)
    });
    TurnStart::Started(RunningTurn {
        task: turn,
        turn_id,
    })
}

/// 等待回合结束期间的命令处理结果。
enum TurnWait {
    /// 回合正常结束（带回会话历史）。
    Finished(Vec<ChatMessage>),
    /// 用户要求退出：调用方应当结束主循环。
    Quit,
}

/// 一次"保存设置"请求（仅测试使用）。
///
/// 界面上的保存走 [`SaveAllRequest`]（端点 + 播报一起提交）；
/// 这里保留一个只含端点的入口，让既有测试继续覆盖真实保存路径。
#[cfg(test)]
struct SaveRequest {
    provider_index: i32,
    base_url: String,
    model: String,
    api_key: String,
    permission_index: i32,
}

/// 清除某个提供商的 Key 之后，界面该怎么更新。
///
/// 两个显示位置含义不同，不能混用同一个 provider：
///  - **设置页的 `has-key`** 反映"设置页**当前选中**的提供商有没有 Key"。
///    用户点"清除"时选中的就是 `cleared_provider`，所以按它算。
///    用已保存的 `settings.provider_id` 会答错——用户可能刚切走还没保存。
///  - **主界面端点摘要** 反映"实际生效的配置"，所以仍按 `settings` 算，
///    不跟着未保存的界面选择走。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClearedKeyView {
    /// 设置页应当显示的 has-key。
    selected_has_key: bool,
    /// 主界面应当显示的端点摘要。
    endpoint_summary: String,
}

fn view_after_clear(
    secret: &SecretStore,
    settings: &Settings,
    cleared_provider: &str,
) -> ClearedKeyView {
    ClearedKeyView {
        selected_has_key: secret.has(cleared_provider),
        // 生效摘要看的是已保存的配置，与界面上未保存的选择无关。
        endpoint_summary: config::describe_endpoint(settings, secret.has(&settings.provider_id)),
    }
}

/// 应用一次设置保存。返回 `true` 表示配置已落盘。
///
/// 这是界面上「保存」的**旧入口形状**，内部走统一事务 [`apply_save_all`]：
/// 模型端点与语音播报本来就写在同一份 settings.json 里，分开保存没有意义。
/// 保留这个签名是为了让既有测试继续覆盖真实保存路径（Key 归属、失败不落盘等）。
///
/// 关键规则（由 `apply_save_all` 保证）：
/// - 校验失败 / 写盘失败**都不更新**内存里的 `settings`，界面也拿不到"已保存"；
/// - Key 只写到**目标提供商**名下；留空表示"不动该提供商的 Key"；
/// - **先写配置、再写 Key**。反过来不行：先覆盖 Key 再回滚，
///   会在回滚时把用户原有的 Key A 一起删掉（原来 Bug 就是这样丢数据的）。
#[cfg(test)]
fn handle_save_settings(
    weak: &slint::Weak<AppWindow>,
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    request: SaveRequest,
    speech: &mut SpeechRuntime,
) -> bool {
    let all = SaveAllRequest {
        provider_index: request.provider_index,
        base_url: request.base_url,
        model: request.model,
        api_key: request.api_key,
        permission_index: request.permission_index,
        // 只改端点：播报设置按已保存的值原样带过（不会被覆盖）。
        tts_enabled: settings.tts.enabled,
        tts_backend_index: tts_backend_index(settings.tts.backend),
        local_sid: settings.tts.local_sid.to_string(),
        local_speed: settings.tts.local_speed.to_string(),
        mimo_voice: settings.tts.mimo_voice.clone(),
        mimo_style: settings.tts.mimo_style.clone(),
        local_dir: settings.tts.local_model_dir.clone(),
        speak_mode: speak_mode_code(
            settings.tts.speak_voice_turns,
            settings.tts.speak_text_turns,
        ),
    };
    let outcome = apply_save_all(paths, secret, settings, speech, &all);
    if !outcome.config_written() {
        set_status(weak, &format!("保存设置失败：{}", outcome.message()));
    }
    outcome.config_written()
}

/// 把设置（与 Key 状态）推给界面。
///
/// 打开设置时按当前配置回填界面。
fn apply_settings_to_ui(weak: &slint::Weak<AppWindow>, settings: &Settings, has_key: bool) {
    let provider_index = PROVIDER_IDS
        .iter()
        .position(|id| *id == settings.provider_id)
        .unwrap_or(2) as i32;
    let permission_index = PERMISSION_MODES
        .iter()
        .position(|mode| *mode == settings.permission_mode)
        .unwrap_or(0) as i32;
    let base_url = settings.base_url.clone();
    let model = settings.model.clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_provider_index(provider_index);
            window.set_base_url(base_url.into());
            window.set_model_name(model.into());
            window.set_api_key_input("".into());
            window.set_has_key(has_key);
            window.set_permission_index(permission_index);
            // 重新打开 = 以已保存配置为准：清掉上一次的错误与"未保存"标记。
            window.set_settings_error("".into());
            window.set_settings_dirty(false);
            window.set_settings_open(true);
        }
    });
}

/// 等待一个回合结束，同时响应取消、退出与设置命令。
///
/// 取消只置令牌（不打断进程读写），等工作线程自己走到检查点退出，
/// 这样"界面说停了"与"后台真的停了"是同一件事。
#[allow(clippy::too_many_arguments)]
async fn wait_turn(
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    turn: RunningTurn,
    weak: &slint::Weak<AppWindow>,
    turn_cancel: &mut Option<CancelToken>,
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    confirmer: &Arc<UiConfirmer>,
    voice: &mut VoiceRuntime,
    speech: &mut SpeechRuntime,
    speech_view: &mut SpeechView,
    command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    save_generation: &Arc<std::sync::atomic::AtomicU64>,
    source: TurnSource,
) -> TurnWait {
    let RunningTurn { mut task, turn_id } = turn;
    /*
     * 确认请求出现/结束时也要**立刻**切换聆听模式：
     * 否则回合卡在"等点击"的这段时间里，语音确认永远进不去（没有人按按钮，
     * 也没有命令进来唤醒这个 select）。
     */
    let pending_changes = confirmer.changes();
    loop {
        // 每一轮重新决定聆听模式（播报中 = 听打断词，等待确认 = 听允许/拒绝）。
        sync_listen_mode(voice, speech, confirmer);
        tokio::select! {
            joined = &mut task => {
                *turn_cancel = None;
                match joined {
                    Ok((outcome, history, settings_snapshot)) => {
                        finish_turn(weak, &outcome, &settings_snapshot);
                        maybe_speak(weak, speech, &settings_snapshot, &outcome, turn_id, source);
                        return TurnWait::Finished(history);
                    }
                    Err(error) => {
                        let outcome = TurnOutcome {
                            text: format!("后台任务异常：{error}"),
                            failed: true,
                            failure_reason: Some("worker_panic".to_string()),
                            ..TurnOutcome::default()
                        };
                        finish_turn(weak, &outcome, &Settings::default());
                        return TurnWait::Finished(Vec::new());
                    }
                }
            }
            _ = pending_changes.notified() => {
                // 只是唤醒一次循环：下一轮一开头会重新同步聆听模式。
            }
            command = commands.recv() => {
                match command {
                    Some(UiCommand::Cancel) => {
                        if let Some(token) = turn_cancel.as_ref() {
                            token.cancel();
                            set_status(weak, "正在中断…");
                        }
                        // 取消任务 = 取消 Agent **并且**停掉本回合的声音。
                        speech.stop();
                        // 语音输入也一并取消（不再提交），但不影响已开始回合的收尾。
                        voice.cancel_pending();
                    }
                    Some(UiCommand::StopSpeaking) => {
                        if speech.is_speaking() {
                            speech.stop();
                            set_status(weak, "已停止播报（任务继续）");
                        } else {
                            set_status(weak, "当前没有在播报");
                        }
                    }
                    Some(UiCommand::SpeakTest) => handle_speak_test(weak, speech),
                    Some(UiCommand::SaveAll {
                        request,
                        generation,
                    }) => {
                        // 过期请求丢弃（用户已取消或又提交了新的保存）。
                        if generation_is_current(save_generation, generation) {
                            let _ = handle_save_all_ui(
                                weak,
                                paths,
                                secret,
                                settings,
                                speech,
                                &request,
                                generation,
                                save_generation,
                            );
                        } else {
                            return_current_saving_state(weak);
                        }
                    }
                    Some(UiCommand::CancelSettings { generation }) => {
                        if generation_is_current(save_generation, generation) {
                            publish_tts_ui(weak, &settings.tts, speech);
                            let has_key = secret.has(&settings.provider_id);
                            apply_settings_to_ui(weak, settings, has_key);
                            close_settings(weak);
                        }
                    }
                    Some(UiCommand::Speech(event)) => {
                        apply_speech_event(speech_view, weak, &event);
                    }
                    Some(UiCommand::Quit) => {
                        if let Some(token) = turn_cancel.as_ref() {
                            token.cancel();
                        }
                        confirmer.abort_pending();
                        speech.shutdown();
                        set_status(weak, "正在退出…");
                        /*
                         * 回合会在下一个检查点结束；等它真的收尾再交回主循环，
                         * 这样"退出"不会在后台还留着一个工具在跑。
                         *
                         * 但要**有上限**：某个工具若不响应取消（例如卡在系统调用里），
                         * 无限等待会让"退出"变成"永远退不出去"。超时后直接放弃等待——
                         * 回合任务会在进程退出时被丢弃，PowerShell 子进程由 dispose 清理。
                         */
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            &mut task,
                        )
                        .await;
                        *turn_cancel = None;
                        return TurnWait::Quit;
                    }
                    Some(UiCommand::Send(_)) => {
                        set_status(weak, "上一轮还在跑，先取消再发新指令");
                    }
                    /*
                     * 回合进行中的语音命令：
                     *  - 开关仍走**同一条**路径：关麦必须真的停止采集，
                     *    偏好保存失败也不妨碍本次关麦；开麦同理立即生效。
                     *  - 语音最终文本不在这里插队：`on_event(agent_busy = true)`
                     *    只更新状态、不启动第二回合，也不排队延后执行。
                     *  - 打断词与语音确认**照常生效**：它们不启动回合。
                     *  - 设备状态、加载失败、溢出与会话结束这些合法事件照常处理。
                     */
                    Some(UiCommand::SetVoiceEnabled(enabled)) => {
                        apply_voice_enabled(weak, paths, settings, voice, command_tx, enabled);
                    }
                    Some(UiCommand::Voice(event)) => {
                        let _ = handle_voice_event(weak, voice, confirmer, speech, event, true);
                    }
                    Some(UiCommand::OpenSettings) => {
                        // 执行中也能查看当前（已保存的）配置。
                        let has_key = secret.has(&settings.provider_id);
                        apply_settings_to_ui(weak, settings, has_key);
                    }
                    Some(UiCommand::ProviderChanged(_)) => {}
                    Some(UiCommand::ClearApiKey(provider_id)) => {
                        /*
                         * 任务执行期间清除 Key：**真实执行**，不是只提示。
                         * 本轮已经在用启动时的端点快照（Key 已读进 LlmClient），
                         * 所以立即生效的是"下一轮不再带这个 Key"。
                         *
                         * 界面反馈与空闲路径保持一致：状态按被清除的那家（= 界面选中那家）算，
                         * 端点摘要仍按已生效的配置算。
                         */
                        match secret.clear(&provider_id) {
                            Ok(()) => {
                                let view = view_after_clear(secret, settings, &provider_id);
                                let weak_ui = weak.clone();
                                let provider_id_ui = provider_id.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(window) = weak_ui.upgrade() {
                                        window.set_has_key(view.selected_has_key);
                                        window.set_endpoint_text(view.endpoint_summary.into());
                                        window.set_status(
                                            format!(
                                                "已清除 {provider_id_ui} 的 Key（从下一轮生效）"
                                            )
                                            .into(),
                                        );
                                    }
                                });
                            }
                            Err(error) => {
                                set_status(weak, &format!("清除 Key 失败：{error}"));
                            }
                        }
                    }
                    None => return TurnWait::Quit,
                }
            }
        }
    }
}

/// 在界面线程上应用一次「保存」，并回填结果。
///
/// 关键规则（对应任务书第 2、4 条）：
///  - **配置没写进去**：显示失败、保留草稿、**不**用已生效的配置覆盖可编辑字段；
///  - **配置写了但播报不可用**：算保存成功，说明"已保存，但播报暂不可用"，关面板；
///  - **Key 没写进去**：说明普通配置已保存、只有 Key 没存，同样关面板；
///  - 只有"没写进去"时才留在面板里让用户改。
#[allow(clippy::too_many_arguments)]
fn handle_save_all_ui(
    weak: &slint::Weak<AppWindow>,
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    speech: &mut SpeechRuntime,
    request: &SaveAllRequest,
    generation: u64,
    save_generation: &Arc<std::sync::atomic::AtomicU64>,
) -> SaveOutcome {
    let outcome = apply_save_all(paths, secret, settings, speech, request);
    set_status(weak, &outcome.message());

    /*
     * 落盘期间用户可能已经取消、或又点了一次保存。
     * 只有代次仍是最新时才允许回填界面；否则这一次的结果属于"过期请求"，
     * 不能去覆盖用户当前的表单/状态。
     */
    if !generation_is_current(save_generation, generation) {
        return outcome;
    }

    // 只有配置真的落盘了，才刷新"已生效"状态；失败时**不动**界面上的可编辑字段，
    // 否则会把用户刚输入的非法内容悄悄换回旧值，让他无从修改。
    if outcome.config_written() {
        publish_tts_ui(weak, &settings.tts, speech);
        let has_key = secret.has(&settings.provider_id);
        let endpoint = config::describe_endpoint(settings, has_key);
        let message = outcome.message();
        let close = outcome.can_close_panel();
        let weak_ui = weak.clone();
        let save_generation = save_generation.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak_ui.upgrade() {
                // 事件循环里再核对一次：回填排队期间用户可能又操作了。
                if !generation_is_current(&save_generation, generation) {
                    return;
                }
                window.set_endpoint_text(endpoint.into());
                window.set_has_key(has_key);
                window.set_settings_error(message.into());
                window.set_settings_saving(false);
                if close {
                    window.set_settings_dirty(false);
                    window.set_api_key_input("".into());
                    window.set_settings_open(false);
                }
            }
        });
    } else {
        // 失败：留在面板里、保留草稿、只显示具体原因。
        let message = outcome.message();
        let weak_ui = weak.clone();
        let save_generation = save_generation.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak_ui.upgrade() {
                if !generation_is_current(&save_generation, generation) {
                    return;
                }
                window.set_settings_error(message.into());
                window.set_settings_saving(false);
            }
        });
    }
    outcome
}

/// 这次请求的代次是否仍是"最新"。
fn generation_is_current(
    save_generation: &Arc<std::sync::atomic::AtomicU64>,
    generation: u64,
) -> bool {
    save_generation.load(std::sync::atomic::Ordering::SeqCst) == generation
}

/// 丢弃过期保存请求后，把界面的"保存中"收掉（不能一直卡在保存中）。
fn return_current_saving_state(weak: &slint::Weak<AppWindow>) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_settings_saving(false);
        }
    });
}

/// 试音：只针对**已保存并生效**的配置，并在状态栏说清楚用的是哪一份。
fn handle_speak_test(weak: &slint::Weak<AppWindow>, speech: &mut SpeechRuntime) {
    if !speech.is_available() {
        let reason = speech
            .engine_error()
            .unwrap_or("语音播报未启用")
            .to_string();
        set_status(weak, &format!("试音失败：{reason}"));
        return;
    }
    let label = speech
        .engine_label()
        .unwrap_or_else(|| "当前配置".to_string());
    match speech.speak(TEST_SPEECH_TURN, TEST_SPEECH_TEXT) {
        Ok(_) => set_status(weak, &format!("正在试音（{label}）…")),
        Err(error) => set_status(weak, &format!("试音失败：{error}")),
    }
}

/// 关闭设置面板并清掉错误/脏标记（取消与保存成功共用）。
fn close_settings(weak: &slint::Weak<AppWindow>) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_settings_error("".into());
            window.set_settings_dirty(false);
            window.set_settings_saving(false);
            window.set_settings_open(false);
        }
    });
}

/// 把 Agent 事件映射到界面。
fn apply_event(weak: &slint::Weak<AppWindow>, event: AgentEvent) {
    match event {
        AgentEvent::TextDelta(text) => {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    append_assistant_delta(&window, &text);
                }
            });
        }
        AgentEvent::Reasoning { .. } => {
            // 思考内容不展示（只用于协议回传）。
        }
        AgentEvent::ToolStart { name, .. } => {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    update_tool(&window, &name, "running", "执行中…", "#f1f1f3");
                    window.set_status(format!("正在执行 {name}…").into());
                }
            });
        }
        AgentEvent::ToolEnd { name, ok, text, .. } => {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let (state, tint) = if ok {
                        ("ok", "#eef8ef")
                    } else {
                        ("failed", "#fdeeee")
                    };
                    let summary: String = text.chars().take(400).collect();
                    update_tool(&window, &name, state, &summary, tint);
                }
            });
        }
        AgentEvent::ConfirmationRequired(prompt) => {
            let weak = weak.clone();
            let args_text = serde_json::to_string(&prompt.args_summary).unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_confirm_tool(prompt.tool.into());
                    window.set_confirm_label(prompt.label.into());
                    window.set_confirm_reason(prompt.reason.into());
                    window.set_confirm_args(args_text.into());
                    window.set_confirm_open(true);
                }
            });
        }
        AgentEvent::Done(_) | AgentEvent::Failed { .. } => {
            // 收尾统一在 `finish_turn` 里做，避免两处各写一半状态。
        }
    }
}

/// 把流式增量追加到最后一个助手气泡（没有就新开一个）。
fn append_assistant_delta(window: &AppWindow, delta: &str) {
    let model = window.get_entries();
    let mut items: Vec<ChatEntry> = model.iter().collect();
    match items.last_mut() {
        Some(last) if last.role.as_str() == "助手" => {
            let mut text = last.text.to_string();
            text.push_str(delta);
            last.text = text.into();
        }
        _ => items.push(ChatEntry {
            role: "助手".into(),
            text: delta.into(),
            detail: "".into(),
            tint: slint::Color::from_rgb_u8(255, 255, 255),
        }),
    }
    window.set_entries(ModelRc::new(VecModel::from(items)));
}

/// 回合收尾：更新状态栏、补充错误信息、恢复界面。
fn finish_turn(weak: &slint::Weak<AppWindow>, outcome: &TurnOutcome, settings: &Settings) {
    let text = outcome.text.clone();
    let steps = outcome.steps.len();
    let failed = outcome.failed;
    let cancelled = outcome.cancelled;
    let reason = outcome.failure_reason.clone();
    let context_text = format!(
        "上下文 ≈ {} / {} tokens（{}% 触发压缩）",
        outcome.context_tokens, outcome.context_window, settings.context_compress_percent
    );
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };
        window.set_busy(false);
        window.set_context_text(context_text.into());
        // 如果助手气泡还是空的（纯工具回合），补一条可读结果。
        if !text.is_empty() {
            let model = window.get_entries();
            let items: Vec<ChatEntry> = model.iter().collect();
            let needs_push = !items
                .last()
                .map(|entry| entry.role.as_str() == "助手")
                .unwrap_or(false);
            if needs_push {
                push_entry(&window, "助手", &text, "", "#ffffff");
            }
        }
        let status = if cancelled {
            "已中断".to_string()
        } else if failed {
            format!("失败：{}", reason.unwrap_or_else(|| "未知原因".to_string()))
        } else {
            format!("完成（{} 步工具调用）", steps)
        };
        window.set_status(status.into());
    });
}

fn set_status(weak: &slint::Weak<AppWindow>, message: &str) {
    let weak = weak.clone();
    let message = message.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_status(message.into());
        }
    });
}

fn set_busy(weak: &slint::Weak<AppWindow>, busy: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_busy(busy);
        }
    });
}

/// 供测试使用：把值渲染成界面可见的字符串（不含密钥）。
pub fn settings_summary(settings: &Settings, has_key: bool) -> String {
    config::describe_endpoint(settings, has_key)
}

/// 供测试使用：判断某个 JSON 摘要里是否可能残留密钥。
pub fn summary_is_secret_free(value: &Value) -> bool {
    let text = value.to_string();
    !text.contains("sk-") && !text.contains("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个没有真实窗口的确认器（`submit`/`abort_pending` 不需要界面）。
    fn confirmer_without_ui() -> UiConfirmer {
        UiConfirmer {
            weak: slint::Weak::default(),
            pending: Arc::new(std::sync::Mutex::new(None)),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            pending_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            changed: Arc::new(tokio::sync::Notify::new()),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    /// 直接往槽位里装一条挂起的确认，模拟"界面正在等用户点按钮"。
    fn install_pending(
        confirmer: &UiConfirmer,
        request_id: u64,
    ) -> tokio::sync::oneshot::Receiver<ConfirmAnswer> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        *confirmer.pending.lock().unwrap() = Some(PendingConfirm { request_id, reply });
        confirmer.publish_pending(request_id);
        wait
    }

    /// 测试用播报运行时：没有引擎、没有设备（只验证"不播报"的路径）。
    fn speech_without_engine() -> SpeechRuntime {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SpeechRuntime::new(tx)
    }

    /// **需求 1**：从 MiMo 切到 DeepSeek 并保存后，配置里的 provider 是 DeepSeek，
    /// Key 写到 DeepSeek 名下，**原 MiMo Key 不被覆盖**。
    ///
    /// 这条直接对应"选 DeepSeek 却存成 MiMo"的回归：界面索引必须一路传到
    /// `provider_id`，而 Key 只按目标提供商写入。
    #[cfg(windows)]
    #[test]
    fn switching_provider_saves_to_the_selected_provider_and_keeps_other_keys() {
        let root = unique_test_root("worker-provider-switch");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());

        // 先按 MiMo 保存一次（带 MiMo 的 Key）。
        let mut settings = Settings::default();
        let mut speech = speech_without_engine();
        let mimo = save_all_request(
            0,
            "https://api.xiaomimimo.com/v1",
            "mimo-v2.6-flash",
            "sk-mimo-original",
        );
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &mimo);
        assert!(outcome.config_written(), "{outcome:?}");
        assert_eq!(settings.provider_id, "mimo");

        // 切到 DeepSeek 并保存（填 DeepSeek 的 Key）。
        let request = save_all_request(
            1,
            "https://api.deepseek.com/v1",
            "deepseek-v4-pro",
            "sk-deepseek-new",
        );
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);
        assert!(outcome.config_written(), "{outcome:?}");

        // 内存与磁盘都必须是 DeepSeek（"重新打开仍显示 DeepSeek"的依据）。
        assert_eq!(settings.provider_id, "deepseek");
        let reloaded = config::load_settings(&paths).unwrap();
        assert_eq!(reloaded.provider_id, "deepseek");
        assert_eq!(reloaded.base_url, "https://api.deepseek.com/v1");
        assert_eq!(reloaded.model, "deepseek-v4-pro");

        // Key 落在目标提供商名下；MiMo 的原 Key 必须完好无损。
        assert_eq!(
            secret.load("deepseek").unwrap().as_deref(),
            Some("sk-deepseek-new")
        );
        assert_eq!(
            secret.load("mimo").unwrap().as_deref(),
            Some("sk-mimo-original"),
            "切换提供商不能覆盖原来那家的 Key"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **需求 4**：缺本地 TTS 模型时配置照样保存，结果要报"已保存但播报暂不可用"，
    /// 而不是"没保存"。
    #[cfg(windows)]
    #[test]
    fn missing_local_tts_model_is_saved_but_reported_unavailable() {
        let root = unique_test_root("worker-tts-unavailable");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut speech = speech_without_engine();

        let mut request =
            save_all_request(0, "https://api.xiaomimimo.com/v1", "mimo-v2.6-flash", "");
        request.tts_enabled = true;
        request.tts_backend_index = 2; // 本地 ONNX（模型不存在）
        request.local_dir = "Z:/definitely/not/here".to_string();

        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);

        // 配置必须已经落盘——这是"保存成功"与"当前不可用"分开的关键。
        assert!(outcome.config_written(), "{outcome:?}");
        assert!(
            matches!(outcome, SaveOutcome::SavedSpeechUnavailable(_)),
            "缺模型必须归类为'已保存但暂不可用'：{outcome:?}"
        );
        assert!(
            outcome.message().contains("已保存") && outcome.message().contains("不可用"),
            "{}",
            outcome.message()
        );
        let reloaded = config::load_settings(&paths).unwrap();
        assert_eq!(reloaded.tts.backend, TtsBackend::Local);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **需求 4**：Key 写失败时不能报"全部成功"，要明确说普通配置已保存。
    #[cfg(windows)]
    #[test]
    fn key_write_failure_is_reported_as_partial_not_success() {
        let root = unique_test_root("worker-key-fail-partial");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut speech = speech_without_engine();

        // 让密钥写入一定失败：把 `secrets` 目录的位置占成一个**普通文件**。
        // 布局是 `<root>/secrets/<provider>.bin`，所以建目录这一步会失败，
        // 写 Key 随之返回错误——正是"配置能存、Key 存不了"的场景。
        std::fs::create_dir_all(&root).unwrap();
        let blocker = root.join("secrets");
        std::fs::write(&blocker, b"not-a-directory").unwrap();
        let request = save_all_request(
            1,
            "https://api.deepseek.com/v1",
            "deepseek-v4-pro",
            "sk-cannot-write",
        );
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);

        assert!(outcome.config_written(), "配置本身必须已保存：{outcome:?}");
        assert!(
            matches!(outcome, SaveOutcome::SavedKeyFailed(_)),
            "Key 失败必须单独分类：{outcome:?}"
        );
        assert!(
            outcome.message().contains("Key"),
            "必须说明是 Key 没写进去：{}",
            outcome.message()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **需求 5**：只改模型端点时，TTS 运行时配置不算变化 → 不重建引擎。
    #[test]
    fn llm_only_change_does_not_require_tts_rebuild() {
        let mut previous = Settings::default();
        previous.tts.enabled = true;
        previous.tts.backend = TtsBackend::Mimo;
        previous.tts.mimo_voice = "alloy".to_string();

        let mut next = previous.clone();
        next.provider_id = "deepseek".to_string();
        next.model = "deepseek-v4-pro".to_string();
        next.base_url = "https://api.deepseek.com/v1".to_string();
        next.permission_mode = PermissionMode::Conservative;

        // 这些断言只看配置差异，与凭据无关。
        assert!(
            !tts_runtime_config_changed(&previous, &next, false),
            "只改 LLM 端点 / 权限时不该重建 TTS（会停播并重载模型）"
        );

        // 真正的 TTS 改动仍然要重建。
        next.tts.mimo_voice = "nova".to_string();
        assert!(tts_runtime_config_changed(&previous, &next, false));

        let mut third = previous.clone();
        third.tts.backend = TtsBackend::Local;
        assert!(tts_runtime_config_changed(&previous, &third, false));

        let mut fourth = previous.clone();
        fourth.tts.enabled = false;
        assert!(tts_runtime_config_changed(&previous, &fourth, false));

        // 自动播报开关只影响"要不要播"，不要求重建引擎。
        let mut fifth = previous.clone();
        fifth.tts.speak_text_turns = false;
        assert!(
            !tts_runtime_config_changed(&previous, &fifth, false),
            "只改自动播报开关不该重建引擎"
        );

        // 给当前启用的 MiMo 换了新 Key：凭据变了，必须重建。
        assert!(
            tts_runtime_config_changed(&previous, &previous.clone(), true),
            "换了当前后端的 Key 必须重建引擎"
        );
    }

    /// **需求 5（行为级）**：播报正在进行时保存"只改 LLM"的设置，
    /// 当前播报不能被停掉。
    ///
    /// 这条直接验证"不停止当前播报"这条验收：`apply_save_all` 在
    /// `tts_runtime_config_changed == false` 时根本不调用 `apply_settings`，
    /// 而后者无条件 `stop()`。用真实 SpeechRuntime + 假扬声器证明它还在播。
    #[tokio::test]
    async fn saving_llm_only_settings_does_not_stop_current_speech() {
        use crate::voice::tts::{AudioFormat, SpeechChunk, SpeechStream};

        struct FakeEngine;
        impl crate::voice::tts::TtsEngine for FakeEngine {
            fn label(&self) -> String {
                "fake-mimo".to_string()
            }
            fn start(
                &self,
                request: crate::voice::tts::SynthRequest,
            ) -> Result<SpeechStream, String> {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let (sid, turn, sentence) = (request.speech_id, request.turn, request.sentence);
                tokio::spawn(async move {
                    let _ = tx.send(crate::voice::tts::TtsEvent::Chunk(SpeechChunk {
                        speech_id: sid,
                        turn,
                        sentence,
                        sample_rate: 24_000,
                        channels: 1,
                        format: AudioFormat::Pcm16Le,
                        samples: vec![0.2; 24_000],
                    }));
                    let _ = tx.send(crate::voice::tts::TtsEvent::Finished);
                });
                Ok(SpeechStream::new(rx))
            }
        }

        let root = unique_test_root("worker-llm-only-no-stop");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        let shared = Arc::new(crate::voice::playback::PlaybackShared::new(24_000, 1));
        speech.attach_playback_for_test(shared.clone());
        speech.attach_engine_for_test(Arc::new(FakeEngine));

        // 先让播报真的开始（有一段音频在缓冲里）。
        let mut settings = Settings::default();
        settings.tts.enabled = true;
        settings.tts.backend = TtsBackend::Mimo;
        speech.speak(1, "这一句正在播报").unwrap();
        for _ in 0..200 {
            if speech.is_speaking() && shared.progress().pushed_frames > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(speech.is_speaking(), "播报应当已经开始");
        let speech_id_before = speech.current_speech_id();

        // 保存"只改 LLM 模型"：TTS 配置原样带过。
        let mut request = SaveAllRequest {
            provider_index: 1,
            base_url: "https://api.deepseek.com/v1".to_string(),
            model: "deepseek-v4-pro".to_string(),
            api_key: String::new(),
            permission_index: 0,
            tts_enabled: true,
            tts_backend_index: 1,
            local_sid: "0".to_string(),
            local_speed: "1.0".to_string(),
            mimo_voice: settings.tts.mimo_voice.clone(),
            mimo_style: settings.tts.mimo_style.clone(),
            local_dir: String::new(),
            speak_mode: 0,
        };
        // 让请求里的播报字段与已保存的一致（只有 LLM 部分不同）。
        request.mimo_voice = settings.tts.mimo_voice.clone();
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);
        assert!(outcome.config_written(), "{outcome:?}");

        // 关键断言：播报没有被停掉，身份也没变。
        assert!(speech.is_speaking(), "只改 LLM 设置不该停止正在播报的内容");
        assert_eq!(
            speech.current_speech_id(),
            speech_id_before,
            "不该被当成新播报"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **需求 3**：只有最新一次保存的代号才算"当前"，旧回执必须被丢弃。
    #[test]
    fn stale_save_generation_is_rejected() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        assert!(generation_is_current(&generation, 1));
        assert!(!generation_is_current(&generation, 0));

        // 提交新保存后，旧的一代立刻失效（晚到的回执不能覆盖新表单）。
        generation.fetch_add(1, Ordering::SeqCst);
        assert!(!generation_is_current(&generation, 1));
        assert!(generation_is_current(&generation, 2));

        // 取消同样会把代次推高，作废所有在途请求。
        let cancelled = generation.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(!generation_is_current(&generation, 2));
        assert!(generation_is_current(&generation, cancelled));
    }

    /// 构造一次"只改端点、播报保持已保存值"的保存请求。
    fn save_all_request(
        provider_index: i32,
        base_url: &str,
        model: &str,
        key: &str,
    ) -> SaveAllRequest {
        SaveAllRequest {
            provider_index,
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: key.to_string(),
            permission_index: 0,
            tts_enabled: false,
            tts_backend_index: 0,
            local_sid: "0".to_string(),
            local_speed: "1.0".to_string(),
            mimo_voice: String::new(),
            mimo_style: String::new(),
            local_dir: String::new(),
            speak_mode: 0,
        }
    }

    /// **下拉选择映射**：界面索引 ↔ 配置枚举必须严格一一对应。
    ///
    /// 这条直接对应任务书「保持选中值与已有 Rust 配置的映射正确」：
    /// 用错一位就会把"本地 ONNX"存成"MiMo 云端"，而且界面上看不出来。
    #[test]
    fn selector_indices_map_to_the_right_configuration_values() {
        // 提供商：与界面数组 ["mimo","deepseek","openai-compatible"] 一致。
        assert_eq!(provider_id_for_index(0), "mimo");
        assert_eq!(provider_id_for_index(1), "deepseek");
        assert_eq!(provider_id_for_index(2), "openai-compatible");
        // 越界不能 panic，也不能悄悄落到某个官方提供商上。
        assert_eq!(provider_id_for_index(-1), "openai-compatible");
        assert_eq!(provider_id_for_index(99), "openai-compatible");

        // 权限档位：["standard","conservative","permissive"]
        assert_eq!(permission_mode_for_index(0), PermissionMode::Standard);
        assert_eq!(permission_mode_for_index(1), PermissionMode::Conservative);
        assert_eq!(permission_mode_for_index(2), PermissionMode::Permissive);
        assert_eq!(permission_mode_for_index(-1), PermissionMode::Standard);

        // TTS 后端：["关闭","MiMo 云端","本地 ONNX"]，且必须能往返。
        assert_eq!(tts_backend_for_index(0), TtsBackend::Off);
        assert_eq!(tts_backend_for_index(1), TtsBackend::Mimo);
        assert_eq!(tts_backend_for_index(2), TtsBackend::Local);
        for index in 0..3 {
            assert_eq!(
                tts_backend_index(tts_backend_for_index(index)),
                index,
                "TTS 后端索引必须能原样往返"
            );
        }
        assert_eq!(tts_backend_for_index(9), TtsBackend::Off);

        // 自动播报：0=都播 1=只语音 2=只文本 3=都不，往返一致。
        for (index, (voice, text)) in [
            (0, (true, true)),
            (1, (true, false)),
            (2, (false, true)),
            (3, (false, false)),
            // 越界按第一项处理，不 panic。
            (9, (true, true)),
        ] {
            assert_eq!(
                speak_flags(index),
                (voice, text),
                "自动播报选项 {index} 映射错误"
            );
            assert_eq!(
                speak_mode_code(voice, text),
                if index == 9 { 0 } else { index },
                "自动播报必须能往返"
            );
        }
    }

    /// 语速/音色编号是**文本输入**：非法值必须报具体错误，不能静默改默认值。
    #[test]
    fn invalid_numeric_tts_input_is_reported_not_silently_defaulted() {
        let mut request = save_all_request(0, "https://api.xiaomimimo.com/v1", "m", "");
        // 空 = 采用默认（不是错误）。
        assert_eq!(request.parse_local_numbers().unwrap(), (0, 1.0));

        request.local_speed = "快一点".to_string();
        let error = request.parse_local_numbers().unwrap_err();
        assert!(error.contains("语速"), "{error}");
        assert!(error.contains("快一点"), "错误里要带上用户输入：{error}");

        request.local_speed = "1.0".to_string();
        request.local_sid = "abc".to_string();
        let error = request.parse_local_numbers().unwrap_err();
        assert!(error.contains("音色编号"), "{error}");

        // 合法值照常解析。
        request.local_sid = "3".to_string();
        request.local_speed = "1.25".to_string();
        assert_eq!(request.parse_local_numbers().unwrap(), (3, 1.25));
    }

    /// **一次保存**：端点和播报写在同一次落盘里，两者都生效。
    #[cfg(windows)]
    #[test]
    fn save_all_persists_endpoint_and_tts_together() {
        let root = unique_test_root("worker-save-all");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut speech = speech_without_engine();

        let request = SaveAllRequest {
            provider_index: 1,
            base_url: "https://api.deepseek.com/v1".to_string(),
            model: "deepseek-v4-pro".to_string(),
            api_key: "sk-deepseek-test".to_string(),
            permission_index: 2,
            tts_enabled: true,
            tts_backend_index: 2, // 本地 ONNX
            local_sid: "7".to_string(),
            local_speed: "1.3".to_string(),
            mimo_voice: "alloy".to_string(),
            mimo_style: "语速稍快".to_string(),
            local_dir: "D:/models/kokoro".to_string(),
            speak_mode: 2, // 只播文本
        };
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);
        // 本地模型不存在 → 引擎建不起来，但**配置本身是合法的**：
        // 如实给出警告，不能当成保存失败把用户的选择回滚掉。
        assert!(
            outcome.config_written(),
            "配置本身合法，应当已落盘：{outcome:?}"
        );
        // 本地模型不存在 → 引擎建不起来，但那是"已保存但暂不可用"。
        assert!(matches!(
            outcome,
            SaveOutcome::Saved | SaveOutcome::SavedSpeechUnavailable(_)
        ));

        assert_eq!(settings.provider_id, "deepseek");
        assert_eq!(settings.model, "deepseek-v4-pro");
        assert_eq!(settings.permission_mode, PermissionMode::Permissive);
        assert_eq!(settings.tts.backend, TtsBackend::Local);
        assert!(settings.tts.enabled);
        assert_eq!(settings.tts.local_sid, 7);
        assert!((settings.tts.local_speed - 1.3).abs() < 1e-6);
        assert!(!settings.tts.speak_voice_turns);
        assert!(settings.tts.speak_text_turns);
        assert_eq!(
            secret.load("deepseek").unwrap().as_deref(),
            Some("sk-deepseek-test")
        );

        // 磁盘上是**同一份**配置里同时有端点和播报。
        let reloaded = config::load_settings(&paths).unwrap();
        assert_eq!(reloaded.model, "deepseek-v4-pro");
        assert_eq!(reloaded.tts.backend, TtsBackend::Local);
        assert_eq!(reloaded.tts.local_sid, 7);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 校验失败：**一样都不落盘**，内存配置不动（界面据此保留用户输入）。
    #[cfg(windows)]
    #[test]
    fn save_all_validation_failure_writes_nothing() {
        let root = unique_test_root("worker-save-all-bad");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let before = settings.clone();
        let mut speech = speech_without_engine();

        // 端点非法 + 合法播报设置：整体失败，播报也不该被写进去。
        let mut request =
            save_all_request(0, "ftp://not-http", "mimo-v2.6-flash", "sk-must-not-write");
        request.tts_enabled = true;
        request.tts_backend_index = 1;
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);

        assert!(!outcome.config_written(), "配置不该被写入");
        assert!(
            outcome.message().contains("http"),
            "{:?}",
            outcome.message()
        );
        assert_eq!(settings.base_url, before.base_url, "内存配置不能被改动");
        assert_eq!(
            settings.tts.backend, before.tts.backend,
            "播报设置也不能被写"
        );
        assert!(secret.load("mimo").unwrap().is_none(), "失败时不能写 Key");
        assert!(
            !paths.settings_file().is_file(),
            "失败时不应该有配置文件落盘"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 语速非法：给出具体原因，且不落盘（保留用户输入）。
    #[cfg(windows)]
    #[test]
    fn save_all_rejects_bad_speed_with_a_specific_message() {
        let root = unique_test_root("worker-save-all-speed");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut speech = speech_without_engine();

        let mut request =
            save_all_request(0, "https://api.xiaomimimo.com/v1", "mimo-v2.6-flash", "");
        request.local_speed = "9".to_string();
        let outcome = apply_save_all(&paths, &secret, &mut settings, &mut speech, &request);
        assert!(!outcome.config_written(), "语速非法不能落盘");
        assert!(
            outcome.message().contains("0.5~2.0"),
            "{:?}",
            outcome.message()
        );
        assert!(!paths.settings_file().is_file(), "语速非法时不能落盘");

        let _ = std::fs::remove_dir_all(&root);
    }

    fn subtitle(text: &str, speech_id: SpeechId) -> SpeechEvent {
        SpeechEvent::Subtitle {
            speech_id,
            text: text.to_string(),
        }
    }

    fn state_event(speech_id: SpeechId, state: PlaybackState) -> SpeechEvent {
        SpeechEvent::State {
            speech_id,
            turn: 1,
            state,
        }
    }

    /// **P1 回归**：旧播报的迟到事件不能覆盖新播报的界面状态。
    ///
    /// A 停止后立刻启动 B：A 迟到的 `Stopped` / `Finished` / `Failed` /
    /// 字幕清空都必须被丢掉。
    #[test]
    fn late_events_from_previous_speech_do_not_reach_the_ui() {
        let mut view = SpeechView::default();
        // B 先接管界面。
        assert!(view.accepts(2));
        assert_eq!(view.current(), Some(2));
        // A（id=1）的迟到终态与字幕清空一律不接受。
        assert!(!view.accepts(1), "旧播报的状态不能改界面");
        assert!(!view.accepts(1), "旧播报的字幕不能改界面");
        assert!(!view.accepts(1), "旧播报的提示不能改界面");
        assert_eq!(view.current(), Some(2), "旧事件不能把当前身份改回去");
        // 同一段播报自己的事件照常接受。
        assert!(view.accepts(2));
        // 下一段播报（id=3）自动接管。
        assert!(view.accepts(3));
        assert_eq!(view.current(), Some(3));
        assert!(!view.accepts(2), "已经结束的那一段不能再改界面");
    }

    /// 播报事件路径按 `speech_id` 归属分发：A 迟到的终态不会发出界面更新。
    ///
    /// 这里用一个"只记录接受与否"的替身验证调用顺序，而不是依赖真实窗口：
    /// `apply_speech_event` 的所有副作用都在 `invoke_from_event_loop` 里，
    /// 没有窗口时会被静默丢弃——所以断言收敛在 `SpeechView` 上。
    #[test]
    fn speech_event_ownership_is_checked_before_any_ui_update() {
        let mut view = SpeechView::default();
        // 模拟"B 已经接管"之后再收到 A 的一串事件。
        let _ = view.accepts(7);
        for event in [
            state_event(6, PlaybackState::Stopped),
            state_event(6, PlaybackState::Finished),
            state_event(6, PlaybackState::Failed("旧失败".to_string())),
            subtitle("", 6),
            subtitle("旧的一句", 6),
        ] {
            let speech_id = match &event {
                SpeechEvent::State { speech_id, .. }
                | SpeechEvent::Subtitle { speech_id, .. }
                | SpeechEvent::Notice { speech_id, .. } => *speech_id,
            };
            assert!(
                !view.accepts(speech_id),
                "旧播报的事件必须被拒绝：{event:?}"
            );
        }
        assert_eq!(view.current(), Some(7));
    }

    /// 聆听模式：播报期间是打断，结束后必须回到唤醒词；挂起确认优先。
    ///
    /// 这条依赖 `SpeechRuntime::is_speaking()` 如实反映真实运行状态——
    /// 只改 `sync_listen_mode` 而不收敛 `is_speaking()` 是修不好的。
    #[tokio::test]
    async fn listen_mode_follows_speech_and_confirmation_state() {
        let mut voice = VoiceRuntime::new();
        let confirmer = confirmer_without_ui();
        let speech = speech_without_engine();

        // 没有播报、没有确认：必须要求唤醒词。
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Wake);

        // 有挂起的确认：听「允许 / 拒绝」，并带上请求 id。
        let _wait = install_pending(&confirmer, 42);
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Confirm);
        assert_eq!(voice.mode.get().1, 42);

        // 确认结束后回到唤醒词。
        confirmer.abort_pending();
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Wake);
    }

    /// `SpeechView` 的初始状态：第一条事件无条件接管。
    #[test]
    fn first_speech_event_takes_over_the_view() {
        let mut view = SpeechView::default();
        assert_eq!(view.current(), None);
        assert!(view.accepts(1));
        assert_eq!(view.current(), Some(1));
    }

    /// **并发约束**：回合还在跑（含等待点击确认）时，语音/文本都**不能**启动第二回合。
    ///
    /// 这条是任务书的硬要求：不可启动第二回合，也不可把语音排队延后执行。
    #[test]
    fn busy_turn_rejects_a_second_turn() {
        let root = unique_test_root("worker-busy");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let settings = Settings::default();
        let toolkit: Option<Arc<CuaToolkit>> = None;
        let confirmer = Arc::new(confirmer_without_ui());
        let mut history = Vec::new();

        // 模拟「上一轮还在跑」：令牌槽位非空。
        let mut running = Some(CancelToken::new());
        match start_turn(
            &slint::Weak::default(),
            &paths,
            &secret,
            &settings,
            &toolkit,
            &confirmer,
            &mut history,
            &mut running,
            "打开浏览器".to_string(),
            1,
        ) {
            TurnStart::Rejected(reason) => {
                assert!(reason.contains("上一轮"), "原因必须说清楚：{reason}");
            }
            TurnStart::Started(_) => panic!("忙的时候绝不能启动第二回合"),
        }
        // 原来的令牌不能被语音/Cua 的拒绝路径换掉。
        assert!(running.is_some(), "拒绝新回合不能清掉正在跑的那一轮");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 没有 CUA 后端时也不能启动回合，但必须给出可读原因（而不是静默失败）。
    #[test]
    fn missing_toolkit_rejects_with_reason() {
        let root = unique_test_root("worker-no-toolkit");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let settings = Settings::default();
        let toolkit: Option<Arc<CuaToolkit>> = None;
        let confirmer = Arc::new(confirmer_without_ui());
        let mut history = Vec::new();
        let mut idle = None;
        match start_turn(
            &slint::Weak::default(),
            &paths,
            &secret,
            &settings,
            &toolkit,
            &confirmer,
            &mut history,
            &mut idle,
            "列出窗口".to_string(),
            1,
        ) {
            TurnStart::Rejected(reason) => assert!(reason.contains("Computer Use")),
            TurnStart::Started(_) => panic!("没有工具后端时不该启动"),
        }
        assert!(idle.is_none(), "失败时不能留下取消令牌");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn provider_index_mapping_is_stable() {
        assert_eq!(provider_id_for_index(0), "mimo");
        assert_eq!(provider_id_for_index(1), "deepseek");
        assert_eq!(provider_id_for_index(2), "openai-compatible");
        // 越界不能 panic，也不能悄悄落到某个官方提供商上
        assert_eq!(provider_id_for_index(99), "openai-compatible");
        assert_eq!(provider_id_for_index(-1), "openai-compatible");
    }

    #[tokio::test]
    async fn matching_request_id_settles_the_confirmation() {
        let confirmer = confirmer_without_ui();
        let mut wait = install_pending(&confirmer, 7);
        assert!(confirmer.submit(7, ConfirmAnswer::allow_click()));
        assert!(wait.try_recv().unwrap().allow);
        // 结算后槽位清空
        assert!(confirmer.pending.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_request_id_is_rejected() {
        let confirmer = confirmer_without_ui();
        let mut wait = install_pending(&confirmer, 7);
        // 上一次确认（id=6）的迟到点击必须被丢弃
        assert!(!confirmer.submit(6, ConfirmAnswer::allow_click()));
        assert!(wait.try_recv().is_err(), "不该被旧 id 的答复结算");
        assert!(
            confirmer.pending.lock().unwrap().is_some(),
            "挂起的确认仍然有效"
        );
    }

    #[tokio::test]
    async fn repeated_clicks_only_settle_once() {
        let confirmer = confirmer_without_ui();
        let mut wait = install_pending(&confirmer, 3);
        assert!(confirmer.submit(3, ConfirmAnswer::allow_click()));
        // 第二次点击：槽位已空，必须被丢弃
        assert!(!confirmer.submit(3, ConfirmAnswer::allow_click()));
        assert!(wait.try_recv().unwrap().allow);
        assert!(confirmer.submit(3, ConfirmAnswer::allow_click()).eq(&false));
    }

    #[tokio::test]
    async fn click_without_pending_is_ignored() {
        let confirmer = confirmer_without_ui();
        // 没有挂起确认时，任何点击（包括 id=0 的默认值）都不生效
        assert!(!confirmer.submit(0, ConfirmAnswer::allow_click()));
        assert!(!confirmer.submit(1, ConfirmAnswer::allow_click()));
    }

    #[tokio::test]
    async fn abort_pending_denies_and_clears() {
        let confirmer = confirmer_without_ui();
        let mut wait = install_pending(&confirmer, 11);
        confirmer.abort_pending();
        let answer = wait.try_recv().unwrap();
        assert!(!answer.allow, "退出时挂起的确认必须按拒绝结算");
        assert!(confirmer.pending.lock().unwrap().is_none());
    }

    /// 两次确认之间：第二次的答复不会落到第一次的等待上。
    #[tokio::test]
    async fn answers_never_leak_between_consecutive_confirmations() {
        let confirmer = confirmer_without_ui();

        let mut first = install_pending(&confirmer, 1);
        assert!(confirmer.submit(1, ConfirmAnswer::allow_click()));
        assert!(first.try_recv().unwrap().allow);

        let mut second = install_pending(&confirmer, 2);
        // 第一次那个 id 再点一次，不能结算第二次
        assert!(!confirmer.submit(1, ConfirmAnswer::allow_click()));
        assert!(second.try_recv().is_err());
        // 真正属于第二次的答复才行
        assert!(confirmer.submit(2, ConfirmAnswer::deny_click()));
        assert!(!second.try_recv().unwrap().allow);
    }

    /// 确认器拿到的取消令牌被置位时，等待应当立刻结束。
    #[tokio::test]
    async fn cancel_token_ends_the_wait() {
        let confirmer = Arc::new(confirmer_without_ui());
        let cancel = CancelToken::new();
        let request = ConfirmRequest {
            tool: "read_clipboard".to_string(),
            capability: "clipboard.read",
            label: "读剪贴板",
            reason: String::new(),
            args_summary: serde_json::json!({}),
        };
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            token.cancel();
        });
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            confirmer.confirm(request, &cancel),
        )
        .await
        .expect("取消必须让等待立刻结束");
        assert!(!answer.allow, "取消按拒绝处理");
        assert!(confirmer.pending.lock().unwrap().is_none(), "槽位必须清空");
    }

    /// 确认超时：不点按钮也不会一直挂着。
    #[tokio::test]
    async fn confirmation_times_out() {
        let mut confirmer = confirmer_without_ui();
        confirmer.timeout = std::time::Duration::from_millis(50);
        let confirmer = Arc::new(confirmer);
        let cancel = CancelToken::new();
        let request = ConfirmRequest {
            tool: "read_clipboard".to_string(),
            capability: "clipboard.read",
            label: "读剪贴板",
            reason: String::new(),
            args_summary: serde_json::json!({}),
        };
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            confirmer.confirm(request, &cancel),
        )
        .await
        .expect("超时必须让等待结束");
        assert!(!answer.allow);
        assert!(confirmer.pending.lock().unwrap().is_none());
    }

    /// 保存成功：内存配置与磁盘都更新，并按目标提供商写入 Key。
    #[cfg(windows)]
    #[test]
    fn save_settings_updates_memory_and_disk() {
        let root = unique_test_root("worker-save-ok");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();

        let ok = handle_save_settings(
            &slint::Weak::default(),
            &paths,
            &secret,
            &mut settings,
            SaveRequest {
                provider_index: 1, // deepseek
                base_url: "https://api.deepseek.com/v1".to_string(),
                model: "deepseek-v4-pro".to_string(),
                api_key: "sk-deepseek-test".to_string(),
                permission_index: 1, // conservative
            },
            &mut speech_without_engine(),
        );

        assert!(ok, "合法设置应当保存成功");
        assert_eq!(settings.provider_id, "deepseek");
        assert_eq!(settings.model, "deepseek-v4-pro");
        assert_eq!(settings.permission_mode, PermissionMode::Conservative);
        // Key 写到 deepseek 名下，不是别的提供商
        assert_eq!(
            secret.load("deepseek").unwrap().as_deref(),
            Some("sk-deepseek-test")
        );
        assert_eq!(secret.load("mimo").unwrap(), None);
        // 磁盘上也读得到新值（这就是"下次打开设置显示最新值"的依据）
        let reloaded = config::load_settings(&paths).unwrap();
        assert_eq!(reloaded.model, "deepseek-v4-pro");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 清除界面选中的提供商后：
    /// 设置页按**选中**的那家显示，主界面摘要按**已保存**的配置显示。
    #[cfg(windows)]
    #[test]
    fn clear_updates_selected_state_not_saved_state() {
        let root = unique_test_root("worker-clear-selected");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());

        // 已保存的配置是 mimo，两家都有 Key
        let saved = Settings {
            provider_id: "mimo".to_string(),
            base_url: "https://api.xiaomimimo.com/v1".to_string(),
            model: "mimo-v2.6-flash".to_string(),
            ..Settings::default()
        };
        secret.save("mimo", "sk-mimo").unwrap();
        secret.save("deepseek", "sk-deepseek").unwrap();

        // 用户在设置页切到 deepseek（未保存），点清除
        secret.clear("deepseek").unwrap();
        let view = view_after_clear(&secret, &saved, "deepseek");

        assert!(
            !view.selected_has_key,
            "设置页显示的是选中那家（deepseek），它刚被清掉，应当显示'未保存'"
        );
        assert!(
            view.endpoint_summary.contains("Key 已保存"),
            "主界面摘要反映已生效的 mimo 配置，它仍有 Key：{}",
            view.endpoint_summary
        );
        assert_eq!(
            secret.load("mimo").unwrap().as_deref(),
            Some("sk-mimo"),
            "已保存那家的 Key 不能被动到"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 清除的正是已保存的那家：设置页与摘要都显示"未保存"。
    #[cfg(windows)]
    #[test]
    fn clear_saved_provider_updates_both_views() {
        let root = unique_test_root("worker-clear-saved");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());

        let saved = Settings {
            provider_id: "mimo".to_string(),
            base_url: "https://api.xiaomimimo.com/v1".to_string(),
            model: "mimo-v2.6-flash".to_string(),
            ..Settings::default()
        };
        secret.save("mimo", "sk-mimo").unwrap();
        secret.clear("mimo").unwrap();

        let view = view_after_clear(&secret, &saved, "mimo");
        assert!(!view.selected_has_key);
        assert!(
            view.endpoint_summary.contains("未保存 Key"),
            "生效摘要也应当反映出来：{}",
            view.endpoint_summary
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 清除一家不影响另一家：选中的那家状态变，另一家 Key 不动。
    #[cfg(windows)]
    #[test]
    fn clearing_one_provider_spares_the_other_in_view() {
        let root = unique_test_root("worker-clear-spare");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let saved = Settings {
            provider_id: "mimo".to_string(),
            ..Settings::default()
        };
        secret.save("mimo", "sk-mimo").unwrap();
        secret.save("deepseek", "sk-deepseek").unwrap();

        secret.clear("deepseek").unwrap();
        assert!(!view_after_clear(&secret, &saved, "deepseek").selected_has_key);
        // 切回 mimo 看：它还在
        assert!(view_after_clear(&secret, &saved, "mimo").selected_has_key);
        assert_eq!(secret.load("mimo").unwrap().as_deref(), Some("sk-mimo"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 保存失败（非法地址）：内存配置不变、Key 不写、返回 false。
    #[test]
    fn save_settings_failure_changes_nothing() {
        let root = unique_test_root("worker-save-bad");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let before = settings.clone();

        let ok = handle_save_settings(
            &slint::Weak::default(),
            &paths,
            &secret,
            &mut settings,
            SaveRequest {
                provider_index: 0,
                base_url: "ftp://invalid".to_string(),
                model: "x".to_string(),
                api_key: "sk-should-not-be-written".to_string(),
                permission_index: 0,
            },
            &mut speech_without_engine(),
        );

        assert!(!ok, "非法设置必须返回失败，界面不得显示成功");
        assert_eq!(settings.provider_id, before.provider_id);
        assert_eq!(settings.model, before.model);
        assert!(secret.load("mimo").unwrap().is_none(), "失败时不该写入 Key");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 已有 Key A → 输入 Key B → 配置写盘失败 → Key A 必须仍然可用。
    ///
    /// 这是原先最严重的数据丢失路径：旧实现"先覆盖 Key、配署失败再 clear()"，
    /// 结果把用户原来的 Key A 也清掉了。现在先写配置，失败时 Key 压根没被碰过。
    #[cfg(windows)]
    #[test]
    fn config_failure_keeps_previous_key_in_save_flow() {
        let root = unique_test_root("worker-save-config-fail");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        secret.save("mimo", "sk-key-a").unwrap();

        // 注入故障：占住配置的临时文件路径，让 save_settings 写临时文件失败
        let tmp = paths.settings_file().with_extension("json.tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        let ok = handle_save_settings(
            &slint::Weak::default(),
            &paths,
            &secret,
            &mut settings,
            SaveRequest {
                provider_index: 0,
                base_url: "https://api.xiaomimimo.com/v1".to_string(),
                model: "mimo-v2.6-pro".to_string(),
                api_key: "sk-key-b".to_string(),
                permission_index: 0,
            },
            &mut speech_without_engine(),
        );

        assert!(!ok, "配置写失败必须返回 false");
        assert_eq!(
            settings.model, "mimo-v2.6-flash",
            "内存配置不能被改成未落盘的值"
        );

        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(
            secret.load("mimo").unwrap().as_deref(),
            Some("sk-key-a"),
            "Key B 没保存成功，Key A 也不能丢"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 留空 Key：不改动该提供商已有的 Key。
    #[cfg(windows)]
    #[test]
    fn save_settings_with_blank_key_keeps_existing() {
        let root = unique_test_root("worker-save-blank");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        secret.save("mimo", "sk-existing-mimo").unwrap();
        let mut settings = Settings::default();

        let ok = handle_save_settings(
            &slint::Weak::default(),
            &paths,
            &secret,
            &mut settings,
            SaveRequest {
                provider_index: 0,
                base_url: "https://api.xiaomimimo.com/v1".to_string(),
                model: "mimo-v2.6-pro".to_string(),
                api_key: String::new(),
                permission_index: 0,
            },
            &mut speech_without_engine(),
        );

        assert!(ok);
        assert_eq!(
            secret.load("mimo").unwrap().as_deref(),
            Some("sk-existing-mimo"),
            "留空表示不修改，不能把已有 Key 清掉"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 执行中收到退出：应当取消回合、让等待结束，并返回 Quit。
    ///
    /// 这里用一个"自己循环检查取消令牌"的假回合，模拟真实的 Agent 行为：
    /// 它不会自己结束，只有取消才能让它停下——正是执行中退出的场景。
    #[tokio::test]
    async fn quit_during_turn_cancels_and_returns_quit() {
        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let turn_cancel = CancelToken::new();
        let token_for_turn = turn_cancel.clone();
        let stopper = turn_cancel.clone();

        // 假回合：一直跑到被取消
        let turn = tokio::spawn(async move {
            let mut ticks = 0u64;
            loop {
                if token_for_turn.is_cancelled() {
                    return (
                        TurnOutcome {
                            cancelled: true,
                            failed: true,
                            ..TurnOutcome::default()
                        },
                        Vec::new(),
                        Settings::default(),
                    );
                }
                ticks += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                if ticks > 500 {
                    // 兜底：没被取消就超时失败，避免测试挂死
                    return (TurnOutcome::default(), Vec::new(), Settings::default());
                }
            }
        });

        // 模拟用户点了退出
        let _ = tx.send(UiCommand::Quit);
        let _ = stopper;

        let root = unique_test_root("worker-quit-turn");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut cancel_slot = Some(turn_cancel.clone());
        let confirmer = Arc::new(confirmer_without_ui());

        let mut voice = VoiceRuntime::new();
        let mut speech = speech_without_engine();
        let save_generation_for_test = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (command_tx, _command_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_turn(
                &mut commands,
                RunningTurn {
                    task: turn,
                    turn_id: 1,
                },
                &slint::Weak::default(),
                &mut cancel_slot,
                &paths,
                &secret,
                &mut settings,
                &confirmer,
                &mut voice,
                &mut speech,
                &mut SpeechView::default(),
                &command_tx,
                &save_generation_for_test,
                TurnSource::Text,
            ),
        )
        .await
        .expect("退出不能让等待无限挂起");

        assert!(matches!(result, TurnWait::Quit), "必须返回 Quit");
        assert!(turn_cancel.is_cancelled(), "回合必须被取消");
        assert!(cancel_slot.is_none(), "退出后令牌槽位要清空");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **执行中关麦**：`wait_turn` 里的开关命令必须真的停采集，
    /// 而不是只改一行提示文字；同时不能把正在跑的回合取消掉。
    #[tokio::test]
    async fn closing_voice_while_waiting_a_turn_really_stops_capture() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let watch = CancelToken::new();
        let token_for_turn = watch.clone();

        // 假回合：只有被取消才会结束——正是"关麦不该结束它"的场景。
        let turn = tokio::spawn(async move {
            while !token_for_turn.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            (
                TurnOutcome {
                    cancelled: true,
                    failed: true,
                    ..TurnOutcome::default()
                },
                Vec::new(),
                Settings::default(),
            )
        });

        let root = unique_test_root("worker-close-mic-in-turn");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut cancel_slot = Some(watch.clone());
        let confirmer = Arc::new(confirmer_without_ui());

        let mut voice = VoiceRuntime::new();
        let mut speech = speech_without_engine();
        let save_generation_for_test = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let instance = voice.route.open();
        let stop = Arc::new(AtomicBool::new(false));
        voice.handle = Some(VoiceHandle::for_test(stop.clone(), instance));

        // 模拟用户在执行中点了「关闭麦克风」，随后又点了「取消」。
        // 两个 future 在同一个任务上交替推进（不需要 'static）。
        let mut speech_view = SpeechView::default();
        let driver = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(UiCommand::SetVoiceEnabled(false));
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(stop.load(Ordering::SeqCst), "执行中关麦必须真的停止采集");
            assert!(!watch.is_cancelled(), "关麦不是撤销：回合不能被取消");
            let _ = tx.send(UiCommand::Cancel);
        };
        let weak = slint::Weak::default();
        let wait = wait_turn(
            &mut commands,
            RunningTurn {
                task: turn,
                turn_id: 1,
            },
            &weak,
            &mut cancel_slot,
            &paths,
            &secret,
            &mut settings,
            &confirmer,
            &mut voice,
            &mut speech,
            &mut speech_view,
            &tx,
            &save_generation_for_test,
            TurnSource::Text,
        );
        let (result, ()) = tokio::join!(wait, driver);

        assert!(matches!(result, TurnWait::Finished(_)));
        assert!(stop.load(Ordering::SeqCst), "关麦后采集必须保持停止");
        assert!(voice.handle.is_none(), "关麦后不能继续持有采集线程");
        assert!(voice.route.active.current().is_none(), "在途语音必须过期");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **偏好保存失败只影响持久化**：本次关麦必须照样生效。
    #[test]
    fn voice_preference_save_failure_still_closes_the_mic() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = unique_test_root("worker-voice-pref-fail");
        let paths = AppPaths::with_root(&root);
        let mut settings = Settings::default();
        settings.voice.enabled = true;
        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let stop = Arc::new(AtomicBool::new(false));
        voice.handle = Some(VoiceHandle::for_test(stop.clone(), instance));

        // 注入故障：占住配置写入用的临时文件路径，让 save_settings 失败。
        let tmp = paths.settings_file().with_extension("json.tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        apply_voice_enabled(
            &slint::Weak::default(),
            &paths,
            &mut settings,
            &mut voice,
            &tx,
            false,
        );

        assert!(stop.load(Ordering::SeqCst), "保存失败也必须真的关麦");
        assert!(voice.handle.is_none(), "句柄必须释放");
        assert!(
            settings.voice.enabled,
            "写盘失败时内存里的偏好不应被改成未落盘的值"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------ 语音路由

    /// 归属核对：旧实例（关麦前那个）的事件一律无效。
    #[test]
    fn voice_route_rejects_stale_instances() {
        let mut route = VoiceRoute::default();
        let old = route.open();
        let turn = VoiceTurn {
            instance: old,
            session: 1,
        };
        assert!(route.bind(old, turn));

        // 关麦后立刻重开：新实例号不同，旧实例的事件不再生效。
        route.close();
        let new = route.open();
        assert_ne!(old, new);
        assert!(!route.is_current(old));
        assert!(route.is_current(new));
        assert_eq!(route.take(old, turn), None, "旧实例的结果不得被取走");
        assert_eq!(
            route.take(
                new,
                VoiceTurn {
                    instance: new,
                    session: 1
                }
            ),
            None,
            "新实例没有这一句，也不能提交"
        );
    }

    #[test]
    fn voice_route_takes_each_turn_once() {
        let mut route = VoiceRoute::default();
        let instance = route.open();
        let turn = VoiceTurn {
            instance,
            session: 3,
        };
        assert!(route.bind(instance, turn));
        assert_eq!(route.take(instance, turn), Some(turn));
        assert_eq!(route.take(instance, turn), None, "同一句只能提交一次");

        // 拒绝只清归属匹配的那一句。
        let first = VoiceTurn {
            instance,
            session: 4,
        };
        let second = VoiceTurn {
            instance,
            session: 5,
        };
        assert!(route.bind(instance, first));
        route.release(instance, second);
        assert_eq!(route.pending, Some(first), "别的会话不能清掉当前这句");
        assert_eq!(route.cancel(), Some(first));
        assert_eq!(route.pending, None);
    }

    /// **旧实例收尾不能清掉新实例的活动会话**（快速关麦 / 重开的真实路径）。
    ///
    /// 修复前整个 `VoiceRuntime` 只有一份 `ActiveSession`，每个实例
    /// `spawn` 时都 clone 同一份：旧线程迟到的 `shutdown()` / `fail()` 里
    /// 的无条件 `clear()` 会把新实例正在识别的那一句抹掉 ——
    /// 于是新实例的 Final 被当成过期丢弃（表现为「所有 Final 都无效」）。
    ///
    /// 这里走真实的分配路径：实例号与会话都由 `VoiceRoute::open()` 给出，
    /// 再用两个真实的 `SessionMachine` 驱动识别与收尾。
    #[test]
    fn old_instance_teardown_cannot_clear_the_new_instance_session() {
        use std::sync::Mutex;

        use crate::voice::asr::{AsrError, InferenceCancel, Transcript};
        use crate::voice::coordinator::{AsrSubmitter, ReplySink, SessionMachine};

        /// 记录事件（带实例号，用来判断结果属于谁）。
        #[derive(Clone, Default)]
        struct Sink {
            events: Arc<Mutex<Vec<(VoiceInstance, VoiceEventKind)>>>,
        }

        impl Sink {
            fn shared(&self) -> SharedSink {
                let events = self.events.clone();
                SharedSink::new(move |event| {
                    events.lock().unwrap().push((event.instance, event.kind))
                })
            }

            fn finals(&self) -> Vec<(VoiceInstance, String)> {
                self.events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|(instance, kind)| match kind {
                        VoiceEventKind::Final { text, .. } => Some((*instance, text.clone())),
                        _ => None,
                    })
                    .collect()
            }
        }

        /// 手动控制结果回调的假 ASR：由测试决定「推理什么时候结束」。
        struct DeferredAsr(Arc<Mutex<Vec<(VoiceTurn, ReplySink)>>>);

        impl AsrSubmitter for DeferredAsr {
            fn submit(
                &self,
                turn: VoiceTurn,
                _samples: Vec<f32>,
                _cancel: InferenceCancel,
                reply: ReplySink,
            ) -> Result<(), AsrError> {
                self.0.lock().unwrap().push((turn, reply));
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

        let sink = Sink::default();
        let mut route = VoiceRoute::default();

        // ---- 实例 A：唤醒后进入识别（推理还没回来）----
        let a = route.open();
        let active_a = route.active_session();
        let pending_a = Arc::new(Mutex::new(Vec::new()));
        let mut machine_a = SessionMachine::new(
            a,
            sink.shared(),
            active_a.clone(),
            Box::new(DeferredAsr(pending_a.clone())),
            0,
        );
        machine_a.start_listening();
        machine_a.begin("小爱同学".to_string());
        machine_a.finish(1.0, vec![0.1, -0.1, 0.2, -0.2]);
        assert_eq!(machine_a.state(), &VoiceState::Transcribing);
        assert!(active_a.current().is_some(), "A 应该有在途会话");

        // ---- 用户关麦并立刻重开：B 从这里拿到**自己**的会话 ----
        route.close();
        let b = route.open();
        assert_ne!(a, b, "重开必须是新实例");
        let active_b = route.active_session();
        let pending_b = Arc::new(Mutex::new(Vec::new()));
        let mut machine_b = SessionMachine::new(
            b,
            sink.shared(),
            active_b.clone(),
            Box::new(DeferredAsr(pending_b.clone())),
            0,
        );
        machine_b.start_listening();
        machine_b.begin("小爱同学".to_string());
        machine_b.finish(1.0, vec![0.1, -0.1, 0.2, -0.2]);
        let turn_b = active_b.current().expect("B 应该有在途会话");
        assert!(route.bind(b, turn_b));

        // ---- A 的线程这时才收尾：shutdown / fail 都会无条件 clear ----
        machine_a.shutdown();
        machine_a.fail("麦克风错误：设备断开".to_string());
        assert!(active_a.current().is_none(), "A 自己的会话确实被清了");
        assert!(
            active_b.is_current(turn_b),
            "旧实例的收尾不能清掉新实例的活动会话"
        );

        // ---- B 的识别正常结束：Final 恰好一次，且属于 B ----
        let (turn, reply_b) = pending_b.lock().unwrap().pop().expect("B 应有一次识别");
        reply_b(transcript("打开浏览器", turn));
        assert!(machine_b.poll(), "B 应该回到监听");
        assert!(route.take(b, turn).is_some(), "B 的结果必须能提交");
        assert!(route.take(b, turn).is_none(), "同一句只能提交一次");

        // ---- A 的识别这时才回来：不能交付 ----
        let (late, reply_a) = pending_a.lock().unwrap().pop().expect("A 应有一次识别");
        reply_a(transcript("上一句", late));
        assert!(route.take(a, late).is_none(), "旧实例的结果不能提交");

        assert_eq!(
            sink.finals(),
            vec![(b, "打开浏览器".to_string())],
            "只有新实例的这句被交付，且恰好一次"
        );
    }

    /// 旧实例的迟到事件：不改状态、不动新会话、不触发 Agent。
    #[test]
    fn stale_events_from_previous_instance_are_ignored() {
        let weak = slint::Weak::default();
        let mut voice = VoiceRuntime::new();
        let old = voice.route.open();
        let old_turn = VoiceTurn {
            instance: old,
            session: 1,
        };

        // 关麦 → 立刻重开
        voice.route.close();
        let new = voice.route.open();
        let new_turn = VoiceTurn {
            instance: new,
            session: 1,
        };
        assert!(voice.route.bind(new, new_turn));

        let stale_events = [
            VoiceEventKind::Ready(Box::new(test_capture_info(AsrBackend::Cpu, None))),
            VoiceEventKind::Woke {
                turn: old_turn,
                keyword: "小爱同学".to_string(),
            },
            VoiceEventKind::StateChanged(VoiceState::Listening),
            VoiceEventKind::StateChanged(VoiceState::Error("旧的设备错误".to_string())),
            VoiceEventKind::Final {
                turn: old_turn,
                text: "打开浏览器".to_string(),
                audio_seconds: 1.0,
                decode_ms: 1,
                purpose: ListenPurpose::Wake,
                request_id: 0,
            },
            VoiceEventKind::Rejected {
                turn: old_turn,
                reason: RejectReason::EmptyTranscript,
            },
        ];
        for kind in stale_events {
            let prompt = voice.on_event(
                &weak,
                VoiceEvent {
                    instance: old,
                    kind,
                },
                false,
            );
            assert!(prompt.into_prompt().is_none(), "旧实例的事件不得触发 Agent");
        }
        // 新实例的这一句仍在等结果，不能被旧事件清掉。
        assert_eq!(voice.route.pending, Some(new_turn));
    }

    /// Agent 执行中关麦：采集真的停了，但正在跑的回合不被撤销。
    #[test]
    fn closing_voice_during_turn_stops_capture_without_cancelling_turn() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let weak = slint::Weak::default();
        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let stop = Arc::new(AtomicBool::new(false));
        voice.handle = Some(VoiceHandle::for_test(stop.clone(), instance));
        voice.route.active.begin(VoiceTurn {
            instance,
            session: 1,
        });
        let turn_cancel = CancelToken::new();

        voice.disable(&weak);

        assert!(stop.load(Ordering::SeqCst), "关麦必须真的停止采集");
        assert!(voice.handle.is_none(), "句柄要释放，避免重复持有设备");
        assert!(voice.route.active.current().is_none(), "在途识别必须过期");
        assert!(
            !turn_cancel.is_cancelled(),
            "关麦不是撤销：正在执行的回合继续"
        );
    }

    /// Agent 忙（含等待点击确认）时，有效 Final 不启动第二回合，也不排队。
    #[test]
    fn final_during_busy_turn_is_dropped_not_queued() {
        let weak = slint::Weak::default();
        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let turn = VoiceTurn {
            instance,
            session: 1,
        };
        assert!(voice.route.bind(instance, turn));

        let final_event = VoiceEvent {
            instance,
            kind: VoiceEventKind::Final {
                turn,
                text: "打开浏览器".to_string(),
                audio_seconds: 1.0,
                decode_ms: 5,
                purpose: ListenPurpose::Wake,
                request_id: 0,
            },
        };
        assert!(
            voice
                .on_event(&weak, final_event.clone(), true)
                .into_prompt()
                .is_none(),
            "Agent 忙时不能启动第二回合"
        );
        assert!(voice.route.pending.is_none(), "这句话不能排队延后执行");

        // 合法的状态事件仍然照常处理：不会被当成“另一条指令”，
        // 也不会在忙的时候触发任何回合。
        assert!(voice
            .on_event(
                &weak,
                VoiceEvent {
                    instance,
                    kind: VoiceEventKind::StateChanged(VoiceState::Listening),
                },
                true,
            )
            .into_prompt()
            .is_none());
        assert!(voice
            .on_event(
                &weak,
                VoiceEvent {
                    instance,
                    kind: VoiceEventKind::Overflow,
                },
                true,
            )
            .into_prompt()
            .is_none());
    }

    /// 正常路径：有效 Final 恰好提交一次；重复事件不会提交第二次。
    #[test]
    fn valid_final_is_submitted_exactly_once() {
        let weak = slint::Weak::default();
        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let turn = VoiceTurn {
            instance,
            session: 2,
        };
        assert!(voice.route.bind(instance, turn));
        let event = VoiceEvent {
            instance,
            kind: VoiceEventKind::Final {
                turn,
                text: "打开记事本".to_string(),
                audio_seconds: 1.2,
                decode_ms: 9,
                purpose: ListenPurpose::Wake,
                request_id: 0,
            },
        };

        let prompt = voice
            .on_event(&weak, event.clone(), false)
            .into_prompt()
            .expect("空闲时有效 Final 应当提交");
        assert_eq!(prompt.text, "打开记事本");
        assert!((prompt.audio_seconds - 1.2).abs() < f32::EPSILON);

        assert!(
            voice.on_event(&weak, event, false).into_prompt().is_none(),
            "同一结果不得提交第二次"
        );
    }

    /// 取消当前输入后，迟到的 Final 不得提交。
    #[test]
    fn late_final_after_cancel_is_dropped() {
        let weak = slint::Weak::default();
        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let turn = VoiceTurn {
            instance,
            session: 1,
        };
        assert!(voice.route.bind(instance, turn));
        voice.cancel_pending();
        assert!(voice.route.active.current().is_none());

        let prompt = voice.on_event(
            &weak,
            VoiceEvent {
                instance,
                kind: VoiceEventKind::Final {
                    turn,
                    text: "打开浏览器".to_string(),
                    audio_seconds: 1.0,
                    decode_ms: 5,
                    purpose: ListenPurpose::Wake,
                    request_id: 0,
                },
            },
            false,
        );
        assert!(prompt.into_prompt().is_none(), "取消之后的结果不得提交");
    }

    /// 旧实例的 Final 不得冒充新实例的会话（两者 session 号可能相同）。
    #[test]
    fn old_instance_final_cannot_impersonate_new_session() {
        let weak = slint::Weak::default();
        let mut voice = VoiceRuntime::new();
        let old = voice.route.open();
        let old_turn = VoiceTurn {
            instance: old,
            session: 1,
        };
        voice.route.close();
        let new = voice.route.open();
        let new_turn = VoiceTurn {
            instance: new,
            session: 1,
        };
        assert!(voice.route.bind(new, new_turn));

        assert!(voice
            .on_event(
                &weak,
                VoiceEvent {
                    instance: old,
                    kind: VoiceEventKind::Final {
                        turn: old_turn,
                        text: "上一句".to_string(),
                        audio_seconds: 1.0,
                        decode_ms: 5,
                        purpose: ListenPurpose::Wake,
                        request_id: 0,
                    },
                },
                false,
            )
            .into_prompt()
            .is_none());
        assert_eq!(voice.route.pending, Some(new_turn));
    }

    /// 界面文字只反映**实际**后端。
    #[test]
    fn backend_label_reflects_actual_backend() {
        assert_eq!(
            describe_backend(&test_capture_info(AsrBackend::Cuda, None)),
            "Whisper CUDA"
        );
        assert_eq!(
            describe_backend(&test_capture_info(AsrBackend::Cpu, None)),
            "Whisper CPU"
        );
        let degraded = test_capture_info(
            AsrBackend::Cpu,
            Some("CUDA：初始化失败（缺少 CUDA 运行库）".to_string()),
        );
        assert_eq!(
            describe_backend(&degraded),
            "Whisper CPU（CUDA：初始化失败（缺少 CUDA 运行库））"
        );
        let cpu_build = test_capture_info(AsrBackend::Cpu, Some("本构建未启用 CUDA".to_string()));
        assert_eq!(
            describe_backend(&cpu_build),
            "Whisper CPU（本构建未启用 CUDA）"
        );
    }

    fn test_capture_info(backend: AsrBackend, fallback_reason: Option<String>) -> CaptureInfoForUi {
        CaptureInfoForUi {
            device_name: "测试麦克风".to_string(),
            sample_rate: 48_000,
            resampled: true,
            wake_labels: vec!["小爱同学".to_string()],
            keywords_file: std::path::PathBuf::from("kws-keywords.txt"),
            whisper_model: std::path::PathBuf::from("ggml-small.bin"),
            backend,
            fallback_reason,
        }
    }

    fn unique_test_root(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("{prefix}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn permission_index_mapping_is_stable() {
        assert_eq!(permission_mode_for_index(0), PermissionMode::Standard);
        assert_eq!(permission_mode_for_index(1), PermissionMode::Conservative);
        assert_eq!(permission_mode_for_index(2), PermissionMode::Permissive);
        assert_eq!(permission_mode_for_index(9), PermissionMode::Standard);
    }

    #[test]
    fn endpoint_summary_never_contains_key() {
        let settings = Settings::default();
        let summary = settings_summary(&settings, true);
        assert!(!summary.contains("sk-"));
        assert!(summary.contains("Key 已保存"));
    }

    #[test]
    fn arg_summary_is_secret_free() {
        let value = serde_json::json!({ "app": "notepad", "x": 10 });
        assert!(summary_is_secret_free(&value));
    }
}
