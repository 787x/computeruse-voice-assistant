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
    /// 清除已保存的 Key。
    ClearApiKey,
    /// 切换提供商（界面据此回填地址与模型）。
    ProviderChanged(i32),
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
    let settings = config::load_settings(&paths).unwrap_or_else(|error| {
        eprintln!("[config] {error}");
        Settings::default()
    });
    let has_key = secret.load().map(|value| value.is_some()).unwrap_or(false);
    seed_ui(&window, &settings, has_key);

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();
    let (confirm_tx, confirm_rx) = tokio::sync::mpsc::unbounded_channel::<ConfirmAnswer>();

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
        let tx = command_tx.clone();
        let weak = window.as_weak();
        let settings_snapshot = settings.clone();
        let has_key_now = has_key;
        window.on_open_settings(move || {
            if let Some(window) = weak.upgrade() {
                let index = PROVIDER_IDS
                    .iter()
                    .position(|id| *id == settings_snapshot.provider_id)
                    .unwrap_or(2) as i32;
                window.set_provider_index(index);
                window.set_base_url(settings_snapshot.base_url.clone().into());
                window.set_model_name(settings_snapshot.model.clone().into());
                window.set_api_key_input("".into());
                window.set_has_key(has_key_now);
                let permission_index = PERMISSION_MODES
                    .iter()
                    .position(|mode| *mode == settings_snapshot.permission_mode)
                    .unwrap_or(0) as i32;
                window.set_permission_index(permission_index);
                window.set_settings_open(true);
            }
            let _ = &tx;
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
        window.on_clear_api_key(move || {
            let _ = tx.send(UiCommand::ClearApiKey);
        });
    }
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_provider_changed(move |index| {
            let _ = tx.send(UiCommand::ProviderChanged(index));
            // 界面侧同步回填该提供商的默认地址/模型（用户仍可手改）。
            if let Some(window) = weak.upgrade() {
                let id = provider_id_for_index(index);
                if let Some(provider) = config::get_provider(id) {
                    window.set_base_url(provider.base_url.into());
                    if !provider.default_model.is_empty() {
                        window.set_model_name(provider.default_model.into());
                    }
                }
            }
        });
    }
    {
        let confirm_tx = confirm_tx.clone();
        window.on_confirm_decision(move |allow| {
            let _ = confirm_tx.send(if allow {
                ConfirmAnswer::allow_click()
            } else {
                ConfirmAnswer::deny_click()
            });
        });
    }
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_quit_app(move || {
            if let Some(window) = weak.upgrade() {
                window.set_confirm_open(false);
            }
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
            runtime.block_on(run_worker(
                weak, paths, secret, settings, command_rx, confirm_rx,
            ));
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

/// 把确认请求发到界面并等待用户点击。
struct UiConfirmer {
    weak: slint::Weak<AppWindow>,
    replies: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ConfirmAnswer>>>,
}

impl Confirmer for UiConfirmer {
    fn confirm<'a>(&'a self, request: ConfirmRequest) -> ConfirmFuture<'a> {
        let weak = self.weak.clone();
        let replies = self.replies.clone();
        Box::pin(async move {
            let prompt = ConfirmationPrompt {
                tool: request.tool.clone(),
                capability: request.capability.to_string(),
                label: request.label.to_string(),
                reason: request.reason.clone(),
                args_summary: request.args_summary.clone(),
            };
            let args_text = serde_json::to_string(&prompt.args_summary).unwrap_or_default();
            let weak_for_ui = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_ui.upgrade() {
                    window.set_confirm_tool(prompt.tool.clone().into());
                    window.set_confirm_label(prompt.label.clone().into());
                    window.set_confirm_reason(prompt.reason.clone().into());
                    window.set_confirm_args(args_text.clone().into());
                    window.set_confirm_open(true);
                    window.set_status("等待你确认…".into());
                }
            });
            let mut guard = replies.lock().await;
            let answer = guard.recv().await.unwrap_or_else(ConfirmAnswer::deny_click);
            let weak_for_close = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_close.upgrade() {
                    window.set_confirm_open(false);
                }
            });
            answer
        })
    }
}

/// 工作线程主循环。
async fn run_worker(
    weak: slint::Weak<AppWindow>,
    paths: AppPaths,
    secret: SecretStore,
    mut settings: Settings,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    confirm_rx: tokio::sync::mpsc::UnboundedReceiver<ConfirmAnswer>,
) {
    let confirmer: Arc<UiConfirmer> = Arc::new(UiConfirmer {
        weak: weak.clone(),
        replies: Arc::new(tokio::sync::Mutex::new(confirm_rx)),
    });

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
                if let Some(token) = turn_cancel.take() {
                    token.cancel();
                }
                break;
            }
            UiCommand::Cancel => {
                if let Some(token) = turn_cancel.take() {
                    token.cancel();
                    set_status(&weak, "正在中断…");
                }
            }
            UiCommand::ProviderChanged(_index) => {
                // 地址/模型由界面侧回填；这里不需要动作。
            }
            UiCommand::ClearApiKey => match secret.clear() {
                Ok(()) => {
                    set_status(&weak, "已清除保存的 Key");
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            window.set_has_key(false);
                            window.set_endpoint_text(
                                config::describe_endpoint(&Settings::default(), false).into(),
                            );
                        }
                    });
                }
                Err(error) => set_status(&weak, &format!("清除 Key 失败：{error}")),
            },
            UiCommand::SaveSettings {
                provider_index,
                base_url,
                model,
                api_key,
                permission_index,
            } => {
                let mut next = settings.clone();
                next.provider_id = provider_id_for_index(provider_index).to_string();
                next.base_url = base_url;
                next.model = model;
                next.permission_mode = permission_mode_for_index(permission_index);
                match next.normalized() {
                    Ok(next) => {
                        // 新的 API Key：只有用户真的填了才写（留空 = 不修改）。
                        let mut key_error = None;
                        if !api_key.trim().is_empty() {
                            if let Err(error) = secret.save(api_key.trim()) {
                                key_error = Some(error.to_string());
                            }
                        }
                        match config::save_settings(&paths, &next) {
                            Ok(()) => {
                                settings = next;
                                let has_key =
                                    secret.load().map(|value| value.is_some()).unwrap_or(false);
                                let weak_ui = weak.clone();
                                let endpoint = config::describe_endpoint(&settings, has_key);
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(window) = weak_ui.upgrade() {
                                        window.set_settings_open(false);
                                        window.set_api_key_input("".into());
                                        window.set_has_key(has_key);
                                        window.set_endpoint_text(endpoint.into());
                                        window.set_status("设置已保存".into());
                                    }
                                });
                                match key_error {
                                    Some(error) => set_status(
                                        &weak,
                                        &format!("设置已保存，但 Key 保存失败：{error}"),
                                    ),
                                    None => set_status(&weak, "设置已保存"),
                                }
                            }
                            Err(error) => set_status(&weak, &format!("保存设置失败：{error}")),
                        }
                    }
                    Err(error) => set_status(&weak, &format!("设置无效：{error}")),
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
                let api_key = secret.load().unwrap_or(None).unwrap_or_default();
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
                let confirmer = confirmer.clone();
                let turn_history = std::mem::take(&mut history);
                let turn = tokio::spawn(async move {
                    let emit = move |event: AgentEvent| {
                        apply_event(&weak_events, event);
                    };
                    let mut turn_history = turn_history;
                    let outcome = agent
                        .run(&input, &mut turn_history, &token, &confirmer, &emit)
                        .await;
                    (outcome, turn_history, settings_snapshot)
                });

                // 等待结束，但**同时**继续处理取消/退出命令。
                history = wait_turn(&mut commands, turn, &weak, &mut turn_cancel).await;
            }
        }
    }

    if let Some(toolkit) = toolkit.take() {
        toolkit.bridge().dispose().await;
    }
}

/// 等待一个回合结束，同时响应取消与退出。
///
/// 取消只置令牌（不打断进程读写），等工作线程自己走到检查点退出，
/// 这样"界面说停了"与"后台真的停了"是同一件事。
async fn wait_turn(
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    mut turn: tokio::task::JoinHandle<(TurnOutcome, Vec<ChatMessage>, Settings)>,
    weak: &slint::Weak<AppWindow>,
    turn_cancel: &mut Option<CancelToken>,
) -> Vec<ChatMessage> {
    loop {
        tokio::select! {
            joined = &mut turn => {
                *turn_cancel = None;
                match joined {
                    Ok((outcome, history, settings)) => {
                        finish_turn(weak, &outcome, &settings);
                        return history;
                    }
                    Err(error) => {
                        let outcome = TurnOutcome {
                            text: format!("后台任务异常：{error}"),
                            failed: true,
                            failure_reason: Some("worker_panic".to_string()),
                            ..TurnOutcome::default()
                        };
                        finish_turn(weak, &outcome, &Settings::default());
                        return Vec::new();
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
                        // 让工作线程自己收敛：回合会走到检查点并结束。
                        set_status(weak, "正在退出…");
                    }
                    Some(UiCommand::Send(_)) => {
                        set_status(weak, "上一轮还在跑，先取消再发新指令");
                    }
                    Some(UiCommand::ProviderChanged(_))
                    | Some(UiCommand::ClearApiKey)
                    | Some(UiCommand::SaveSettings { .. }) => {
                        set_status(weak, "正在执行任务，设置改动将在本轮结束后生效");
                    }
                    None => return Vec::new(),
                }
            }
        }
    }
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

    #[test]
    fn provider_index_mapping_is_stable() {
        assert_eq!(provider_id_for_index(0), "mimo");
        assert_eq!(provider_id_for_index(1), "deepseek");
        assert_eq!(provider_id_for_index(2), "openai-compatible");
        // 越界不能 panic，也不能悄悄落到某个官方提供商上
        assert_eq!(provider_id_for_index(99), "openai-compatible");
        assert_eq!(provider_id_for_index(-1), "openai-compatible");
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
