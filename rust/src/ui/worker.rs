//! 后台工作线程：持有 Agent、Computer Use 工具与配置，驱动一条回合。
//!
//! 这里只做"把界面动作翻译成 Agent 调用、再把结果翻译成界面状态"，
//! 具体的协议、权限、预算逻辑都在各自模块里，便于单独测试。

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
    coordinator::{self, ActiveSession, CoordinatorConfig, SharedSink, VoiceHandle},
    settings::VoicePaths,
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
    /// 取消当前回合。
    Cancel,
    /// 保存设置（含可选的新 Key）。
    SaveSettings {
        provider_index: i32,
        base_url: String,
        model: String,
        api_key: String,
        permission_index: i32,
    },
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
        let tx = command_tx.clone();
        window.on_save_settings(
            move |provider_index, base_url, model, api_key, permission_index| {
                let _ = tx.send(UiCommand::SaveSettings {
                    provider_index,
                    base_url: base_url.to_string(),
                    model: model.to_string(),
                    api_key: api_key.to_string(),
                    permission_index,
                });
            },
        );
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
                weak, paths, secret, settings, command_rx, command_tx, confirmer,
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
    /// 用户多久不点就自动拒绝——避免"确认框一直开着，回合永远不结束"。
    timeout: std::time::Duration,
}

impl UiConfirmer {
    fn new(weak: slint::Weak<AppWindow>) -> Self {
        Self {
            weak,
            pending: Arc::new(std::sync::Mutex::new(None)),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            timeout: std::time::Duration::from_secs(120),
        }
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
        pending.reply.send(answer).is_ok()
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
async fn run_worker(
    weak: slint::Weak<AppWindow>,
    paths: AppPaths,
    secret: SecretStore,
    mut settings: Settings,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    command_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    confirmer: Arc<UiConfirmer>,
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

    // 语音线程发事件回工作线程：复用同一条命令通道，主循环天然串行处理。
    // 实例号/会话号的归属核对也在 `VoiceRuntime` 里，旧实例的迟到事件不会生效。
    let mut voice = VoiceRuntime::new();

    /*
     * 已保存的启用偏好在启动时**不自动开麦**：
     * 任务书要求「新安装启动时不意外开麦」，主人不在电脑前时更不该开。
     * 偏好只在用户点了开关之后才写回。
     */
    seed_voice_ui(&weak, &settings);

    loop {
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
                voice.disable(&weak);
                break;
            }
            UiCommand::Cancel => {
                if let Some(token) = turn_cancel.take() {
                    token.cancel();
                    set_status(&weak, "正在中断…");
                }
                // 取消也作用于「正在录/正在识别的那一句」：之后返回的结果不提交。
                // 已经开始的 Agent 回合仍按原有取消语义处理。
                voice.cancel_pending();
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
            UiCommand::SaveSettings {
                provider_index,
                base_url,
                model,
                api_key,
                permission_index,
            } => {
                if handle_save_settings(
                    &weak,
                    &paths,
                    &secret,
                    &mut settings,
                    SaveRequest {
                        provider_index,
                        base_url,
                        model,
                        api_key,
                        permission_index,
                    },
                ) {
                    // 切换端点后要重新核实 Key 状态（不同提供商可能有不同的 Key）。
                    let has_key = secret.has(&settings.provider_id);
                    publish_endpoint(&weak, &settings, has_key, "设置已保存");
                }
            }
            UiCommand::Send(input) => {
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
                            &command_tx,
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
                let prompt = voice.on_event(&weak, event, turn_cancel.is_some());
                if let Some(prompt) = prompt {
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
                            &command_tx,
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

    // 退出前：关麦克风、取消回合、清掉确认、释放子进程。
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
}

impl VoiceRoute {
    /// 打开麦克风：分配新的实例号。旧实例的迟到事件从这里开始不再有效。
    fn open(&mut self) -> VoiceInstance {
        self.next_instance += 1;
        self.instance = Some(self.next_instance);
        self.pending = None;
        self.next_instance
    }

    /// 关闭麦克风：当前实例作废（旧线程可以继续收尾，但不再代表“当前”）。
    fn close(&mut self) {
        self.instance = None;
        self.pending = None;
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

/// 语音运行时：线程句柄 + 实例路由 + 共享的「当前会话」。
struct VoiceRuntime {
    handle: Option<VoiceHandle>,
    route: VoiceRoute,
    active: ActiveSession,
}

impl VoiceRuntime {
    fn new() -> Self {
        Self {
            handle: None,
            route: VoiceRoute::default(),
            active: ActiveSession::new(),
        }
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
        self.active.clear();

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
        match coordinator::spawn(config, sink, self.active.clone(), stop, instance) {
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
        self.active.clear();
        if let Some(mut handle) = self.handle.take() {
            handle.stop();
        }
        set_voice_state(weak, &VoiceState::Off);
    }

    /// 取消当前这一句（不关麦）：之后返回的识别结果不再提交。
    fn cancel_pending(&mut self) {
        self.route.cancel();
        self.active.clear();
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
    ) -> Option<VoicePrompt> {
        // 旧实例的迟到事件：不改界面状态、不动新会话、不触发任何工具。
        if !self.route.is_current(event.instance) {
            return None;
        }
        match event.kind {
            VoiceEventKind::StateChanged(state) => {
                set_voice_state(weak, &state);
                set_status(weak, &state.label());
                if !state.is_active() {
                    // 线程已结束或即将结束：释放句柄，但保留界面上的原因。
                    self.route.close();
                    self.active.clear();
                    if let Some(mut handle) = self.handle.take() {
                        handle.stop();
                    }
                }
                None
            }
            VoiceEventKind::Ready(info) => {
                apply_capture_info(weak, &info);
                None
            }
            VoiceEventKind::Woke { turn, keyword } => {
                self.route.bind(event.instance, turn);
                set_voice_state(weak, &VoiceState::Capturing);
                set_status(weak, &format!("已唤醒（{keyword}），请说指令"));
                None
            }
            VoiceEventKind::Level { level_db, floor_db } => {
                apply_level(weak, level_db, floor_db);
                None
            }
            VoiceEventKind::Rejected { turn, reason } => {
                self.route.release(event.instance, turn);
                // 空转录/太短属于「没听清」，给一句可读的提示；
                // 不需要弹窗，也不能触发工具。
                if matches!(
                    reason,
                    RejectReason::EmptyTranscript | RejectReason::TooShort
                ) {
                    set_status(weak, "没听清，再说一次？");
                } else {
                    set_status(weak, &reason.message());
                }
                None
            }
            VoiceEventKind::Overflow => {
                set_status(weak, "音频积压，已丢弃这一句");
                None
            }
            VoiceEventKind::Final {
                turn,
                text,
                audio_seconds,
                decode_ms,
            } => {
                // 归属核对 + 一次性消费：过期/重复的结果绝不提交。
                if self.route.take(event.instance, turn).is_none() {
                    set_status(weak, "这次语音已取消");
                    return None;
                }
                set_transcript(weak, &text);
                if agent_busy {
                    set_status(weak, "上一轮还在跑；这句话没有执行");
                    return None;
                }
                Some(VoicePrompt {
                    text,
                    audio_seconds,
                    decode_ms,
                })
            }
        }
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
/// 返回 `Some(handle)` 表示回合已启动，调用方负责等待；`None` 表示没有启动
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
) -> Option<tokio::task::JoinHandle<(TurnOutcome, Vec<ChatMessage>, Settings)>> {
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

/// 一次回合的启动结果。
enum TurnStart {
    Started(tokio::task::JoinHandle<(TurnOutcome, Vec<ChatMessage>, Settings)>),
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
    TurnStart::Started(turn)
}

/// 等待回合结束期间的命令处理结果。
enum TurnWait {
    /// 回合正常结束（带回会话历史）。
    Finished(Vec<ChatMessage>),
    /// 用户要求退出：调用方应当结束主循环。
    Quit,
}

/// 一次"保存设置"请求（来自界面）。
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
/// 关键规则：
/// - 校验失败 / 写盘失败**都不更新**内存里的 `settings`，界面也拿不到"已保存"；
/// - Key 只写到**目标提供商**名下；留空表示"不动该提供商的 Key"；
/// - **先写配置、再写 Key**。反过来不行：先覆盖 Key 再回滚，
///   会在回滚时把用户原有的 Key A 一起删掉（原来 Bug 就是这样丢数据的）。
///   现在配置写失败时，Key 根本还没被动过，Key A 必然还在。
/// - 任何错误都会如实回报界面，不假装成功。
fn handle_save_settings(
    weak: &slint::Weak<AppWindow>,
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    request: SaveRequest,
) -> bool {
    let mut next = settings.clone();
    next.provider_id = provider_id_for_index(request.provider_index).to_string();
    next.base_url = request.base_url;
    next.model = request.model;
    next.permission_mode = permission_mode_for_index(request.permission_index);

    let next = match next.normalized() {
        Ok(next) => next,
        Err(error) => {
            set_status(weak, &format!("设置无效：{error}"));
            return false;
        }
    };

    // 1) 先落盘配置。失败时 Key 还没动过，用户原来的 Key 完好无损。
    if let Err(error) = config::save_settings(paths, &next) {
        set_status(weak, &format!("保存设置失败：{error}"));
        return false;
    }

    // 2) 配置已生效，再写 Key（只有用户真的填了才写，且只写到目标提供商名下）。
    let key_provider = next.provider_id.clone();
    let new_key = request.api_key.trim().to_string();
    if !new_key.is_empty() {
        if let Err(error) = secret.save(&key_provider, &new_key) {
            /*
             * 配置已经写成功了，但 Key 没写进去。
             *
             * 这里**不回滚配置**：配置本身是合法的、用户也确实想改，
             * 回滚反而会让"地址/模型改了、Key 没改"这个更常见的意图落空。
             * 如实告诉用户"设置已保存，但 Key 没保存"，并让界面保持 Key 未保存的状态。
             */
            *settings = next;
            set_status(weak, &format!("设置已保存，但 Key 保存失败：{error}"));
            return false;
        }
    }

    *settings = next;
    true
}

/// 把设置（与 Key 状态）推给界面。
fn publish_endpoint(
    weak: &slint::Weak<AppWindow>,
    settings: &Settings,
    has_key: bool,
    status: &str,
) {
    let endpoint = config::describe_endpoint(settings, has_key);
    let status = status.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_settings_open(false);
            window.set_api_key_input("".into());
            window.set_has_key(has_key);
            window.set_endpoint_text(endpoint.into());
            window.set_status(status.into());
        }
    });
}

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
    mut turn: tokio::task::JoinHandle<(TurnOutcome, Vec<ChatMessage>, Settings)>,
    weak: &slint::Weak<AppWindow>,
    turn_cancel: &mut Option<CancelToken>,
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    confirmer: &UiConfirmer,
    voice: &mut VoiceRuntime,
    command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
) -> TurnWait {
    loop {
        tokio::select! {
            joined = &mut turn => {
                *turn_cancel = None;
                match joined {
                    Ok((outcome, history, settings_snapshot)) => {
                        finish_turn(weak, &outcome, &settings_snapshot);
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
            command = commands.recv() => {
                match command {
                    Some(UiCommand::Cancel) => {
                        if let Some(token) = turn_cancel.as_ref() {
                            token.cancel();
                            set_status(weak, "正在中断…");
                        }
                        // 语音输入也一并取消（不再提交），但不影响已开始回合的收尾。
                        voice.cancel_pending();
                    }
                    Some(UiCommand::Quit) => {
                        if let Some(token) = turn_cancel.as_ref() {
                            token.cancel();
                        }
                        confirmer.abort_pending();
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
                            &mut turn,
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
                     *  - 设备状态、加载失败、溢出与会话结束这些合法事件照常处理。
                     */
                    Some(UiCommand::SetVoiceEnabled(enabled)) => {
                        apply_voice_enabled(weak, paths, settings, voice, command_tx, enabled);
                    }
                    Some(UiCommand::Voice(event)) => {
                        let _ = voice.on_event(weak, event, true);
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
                    Some(UiCommand::SaveSettings {
                        provider_index,
                        base_url,
                        model,
                        api_key,
                        permission_index,
                    }) => {
                        /*
                         * 任务执行期间保存设置：**真的保存**，只是本轮继续用旧快照。
                         * 之前这里只显示"稍后生效"却没有保存，用户以为改了其实没改。
                         */
                        if handle_save_settings(
                            weak,
                            paths,
                            secret,
                            settings,
                            SaveRequest {
                                provider_index,
                                base_url,
                                model,
                                api_key,
                                permission_index,
                            },
                        ) {
                            let has_key = secret.has(&settings.provider_id);
                            let endpoint = config::describe_endpoint(settings, has_key);
                            set_settings_saved_mid_turn(weak, &endpoint, has_key);
                        }
                    }
                    None => return TurnWait::Quit,
                }
            }
        }
    }
}

/// 执行中保存成功的提示：关闭设置页、刷新端点摘要，并说明本轮仍用旧快照。
fn set_settings_saved_mid_turn(weak: &slint::Weak<AppWindow>, endpoint: &str, has_key: bool) {
    let endpoint = endpoint.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_settings_open(false);
            window.set_api_key_input("".into());
            window.set_has_key(has_key);
            window.set_endpoint_text(endpoint.into());
            window.set_status("设置已保存；本轮仍用原配置，新配置从下一轮生效".into());
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
        wait
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
        let confirmer = confirmer_without_ui();

        let mut voice = VoiceRuntime::new();
        let (command_tx, _command_rx) = tokio::sync::mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_turn(
                &mut commands,
                turn,
                &slint::Weak::default(),
                &mut cancel_slot,
                &paths,
                &secret,
                &mut settings,
                &confirmer,
                &mut voice,
                &command_tx,
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
        let confirmer = confirmer_without_ui();

        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let stop = Arc::new(AtomicBool::new(false));
        voice.handle = Some(VoiceHandle::for_test(stop.clone(), instance));

        // 模拟用户在执行中点了「关闭麦克风」，随后又点了「取消」。
        // 两个 future 在同一个任务上交替推进（不需要 'static）。
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
            turn,
            &weak,
            &mut cancel_slot,
            &paths,
            &secret,
            &mut settings,
            &confirmer,
            &mut voice,
            &tx,
        );
        let (result, ()) = tokio::join!(wait, driver);

        assert!(matches!(result, TurnWait::Finished(_)));
        assert!(stop.load(Ordering::SeqCst), "关麦后采集必须保持停止");
        assert!(voice.handle.is_none(), "关麦后不能继续持有采集线程");
        assert!(voice.active.current().is_none(), "在途语音必须过期");
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
            assert!(prompt.is_none(), "旧实例的事件不得触发 Agent");
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
        voice.active.begin(VoiceTurn {
            instance,
            session: 1,
        });
        let turn_cancel = CancelToken::new();

        voice.disable(&weak);

        assert!(stop.load(Ordering::SeqCst), "关麦必须真的停止采集");
        assert!(voice.handle.is_none(), "句柄要释放，避免重复持有设备");
        assert!(voice.active.current().is_none(), "在途识别必须过期");
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
            },
        };
        assert!(
            voice.on_event(&weak, final_event.clone(), true).is_none(),
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
            },
        };

        let prompt = voice
            .on_event(&weak, event.clone(), false)
            .expect("空闲时有效 Final 应当提交");
        assert_eq!(prompt.text, "打开记事本");
        assert!((prompt.audio_seconds - 1.2).abs() < f32::EPSILON);

        assert!(
            voice.on_event(&weak, event, false).is_none(),
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
        assert!(voice.active.current().is_none());

        let prompt = voice.on_event(
            &weak,
            VoiceEvent {
                instance,
                kind: VoiceEventKind::Final {
                    turn,
                    text: "打开浏览器".to_string(),
                    audio_seconds: 1.0,
                    decode_ms: 5,
                },
            },
            false,
        );
        assert!(prompt.is_none(), "取消之后的结果不得提交");
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
                    },
                },
                false,
            )
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
