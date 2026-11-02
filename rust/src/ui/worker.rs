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

    // 初次加载：把当前配置灌进界面。
    let mut settings = config::load_settings(&paths).unwrap_or_else(|error| {
        eprintln!("[config] {error}");
        Settings::default()
    });
    // 旧版单 Key 数据：只迁移到配置里**明确记录**的提供商，不猜测、不删除。
    if let Ok(true) = secret.migrate_legacy(&settings.provider_id) {
        eprintln!(
            "[secret] 已把旧版单 Key 迁移到提供商 {}",
            settings.provider_id
        );
    }
    config::apply_env_overrides(&mut settings);
    let has_key = secret.has(&settings.provider_id);
    seed_ui(&window, &settings, has_key);

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
                weak, paths, secret, settings, command_rx, confirmer,
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
                break;
            }
            UiCommand::Cancel => {
                if let Some(token) = turn_cancel.take() {
                    token.cancel();
                    set_status(&weak, "正在中断…");
                }
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
                         * 界面上的 `has-key` 永远表示"**当前选中**的提供商有没有 Key"。
                         * 用户可能在设置页切到 DeepSeek（未保存）后清除 MiMo 的 Key，
                         * 这时该显示 DeepSeek 自己的状态，而不是被清掉的那家。
                         */
                        let active_has_key = secret.has(&settings.provider_id);
                        let endpoint = config::describe_endpoint(&settings, active_has_key);
                        let weak_ui = weak.clone();
                        let provider_id_ui = provider_id.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = weak_ui.upgrade() {
                                window.set_has_key(active_has_key);
                                window.set_endpoint_text(endpoint.into());
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
                if turn_cancel.is_some() {
                    set_status(&weak, "上一轮还在跑，先取消再发新指令");
                    continue;
                }
                let Some(toolkit) = toolkit.clone() else {
                    set_status(&weak, "Computer Use 后端不可用，无法执行指令");
                    set_busy(&weak, false);
                    continue;
                };

                // 端点快照：整轮用同一套「地址 + Key + 模型 + 推理档位」。
                // **按提供商取 Key**：当前提供商没有就拿空串，绝不借用别家的。
                let api_key = secret
                    .load(&settings.provider_id)
                    .unwrap_or(None)
                    .unwrap_or_default();
                let profile = settings.profile();
                let echo_reasoning = profile.capability.requires_reasoning_echo();
                let llm = match LlmClient::new(
                    profile,
                    api_key,
                    settings.temperature,
                    settings.timeout_ms,
                ) {
                    Ok(client) => client,
                    Err(error) => {
                        set_status(&weak, &format!("端点配置无效：{error}"));
                        set_busy(&weak, false);
                        continue;
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
                turn_cancel = Some(token.clone());

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
                let turn_history = std::mem::take(&mut history);
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
                )
                .await
                {
                    TurnWait::Finished(returned) => history = returned,
                    TurnWait::Quit => break,
                }
            }
        }
    }

    // 退出前：取消回合、清掉确认、释放子进程。
    confirmer.abort_pending();
    if let Some(token) = turn_cancel.take() {
        token.cancel();
    }
    if let Some(toolkit) = toolkit.take() {
        toolkit.bridge().dispose().await;
    }
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
                         */
                        match secret.clear(&provider_id) {
                            Ok(()) => {
                                set_status(
                                    weak,
                                    &format!("已清除 {provider_id} 的 Key（从下一轮生效）"),
                                );
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
            ),
        )
        .await
        .expect("退出不能让等待无限挂起");

        assert!(matches!(result, TurnWait::Quit), "必须返回 Quit");
        assert!(turn_cancel.is_cancelled(), "回合必须被取消");
        assert!(cancel_slot.is_none(), "退出后令牌槽位要清空");

        let _ = std::fs::remove_dir_all(&root);
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
