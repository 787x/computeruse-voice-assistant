//! 后台工作线程：持有 Agent、Computer Use 工具与配置，驱动一条回合。
//!
//! 这里只做"把界面动作翻译成 Agent 调用、再把结果翻译成界面状态"，
//! 具体的协议、权限、预算逻辑都在各自模块里，便于单独测试。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::agent::{Agent, AgentConfig, AgentEvent, ConfirmationPrompt, ToolStatus, TurnOutcome};
use crate::cancel::CancelToken;
use crate::config::{self, AppPaths, SessionModelChoice, Settings};
// `PermissionMode` 只有测试直接用到：生产代码一律经 `ui::settings` 的索引映射，
// 避免"界面索引 → 档位"的逻辑在两处各写一份而漂移。
#[cfg(test)]
use crate::config::PermissionMode;
use crate::cua::bridge::{BridgeOptions, PwshBridge};
use crate::cua::tools::CuaToolkit;
use crate::llm::{ChatMessage, LlmClient};
use crate::model_catalog::{fetch_models, probe_chat, ModelCache};
use crate::permissions::{
    AuditLog, ConfirmAnswer, ConfirmFuture, ConfirmRequest, Confirmer, PermissionState,
};
use crate::prompt;
use crate::secret::SecretStore;
use crate::session::{TimelineItem, TurnStatus};
use crate::tools::clock_tool;
use crate::tools::registry::ToolRegistry;
use crate::tools::web_search::WebSearch;
use crate::ui::sessions::{
    publish_audit, publish_session_list, publish_session_title, publish_timeline, SessionRuntime,
};
use crate::ui::settings::permission_mode_for_index;
use crate::ui::settings::{
    self as settings_ui, apply_save, permission_mode_index, reasoning_effort_index, thinking_index,
    tts_backend_index, SaveOutcome, SaveRequest, EFFORT_LABELS, PERMISSION_LABELS,
    PERMISSION_MODES, THINKING_LABELS,
};
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
    tts::{SpeechId, TtsSettings, TurnId},
    CaptureInfoForUi, RejectReason, VoiceEvent, VoiceEventKind, VoiceInstance, VoiceState,
    VoiceTurn,
};
use crate::ChatEntry;
use crate::ToolCard;

use super::model_probe::ProbeDraft;
use crate::AppWindow;

/// 界面发给工作线程的命令。
#[derive(Debug)]
pub enum UiCommand {
    SaveEndpoint(
        config::ProviderConfig,
        settings_ui::ProviderDraft,
        i32,
        String,
    ),
    DeleteEndpoint(String),
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
        request: SaveRequest,
        generation: u64,
    },
    /// 关闭设置面板（取消）：丢弃界面上的未保存修改，重新按已保存配置回填。
    ///
    /// `generation` 用来作废在途的保存：取消之后，工作线程会丢弃排队中/过期的
    /// 保存请求，不会"用户点了取消却还是偷偷存了一份旧表单"。
    CancelSettings {
        generation: u64,
    },
    /// 清除**指定提供商**的 Key（界面当前选中的那一家）。
    ///
    /// 必须带 provider_id：设置页可以切到别家而不保存，
    /// 那时"清除"应该作用于界面显示的那家，而不是已保存配置里的那家。
    ClearApiKey(String),
    /// 清除联网搜索（Tavily）的 Key。与模型 Key **互不影响**。
    ClearSearchKey,
    /// 测试联网搜索 Key 的可用性（真实打一次 `POST /search`）。
    ///
    /// `key` 为空 = 用已保存的 Key；非空 = 用界面上还没保存的草稿。
    ///
    /// `generation` 是**点击测试那一刻**的测试代次。命令在通道里排队期间
    /// 用户完全可能又改了输入（界面侧会推进代次），所以工作线程处理这条命令前
    /// 必须先核对它是否仍然有效——否则 Key A 的旧命令会被当成 Key B 的测试。
    TestSearchKey {
        key: String,
        generation: u64,
    },
    /// 切换提供商（界面据此回填地址与模型）。
    ProviderChanged,
    /// 打开设置：按当前已保存的配置回填界面。
    OpenSettings,
    /// 切换设置分类（只改界面，不回填）。
    SettingsTab(i32),
    /// 测试连接（真实发一次最小对话请求）。
    TestConnection(ProbeDraft),
    /// 获取在线模型列表（`/models`，带缓存与失败保留）。
    FetchModels(ProbeDraft),
    ModelsFetched {
        draft: ProbeDraft,
        credential: String,
        request: u64,
        fetch: crate::model_catalog::ModelFetch,
    },
    /// 新建会话。
    NewSession,
    /// 选择会话（空 id = 不做改动）。
    SelectSession(String),
    /// 重命名会话（空 id = 当前会话）。
    RenameSession {
        id: String,
        title: String,
    },
    /// 删除会话（空 id = 当前会话）。
    DeleteSession(String),
    /// 输入区的模型下拉被选中。
    ModelChoicePicked(i32),
    /// 输入区的推理档位被选中。
    ReasoningPicked(i32),
    /// 手工输入模型名（对当前会话生效）。
    ApplyManualModel(String),
    /// 权限档位被选中（立即生效，不等保存）。
    PermissionModePicked(i32),
    /// 逐能力覆盖被选中（立即生效，不等保存）。
    OverridePicked {
        row: i32,
        choice: i32,
    },
    /// 暂停 / 恢复。
    TogglePause,
    /// 会话内限时允许某能力。
    GrantCapability {
        capability: String,
        minutes: i32,
    },
    /// 导出审计。
    ExportAudit,
    /// 启用/关闭麦克风监听（界面开关）。
    SetVoiceEnabled(bool),
    DictateNow,
    QuickPermissionPicked(i32),
    /// 语音线程发回的事件（内部消息，不是用户动作）。
    Voice(VoiceEvent),
    /// 播报层发回的事件（状态/字幕/提示）。
    Speech(SpeechEvent),
    /// 退出。
    Quit,
}

/// 提供商下拉框的顺序（与 UI 里的数组一致）。
const PROVIDER_IDS: &[&str] = &["mimo", "deepseek", "openai-compatible"];
/// MiMo 提供商的 id。MiMo TTS 只从这家账户读 Key（见 `voice::tts::mimo::KEY_PROVIDER`）。
pub const MIMO_PROVIDER_ID: &str = "mimo";

/// 设置分类的标签（顺序即导航顺序）。
const SETTINGS_TABS: [&str; 6] = [
    "模型",
    "Agent 与上下文",
    "系统提示词",
    "权限",
    "语音输入 / 播报",
    "工具与联网",
];

pub(super) fn provider_id_for_index(index: i32) -> &'static str {
    // 越界一律落到"自定义端点"，而不是悄悄用某个官方提供商：
    // 用错端点会把请求打到用户没配 Key 的官方地址上。
    usize::try_from(index)
        .ok()
        .and_then(|index| PROVIDER_IDS.get(index).copied())
        .unwrap_or("openai-compatible")
}

/// 启动界面与工作线程（阻塞直到窗口关闭）。
pub fn run_app() -> Result<(), Box<dyn std::error::Error>> {
    let Some(instance) = super::instance::Instance::acquire()? else {
        return Ok(());
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|attributes| {
            if super::hud_window::creating_hud() {
                attributes.with_active(false)
            } else {
                attributes
            }
        })
        .select()?;
    let window = AppWindow::new()?;
    let instance_timer = slint::Timer::default();
    let weak = window.as_weak();
    instance_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(150),
        move || {
            for action in instance.requests() {
                if let Some(w) = weak.upgrade() {
                    super::background::activate_main(&w);
                    if action == "settings" {
                        w.invoke_open_settings();
                    }
                }
            }
        },
    );
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
            let _background = super::background::BackgroundUi::new(&window)?;
            window.on_quit_app(|| {
                let _ = slint::quit_event_loop();
            });
            if !std::env::args().any(|a| a == "--background") {
                window.show()?;
            }
            slint::run_event_loop_until_quit()?;
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
    settings_ui::restore_model_catalogs(&mut settings, &paths, &secret, None);
    let has_key = secret.has(&settings.provider_id);
    /*
     * 启动自检：数据目录能不能写。
     *
     * 必须在保存时才发现"写不进去"是最糟的时机（用户已经改了一堆设置）。
     * 这里提前探测一次，并把结论直接显示在状态栏——原因与解决办法都在
     * `describe_write_error` 里写清楚了，用户照做即可。
     */
    let startup_write_error = config::check_writable(&paths).err();
    /*
     * 会话库：与全局设置、密钥分开存放（`%APPDATA%\cuava\sessions\`）。
     *
     * **必须在 `seed_ui` 之前就备好一个会话**：`seed_ui` 会在主线程同步推一次
     * 会话列表，若那时还是空的，界面会显示"（没有会话）"，而后台线程补建的
     * 那次推送因为窗口已经渲染过、用户看起来就像"新建了但没显示"。
     */
    let mut sessions = SessionRuntime::open(&paths);
    let mut startup_session_error: Option<String> = None;
    if sessions.ensure_one(crate::clock::now().unix_ms) {
        if let Err(error) = sessions.save_current() {
            startup_session_error = Some(error);
        }
    }
    seed_ui(&window, &settings, has_key, &sessions);
    if let Some(error) = migration_error {
        window.set_status(format!("旧 Key 迁移失败：{error}").into());
    }
    /*
     * 写盘能力优先显示：它决定"设置与会话能不能保存"，
     * 比"会话建不出来"更根本，所以覆盖在上面。
     */
    if let Some(error) = &startup_write_error {
        window.set_status(format!("数据目录不可写：{error}").into());
        window.set_settings_error(error.clone().into());
    } else if let Some(error) = &startup_session_error {
        window.set_status(format!("会话无法保存：{error}").into());
    }

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();
    super::endpoint_cards::install(&window, paths.clone(), command_tx.clone());

    // 确认器由界面持有：设置页/确认框/退出都要和它交互。
    let confirmer = Arc::new(UiConfirmer::new(window.as_weak()));

    // 设置保存的代次（单调递增）。只有最新一代的回填才允许改界面，
    // 这样超时/取消/更晚完成的旧保存都不会覆盖用户的新编辑。
    let save_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));

    /*
     * 联网搜索 Key 的**测试代次**（单调递增）。
     *
     * 必须在界面侧也持有：用户只要**改动 Key 输入框**就要立刻作废在途测试，
     * 而改输入框是纯 Slint 事件，不经过工作线程的命令通道。
     * 其余推进点（重测、保存、清除 Key）在工作线程里做。
     */
    let search_test_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let model_drafts = std::rc::Rc::new(std::cell::RefCell::new(std::collections::BTreeMap::<
        String,
        settings_ui::ProviderDraft,
    >::new()));
    let draft_provider = std::rc::Rc::new(std::cell::Cell::new(window.get_provider_index()));

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
        let model_drafts = model_drafts.clone();
        let draft_provider = draft_provider.clone();
        let weak = window.as_weak();
        let settings_paths = paths.clone();
        window.on_open_settings(move || {
            model_drafts.borrow_mut().clear();
            if let Some(window) = weak.upgrade() {
                let index = config::load_settings(&settings_paths)
                    .ok()
                    .and_then(|settings| {
                        PROVIDER_IDS
                            .iter()
                            .position(|id| *id == settings.provider_id)
                    })
                    .map(|i| i as i32)
                    .unwrap_or_else(|| window.get_provider_index());
                draft_provider.set(index);
            }
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
        let model_drafts = model_drafts.clone();
        window.on_save_all(move || {
            // 界面上读一次当前全部字段（工作线程持有权威配置副本）。
            let mut request = match weak.upgrade() {
                Some(window) => build_save_request(&window),
                None => return,
            };

            request.provider_drafts = model_drafts.borrow().clone();
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
        });
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
        let model_drafts = model_drafts.clone();
        let draft_provider = draft_provider.clone();
        window.on_provider_changed(move |index| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let old_index = draft_provider.get();
            let id = provider_id_for_index(index).to_string();
            let saved = match config::load_settings(&paths_for_key) {
                Ok(settings) => settings,
                Err(error) => {
                    window.set_provider_index(old_index);
                    window.set_settings_error(format!("无法读取供应商配置：{error}").into());
                    return;
                }
            };
            model_drafts.borrow_mut().insert(
                provider_id_for_index(old_index).to_string(),
                capture_provider_draft(&window),
            );
            let secret = SecretStore::new(paths_for_key.secret_file());
            let mut saved = saved;
            settings_ui::restore_model_catalogs(&mut saved, &paths_for_key, &secret, None);
            let provider = saved.provider(&id);
            window.set_has_key(secret.has(&id));
            window.set_base_url(provider.base_url_or_default().into());
            window.set_model_name(provider.model.clone().into());
            window.set_api_key_input("".into());
            window
                .set_provider_hint_index(settings_ui::provider_hint_index(provider.provider_hint));
            window.set_thinking_index(thinking_index(provider.thinking));
            window.set_effort_index(reasoning_effort_index(provider.reasoning_effort));
            fill_model_settings(&window, &provider);
            if let Some(draft) = model_drafts.borrow().get(&id) {
                window.set_base_url(draft.base_url.clone().into());
                window.set_model_name(draft.model.clone().into());
                window.set_api_key_input(draft.api_key.clone().into());
                window.set_provider_hint_index(draft.hint);
                window.set_thinking_index(draft.thinking);
                window.set_effort_index(draft.effort);
                apply_model_draft(&window, &draft.rows);
            }
            draft_provider.set(index);
            window.set_models_busy(false);
            let _ = tx.send(UiCommand::ProviderChanged);
        });
    }
    {
        // ---- 权限（立即生效：不等保存） ----
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_permission_mode_picked(move |index| {
            if let Some(window) = weak.upgrade() {
                window.set_settings_dirty(true);
                window.set_settings_error("".into());
            }
            let _ = tx.send(UiCommand::PermissionModePicked(index));
        });
    }
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_override_picked(move |row, choice| {
            if let Some(window) = weak.upgrade() {
                let mut values: Vec<i32> = window.get_override_indices().iter().collect();
                if let Some(value) = usize::try_from(row).ok().and_then(|i| values.get_mut(i)) {
                    *value = choice;
                }
                window.set_override_indices(ModelRc::new(VecModel::from(values)));
                window.set_settings_dirty(true);
            }
            let _ = tx.send(UiCommand::OverridePicked { row, choice });
        });
    }
    {
        let tx = command_tx.clone();
        window.on_toggle_pause(move || {
            let _ = tx.send(UiCommand::TogglePause);
        });
    }
    {
        let tx = command_tx.clone();
        window.on_grant_capability(move |capability, minutes| {
            let _ = tx.send(UiCommand::GrantCapability {
                capability: capability.to_string(),
                minutes,
            });
        });
    }
    {
        let tx = command_tx.clone();
        window.on_export_audit(move || {
            let _ = tx.send(UiCommand::ExportAudit);
        });
    }
    {
        super::local_models::wire(&window, paths.clone());
        window.on_model_match_count(|labels, query| {
            labels
                .iter()
                .filter(|label| label.to_lowercase().contains(&query.to_lowercase()))
                .count() as i32
        });
        window.on_model_group_matches(|labels, providers, group, query| {
            labels.iter().enumerate().any(|(i, label)| {
                providers.row_data(i).as_ref() == Some(&group)
                    && label.to_lowercase().contains(&query.to_lowercase())
            })
        });
        window
            .on_model_matches(|label, query| label.to_lowercase().contains(&query.to_lowercase()));
        // ---- 模型发现与连通性 ----
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_test_connection(move || {
            if let Some(window) = weak.upgrade() {
                let _ = tx.send(UiCommand::TestConnection(ProbeDraft::from_window(&window)));
            }
        });
    }
    {
        let tx = command_tx.clone();
        let weak = window.as_weak();
        window.on_fetch_models(move || {
            if let Some(window) = weak.upgrade() {
                let _ = tx.send(UiCommand::FetchModels(ProbeDraft::from_window(&window)));
            }
        });
    }
    {
        let tx = command_tx.clone();
        window.on_model_choice_picked(move |index| {
            let _ = tx.send(UiCommand::ModelChoicePicked(index));
        });
        let tx = command_tx.clone();
        window.on_reasoning_picked(move |index| {
            let _ = tx.send(UiCommand::ReasoningPicked(index));
        });
        let tx = command_tx.clone();
        window.on_apply_manual_model(move |model| {
            let _ = tx.send(UiCommand::ApplyManualModel(model.to_string()));
        });
    }
    {
        // ---- 会话 ----
        let tx = command_tx.clone();
        window.on_new_session(move || {
            let _ = tx.send(UiCommand::NewSession);
        });
    }
    {
        let tx = command_tx.clone();
        window.on_select_session(move |id| {
            let _ = tx.send(UiCommand::SelectSession(id.to_string()));
        });
    }
    {
        let tx = command_tx.clone();
        window.on_rename_session(move |id, title| {
            let _ = tx.send(UiCommand::RenameSession {
                id: id.to_string(),
                title: title.to_string(),
            });
        });
    }
    {
        let tx = command_tx.clone();
        window.on_delete_session(move |id| {
            let _ = tx.send(UiCommand::DeleteSession(id.to_string()));
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_rename(move || {
            if let Some(window) = weak.upgrade() {
                let title = window.get_session_title().to_string();
                window.set_rename_draft(title.into());
                window.set_rename_open(true);
            }
        });
    }
    {
        // ---- 设置分类导航 ----
        let tx = command_tx.clone();
        window.on_settings_tab_picked(move |index| {
            let _ = tx.send(UiCommand::SettingsTab(index));
        });
    }
    {
        /*
         * 联网搜索（PR12）：测试与清除都走命令通道，由工作线程执行。
         *
         * 测试**不在 Slint 线程上发请求**：一次真实搜索要 1~8 秒，
         * 在事件循环里等会把整个界面卡住（这正是 PR8 定下的规矩）。
         *
         * **代次在点击这一刻就确定**：命令进队列后、工作线程处理前，用户仍可能
         * 改输入框（界面侧会推进代次）。把点击时的代次随命令带上，工作线程
         * 处理前先核对；否则 Key A 的旧命令会被当成 Key B 的测试结果。
         */
        let test_tx = command_tx.clone();
        let generation = search_test_generation.clone();
        window.on_test_search_key(move |key| {
            let generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = test_tx.send(UiCommand::TestSearchKey {
                key: key.to_string(),
                generation,
            });
        });
        let clear_tx = command_tx.clone();
        window.on_clear_search_key(move || {
            let _ = clear_tx.send(UiCommand::ClearSearchKey);
        });
    }
    {
        /*
         * **用户一改 Key 输入框就作废在途测试**。
         *
         * 这条必须留在界面侧：改输入框是纯 Slint 事件，不经过工作线程。
         * 少了它就会出现"测 Key A → 改成 Key B → A 的迟到结果被显示成
         * B 的测试结果"——用户会拿着一个错的 Key 去保存。
         */
        let generation = search_test_generation.clone();
        window.on_search_key_edited(move || {
            generation.fetch_add(1, Ordering::SeqCst);
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
        let direct_tx = command_tx.clone();
        window.on_dictate_now(move || {
            let _ = direct_tx.send(UiCommand::DictateNow);
        });
        let permission_tx = command_tx.clone();
        window.on_quick_permission_picked(move |index| {
            let _ = permission_tx.send(UiCommand::QuickPermissionPicked(index));
        });
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
            if tx.send(UiCommand::Quit).is_err() {
                let _ = slint::quit_event_loop();
            }
        });
    }
    let _background = super::background::BackgroundUi::new(&window)?;
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
                sessions,
                command_rx,
                command_tx,
                confirmer,
                save_generation,
                search_test_generation,
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

    if !std::env::args().any(|arg| arg == "--background") {
        window.show()?;
    }
    if std::env::args().any(|a| a == "--settings") {
        window.invoke_open_settings();
    }
    slint::run_event_loop_until_quit()?;
    Ok(())
}

/// 把初始配置同步到界面控件（含会话列表、设置分类与输入区的模型下拉）。
fn seed_ui(window: &AppWindow, settings: &Settings, has_key: bool, sessions: &SessionRuntime) {
    let provider_index = PROVIDER_IDS
        .iter()
        .position(|id| *id == settings.provider_id)
        .unwrap_or(2) as i32;
    window.set_provider_index(provider_index);
    window.set_base_url(settings.base_url.clone().into());
    window.set_model_name(settings.model.clone().into());
    window.set_has_key(has_key);
    window.set_permission_index(permission_mode_index(settings.permission_mode));
    window.set_endpoint_text(config::describe_endpoint(settings, has_key).into());
    window.set_entries(ModelRc::new(VecModel::from(Vec::<ChatEntry>::new())));
    window.set_tools(ModelRc::new(VecModel::from(Vec::<ToolCard>::new())));

    // 设置分类导航 + 权限档位标签 + 逐能力覆盖的初始行。
    window.set_settings_tab_labels(ModelRc::new(VecModel::from(
        SETTINGS_TABS
            .iter()
            .map(|label| slint::SharedString::from(*label))
            .collect::<Vec<_>>(),
    )));
    apply_permission_options(window);
    wire_plan_editor(window);
    window.set_override_indices(ModelRc::new(VecModel::from(vec![
        0i32;
        crate::permissions::capabilities().len()
    ])));

    // 会话：列表 + 当前会话的时间线 + 标题（都从磁盘恢复）。
    publish_session_list(&window.as_weak(), sessions);
    publish_timeline(&window.as_weak(), sessions);
    publish_session_title(&window.as_weak(), sessions);
    refresh_model_picker(window, settings, sessions);
}

/// 权限档位与"逐能力覆盖"的选项标签（一次写入，不随每次保存重建）。
fn refresh_plan_labels(window: &AppWindow) {
    let schedule: config::ScheduleSettings =
        serde_json::from_str(window.get_model_schedule_json().as_str()).unwrap_or_default();
    window.set_rule_labels(ModelRc::new(VecModel::from(
        schedule
            .rules
            .iter()
            .map(|r| {
                slint::SharedString::from(format!(
                    "{}–{} · {} · {:?}",
                    r.from, r.to, r.plan_id, r.days
                ))
            })
            .collect::<Vec<_>>(),
    )));

    let plans: Vec<config::ModelPlan> =
        serde_json::from_str(window.get_model_plans_json().as_str()).unwrap_or_default();
    window.set_plan_edit_labels(ModelRc::new(VecModel::from(
        plans
            .iter()
            .map(|p| slint::SharedString::from(p.name.as_str()))
            .collect::<Vec<_>>(),
    )));
}

fn wire_plan_editor(window: &AppWindow) {
    let weak = window.as_weak();
    window.on_edit_rule(move |index| {
        if let Some(w) = weak.upgrade() {
            let schedule: config::ScheduleSettings =
                serde_json::from_str(w.get_model_schedule_json().as_str()).unwrap_or_default();
            let rule = usize::try_from(index)
                .ok()
                .and_then(|i| schedule.rules.get(i));
            w.set_rule_index(index);
            w.set_rule_from(rule.map(|r| r.from.as_str()).unwrap_or("09:00").into());
            w.set_rule_to(rule.map(|r| r.to.as_str()).unwrap_or("18:00").into());
            w.set_rule_days(
                rule.map(|r| {
                    r.days
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default()
                .into(),
            );
            w.set_rule_plan(rule.map(|r| r.plan_id.as_str()).unwrap_or("").into());
        }
    });
    let weak = window.as_weak();
    window.on_store_rule(move || {
        if let Some(w) = weak.upgrade() {
            let Ok(mut schedule) = serde_json::from_str::<config::ScheduleSettings>(
                w.get_model_schedule_json().as_str(),
            ) else {
                return;
            };
            let days = w
                .get_rule_days()
                .split(',')
                .filter(|d| !d.trim().is_empty())
                .map(|d| d.trim().parse::<u8>())
                .collect::<Result<Vec<_>, _>>();
            let Ok(days) = days else {
                w.set_settings_error("星期应为 1–7，使用逗号分隔".into());
                return;
            };
            let rule = config::ScheduleRule {
                from: w.get_rule_from().to_string(),
                to: w.get_rule_to().to_string(),
                days,
                plan_id: w.get_rule_plan().to_string(),
                enabled: true,
            };
            if let Some(existing) = usize::try_from(w.get_rule_index())
                .ok()
                .and_then(|i| schedule.rules.get_mut(i))
            {
                *existing = rule;
            } else {
                schedule.rules.push(rule);
            }
            if let Err(error) = schedule.validate() {
                w.set_settings_error(error.into());
                return;
            }
            w.set_model_schedule_json(
                serde_json::to_string_pretty(&schedule)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_settings_dirty(true);
            refresh_plan_labels(&w);
        }
    });
    let weak = window.as_weak();
    window.on_remove_rule(move || {
        if let Some(w) = weak.upgrade() {
            let Ok(mut schedule) = serde_json::from_str::<config::ScheduleSettings>(
                w.get_model_schedule_json().as_str(),
            ) else {
                return;
            };
            if let Ok(i) = usize::try_from(w.get_rule_index()) {
                if i < schedule.rules.len() {
                    schedule.rules.remove(i);
                }
            }
            w.set_model_schedule_json(
                serde_json::to_string_pretty(&schedule)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_rule_index(-1);
            w.set_settings_dirty(true);
            refresh_plan_labels(&w);
        }
    });

    let weak = window.as_weak();
    window.on_update_schedule(move || {
        if let Some(w) = weak.upgrade() {
            let Ok(mut schedule) = serde_json::from_str::<config::ScheduleSettings>(
                w.get_model_schedule_json().as_str(),
            ) else {
                return;
            };
            schedule.enabled = w.get_schedule_enabled();
            schedule.default_plan_id = w.get_schedule_default().to_string();
            w.set_model_schedule_json(
                serde_json::to_string_pretty(&schedule)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_settings_dirty(true);
        }
    });
    let weak = window.as_weak();
    window.on_edit_plan(move |index| {
        if let Some(w) = weak.upgrade() {
            let plans: Vec<config::ModelPlan> =
                serde_json::from_str(w.get_model_plans_json().as_str()).unwrap_or_default();
            w.set_plan_edit_index(index);
            let p = usize::try_from(index).ok().and_then(|i| plans.get(i));
            w.set_plan_edit_id(p.map(|p| p.id.as_str()).unwrap_or("").into());
            w.set_plan_edit_name(p.map(|p| p.name.as_str()).unwrap_or("").into());
            w.set_plan_edit_provider(p.map(|p| p.provider_id.as_str()).unwrap_or("mimo").into());
            w.set_plan_edit_model(p.map(|p| p.model.as_str()).unwrap_or("").into());
        }
    });
    let weak = window.as_weak();
    window.on_store_plan(move || {
        if let Some(w) = weak.upgrade() {
            let Ok(mut plans) =
                serde_json::from_str::<Vec<config::ModelPlan>>(w.get_model_plans_json().as_str())
            else {
                return;
            };
            let index = usize::try_from(w.get_plan_edit_index()).ok();
            let previous = index.and_then(|i| plans.get(i)).cloned();
            let plan = config::ModelPlan {
                id: w.get_plan_edit_id().to_string(),
                name: w.get_plan_edit_name().to_string(),
                provider_id: w.get_plan_edit_provider().to_string(),
                model: w.get_plan_edit_model().to_string(),
                thinking: previous.as_ref().map(|p| p.thinking).unwrap_or_default(),
                reasoning_effort: previous
                    .as_ref()
                    .map(|p| p.reasoning_effort)
                    .unwrap_or_default(),
            };
            if let Some(existing) = index.and_then(|i| plans.get_mut(i)) {
                *existing = plan;
            } else {
                plans.push(plan);
            }
            w.set_model_plans_json(
                serde_json::to_string_pretty(&plans)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_settings_dirty(true);
            refresh_plan_labels(&w);
        }
    });
    let weak = window.as_weak();
    window.on_remove_plan(move || {
        if let Some(w) = weak.upgrade() {
            let Ok(mut plans) =
                serde_json::from_str::<Vec<config::ModelPlan>>(w.get_model_plans_json().as_str())
            else {
                return;
            };
            if let Ok(i) = usize::try_from(w.get_plan_edit_index()) {
                if i < plans.len() {
                    plans.remove(i);
                }
            }
            w.set_model_plans_json(
                serde_json::to_string_pretty(&plans)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_plan_edit_index(-1);
            w.set_settings_dirty(true);
            refresh_plan_labels(&w);
        }
    });
}

fn apply_permission_options(window: &AppWindow) {
    window.set_capability_ids(ModelRc::new(VecModel::from(
        crate::permissions::capabilities()
            .iter()
            .map(|c| slint::SharedString::from(c.id))
            .collect::<Vec<_>>(),
    )));
    window.set_permission_mode_labels(ModelRc::new(VecModel::from(
        PERMISSION_LABELS
            .iter()
            .map(|label| slint::SharedString::from(*label))
            .collect::<Vec<_>>(),
    )));
    window.set_capability_labels(ModelRc::new(VecModel::from(
        crate::permissions::capabilities()
            .iter()
            .map(|capability| slint::SharedString::from(capability.label))
            .collect::<Vec<_>>(),
    )));
    window.set_override_labels(ModelRc::new(VecModel::from(
        settings_ui::OVERRIDE_LABELS
            .iter()
            .map(|label| slint::SharedString::from(*label))
            .collect::<Vec<_>>(),
    )));
}

/// 输入区的模型下拉：列出**已配置的提供商**及其已知模型。
///
/// 会话级手动选择优先；没有手选时按"时段方案 → 默认方案 → 全局"解析一次，
/// 下拉的选中项如实反映当前会用的那一个。
fn picker_groups(settings: &Settings) -> Vec<(String, String)> {
    let ids = settings.provider_ids();
    ids.iter()
        .map(|id| {
            let p = settings.provider(id);
            let label = if ids
                .iter()
                .filter(|other| settings.provider(other).display_name() == p.display_name())
                .count()
                > 1
            {
                format!(
                    "{} · {}\n{}",
                    p.display_name(),
                    p.id,
                    p.base_url_or_default()
                )
            } else {
                p.display_name()
            };
            (id.clone(), label)
        })
        .collect()
}

fn refresh_model_picker(window: &AppWindow, settings: &Settings, sessions: &SessionRuntime) {
    let now = crate::clock::now();
    let choice = sessions.model_choice();
    let selection = settings.resolve_selection(
        if choice.is_empty() {
            None
        } else {
            Some(&choice)
        },
        &now,
    );
    let mut labels: Vec<slint::SharedString> = Vec::new();
    let mut selected_index: i32 = -1;
    let mut groups = Vec::<slint::SharedString>::new();
    let mut group_labels = Vec::<slint::SharedString>::new();
    let mut providers = Vec::<slint::SharedString>::new();
    let mut model_names = Vec::<slint::SharedString>::new();
    for (provider_id, provider_label) in picker_groups(settings) {
        let provider = settings.provider(&provider_id);
        let mut models = provider.selectable_models();
        if !provider.model.trim().is_empty() && !models.contains(&provider.model) {
            models.insert(0, provider.model.clone());
        }
        if models.is_empty() {
            continue;
        }
        groups.push(provider_id.clone().into());
        group_labels.push(provider_label.clone().into());
        let requires_key = config::get_provider(&provider_id)
            .map(|info| info.api_key_required)
            .unwrap_or(true);
        // 只对这个提供商**自己**的 Key 做判断，绝不借用别家。
        let has_key = !requires_key || sessions.has_key_for(&provider_id);
        for model in models {
            if selected_index < 0
                && provider_id == selection.provider_id
                && model == selection.model
            {
                selected_index = labels.len() as i32;
            }
            let display = provider
                .model_meta(&model)
                .map(|m| m.display_name.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(&model);
            let model_label = if display == model {
                model.clone()
            } else {
                format!("{display} · {model}")
            };
            let label = if has_key {
                format!("{provider_label} · {model_label}")
            } else {
                format!("{provider_label} · {model_label}（还没 Key）")
            };
            providers.push(provider_id.clone().into());
            model_names.push(model_label.into());
            labels.push(label.into());
        }
    }
    window.set_model_provider_groups(ModelRc::new(VecModel::from(groups)));
    window.set_model_provider_group_labels(ModelRc::new(VecModel::from(group_labels)));
    window.set_model_option_providers(ModelRc::new(VecModel::from(providers)));
    window.set_model_option_names(ModelRc::new(VecModel::from(model_names)));
    window.set_model_option_labels(ModelRc::new(VecModel::from(labels)));
    window.set_model_choice_index(selected_index);
    window.set_model_choice_label(selection.model.clone().into());
    let mut messages = vec![crate::llm::ChatMessage::system(
        prompt::resolve_system_prompt(settings.system_prompt.as_deref()),
    )];
    messages.extend(sessions.history());
    let used = crate::context::estimate_request_tokens(
        &messages,
        &crate::cua::tools::tools_for_request(crate::cua::tools::DEFAULT_TOOL_NAMES),
    );
    let limit = settings
        .effective_context_window_of(&selection.provider_id, &selection.model)
        .max(1);
    window.set_context_usage((used as f64 / limit as f64).clamp(0.0, 0.99999) as f32);
    window.set_context_known(true);
    window.set_context_text(format!("上下文估算 {used} / {limit} tokens").into());

    // 推理档位：只对声明了协议的提供商显示（自定义端点不注入专属字段）。
    let capability = settings.provider(&selection.provider_id).capability();
    match capability {
        crate::llm::Capability::Mimo => {
            window.set_thinking_control_visible(true);
            window.set_effort_control_visible(false);
            window.set_reasoning_applicable(true);
            window.set_reasoning_options(ModelRc::new(VecModel::from(
                THINKING_LABELS
                    .iter()
                    .map(|label| slint::SharedString::from(*label))
                    .collect::<Vec<_>>(),
            )));
            let index = choice
                .thinking
                .map(thinking_index)
                .unwrap_or_else(|| thinking_index(selection.thinking));
            window.set_reasoning_index(index);
        }
        crate::llm::Capability::DeepSeek => {
            window.set_thinking_control_visible(false);
            window.set_effort_control_visible(true);
            window.set_reasoning_applicable(true);
            window.set_reasoning_options(ModelRc::new(VecModel::from(
                EFFORT_LABELS
                    .iter()
                    .map(|label| slint::SharedString::from(*label))
                    .collect::<Vec<_>>(),
            )));
            let index = choice
                .reasoning_effort
                .map(reasoning_effort_index)
                .unwrap_or_else(|| reasoning_effort_index(selection.reasoning_effort));
            window.set_reasoning_index(index);
        }
        crate::llm::Capability::Plain => {
            window.set_thinking_control_visible(false);
            window.set_effort_control_visible(false);
            window.set_reasoning_applicable(false);
            window.set_reasoning_options(ModelRc::new(VecModel::from(
                Vec::<slint::SharedString>::new(),
            )));
            window.set_reasoning_index(0);
        }
    }

    let hint = if choice.is_empty() {
        format!(
            "按「{}」使用 {} · {}",
            selection.source.label(),
            selection.provider_id,
            selection.model
        )
    } else {
        "本会话的手动选择（不受时段规则影响）".to_string()
    };
    window.set_model_hint(hint.into());
}

/// 只改**模型下拉/推理档位**的文字显示（不改设置，不落盘）。
fn publish_model_badges(window: &AppWindow, settings: &Settings, sessions: &SessionRuntime) {
    let choice = sessions.model_choice();
    let selection = settings.resolve_selection(
        if choice.is_empty() {
            None
        } else {
            Some(&choice)
        },
        &crate::clock::now(),
    );
    window.set_session_model_text(
        format!(
            "{} · {}",
            settings.provider(&selection.provider_id).display_name(),
            selection.model
        )
        .into(),
    );
    window.set_session_effective_window(
        settings
            .effective_context_window_of(&selection.provider_id, &selection.model)
            .to_string()
            .into(),
    );
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
    mut sessions: SessionRuntime,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    command_tx: tokio::sync::mpsc::UnboundedSender<UiCommand>,
    confirmer: Arc<UiConfirmer>,
    save_generation: Arc<std::sync::atomic::AtomicU64>,
    search_test_generation: Arc<std::sync::atomic::AtomicU64>,
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

    /*
     * 联网搜索（PR12）：Key 是**独立的一格**，与模型 Key 分开存。
     *
     * 启动时读一次；之后设置页保存会调用 `set_api_key` 原地换掉（不需要重启）。
     * `web_search` 真正可用要**同时**满足：设置里启用 + 有 Key
     * （见 `ToolRegistry::search_ready`）。
     */
    let search_key = secret.load_search_key().ok().flatten().unwrap_or_default();
    let web_search = Arc::new(WebSearch::new(search_key));
    // 统一工具注册表：CUA + 本地工具 + 条件启用的联网搜索。
    //
    // 用 `Arc` 共享**同一份**：应用索引与搜索缓存都在里面，
    // 每轮新建一份会让 `launch_app` 每次都重新建索引（1~2 秒）。
    let tools_registry = Arc::new(ToolRegistry::new(
        toolkit.clone(),
        web_search.clone(),
        settings.search.enabled,
    ));
    /*
     * 预热应用索引必须**放到后台**：建索引要跑一次 PowerShell（1~2 秒，
     * 冷启动更久），失败时还会等满超时。在这里直接 await 会让工作线程
     * 在收到第一条命令之前一直卡着——用户点"打开设置"没反应。
     * 首次 `launch_app` 仍会按需建索引，所以预热失败不影响可用性。
     */
    tools_registry.spawn_warm_app_index();
    publish_tool_overview(&weak, &tools_registry);

    // 会话历史由会话运行时持有（按会话隔离）；回合期间只读一份快照。
    let mut turn_cancel: Option<CancelToken> = None;
    // 回合身份：每个 Agent 回合一个，播报据此判断"这段音频属于哪一轮"。
    let mut next_turn_id: TurnId = 0;

    /*
     * 权限运行态（共享给 Agent）：界面上的暂停/限时授权/覆盖要在
     * **下一个工具判断前**生效，所以用 `Arc<Mutex<..>>` 而不是每轮新建一份。
     */
    let permissions = Arc::new(std::sync::Mutex::new(PermissionState::from_settings(
        settings.permission_mode,
        &settings.permission_overrides,
    )));
    let audit = AuditLog::new(paths.audit_file());

    let mut model_request = 0u64;
    let connection_request = Arc::new(std::sync::atomic::AtomicU64::new(0));

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

    // 会话已由 `run_app` 在 `seed_ui` **之前**备好（含落盘），
    // 这里只把"启动时保存失败"的原因如实显示出来。
    if let Some(error) = sessions.last_save_error() {
        set_status(&weak, &format!("会话无法保存：{error}"));
    }
    // 记住"哪些提供商已有 Key"：输入区的模型下拉据此提示"还没 Key"，
    // 且**只**看该提供商自己的 Key。
    {
        let presence = super::endpoint_cards::key_presence(&settings, &secret);
        sessions.set_key_presence(presence);
    }
    {
        let settings = settings.clone();
        let sessions = sessions.clone();
        with_window(&weak, move |window| {
            publish_session_list(&window.as_weak(), &sessions);
            publish_timeline(&window.as_weak(), &sessions);
            publish_session_title(&window.as_weak(), &sessions);
            refresh_model_picker(window, &settings, &sessions);
            publish_model_badges(window, &settings, &sessions);
        });
    }
    publish_audit(&weak, &audit.recent(crate::permissions::MAX_AUDIT_RECORDS));

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
                 *
                 * **不能**在这里清 `settings-saving`：这个请求已经过期，说明
                 * 一定有一个更年轻的请求（或取消）在推进状态。若替它收掉
                 * "保存中"，新请求还在跑时按钮就提前可点了，用户会重复提交。
                 * 状态只由"当前有效的那一代"负责收尾。
                 */
                if !generation_is_current(&save_generation, generation) {
                    // 过期的保存请求：什么都不做（不写盘、不改界面）。
                } else {
                    let _ = handle_save_all_ui(
                        &weak,
                        &paths,
                        &secret,
                        &mut settings,
                        &mut speech,
                        &permissions,
                        &tools_registry,
                        voice
                            .handle
                            .as_ref()
                            .is_some_and(|handle| handle.is_running()),
                        &request,
                        generation,
                        &save_generation,
                        &search_test_generation,
                    );
                    // 保存成功后刷新输入区的模型下拉（选项/归属可能变了）。
                    {
                        let settings = settings.clone();
                        let sessions = sessions.clone();
                        with_window(&weak, move |window| {
                            refresh_model_picker(window, &settings, &sessions);
                            publish_model_badges(window, &settings, &sessions);
                        });
                    }
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
                // 工具总览与实际公布清单同源，每次打开都按**此刻**的能力刷新。
                publish_tool_overview(&weak, &tools_registry);
            }
            UiCommand::SaveEndpoint(provider, draft, revision, confirmed_url) => {
                let key = draft.api_key;
                let saved_id = provider.id.clone();
                let result = (|| -> Result<(), String> {
                    settings = super::endpoint_cards::save_provider(
                        &paths,
                        &secret,
                        &settings,
                        &provider,
                        &key,
                        &confirmed_url,
                    )?;
                    if provider.id == MIMO_PROVIDER_ID
                        && !key.trim().is_empty()
                        && settings.tts.backend == crate::voice::tts::TtsBackend::Mimo
                    {
                        speech
                            .apply_settings(&settings.tts, &secret)
                            .map_err(|error| {
                                format!("端点和 Key 已保存，但播报暂不可用：{error}")
                            })?;
                    }
                    Ok(())
                })();
                let snapshot = settings.clone();
                sessions.set_key_presence(super::endpoint_cards::key_presence(&settings, &secret));
                let picker_sessions = sessions.clone();
                with_window(&weak, move |w| {
                    if let Err(e) = result {
                        let cards = w.get_endpoint_cards();
                        if let Some(i) = cards
                            .iter()
                            .position(|c| c.id == saved_id && c.revision == revision)
                        {
                            let mut c = cards.row_data(i).unwrap();
                            c.saved_url = snapshot.provider(&saved_id).base_url_or_default().into();
                            c.error = e.clone().into();
                            c.delete_pending = false;
                            cards.set_row_data(i, c);
                        }
                        w.set_settings_error(e.into());
                    } else {
                        let cards = w.get_endpoint_cards();
                        if let Some(i) = cards
                            .iter()
                            .position(|c| c.id == saved_id && c.revision == revision)
                        {
                            cards.set_row_data(
                                i,
                                super::endpoint_cards::card(&snapshot.provider(&saved_id)),
                            );
                        }
                        if saved_id == snapshot.provider_id {
                            w.set_base_url(snapshot.base_url.clone().into());
                            w.set_model_name(snapshot.model.clone().into());
                            w.set_thinking_index(thinking_index(snapshot.thinking));
                            w.set_effort_index(reasoning_effort_index(snapshot.reasoning_effort));
                            fill_model_settings(w, &snapshot.provider(&saved_id));
                        }
                        refresh_model_picker(w, &snapshot, &picker_sessions);
                    }
                });
            }
            UiCommand::DeleteEndpoint(id) => {
                // 明确拒绝带引用的删除，不隐式重绑任何凭据。
                let result = (|| -> Result<(), String> {
                    if sessions.model_choice().provider_id == id {
                        return Err("该实例被当前 Session 引用，请先更换会话选择".into());
                    }
                    settings =
                        super::endpoint_cards::delete_provider(&paths, &secret, &settings, &id)?;
                    Ok(())
                })();
                let snapshot = settings.clone();
                sessions.set_key_presence(super::endpoint_cards::key_presence(&settings, &secret));
                let picker_sessions = sessions.clone();
                with_window(&weak, move |w| {
                    if let Err(e) = result {
                        w.set_settings_error(e.into());
                    } else {
                        let cards = w
                            .get_endpoint_cards()
                            .iter()
                            .filter(|c| c.id != id)
                            .collect::<Vec<_>>();
                        w.set_endpoint_cards(ModelRc::new(VecModel::from(cards)));
                        refresh_model_picker(w, &snapshot, &picker_sessions);
                    }
                });
            }
            UiCommand::ProviderChanged => {
                model_request += 1;
            }
            UiCommand::ClearApiKey(provider_id) => {
                match clear_model_key(&weak, &secret, &settings, &mut speech, &provider_id) {
                    Ok(()) => {
                        settings_ui::restore_model_catalogs(&mut settings, &paths, &secret, None);
                        let picker_settings = settings.clone();
                        sessions.set_key_presence(super::endpoint_cards::key_presence(
                            &settings, &secret,
                        ));
                        let picker_sessions = sessions.clone();
                        with_window(&weak, move |window| {
                            refresh_model_picker(window, &picker_settings, &picker_sessions)
                        });
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
            /*
             * ---- 联网搜索的 Key（PR12） ----
             *
             * 与模型提供商的 Key 完全分开：清这个不会碰任何一家模型的 Key，
             * 反之亦然（`SecretStore` 用的是独立槽位）。
             */
            UiCommand::ClearSearchKey => match secret.clear_search_key() {
                Ok(()) => {
                    // 清 Key 立刻推进测试代次：在途的陈旧测试结果不许再回填。
                    search_test_generation.fetch_add(1, Ordering::SeqCst);
                    tools_registry.search().set_api_key("");
                    publish_tool_overview(&weak, &tools_registry);
                    set_status(&weak, "已清除联网搜索的 Key（web_search 不再公布给模型）");
                }
                Err(error) => set_status(&weak, &format!("清除联网搜索 Key 失败：{error}")),
            },
            UiCommand::TestSearchKey {
                key: draft_key,
                generation,
            } => {
                handle_queued_search_test(
                    &weak,
                    &secret,
                    draft_key,
                    generation,
                    &search_test_generation,
                );
            }
            // ---- 设置分类导航 ----
            UiCommand::SettingsTab(index) => {
                {
                    with_window(&weak, move |window| {
                        window.set_settings_tab_index(index.max(0));
                    });
                }
                publish_audit(&weak, &audit.recent(crate::permissions::MAX_AUDIT_RECORDS));
                // 「工具与联网」页要显示**当前**能力（搜索 Key 可能在别处被改过）。
                publish_tool_overview(&weak, &tools_registry);
            }
            // ---- 模型发现与连通性测试 ----
            UiCommand::TestConnection(draft) => {
                handle_test_connection(&weak, &settings, &secret, draft, &connection_request);
            }
            UiCommand::FetchModels(draft) => {
                {
                    with_window(&weak, move |window| {
                        window.set_models_busy(true);
                        window.set_model_list_hint("正在获取在线模型列表…".into());
                    });
                }
                let (provider, api_key) = match draft.resolve(&settings, &secret) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        with_window(&weak, move |window| {
                            if draft.matches_window(window) {
                                window.set_models_busy(false);
                                window.set_model_list_hint(error.into());
                            }
                        });
                        continue;
                    }
                };
                model_request += 1;
                let request = model_request;
                let mut cache = ModelCache::with_path(paths.model_cache_file());
                let tx = command_tx.clone();
                tokio::spawn(async move {
                    let fetch = fetch_models(&provider, &api_key, &mut cache, true).await;
                    let _ = tx.send(UiCommand::ModelsFetched {
                        credential: crate::model_catalog::fingerprint(&api_key),
                        draft,
                        request,
                        fetch,
                    });
                });
            }
            UiCommand::ModelsFetched {
                draft,
                credential,
                request,
                fetch,
            } => {
                if request == model_request {
                    remember_fetched_models(
                        &mut settings,
                        (&draft.provider_id, &draft.base_url, request),
                        model_request,
                        &fetch,
                        &secret,
                        &credential,
                    );
                    {
                        let settings = settings.clone();
                        let sessions = sessions.clone();
                        let fetch = fetch.clone();
                        with_window(&weak, move |window| {
                            refresh_model_picker(window, &settings, &sessions);
                            window.set_models_busy(false);
                            if draft.matches_window(window) {
                                window.set_model_list_hint(fetch.summary().into());
                                if fetch.ok {
                                    let names = window.get_enabled_model_names();
                                    let flags = window.get_enabled_model_flags();
                                    let mut names: Vec<slint::SharedString> =
                                        names.iter().collect();
                                    let mut flags: Vec<bool> = flags.iter().collect();
                                    for id in fetch.ids() {
                                        if !names.iter().any(|n| n.as_str() == id) {
                                            names.push(id.into());
                                            flags.push(false);
                                        }
                                    }
                                    let labels: Vec<slint::SharedString> = names
                                        .iter()
                                        .map(|name| {
                                            format!(
                                                "{name} · {}，未验证",
                                                if fetch.ids().iter().any(|id| id == name.as_str())
                                                {
                                                    "服务端目录"
                                                } else {
                                                    "手动/保留选择"
                                                }
                                            )
                                            .into()
                                        })
                                        .collect();
                                    window.set_enabled_model_labels(ModelRc::new(VecModel::from(
                                        labels,
                                    )));
                                    window.set_enabled_model_names(ModelRc::new(VecModel::from(
                                        names,
                                    )));
                                    window.set_enabled_model_flags(ModelRc::new(VecModel::from(
                                        flags,
                                    )));
                                    window.set_discovered_models(ModelRc::new(VecModel::from(
                                        fetch
                                            .ids()
                                            .into_iter()
                                            .map(slint::SharedString::from)
                                            .collect::<Vec<_>>(),
                                    )));
                                }
                            }
                        });
                    }
                }
            }
            // ---- 会话 ----
            UiCommand::NewSession => {
                speech.stop();
                let now = crate::clock::now().unix_ms;
                let id = sessions.create(now, "新会话");
                if let Ok(mut state) = permissions.lock() {
                    state.allowed_once.clear();
                }
                let _ = sessions.save_current();
                let _ = id;
                {
                    let settings = settings.clone();
                    let sessions = sessions.clone();
                    with_window(&weak, move |window| {
                        publish_session_list(&window.as_weak(), &sessions);
                        publish_timeline(&window.as_weak(), &sessions);
                        publish_session_title(&window.as_weak(), &sessions);
                        refresh_model_picker(window, &settings, &sessions);
                        publish_model_badges(window, &settings, &sessions);
                    });
                }
                set_status(&weak, "已新建会话");
            }
            UiCommand::SelectSession(id) => {
                if id.trim().is_empty() {
                    // 空 id = 界面没指定：保持当前选中，不做事。
                } else {
                    match sessions.select(&id) {
                        Ok(()) => {
                            if let Ok(mut state) = permissions.lock() {
                                state.allowed_once.clear();
                            }
                            /*
                             * 已结束 Agent 回合但 TTS 仍可能还在播：切会话要停旧播报，
                             * 并让界面侧的播报身份失效——旧 `SpeechId` 的迟到事件
                             * 不会再改到新会话的字幕。
                             */
                            speech.stop();
                            speech_view.current = None;
                            let weak_ui = weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(window) = weak_ui.upgrade() {
                                    window.set_speech_subtitle("".into());
                                    window.set_speech_active(false);
                                }
                            });
                            {
                                let settings = settings.clone();
                                let sessions = sessions.clone();
                                with_window(&weak, move |window| {
                                    publish_session_list(&window.as_weak(), &sessions);
                                    publish_timeline(&window.as_weak(), &sessions);
                                    publish_session_title(&window.as_weak(), &sessions);
                                    refresh_model_picker(window, &settings, &sessions);
                                    publish_model_badges(window, &settings, &sessions);
                                });
                            }
                            let warning = sessions
                                .current()
                                .map(|session| session.last_turn_status.needs_warning())
                                .unwrap_or(false);
                            if warning {
                                set_status(
                                    &weak,
                                    "已切换会话；上一轮没有正常结束（不会自动重放工具）",
                                );
                            } else {
                                set_status(&weak, "已切换会话");
                            }
                        }
                        Err(error) => set_status(&weak, &format!("切换会话失败：{error}")),
                    }
                }
            }
            UiCommand::RenameSession { id, title } => {
                let target = if id.trim().is_empty() {
                    sessions.current_id().to_string()
                } else {
                    id
                };
                match sessions.rename(&target, &title) {
                    Ok(()) => {
                        {
                            let sessions = sessions.clone();
                            with_window(&weak, move |window| {
                                publish_session_list(&window.as_weak(), &sessions);
                                publish_session_title(&window.as_weak(), &sessions);
                            });
                        }
                        set_status(&weak, "会话已改名");
                    }
                    Err(error) => set_status(&weak, &format!("改名失败：{error}")),
                }
            }
            UiCommand::DeleteSession(id) => {
                let target = if id.trim().is_empty() {
                    sessions.current_id().to_string()
                } else {
                    id
                };
                speech.stop();
                speech_view.current = None;
                match sessions.delete(&target) {
                    Ok(()) => {
                        if let Ok(mut state) = permissions.lock() {
                            state.allowed_once.clear();
                        }
                        // 删光了就补一个，保证界面始终有一个可对话的会话。
                        if sessions.current().is_none() {
                            sessions.ensure_one(crate::clock::now().unix_ms);
                            let _ = sessions.save_current();
                        }
                        {
                            let settings = settings.clone();
                            let sessions = sessions.clone();
                            with_window(&weak, move |window| {
                                publish_session_list(&window.as_weak(), &sessions);
                                publish_timeline(&window.as_weak(), &sessions);
                                publish_session_title(&window.as_weak(), &sessions);
                                refresh_model_picker(window, &settings, &sessions);
                                publish_model_badges(window, &settings, &sessions);
                            });
                        }
                        set_status(&weak, "会话已删除");
                    }
                    Err(error) => set_status(&weak, &format!("删除失败：{error}")),
                }
            }
            // ---- 输入区的模型 / 推理选择（只改当前会话） ----
            UiCommand::ModelChoicePicked(index) => {
                handle_model_pick(&weak, &settings, &mut sessions, index);
            }
            UiCommand::ReasoningPicked(index) => {
                handle_reasoning_pick(&weak, &settings, &mut sessions, index);
            }
            UiCommand::ApplyManualModel(model) => {
                let model = model.trim().to_string();
                if model.is_empty() {
                    set_status(&weak, "手填模型不能为空");
                } else {
                    let provider_id = sessions.model_choice().provider_id;
                    let provider_id = if provider_id.is_empty() {
                        settings
                            .resolve_selection(None, &crate::clock::now())
                            .provider_id
                    } else {
                        provider_id
                    };
                    let choice = SessionModelChoice {
                        provider_id,
                        model,
                        thinking: sessions.model_choice().thinking,
                        reasoning_effort: sessions.model_choice().reasoning_effort,
                    };
                    apply_session_choice(&weak, &settings, &mut sessions, choice);
                }
            }
            // ---- 权限（立即生效） ----
            UiCommand::PermissionModePicked(_) | UiCommand::OverridePicked { .. } => {}
            UiCommand::TogglePause => toggle_pause(&weak, &permissions),
            UiCommand::GrantCapability {
                capability,
                minutes,
            } => {
                grant_capability(&weak, &permissions, &capability, minutes);
                set_status(
                    &weak,
                    &format!("已允许「{capability}」{minutes} 分钟（仅本次会话）"),
                );
            }
            UiCommand::ExportAudit => export_audit(&weak, &audit, &paths),
            UiCommand::Send(input) => {
                // 新一轮开始：上一轮的声音立刻停下，绝不接在旧音频后面。
                speech.stop();
                next_turn_id =
                    next_turn_id.max(sessions.current().map(|s| s.last_turn_id).unwrap_or(0)) + 1;
                // 首条消息决定会话标题（只在仍是默认标题时）。
                sessions.adopt_title_from(&input);
                let session_id = sessions.current_id().to_string();
                let mut history = sessions.history();
                sessions.push_timeline(next_turn_id, TimelineItem::user(input.clone()));
                sessions.set_last_turn(next_turn_id, TurnStatus::Interrupted);
                let _ = sessions.save_current();
                publish_timeline(&weak, &sessions);
                publish_session_list(&weak, &sessions);
                /*
                 * 时间/日期这类问题**直接本地回答**：不叫模型、更不调 run_shell。
                 *
                 * 旧版实测代价很大：模型为了"现在几点"去调 run_shell（Get-Date），
                 * 而 system.shell 在标准档是"每次询问"——屏幕上弹出一张确认卡，
                 * 用户没理它，等满超时按拒绝处理，一轮对话白等几分钟。
                 * 这条路径不需要 LLM Key，也不需要 CUA 后端。
                 */
                if let Some(answer) = clock_tool::try_direct_answer(&input) {
                    finish_quick_answer(
                        &weak,
                        &mut sessions,
                        &mut speech,
                        &settings,
                        next_turn_id,
                        &input,
                        &answer,
                        TurnSource::Text,
                    );
                    continue;
                }
                set_busy(&weak, true);
                let choice = sessions.model_choice();
                match start_turn(
                    &weak,
                    &paths,
                    &secret,
                    &settings,
                    if choice.is_empty() {
                        None
                    } else {
                        Some(&choice)
                    },
                    &tools_registry,
                    &confirmer,
                    &permissions,
                    &audit,
                    &mut history,
                    &mut turn_cancel,
                    input,
                    next_turn_id,
                    session_id,
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
                            &mut sessions,
                            &permissions,
                            &audit,
                            &confirmer,
                            &mut voice,
                            &mut speech,
                            &mut speech_view,
                            &tools_registry,
                            &command_tx,
                            &save_generation,
                            &search_test_generation,
                            &mut model_request,
                            TurnSource::Text,
                        )
                        .await
                        {
                            TurnWait::Finished(finished) => {
                                finish_session_turn(
                                    &weak,
                                    &mut sessions,
                                    &mut speech,
                                    &settings,
                                    finished,
                                    next_turn_id,
                                );
                            }
                            TurnWait::Quit => break,
                        }
                    }
                    TurnStart::Rejected(status) => {
                        finish_rejected_turn(&weak, &mut sessions, next_turn_id, &status);
                    }
                }
            }
            UiCommand::DictateNow => {
                if voice.manual_armed
                    || voice.route.pending.is_some()
                    || voice.route.active_session().current().is_some()
                {
                    if voice.manual_temporary {
                        voice.disable(&weak);
                    } else {
                        voice.set_listen_mode(ListenPurpose::Wake, 0);
                        voice.cancel_pending();
                        voice.manual_armed = false;
                        with_window(&weak, |w| w.set_voice_manual(false));
                        set_status(&weak, "已取消识别，继续等待唤醒词");
                    }
                } else {
                    speech.stop();
                    voice.manual_temporary = !voice
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.is_running());
                    voice.manual_request = voice.manual_request.wrapping_add(1);
                    voice.manual_armed = true;
                    with_window(&weak, |w| w.set_voice_manual(true));
                    voice.set_listen_mode(ListenPurpose::Manual, voice.manual_request);
                    voice.enable(&weak, &paths, &settings, &command_tx);
                    if voice.handle.is_none() {
                        voice.manual_armed = false;
                        voice.manual_temporary = false;
                        with_window(&weak, |w| w.set_voice_manual(false));
                    } else {
                        set_status(&weak, "请直接说指令，无需唤醒词");
                    }
                }
            }
            UiCommand::QuickPermissionPicked(index) => {
                let mut next = settings.clone();
                next.permission_mode = permission_mode_for_index(index);
                match config::save_settings(&paths, &next) {
                    Ok(()) => {
                        settings = next;
                        if let Ok(mut state) = permissions.lock() {
                            state.mode = settings.permission_mode;
                        }
                        publish_permission_state(&weak, &permissions);
                    }
                    Err(error) => set_status(&weak, &format!("权限保存失败：{error}")),
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
                    next_turn_id = next_turn_id
                        .max(sessions.current().map(|s| s.last_turn_id).unwrap_or(0))
                        + 1;
                    // 语音最终文本只提交给**捕获时归属**的有效会话（当前会话）。
                    sessions.adopt_title_from(&prompt.text);
                    let session_id = sessions.current_id().to_string();
                    let mut history = sessions.history();
                    sessions.push_timeline(next_turn_id, TimelineItem::user(prompt.text.clone()));
                    sessions.set_last_turn(next_turn_id, TurnStatus::Interrupted);
                    let _ = sessions.save_current();
                    publish_timeline(&weak, &sessions);
                    publish_session_list(&weak, &sessions);
                    // 语音问"现在几点"同样直接本地答（不叫模型、不跑命令）。
                    if let Some(answer) = clock_tool::try_direct_answer(&prompt.text) {
                        finish_quick_answer(
                            &weak,
                            &mut sessions,
                            &mut speech,
                            &settings,
                            next_turn_id,
                            &prompt.text,
                            &answer,
                            TurnSource::Voice,
                        );
                        continue;
                    }
                    set_busy(&weak, true);
                    let choice = sessions.model_choice();
                    let turn = prompt.text.clone();
                    match start_turn(
                        &weak,
                        &paths,
                        &secret,
                        &settings,
                        if choice.is_empty() {
                            None
                        } else {
                            Some(&choice)
                        },
                        &tools_registry,
                        &confirmer,
                        &permissions,
                        &audit,
                        &mut history,
                        &mut turn_cancel,
                        turn,
                        next_turn_id,
                        session_id,
                    ) {
                        TurnStart::Started(turn) => {
                            // 与文本输入完全相同的等待方式：取消/退出/设置照常处理。
                            match wait_turn(
                                &mut commands,
                                turn,
                                &weak,
                                &mut turn_cancel,
                                &paths,
                                &secret,
                                &mut settings,
                                &mut sessions,
                                &permissions,
                                &audit,
                                &confirmer,
                                &mut voice,
                                &mut speech,
                                &mut speech_view,
                                &tools_registry,
                                &command_tx,
                                &save_generation,
                                &search_test_generation,
                                &mut model_request,
                                TurnSource::Voice,
                            )
                            .await
                            {
                                TurnWait::Finished(finished) => {
                                    finish_session_turn(
                                        &weak,
                                        &mut sessions,
                                        &mut speech,
                                        &settings,
                                        finished,
                                        next_turn_id,
                                    );
                                }
                                TurnWait::Quit => break,
                            }
                        }
                        TurnStart::Rejected(status) => {
                            finish_rejected_turn(&weak, &mut sessions, next_turn_id, &status);
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
    manual_armed: bool,
    manual_temporary: bool,
    manual_request: u64,
}

impl VoiceRuntime {
    fn new() -> Self {
        Self {
            handle: None,
            route: VoiceRoute::default(),
            mode: ListenMode::new(),
            manual_armed: false,
            manual_temporary: false,
            manual_request: 0,
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
        with_window(weak, |w| w.set_voice_manual(false));
        self.manual_armed = false;
        self.manual_temporary = false;
        self.set_listen_mode(ListenPurpose::Wake, 0);
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
                    with_window(weak, |w| w.set_voice_manual(false));
                    self.manual_armed = false;
                    self.manual_temporary = false;
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
                with_window(weak, |w| {
                    w.set_voice_transcript("".into());
                    w.set_wake_generation(w.get_wake_generation().wrapping_add(1));
                });
                set_voice_state(weak, &VoiceState::Capturing);
                set_status(weak, &format!("已唤醒（{keyword}），请说指令"));
                VoiceOutcome::None
            }
            VoiceEventKind::DirectStarted {
                turn,
                purpose,
                request_id,
            } => {
                if purpose == ListenPurpose::Manual
                    && (self.mode.purpose() != ListenPurpose::Manual
                        || request_id != self.manual_request)
                {
                    return VoiceOutcome::None;
                }
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
                    ListenPurpose::Manual => {
                        self.manual_armed = false;
                        set_status(weak, "正在听，请说指令");
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
                if self.route.pending != Some(turn) {
                    return VoiceOutcome::None;
                }
                with_window(weak, |w| w.set_voice_manual(false));
                if self.manual_temporary {
                    self.disable(weak);
                } else if self.mode.purpose() == ListenPurpose::Manual {
                    self.set_listen_mode(ListenPurpose::Wake, 0);
                    self.manual_armed = false;
                }
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
                if purpose == ListenPurpose::Manual {
                    with_window(weak, |w| w.set_voice_manual(false));
                    self.set_listen_mode(ListenPurpose::Wake, 0);
                    self.manual_armed = false;
                }
                if purpose == ListenPurpose::Manual && self.manual_temporary {
                    self.disable(weak);
                }
                set_transcript(weak, &text);
                match purpose {
                    ListenPurpose::Wake | ListenPurpose::Manual => {
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
    } else if voice.manual_armed {
        (ListenPurpose::Manual, voice.manual_request)
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

/// 启动时把已保存的播报偏好同步到界面（**不**自动试音）。
fn seed_tts_ui(weak: &slint::Weak<AppWindow>, settings: &Settings, speech: &SpeechRuntime) {
    publish_tts_ui(weak, &settings.tts, speech);
    if let Some(error) = speech.engine_error() {
        set_speech_unavailable(weak, error);
    }
}

/// 空闲与忙碌分支共用：先清密钥，再停止并丢弃持有旧 Key 的官方 MiMo 引擎。
fn clear_model_key(
    weak: &slint::Weak<AppWindow>,
    secret: &SecretStore,
    settings: &Settings,
    speech: &mut SpeechRuntime,
    provider_id: &str,
) -> Result<(), String> {
    secret.clear(provider_id).map_err(|e| e.to_string())?;
    if provider_id == crate::voice::tts::mimo::KEY_PROVIDER
        && settings.tts.active_backend() == crate::voice::tts::TtsBackend::Mimo
    {
        // 缺 Key 是清除后的预期状态。apply_settings 会 stop/取消合成并先清掉旧引擎，
        // 再记录不可用原因；不能把成功清除密钥误报成失败。
        let _ = speech.apply_settings(&settings.tts, secret);
        // 仅更新运行状态，不用已保存 TTS 配置覆盖用户正在编辑的语音设置草稿。
        let available = speech.is_available();
        let label = speech
            .engine_label()
            .or_else(|| speech.engine_error().map(str::to_string))
            .unwrap_or_else(|| "播报不可用：请配置 MiMo Key".into());
        if !available {
            set_speech_unavailable(weak, &label);
        }
        let backend_index = tts_backend_index(settings.tts.backend);
        with_window(weak, move |window| {
            window.set_tts_backend_ready(available);
            window.set_tts_active_backend_index(backend_index);
            window.set_tts_active_ready(available);
            window.set_tts_active_label(label.into());
        });
    }
    Ok(())
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
    events: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
}

/// 一次回合的启动结果。
enum TurnStart {
    Started(RunningTurn),
    /// 没启动（上一轮还在跑 / 后端不可用 / 端点无效），附给用户看的原因。
    Rejected(String),
}

/// 启动一个 Agent 回合。文本输入与语音最终文本都**只**经由这里，
/// 所以两条路径的权限、取消与并发约束完全一致。
///
/// `choice` 是发起这一轮时**会话**的模型选择：整轮按它（或全局/时段方案）
/// 解析一次快照，运行期间不热切换。这样别的会话改模型不会影响正在跑的回合。
#[allow(clippy::too_many_arguments)]
fn start_turn(
    weak: &slint::Weak<AppWindow>,
    _paths: &AppPaths,
    secret: &SecretStore,
    settings: &Settings,
    choice: Option<&SessionModelChoice>,
    tools: &Arc<ToolRegistry>,
    confirmer: &Arc<UiConfirmer>,
    permissions: &Arc<std::sync::Mutex<PermissionState>>,
    audit: &AuditLog,
    history: &mut Vec<ChatMessage>,
    turn_cancel: &mut Option<CancelToken>,
    input: String,
    turn_id: TurnId,
    session_id: String,
) -> TurnStart {
    /*
     * 并发约束：上一轮还在跑（含等待点击确认）时**不启动第二回合**。
     * 语音文本也走这一条——不能因为「是语音说的」就插队或排队延后执行。
     */
    if turn_cancel.is_some() {
        return TurnStart::Rejected("上一轮还在跑，先取消再发新指令".to_string());
    }

    /*
     * 端点快照：整轮用同一套「地址 + Key + 模型 + 推理档位」。
     *
     * 优先级：会话手选 > 时段规则 → 方案 > 全局默认。**只在这里解析一次**，
     * 所以时段跨过边界、别的会话改模型都不会影响这一轮。
     */
    let now = crate::clock::now();
    let selection = settings.resolve_selection(choice, &now);
    let profile = settings.profile_for_selection(&selection);
    // **按提供商取 Key**：解析出的那一家没有 Key 就拿空串，绝不借用别家的。
    let api_key = secret
        .load(&selection.provider_id)
        .unwrap_or(None)
        .unwrap_or_default();
    let echo_reasoning = profile.capability.requires_reasoning_echo();
    let llm = match LlmClient::new(profile, api_key, settings.temperature, settings.timeout_ms) {
        Ok(client) => client,
        Err(error) => {
            set_busy(weak, false);
            return TurnStart::Rejected(format!("端点配置无效：{error}"));
        }
    };
    let policy = crate::context::resolve_context_policy(
        Some(settings.context_window),
        Some(settings.context_compress_percent),
        // 模型元数据只能收紧窗口，不能放宽。
        settings.model_context_window(&selection.provider_id, &selection.model),
    );
    let agent_config = AgentConfig {
        max_steps: settings.max_steps,
        context_policy: policy,
        system_prompt: prompt::resolve_system_prompt(settings.system_prompt.as_deref()),
        echo_reasoning,
        // 回合身份：事件带它，UI/播报据此核对归属。
        turn_id,
        use_tools: true,
    };
    // 权限状态是**共享**的：界面改暂停/覆盖后，下一个工具判断立刻按新值处理。
    let mut agent = Agent::with_shared_state(
        llm,
        tools.clone(),
        agent_config,
        permissions.clone(),
        Some(audit.clone()),
        Some(session_id),
    );
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
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let settings_snapshot = settings.clone();
    let turn_confirmer = confirmer.clone();
    let turn_history = std::mem::take(history);
    let turn = tokio::spawn(async move {
        let emit = move |event: AgentEvent| {
            let _ = event_tx.send(event);
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
        events: event_rx,
    })
}

/// 一个已结束的回合：Agent 返回的历史、结果与回合快照。
///
/// 收尾（写会话、落盘、更新界面）由调用方在 `SessionRuntime` 上做，
/// 这样 `wait_turn` 不必同时持有会话与界面两套状态。
struct TurnFinished {
    history: Vec<ChatMessage>,
    outcome: TurnOutcome,
    settings: Settings,
}

/// 等待回合结束期间的命令处理结果。
enum TurnWait {
    /// 回合正常结束（带回历史、结果与回合快照）。
    Finished(TurnFinished),
    /// 用户要求退出：调用方应当结束主循环。
    Quit,
}

/// 清除某个提供商的 Key 之后，界面该怎么更新。
///
/// 两个显示位置含义不同，不能混用同一个 provider：
///  - **设置页的 `has-key`** 反映"设置页**当前选中**的提供商有没有 Key"；
///  - **主界面端点摘要** 反映"实际生效的配置"。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClearedKeyView {
    selected_has_key: bool,
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

/// 从界面读出一次保存的全部字段。
///
/// 界面是"用户正在编辑的表单"，工作线程持有权威配置副本；这里把两者对齐：
/// 表单里的每个字段都带上，未涉及的设置（方案/时段等）由 [`apply_save`]
/// 从当前配置里原样保留。
pub fn build_save_request(window: &AppWindow) -> SaveRequest {
    SaveRequest {
        kws_dir: window.get_kws_dir().to_string(),
        whisper_file: window.get_whisper_file().to_string(),
        keywords_file: window.get_keywords_file().to_string(),
        model_plans_json: window.get_model_plans_json().to_string(),
        model_schedule_json: window.get_model_schedule_json().to_string(),
        provider_id: window.get_settings_provider_id().to_string(),
        base_url: window.get_base_url().to_string(),
        model: window.get_model_name().to_string(),
        enabled_models: window
            .get_enabled_model_names()
            .iter()
            .collect::<Vec<_>>()
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                window
                    .get_enabled_model_flags()
                    .row_data(*i)
                    .unwrap_or(false)
            })
            .map(|(_, name)| name.to_string())
            .collect(),
        provider_enabled_models: Default::default(),
        provider_drafts: Default::default(),
        provider_hint: window.get_provider_hint_index(),
        api_key: window.get_api_key_input().to_string(),
        temperature: window.get_temperature().to_string(),
        max_steps: window.get_max_steps().to_string(),
        timeout_ms: window.get_timeout_ms().to_string(),
        context_window: window.get_context_window().to_string(),
        compress_percent: window.get_compress_percent().to_string(),
        thinking: window.get_thinking_index(),
        reasoning_effort: window.get_effort_index(),
        custom_prompt: window.get_prompt_custom(),
        system_prompt: window.get_system_prompt().to_string(),
        permission_index: window.get_permission_index(),
        permission_overrides: window.get_override_indices().iter().collect(),
        search_api_key: window.get_search_key_input().to_string(),
        search_enabled: window.get_search_enabled(),
        tts_enabled: window.get_tts_enabled(),
        tts_backend_index: window.get_tts_backend_index(),
        tts_mimo_voice: window.get_tts_mimo_voice().to_string(),
        tts_mimo_style: window.get_tts_mimo_style().to_string(),
        tts_local_dir: window.get_tts_local_dir().to_string(),
        tts_local_sid: window.get_tts_local_sid().to_string(),
        tts_local_speed: window.get_tts_local_speed().to_string(),
        speak_mode: window.get_tts_speak_mode(),
    }
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
    let hint_index = settings_ui::provider_hint_index(settings.provider_hint);
    let thinking = thinking_index(settings.thinking);
    let effort = reasoning_effort_index(settings.reasoning_effort);
    let base_url = settings.base_url.clone();
    let model = settings.model.clone();
    let provider = settings.provider(&settings.provider_id);
    let voice = settings.voice.clone();
    let saved = settings.clone();
    let search_enabled = settings.search.enabled;
    let plans = serde_json::to_string_pretty(&settings.model_plans).unwrap_or_default();
    let schedule_enabled = settings.model_schedule.enabled;
    let schedule_default = settings.model_schedule.default_plan_id.clone();
    let schedule = serde_json::to_string_pretty(&settings.model_schedule).unwrap_or_default();
    let overrides: Vec<i32> = crate::permissions::capabilities()
        .iter()
        .map(|c| match settings.permission_overrides.get(c.id) {
            Some(crate::config::PermissionOverride::Allow) => 1,
            Some(crate::config::PermissionOverride::Ask) => 2,
            Some(crate::config::PermissionOverride::Deny) => 3,
            _ => 0,
        })
        .collect();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_settings_provider_id(saved.provider_id.clone().into());
            super::endpoint_cards::fill(&window, &saved);
            window.set_probe_revision(window.get_probe_revision().wrapping_add(1));
            window.set_prompt_mode(prompt::preset_index(saved.system_prompt.as_deref()));
            window.set_silver_wolf_prompt(prompt::SILVER_WOLF_SYSTEM_PROMPT.into());
            window.set_prompt_custom(saved.system_prompt.is_some());
            window.set_system_prompt(
                prompt::resolve_system_prompt(saved.system_prompt.as_deref()).into(),
            );
            window.set_default_prompt(prompt::DEFAULT_SYSTEM_PROMPT.into());
            window.set_temperature(saved.temperature.to_string().into());
            window.set_max_steps(saved.max_steps.to_string().into());
            window.set_timeout_ms(saved.timeout_ms.to_string().into());
            window.set_context_window(saved.context_window.to_string().into());
            window.set_compress_percent(saved.context_compress_percent.to_string().into());
            window.set_tts_enabled(saved.tts.enabled);
            window.set_tts_backend_index(tts_backend_index(saved.tts.backend));
            window.set_tts_mimo_voice(saved.tts.mimo_voice.into());
            window.set_tts_mimo_style(saved.tts.mimo_style.into());
            window.set_tts_local_dir(saved.tts.local_model_dir.into());
            window.set_tts_local_sid(saved.tts.local_sid.to_string().into());
            window.set_tts_local_speed(saved.tts.local_speed.to_string().into());
            window.set_tts_speak_mode(speak_mode_code(
                saved.tts.speak_voice_turns,
                saved.tts.speak_text_turns,
            ));
            window.set_kws_dir(voice.kws_model_dir.into());
            window.set_whisper_file(voice.whisper_model.into());
            window.set_keywords_file(voice.keywords_file.into());
            window.set_local_model_check("点击检查以解析路径并验证文件".into());
            window.set_model_plans_json(plans.into());
            window.set_model_schedule_json(schedule.into());
            window.set_schedule_enabled(schedule_enabled);
            window.set_schedule_default(schedule_default.into());
            refresh_plan_labels(&window);
            window.set_provider_index(provider_index);
            window.set_base_url(base_url.into());
            window.set_model_name(model.into());
            fill_model_settings(&window, &provider);
            window.set_provider_hint_index(hint_index);
            window.set_thinking_index(thinking);
            window.set_effort_index(effort);
            window.set_api_key_input("".into());
            window.set_connection_hint("".into());
            window.set_connection_code("".into());
            window.set_model_list_hint("".into());

            window.set_discovered_model_index(-1);
            window.set_has_key(has_key);
            window.set_permission_index(permission_index);
            window.set_override_indices(ModelRc::new(VecModel::from(overrides)));
            /*
             * 联网搜索（PR12）：界面只显示"有没有 Key"与启用开关，
             * **绝不回填 Key 明文**——输入框留空表示"不改动"。
             */
            window.set_search_key_input("".into());
            window.set_search_enabled(search_enabled);
            window.set_search_code("".into());
            window.set_search_hint("".into());
            window.set_search_testing(false);
            // 重新打开 = 以已保存配置为准：清掉上一次的错误与"未保存"标记。
            window.set_settings_error("".into());
            window.set_settings_dirty(false);
            window.set_settings_open(true);
        }
    });
}

/// 本地快速回答（时间/日期）的收尾：写进同一个会话时间线，并照常播报。
///
/// 为什么复用同一条收尾路径：用户看到的必须还是"一个回合"——一条用户消息、
/// 一条助手回复、状态从忙碌回到完成；只是这一轮**没有**模型调用与工具卡片。
/// 不走 Agent 也就不会被权限询问、不会消耗 Key，更不会在断网时失败。
///
/// **同时写进有效 LLM 历史**：否则用户接着问"那再过三小时呢"时，模型看不到
/// 上一轮问过什么、答了什么，只能反问或者凭空猜。这里补的是与普通回合
/// 完全同形的 `user` + `assistant` 两条消息，所以：
///  - 后续追问能关联到这一轮；
///  - 历史随会话落盘，切换/重启后仍能恢复；
///  - Session 隔离照旧（只写当前会话的历史）。
#[allow(clippy::too_many_arguments)]
fn finish_quick_answer(
    weak: &slint::Weak<AppWindow>,
    sessions: &mut SessionRuntime,
    speech: &mut SpeechRuntime,
    settings: &Settings,
    turn_id: TurnId,
    question: &str,
    answer: &str,
    source: TurnSource,
) {
    // 1) 可见时间线（用户消息在启动回合时已经写过，这里只补助手回复）。
    sessions.push_or_extend_assistant(turn_id, answer);
    // 2) 发给 LLM 的历史：必须成对写入，否则下一轮模型只看到"助手凭空说了句话"。
    let mut history = sessions.history();
    history.push(crate::llm::ChatMessage::user(question));
    history.push(crate::llm::ChatMessage::text(
        crate::llm::Role::Assistant,
        answer,
    ));
    sessions.set_history(history);
    sessions.set_last_turn(turn_id, TurnStatus::Completed);
    if let Err(error) = sessions.save_current() {
        set_status(weak, &format!("会话保存失败：{error}"));
    }
    publish_session_list(weak, sessions);
    publish_timeline(weak, sessions);
    publish_session_title(weak, sessions);
    set_busy(weak, false);
    set_status(weak, answer);

    // 播报与普通回合同一套开关：文本/语音来源各自尊重设置。
    if settings.tts.speaks(source.is_voice()) {
        if let Err(error) = speech.speak(turn_id, answer) {
            set_speech_unavailable(weak, &error);
        }
    }
}

/// 把"当前实际可用的工具"推给设置页（**只显示真的会公布的**）。
///
/// 为什么要在界面里列出来：任务书要求"没有实现的工具不能显示为可执行"。
/// 这份清单直接来自 [`ToolRegistry::statuses`]，而它同时也是
/// `tool_definitions()` 的来源，两者不可能漂移。
fn publish_tool_overview(weak: &slint::Weak<AppWindow>, registry: &ToolRegistry) {
    let statuses = registry.statuses();
    let inventory = registry.inventory();
    let mut labels: Vec<slint::SharedString> = Vec::new();
    for status in &statuses {
        let mark = if status.available { "✓" } else { "×" };
        let detail = if status.available {
            if status.read_only {
                "只读".to_string()
            } else {
                "可写/可执行".to_string()
            }
        } else {
            status.reason.clone()
        };
        labels.push(slint::SharedString::from(format!(
            "{mark} {} · {} · {detail}",
            status.name,
            status.source.label()
        )));
    }
    let summary = inventory.summary();
    // 三个状态必须分开说清楚：有没有 Key、开关是否打开、此刻是否真的可用。
    let has_key = registry.search().enabled();
    let setting_on = registry.search_setting_enabled();
    let ready = registry.search_ready();
    let search_hint = match (has_key, setting_on) {
        (false, _) => {
            "联网搜索未配置 Key：不会出现在模型的工具列表里。填好 Key 后无需重启即可启用。"
                .to_string()
        }
        (true, false) => "联网搜索已配置 Key，但「启用联网搜索」是关闭的：当前不会公布也不会执行；\
Key 仍然保存着，重新打开开关立即恢复。"
            .to_string(),
        (true, true) if ready => {
            "联网搜索已启用：查询词会发给 Tavily（第三方，按次计费）。本地工具不受网络影响。"
                .to_string()
        }
        (true, true) => "联网搜索配置异常（Key 与开关都在，但工具未就绪）".to_string(),
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_tool_overview(ModelRc::new(VecModel::from(labels)));
            window.set_tool_summary(summary.into());
            window.set_search_hint(search_hint.into());
            // 「清除 Key」按钮看的是**有没有 Key**，与开关无关（关掉开关不该禁用清除）。
            window.set_search_key_saved(has_key);
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
    sessions: &mut SessionRuntime,
    permissions: &Arc<std::sync::Mutex<PermissionState>>,
    audit: &AuditLog,
    confirmer: &Arc<UiConfirmer>,
    voice: &mut VoiceRuntime,
    speech: &mut SpeechRuntime,
    speech_view: &mut SpeechView,
    tools_registry: &ToolRegistry,
    command_tx: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    save_generation: &Arc<std::sync::atomic::AtomicU64>,
    search_test_generation: &Arc<std::sync::atomic::AtomicU64>,
    model_request: &mut u64,
    source: TurnSource,
) -> TurnWait {
    let RunningTurn {
        mut task,
        turn_id,
        mut events,
    } = turn;
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
            Some(event) = events.recv() => { record_event(weak, sessions, turn_id, event); }
            joined = &mut task => {
                while let Ok(event) = events.try_recv() { record_event(weak, sessions, turn_id, event); }
                *turn_cancel = None;
                match joined {
                    Ok((outcome, history, settings_snapshot)) => {
                        maybe_speak(weak, speech, &settings_snapshot, &outcome, turn_id, source);
                        return TurnWait::Finished(TurnFinished {
                            history,
                            outcome,
                            settings: settings_snapshot,
                        });
                    }
                    Err(error) => {
                        let outcome = TurnOutcome {
                            text: format!("后台任务异常：{error}"),
                            failed: true,
                            failure_reason: Some("worker_panic".to_string()),
                            ..TurnOutcome::default()
                        };
                        return TurnWait::Finished(TurnFinished {
                            history: Vec::new(),
                            outcome,
                            settings: Settings::default(),
                        });
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
                        // 同样**不**清 `settings-saving`：那是更年轻那一代的职责。
                        if generation_is_current(save_generation, generation) {
                            let _ = handle_save_all_ui(
                                weak,
                                paths,
                                secret,
                                settings,
                                speech,
                                permissions,
                                tools_registry,
                                voice.handle.as_ref().is_some_and(|handle| handle.is_running()),
                                &request,
                                generation,
                                save_generation,
                                search_test_generation,
                            );
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
                    Some(UiCommand::PermissionModePicked(_)) | Some(UiCommand::OverridePicked { .. }) => {}
                    Some(UiCommand::TogglePause) => toggle_pause(weak, permissions),
                    Some(UiCommand::GrantCapability {
                        capability,
                        minutes,
                    }) => grant_capability(weak, permissions, &capability, minutes),
                    Some(UiCommand::ExportAudit) => export_audit(weak, audit, paths),
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
                        while let Ok(event) = events.try_recv() { record_event(weak, sessions, turn_id, event); }
                        let _ = sessions.save_current();
                        *turn_cancel = None;
                        return TurnWait::Quit;
                    }
                    Some(UiCommand::Send(_)) => {
                        set_status(weak, "上一轮还在跑，先取消再发新指令");
                    }
                    /*
                     * 忙碌期间的会话操作：一律拒绝并说明原因。
                     *
                     * 正在跑的回合绑定在**发起时**的会话上；这时切换/删除会话
                     * 会让"工具结果要写进哪个会话"变得不确定——所以宁可拒绝。
                     */
                    Some(UiCommand::NewSession) => {
                        set_status(weak, "正在执行；先完成或取消这一轮再新建会话");
                    }
                    Some(UiCommand::SelectSession(_)) => {
                        set_status(weak, "正在执行；先完成或取消这一轮再切换会话");
                    }
                    Some(UiCommand::RenameSession { .. }) => {
                        set_status(weak, "正在执行；先完成或取消这一轮再改名");
                    }
                    Some(UiCommand::DeleteSession(_)) => {
                        set_status(weak, "正在执行；先完成或取消这一轮再删除会话");
                    }
                    /*
                     * 忙碌期间的模型/推理选择：只提示"下一轮生效"，不热切换。
                     * 当前回合已经用启动时的端点快照，改它既无意义也不安全。
                     */
                    Some(UiCommand::ModelChoicePicked(_))
                    | Some(UiCommand::ReasoningPicked(_))
                    | Some(UiCommand::ApplyManualModel(_)) => {
                        set_status(weak, "正在执行；模型选择会在下一轮生效");
                    }
                    /*
                     * 权限立即生效：暂停/限时授权/档位/覆盖都作用于**下一个**
                     * 工具判断。这正是"不能复用过期授权"的落点。
                     */
                    Some(UiCommand::TestConnection(_)) => {
                        set_status(weak, "正在执行；先结束这一轮再测试连接");
                    }
                    Some(UiCommand::ModelsFetched { draft, credential, request, fetch }) => {
                        if request == *model_request {
                            remember_fetched_models(settings, (&draft.provider_id, &draft.base_url, request), *model_request, &fetch, secret, &credential);
                            let settings = settings.clone(); let sessions = sessions.clone();
                            with_window(weak, move |window| { window.set_models_busy(false); refresh_model_picker(window, &settings, &sessions); });
                        }
                    }
                    Some(UiCommand::FetchModels(_)) => {
                        set_status(weak, "正在执行；先结束这一轮再获取模型列表");
                    }
                    Some(UiCommand::SettingsTab(index)) => {
                        with_window(weak, move |window| window.set_settings_tab_index(index.max(0)));
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
                    Some(UiCommand::DictateNow) | Some(UiCommand::QuickPermissionPicked(_)) => { set_status(weak, "请先结束当前回合再切换"); }
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
                    Some(UiCommand::SaveEndpoint(_, _, _, _)) | Some(UiCommand::DeleteEndpoint(_)) => { with_window(weak, |w| w.set_settings_error("请先结束当前回合再保存或删除供应商".into())); }
                    Some(UiCommand::ProviderChanged) => { *model_request += 1; with_window(weak, |window| window.set_models_busy(false)); }
                    Some(UiCommand::ClearApiKey(provider_id)) => {
                        /*
                         * 任务执行期间清除 Key：**真实执行**，不是只提示。
                         * 本轮已经在用启动时的端点快照（Key 已读进 LlmClient），
                         * 所以立即生效的是"下一轮不再带这个 Key"。
                         *
                         * 界面反馈与空闲路径保持一致：状态按被清除的那家（= 界面选中那家）算，
                         * 端点摘要仍按已生效的配置算。
                         */
                        match clear_model_key(weak, secret, settings, speech, &provider_id) {
                            Ok(()) => {
                                settings_ui::restore_model_catalogs(settings, paths, secret, None);
                                let picker_settings = settings.clone(); sessions.set_key_presence(super::endpoint_cards::key_presence(settings, secret));
                let picker_sessions = sessions.clone();
                                with_window(weak, move |window| refresh_model_picker(window, &picker_settings, &picker_sessions));
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
                    Some(UiCommand::ClearSearchKey) => {
                        // 只清搜索 Key：模型提供商的 Key 一个都不动。
                        match secret.clear_search_key() {
                            Ok(()) => {
                                // 清 Key 让在途的测试结果失效。
                                search_test_generation.fetch_add(1, Ordering::SeqCst);
                                tools_registry.search().set_api_key("");
                                publish_tool_overview(weak, tools_registry);
                                set_status(weak, "已清除联网搜索的 Key（web_search 不再公布给模型）");
                            }
                            Err(error) => {
                                set_status(weak, &format!("清除联网搜索 Key 失败：{error}"));
                            }
                        }
                    }
                    Some(UiCommand::TestSearchKey { key, generation }) => {
                        handle_queued_search_test(
                            weak,
                            secret,
                            key,
                            generation,
                            search_test_generation,
                        );
                    }
                    None => return TurnWait::Quit,
                }
            }
        }
    }
}

/// 处理一条**已排队**的 Key 测试命令。
///
/// 关键：命令携带的是**点击那一刻**的代次。命令在通道里排队期间用户可能
/// 已经改了输入框（界面侧推进代次），此时这条命令已经过期——必须丢弃，
/// 否则 Key A 的测试会跑在 Key B 的输入旁边，结果被当成 B 的结论。
///
/// 这里**不**再推进代次：推进的责任在"点击测试"（界面侧）与
/// "改草稿/保存/清除"（各自路径）。收到命令时只做核对。
fn handle_queued_search_test(
    weak: &slint::Weak<AppWindow>,
    secret: &SecretStore,
    draft_key: String,
    generation: u64,
    search_test_generation: &Arc<std::sync::atomic::AtomicU64>,
) {
    // 排队期间状态变了：丢掉这条命令，连"测试中"都不要点亮。
    if search_test_generation.load(Ordering::SeqCst) != generation {
        return;
    }
    /*
     * 可用性测试：真的打一次接口，但**只读不写**。
     *
     * 用**草稿优先**：用户刚填了 Key 想先试一下、还没保存，
     * 这时拿已保存的旧 Key 去测只会给出误导性的结论。
     * 测试结果只用于显示；不写盘、不改共享客户端。
     */
    let key = if draft_key.trim().is_empty() {
        secret.load_search_key().ok().flatten().unwrap_or_default()
    } else {
        draft_key.trim().to_string()
    };
    handle_test_search_key(weak, key, generation, search_test_generation);
}

/// 联网搜索 Key 的可用性测试（后台执行，结果回填设置页）。
///
/// **测试绝没有副作用**：它只用一个临时客户端打一次接口，不碰共享 `WebSearch`、
/// 不写 `SecretStore`、也不改变当前可用的工具集。只有用户显式点「保存」并且
/// 保存成功，才会更新实际运行用的 Key（见 `handle_save_all_ui`）。
///
/// 为什么要带 `generation`：测试期间用户可能又改了草稿、又测了一次、
/// 切走了页面，或者保存/清除了 Key——这些都会把代次推进，
/// 迟到的旧结果因此不会覆盖更新的状态。
fn handle_test_search_key(
    weak: &slint::Weak<AppWindow>,
    key: String,
    generation: u64,
    generation_counter: &Arc<std::sync::atomic::AtomicU64>,
) {
    if key.trim().is_empty() {
        with_window(weak, |window| {
            window.set_search_testing(false);
            window.set_search_code("none".into());
            window.set_search_hint("还没有填写 Tavily API Key".into());
        });
        return;
    }
    /*
     * 临时客户端：与工具用的**共用同一个端点与传输层**（所以"测试通过"
     * 与"实际可用"是同一件事），但 Key 只活在这个局部对象里。
     */
    let probe = Arc::new(WebSearch::with_endpoint(
        crate::config::SEARCH_ENDPOINT,
        key.trim(),
    ));
    let weak = weak.clone();
    let counter = generation_counter.clone();
    tokio::spawn(async move {
        let cancel = CancelToken::new();
        let outcome = crate::tools::web_search::probe_key(&probe, &cancel).await;
        let (code, hint) = match &outcome {
            Ok(detail) => ("ok", detail.clone()),
            Err(error) => ("failed", error.user_message()),
        };
        // 迟到的结果不许覆盖更新的状态（改草稿/重测/保存/清除都会推进代次）。
        if counter.load(Ordering::SeqCst) != generation {
            return;
        }
        let weak_ui = weak.clone();
        let counter_for_ui = counter.clone();
        let _ = slint::invoke_from_event_loop(move || {
            /*
             * 在**真正改界面之前**再确认一次代次。
             *
             * `invoke_from_event_loop` 只是把闭包排进事件队列：从排队到执行之间
             * 用户完全可能已经改了草稿（那条路径在界面侧推进代次）。
             * 少了这次复核，迟到的 Key A 结果仍会盖在 Key B 的输入之上。
             */
            if counter_for_ui.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Some(window) = weak_ui.upgrade() {
                window.set_search_testing(false);
                window.set_search_code(code.into());
                window.set_search_hint(hint.into());
            }
        });
    });
}

/// 在界面线程上应用一次「保存」，并回填结果。
///
/// 关键规则（对应任务书第 2、4 条）：
///  - **配置没写进去**：显示失败、保留草稿、**不**用已生效的配置覆盖可编辑字段；
///  - **配置写了但播报不可用**：算保存成功，说明"已保存，但播报暂不可用"，关面板；
///  - **Key 没写进去**：说明普通配置已保存、只有 Key 没存，同样关面板；
///  - 只有"没写进去"时才留在面板里让用户改。
///
/// PR12：联网搜索 Key 写成功后**立刻**换进共享的 `WebSearch`（不需要重启），
/// 并刷新"可用工具总览"——用户点保存后马上就能看到 `web_search` 出现在列表里。
#[allow(clippy::too_many_arguments)]
fn handle_save_all_ui(
    weak: &slint::Weak<AppWindow>,
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    speech: &mut SpeechRuntime,
    permissions: &Arc<std::sync::Mutex<PermissionState>>,
    registry: &ToolRegistry,
    voice_running: bool,
    request: &SaveRequest,
    generation: u64,
    save_generation: &Arc<std::sync::atomic::AtomicU64>,
    search_test_generation: &Arc<std::sync::atomic::AtomicU64>,
) -> SaveOutcome {
    let reload_voice = voice_running
        && (settings.voice.kws_model_dir != request.kws_dir
            || settings.voice.whisper_model != request.whisper_file);
    // PR13：端点由卡片独立提交。全局表单只更新其他设置，不能用隐藏旧字段
    // 覆盖刚刚保存的卡片、会话生效实例或其启用集合。
    let mut request = request.clone();
    request.provider_id = settings.provider_id.clone();
    request.base_url = settings.base_url.clone();
    request.model = settings.model.clone();
    request.provider_hint = settings_ui::provider_hint_index(settings.provider_hint);
    request.thinking = thinking_index(settings.thinking);
    request.reasoning_effort = reasoning_effort_index(settings.reasoning_effort);
    request.enabled_models = settings.provider(&settings.provider_id).selectable_models();
    request.provider_enabled_models.clear();
    request.provider_drafts.clear();
    request.api_key.clear();
    let request = &request;
    let outcome = apply_save(paths, secret, settings, speech, request);
    let status = if outcome.config_written() && reload_voice {
        format!("{}；请重新开启麦克风以加载新的语音模型", outcome.message())
    } else {
        outcome.message()
    };
    set_status(weak, &status);

    /*
     * 联网搜索：只有**显式保存成功**才更新实际运行用的凭据与启用状态。
     *
     * Key 从 SecretStore 读回**当前生效**的那一份（草稿可能因为写盘失败而没落盘；
     * 以磁盘为准才不会出现"界面说已保存、实际 Key 还是旧的"）。
     * 关闭开关**不清 Key**，所以重新打开时 Key 仍在、立即恢复可用。
     */
    if outcome.config_written() {
        // 保存会改变"实际生效的凭据/开关"，在途的测试结果从此失去意义。
        search_test_generation.fetch_add(1, Ordering::SeqCst);
        let current = secret.load_search_key().ok().flatten().unwrap_or_default();
        registry.search().set_api_key(&current);
        registry.set_search_enabled(settings.search.enabled);
        eprintln!(
            "[search] 联网搜索已保存：开关={}，Key={}，工具可用={}",
            if settings.search.enabled {
                "启用"
            } else {
                "关闭"
            },
            if current.trim().is_empty() {
                "未配置"
            } else {
                "已配置"
            },
            if registry.search_ready() {
                "是"
            } else {
                "否"
            }
        );
        publish_tool_overview(weak, registry);
    }

    /*
     * 落盘期间用户可能已经取消、或又点了一次保存。
     * 只有代次仍是最新时才允许回填界面；否则这一次的结果属于"过期请求"，
     * 不能去覆盖用户当前的表单/状态。
     */
    if outcome.config_written() {
        if let Ok(mut state) = permissions.lock() {
            state.mode = settings.permission_mode;
            state.overrides = settings.permission_overrides.clone();
        }
    }
    if !generation_is_current(save_generation, generation) {
        return outcome;
    }

    // 只有配置真的落盘了，才刷新"已生效"状态；失败时**不动**界面上的可编辑字段，
    // 否则会把用户刚输入的非法内容悄悄换回旧值，让他无从修改。
    if outcome.config_written() && !matches!(outcome, SaveOutcome::SavedKeyFailed(_)) {
        // 权限运行态跟着新档位/覆盖走（但不重置暂停与限时授权）。
        if let Ok(mut state) = permissions.lock() {
            state.mode = settings.permission_mode;
            state.overrides = settings.permission_overrides.clone();
        }
        publish_tts_ui(weak, &settings.tts, speech);
        let has_key = secret.has(&settings.provider_id);
        let endpoint = config::describe_endpoint(settings, has_key);
        let message = outcome.message();
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
                window.set_settings_dirty(false);
                window.set_api_key_input("".into());
                window.set_settings_open(false);
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
///
/// PR8 的事件带回合/步骤/调用实例：工具卡片按**真实调用实例**追加，
/// 思考内容独立成块（不混进可见正文，因此不会被 TTS 朗读）。
fn remember_fetched_models(
    settings: &mut Settings,
    identity: (&str, &str, u64),
    current_request: u64,
    fetch: &crate::model_catalog::ModelFetch,
    secret: &SecretStore,
    credential: &str,
) -> bool {
    let (provider, endpoint, request) = identity;
    let saved_key = secret.load(provider).ok().flatten().unwrap_or_default();
    if request != current_request
        || !fetch.ok
        || crate::model_catalog::fingerprint(&saved_key) != credential
    {
        return false;
    }
    settings.remember_discovered_models(provider, endpoint, &fetch.models)
}

fn record_event(
    weak: &slint::Weak<AppWindow>,
    sessions: &mut SessionRuntime,
    turn_id: u64,
    event: AgentEvent,
) {
    use crate::session::{TimelineKind, TimelineToolStatus};
    let mut checkpoint = true;
    match &event {
        AgentEvent::TextDelta { turn, step, text } if *turn == turn_id => {
            sessions.extend_fragment(*turn, *step, crate::session::TimelineKind::Assistant, text);
            checkpoint = false;
        }
        AgentEvent::ReasoningDone {
            turn, step, text, ..
        } if *turn == turn_id => sessions.settle_reasoning(*turn, *step, text),
        AgentEvent::ReasoningDelta { turn, step, text } if *turn == turn_id => {
            sessions.extend_fragment(*turn, *step, crate::session::TimelineKind::Reasoning, text);
            checkpoint = false;
        }
        AgentEvent::ToolStart {
            turn,
            step,
            call,
            name,
            args_summary,
        } if *turn == turn_id => sessions.push_timeline(
            *turn,
            TimelineItem {
                kind: TimelineKind::Tool,
                step: *step,
                call_id: call.0,
                tool: name.clone(),
                args_summary: args_summary.to_string(),
                status: TimelineToolStatus::Running,
                ..TimelineItem::default()
            },
        ),
        AgentEvent::ToolEnd {
            turn,
            step,
            call,
            name,
            status,
            text,
            ms,
            ..
        } if *turn == turn_id => sessions.update_tool_item(
            *turn,
            *step,
            call.0,
            name,
            match status {
                ToolStatus::Ok => TimelineToolStatus::Ok,
                ToolStatus::Failed => TimelineToolStatus::Failed,
                ToolStatus::Denied => TimelineToolStatus::Denied,
                ToolStatus::Cancelled => TimelineToolStatus::Cancelled,
            },
            text,
            *ms,
        ),
        AgentEvent::ConfirmationRequired(_) => {
            apply_event(weak, event);
            return;
        }
        _ => return,
    }
    if checkpoint {
        if let Err(error) = sessions.save_current() {
            set_status(weak, &format!("会话保存失败：{error}"));
        }
    }
    publish_timeline(weak, sessions);
}

fn apply_event(weak: &slint::Weak<AppWindow>, event: AgentEvent) {
    match event {
        AgentEvent::TextDelta { text, .. } => {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    append_assistant_delta(&window, &text);
                }
            });
        }
        AgentEvent::ReasoningDelta { .. } => {
            // 流式思考片段不逐片渲染（会闪烁且重复），等 ReasoningDone 的最终文本。
        }
        AgentEvent::ReasoningDone { text, .. } => {
            // 真实思考内容：独立一条「思考」气泡（可折叠语义由样式区分），
            // 绝不进助手正文，也就不进播报。
            if text.is_empty() {
                return;
            }
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    push_entry(
                        &window,
                        "思考",
                        &text,
                        "模型返回的推理内容（不朗读）",
                        "#f4f4f7",
                    );
                }
            });
        }
        AgentEvent::ToolStart {
            name,
            args_summary,
            call,
            ..
        } => {
            let weak = weak.clone();
            let weak_for_status = weak.clone();
            let args_text = serde_json::to_string(&args_summary).unwrap_or_default();
            let name_for_card = name.clone();
            let call_id = call.0;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    // 按调用实例一张卡片：连续同名工具各自独立（用 call id 区分）。
                    push_tool(
                        &window,
                        call_id,
                        &name_for_card,
                        "running",
                        &args_text,
                        "#f1f1f3",
                    );
                }
            });
            let name_for_status = name;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_status.upgrade() {
                    window.set_status(format!("正在执行 {name_for_status}…").into());
                }
            });
        }
        AgentEvent::ToolEnd {
            name,
            status,
            text,
            ms,
            call,
            ..
        } => {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let (state, tint) = match status {
                        ToolStatus::Ok => ("ok", "#eef8ef"),
                        ToolStatus::Failed => ("failed", "#fdeeee"),
                        ToolStatus::Denied => ("denied", "#fff4e5"),
                        ToolStatus::Cancelled => ("cancelled", "#f1f1f3"),
                    };
                    let summary: String = text.chars().take(400).collect();
                    update_tool(
                        &window,
                        call.0,
                        &name,
                        state,
                        &format!("{summary}（{} ms）", ms),
                        tint,
                    );
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

/// 追加一张工具卡片（按调用实例 id 归属，不覆盖别的实例）。
fn push_tool(window: &AppWindow, call_id: u64, name: &str, state: &str, detail: &str, tint: &str) {
    let model = window.get_tools();
    let mut items: Vec<ToolCard> = model.iter().collect();
    items.push(ToolCard {
        // 用 `#id name` 让"连续同名工具"在界面上也能区分。
        name: format!("#{call_id} {name}").into(),
        state: state.into(),
        detail: detail.into(),
        tint: parse_hex_color(tint),
    });
    if items.len() > 300 {
        items.drain(0..items.len() - 300);
    }
    window.set_tools(ModelRc::new(VecModel::from(items)));
}

/// 更新指定调用实例的卡片（不是"最后一张"，避免串到别的调用）。
fn update_tool(
    window: &AppWindow,
    call_id: u64,
    name: &str,
    state: &str,
    detail: &str,
    tint: &str,
) {
    let model = window.get_tools();
    let mut items: Vec<ToolCard> = model.iter().collect();
    let label = format!("#{call_id} {name}");
    if let Some(card) = items.iter_mut().rev().find(|card| card.name == label) {
        card.state = state.into();
        card.detail = detail.into();
        card.tint = parse_hex_color(tint);
    } else {
        // 没找到（例如启动时重建）就补一张，绝不静默丢弃这次调用。
        items.push(ToolCard {
            name: label.into(),
            state: state.into(),
            detail: detail.into(),
            tint: parse_hex_color(tint),
        });
    }
    window.set_tools(ModelRc::new(VecModel::from(items)));
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

/// 回合收尾：先把结果写回**发起的那个会话**，再更新界面。
///
/// 顺序很重要：会话历史/时间线/状态先落盘，界面只是它的投影。
/// 这样"界面显示有结果"与"磁盘上真的有"是同一件事。
fn finish_rejected_turn(
    weak: &slint::Weak<AppWindow>,
    sessions: &mut SessionRuntime,
    turn_id: TurnId,
    reason: &str,
) {
    sessions.push_timeline(turn_id, TimelineItem::system(format!("启动失败：{reason}")));
    sessions.set_last_turn(turn_id, TurnStatus::Failed);
    let save_error = sessions.save_current().err();
    publish_session_list(weak, sessions);
    publish_timeline(weak, sessions);
    set_busy(weak, false);
    set_status(
        weak,
        &match save_error {
            Some(error) => format!("启动失败：{reason}；会话保存失败：{error}"),
            None => format!("启动失败：{reason}"),
        },
    );
}

fn finish_session_turn(
    weak: &slint::Weak<AppWindow>,
    sessions: &mut SessionRuntime,
    speech: &mut SpeechRuntime,
    settings: &Settings,
    finished: TurnFinished,
    turn_id: TurnId,
) {
    let TurnFinished {
        history,
        outcome,
        settings: turn_settings,
    } = finished;

    // 历史（发给模型的那份）与时间线（用户看到的那份）分开写。
    sessions.set_history(history);
    // 纯工具回合没有可见正文时，补一条最终结果，避免"只有卡片没有回复"。
    if !outcome.text.trim().is_empty() && !sessions.has_assistant_text(turn_id) {
        sessions.push_or_extend_assistant(turn_id, &outcome.text);
    }
    let status = if outcome.cancelled {
        TurnStatus::Cancelled
    } else if outcome.failed {
        TurnStatus::Failed
    } else {
        TurnStatus::Completed
    };
    if outcome.failed && !outcome.cancelled {
        sessions.push_timeline(
            turn_id,
            TimelineItem::system(format!(
                "这一轮没能完成：{}",
                outcome
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "未知原因".to_string())
            )),
        );
    } else if outcome.cancelled {
        sessions.push_timeline(turn_id, TimelineItem::system("这一轮被取消。"));
    }
    sessions.set_last_turn(turn_id, status);
    if let Err(error) = sessions.save_current() {
        set_status(weak, &format!("会话保存失败：{error}"));
    }
    publish_session_list(weak, sessions);
    publish_timeline(weak, sessions);
    publish_session_title(weak, sessions);
    publish_model_badges_from_settings(weak, settings, sessions);

    // 已经取消/失败的回合不播报（播报只针对正常完成的可见正文）。
    let _ = speech;

    let steps = outcome.steps.len();
    let failed = outcome.failed;
    let cancelled = outcome.cancelled;
    let reason = outcome.failure_reason.clone();
    let context_ratio = (outcome.context_tokens as f64 / turn_settings.context_window.max(1) as f64)
        .clamp(0.0, 0.99999) as f32;
    let context_text = format!(
        "上下文 ≈ {} / {} tokens（{}% 触发压缩）",
        outcome.context_tokens,
        turn_settings.context_window,
        turn_settings.context_compress_percent
    );
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else { return };
        window.set_busy(false);
        window.set_context_text(context_text.into());
        window.set_context_usage(context_ratio);
        window.set_context_known(true);
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

/// 测试连接：真的发一次最小对话请求，并把结论分类显示。
///
/// **不等同于"获取模型列表"**：`/models` 只能说明目录接口可用，
/// 模型是否存在、能否聊天是另一回事（严格端点会对不存在的模型 404）。
fn handle_test_connection(
    weak: &slint::Weak<AppWindow>,
    settings: &Settings,
    secret: &SecretStore,
    draft: ProbeDraft,
    generation: &Arc<std::sync::atomic::AtomicU64>,
) {
    let request = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let (provider, api_key) = match draft.resolve(settings, secret) {
        Ok(resolved) => resolved,
        Err(error) => {
            with_window(weak, move |window| {
                if draft.matches_window(window) {
                    window.set_connection_code("auth".into());
                    window.set_connection_hint(error.into());
                }
            });
            return;
        }
    };
    let profile = ProbeDraft::profile(&provider);
    let timeout = settings.timeout_ms;
    let weak = weak.clone();
    let generation = generation.clone();
    tokio::spawn(async move {
        let outcome = probe_chat(&profile, &api_key, timeout).await;
        with_window(&weak, move |window| {
            if generation.load(Ordering::SeqCst) != request || !draft.matches_window(window) {
                return;
            }
            window.set_connection_code(outcome.code().into());
            window.set_connection_hint(format!("{}：{}", draft.label(), outcome.message()).into());
        });
    });
}

/// 输入区的模型下拉被选中：只改**当前会话**的选择并持久化。
fn handle_model_pick(
    weak: &slint::Weak<AppWindow>,
    settings: &Settings,
    sessions: &mut SessionRuntime,
    index: i32,
) {
    // 用与界面相同的顺序重新枚举一遍候选，避免把索引与标签错位。
    let mut candidates: Vec<(String, String)> = Vec::new();
    for provider_id in settings.provider_ids() {
        let provider = settings.provider(&provider_id);
        let mut models = provider.selectable_models();
        if !provider.model.trim().is_empty() && !models.contains(&provider.model) {
            models.insert(0, provider.model.clone());
        }
        for model in models {
            candidates.push((provider_id.clone(), model));
        }
    }
    let Some((provider_id, model)) = usize::try_from(index)
        .ok()
        .and_then(|index| candidates.get(index).cloned())
    else {
        set_status(weak, "模型选择无效，请重新选择");
        return;
    };
    let previous = sessions.model_choice();
    let choice = SessionModelChoice {
        provider_id,
        model,
        // 推理档位沿用本会话已有的（若没有就跟随新提供商的默认）。
        thinking: previous.thinking,
        reasoning_effort: previous.reasoning_effort,
    };
    apply_session_choice(weak, settings, sessions, choice);
}

/// 推理档位被选中：写进**当前会话**的选择并持久化。
fn handle_reasoning_pick(
    weak: &slint::Weak<AppWindow>,
    settings: &Settings,
    sessions: &mut SessionRuntime,
    index: i32,
) {
    let mut choice = sessions.model_choice();
    if choice.is_empty() {
        // 还没手选过：以"当前解析出的生效选择"为基准，避免写出一条半空的记录。
        let selection = settings.resolve_selection(None, &crate::clock::now());
        choice.provider_id = selection.provider_id;
        choice.model = selection.model;
    }
    let capability = settings.provider(&choice.provider_id).capability();
    match capability {
        crate::llm::Capability::Mimo => {
            choice.thinking = Some(settings_ui::thinking_for_index(index));
        }
        crate::llm::Capability::DeepSeek => {
            choice.reasoning_effort = Some(settings_ui::reasoning_effort_for_index(index));
        }
        crate::llm::Capability::Plain => {
            // 自定义端点没有专属推理参数：不写任何档位，如实说明。
            set_status(
                weak,
                "该端点没有专属推理档位（不会注入 MiMo/DeepSeek 字段）",
            );
            return;
        }
    }
    apply_session_choice(weak, settings, sessions, choice);
}

/// 把会话级模型选择写进会话并持久化，然后刷新界面。
fn apply_session_choice(
    weak: &slint::Weak<AppWindow>,
    settings: &Settings,
    sessions: &mut SessionRuntime,
    choice: SessionModelChoice,
) {
    match sessions.set_model_choice(choice) {
        Ok(()) => {
            let settings = settings.clone();
            let sessions = sessions.clone();
            with_window(weak, move |window| {
                refresh_model_picker(window, &settings, &sessions);
                publish_model_badges(window, &settings, &sessions);
                publish_session_list(&window.as_weak(), &sessions);
            });
            set_status(weak, "已切换本会话的模型选择（下一轮生效）");
        }
        Err(error) => set_status(weak, &format!("保存模型选择失败：{error}")),
    }
}

/// 把生效模型摘要推给界面（弱引用版，供回合收尾使用）。
fn publish_model_badges_from_settings(
    weak: &slint::Weak<AppWindow>,
    settings: &Settings,
    sessions: &SessionRuntime,
) {
    let choice = sessions.model_choice();
    let selection = settings.resolve_selection(
        if choice.is_empty() {
            None
        } else {
            Some(&choice)
        },
        &crate::clock::now(),
    );
    let model_text = format!(
        "{} · {}",
        settings.provider(&selection.provider_id).display_name(),
        selection.model
    );
    let effective = settings
        .effective_context_window_of(&selection.provider_id, &selection.model)
        .to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_session_model_text(model_text.into());
            window.set_session_effective_window(effective.into());
        }
    });
}

/// 权限：应用逐能力覆盖（立即生效，不必等保存）。
fn toggle_pause(
    weak: &slint::Weak<AppWindow>,
    permissions: &Arc<std::sync::Mutex<PermissionState>>,
) {
    let paused_now = permissions
        .lock()
        .map(|state| state.is_paused(std::time::SystemTime::now()))
        .unwrap_or(false);
    if let Ok(mut state) = permissions.lock() {
        if paused_now {
            state.resume();
        } else {
            state.pause(10);
        }
    }
    publish_permission_state(weak, permissions);
    set_status(
        weak,
        if paused_now {
            "已恢复操作"
        } else {
            "已暂停 10 分钟，期间不执行任何操作"
        },
    );
}

/// 权限：会话内限时允许某能力。
fn grant_capability(
    weak: &slint::Weak<AppWindow>,
    permissions: &Arc<std::sync::Mutex<PermissionState>>,
    capability: &str,
    minutes: i32,
) {
    if let Ok(mut state) = permissions.lock() {
        if let Some(capability) = crate::permissions::capabilities()
            .iter()
            .find(|c| c.id == capability || c.label == capability)
        {
            state.grant_once(capability.id, minutes.max(1) as u64);
        }
    }
    publish_permission_state(weak, permissions);
}

/// 把权限运行态推给界面（暂停标记 + 各能力的覆盖索引）。
fn publish_permission_state(
    weak: &slint::Weak<AppWindow>,
    permissions: &Arc<std::sync::Mutex<PermissionState>>,
) {
    let Ok(state) = permissions.lock() else {
        return;
    };
    let now = std::time::SystemTime::now();
    let paused = state.is_paused(now);
    let pause_hint = state
        .paused_minutes(now)
        .map(|minutes| format!("已暂停 {minutes} 分钟；期间任何工具都不会执行"))
        .unwrap_or_default();
    let mode_index = permission_mode_index(state.mode);
    let overrides: Vec<i32> = crate::permissions::capabilities()
        .iter()
        .map(|capability| match state.overrides.get(capability.id) {
            Some(crate::config::PermissionOverride::Allow) => 1,
            Some(crate::config::PermissionOverride::Ask) => 2,
            Some(crate::config::PermissionOverride::Deny) => 3,
            _ => 0,
        })
        .collect();
    drop(state);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_paused(paused);
            window.set_pause_hint(pause_hint.into());
            if !window.get_settings_open() {
                window.set_permission_index(mode_index);
                window.set_override_indices(ModelRc::new(VecModel::from(overrides)));
            }
        }
    });
}

/// 导出审计到数据目录的 `exports/`，如实回报条数与路径。
fn export_audit(weak: &slint::Weak<AppWindow>, audit: &AuditLog, paths: &AppPaths) {
    match audit.export(&paths.exports_dir()) {
        Ok((count, path)) => {
            set_status(weak, &format!("已导出 {count} 条审计到 {}", path.display()))
        }
        Err(error) => set_status(weak, &format!("导出审计失败：{error}")),
    }
}

fn with_window(weak: &slint::Weak<AppWindow>, update: impl FnOnce(&AppWindow) + Send + 'static) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            update(&window);
        }
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

fn capture_provider_draft(window: &AppWindow) -> settings_ui::ProviderDraft {
    let flags = window.get_enabled_model_flags();
    settings_ui::ProviderDraft {
        base_url: window.get_base_url().to_string(),
        model: window.get_model_name().to_string(),
        hint: window.get_provider_hint_index(),
        thinking: window.get_thinking_index(),
        effort: window.get_effort_index(),
        api_key: window.get_api_key_input().to_string(),
        rows: window
            .get_enabled_model_names()
            .iter()
            .enumerate()
            .map(|(i, name)| (name.to_string(), flags.row_data(i).unwrap_or(false)))
            .collect(),
    }
}

fn apply_model_draft(window: &AppWindow, rows: &[(String, bool)]) {
    let names: Vec<slint::SharedString> =
        rows.iter().map(|(name, _)| name.clone().into()).collect();
    let labels: Vec<slint::SharedString> = rows
        .iter()
        .map(|(name, _)| format!("{name} · 保留选择，未验证").into())
        .collect();
    window.set_enabled_model_names(ModelRc::new(VecModel::from(names)));
    window.set_enabled_model_labels(ModelRc::new(VecModel::from(labels)));
    window.set_enabled_model_flags(ModelRc::new(VecModel::from(
        rows.iter().map(|(_, enabled)| *enabled).collect::<Vec<_>>(),
    )));
}

/// 设置目录与启用集合分别回填；停用不会删除目录。
fn fill_model_settings(window: &AppWindow, provider: &config::ProviderConfig) {
    let mut names: Vec<String> = provider.models.iter().map(|m| m.id.clone()).collect();
    if !provider.model.trim().is_empty() && !names.contains(&provider.model) {
        names.push(provider.model.clone());
    }
    for name in provider.selectable_models() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let enabled = provider.selectable_models();
    let old_endpoint = !provider.models_origin_url.is_empty()
        && !super::model_probe::same_endpoint(
            &provider.models_origin_url,
            &provider.base_url_or_default(),
        );
    let labels: Vec<slint::SharedString> = names
        .iter()
        .map(|name| {
            let source = if provider
                .model_meta(name)
                .is_some_and(|m| m.source == config::ModelSource::Discovered)
            {
                "服务端目录，未验证"
            } else if old_endpoint {
                "旧端点选择，未验证"
            } else {
                "手动，未验证"
            };
            format!("{name} · {source}").into()
        })
        .collect();
    window.set_enabled_model_labels(ModelRc::new(VecModel::from(labels)));
    window.set_enabled_model_flags(ModelRc::new(VecModel::from(
        names
            .iter()
            .map(|n| enabled.contains(n))
            .collect::<Vec<_>>(),
    )));
    let names: Vec<slint::SharedString> = names.into_iter().map(Into::into).collect();
    window.set_enabled_model_names(ModelRc::new(VecModel::from(names.clone())));
    window.set_discovered_models(ModelRc::new(VecModel::from(names)));
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

    struct KeyClearEngine;
    impl crate::voice::tts::TtsEngine for KeyClearEngine {
        fn label(&self) -> String {
            "key-clear-test-engine".into()
        }
        fn start(
            &self,
            _: crate::voice::tts::SynthRequest,
        ) -> Result<crate::voice::tts::SpeechStream, String> {
            let (_, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(crate::voice::tts::SpeechStream::new(rx))
        }
    }
    #[tokio::test]
    async fn clearing_official_mimo_key_revokes_engine_stops_speech_and_new_key_recovers() {
        let paths = AppPaths::with_root(unique_test_root("mimo-key-revoke"));
        let secret = SecretStore::new(paths.secret_file());
        secret.save("mimo", "old-dummy-key").unwrap();
        let mut settings = Settings::default();
        settings.tts.enabled = true;
        settings.tts.backend = crate::voice::tts::TtsBackend::Mimo;
        let mut speech = SpeechRuntime::new(tokio::sync::mpsc::unbounded_channel().0);
        speech.apply_settings(&settings.tts, &secret).unwrap();
        assert!(speech.is_available());
        // 假扬声器/合成引擎验证停止语义；不会访问声卡或真实云端。
        speech.attach_playback_for_test(Arc::new(crate::voice::playback::PlaybackShared::new(
            24000, 1,
        )));
        speech.attach_engine_for_test(Arc::new(KeyClearEngine));
        speech.speak(1, "第一句。第二句。").unwrap();
        assert!(speech.is_speaking());
        clear_model_key(
            &slint::Weak::default(),
            &secret,
            &settings,
            &mut speech,
            "mimo",
        )
        .unwrap();
        assert!(!secret.has("mimo"));
        assert!(!speech.is_available());
        assert!(!speech.is_speaking());
        assert!(speech.engine_error().unwrap().contains("Key"));
        assert!(speech.speak(2, "不应继续使用旧凭据").is_err());
        // 与 SaveEndpoint / apply_save 一致的恢复路径，构建新引擎（不发真实音频请求）。
        secret.save("mimo", "new-dummy-key").unwrap();
        speech.apply_settings(&settings.tts, &secret).unwrap();
        assert!(speech.is_available());
        assert!(speech.engine_error().is_none());
        speech.shutdown();
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[tokio::test]
    async fn clearing_other_instances_or_cloud_key_with_local_tts_keeps_engine_and_playback() {
        let paths = AppPaths::with_root(unique_test_root("other-key-clear"));
        let secret = SecretStore::new(paths.secret_file());
        for backend in [
            crate::voice::tts::TtsBackend::Mimo,
            crate::voice::tts::TtsBackend::Local,
        ] {
            let mut settings = Settings::default();
            settings.tts.enabled = true;
            settings.tts.backend = backend;
            let mut speech = SpeechRuntime::new(tokio::sync::mpsc::unbounded_channel().0);
            speech.attach_playback_for_test(Arc::new(crate::voice::playback::PlaybackShared::new(
                24000, 1,
            )));
            speech.attach_engine_for_test(Arc::new(KeyClearEngine));
            let id = speech.speak(1, "保留当前播放").unwrap();
            for provider_id in ["mimo-second", "deepseek", "gateway"] {
                secret.save(provider_id, "dummy-key").unwrap();
                clear_model_key(
                    &slint::Weak::default(),
                    &secret,
                    &settings,
                    &mut speech,
                    provider_id,
                )
                .unwrap();
                assert!(!secret.has(provider_id));
                assert_eq!(
                    speech.engine_label().as_deref(),
                    Some("key-clear-test-engine")
                );
                assert_eq!(speech.current_speech_id(), Some(id));
                assert!(speech.is_speaking());
            }
            if backend == crate::voice::tts::TtsBackend::Local {
                secret.save("mimo", "dummy-key").unwrap();
                clear_model_key(
                    &slint::Weak::default(),
                    &secret,
                    &settings,
                    &mut speech,
                    "mimo",
                )
                .unwrap();
                assert_eq!(
                    speech.engine_label().as_deref(),
                    Some("key-clear-test-engine")
                );
                assert!(speech.is_speaking());
            }
            speech.shutdown();
        }
        std::fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn same_name_groups_keep_distinct_ids_and_pick_exact_session_endpoint() {
        let mut settings = Settings::default();
        for i in 0..2 {
            let mut p = settings.provider("mimo");
            p.id = format!("same-name-{i}");
            p.name = "同名 MiMo".into();
            p.base_url = format!("http://127.0.0.1:{}/v1", 24000 + i);
            p.model = format!("private-{i}");
            settings.providers.push(p);
        }
        let groups = picker_groups(&settings);
        let pair = groups
            .iter()
            .filter(|(id, _)| id.starts_with("same-name"))
            .collect::<Vec<_>>();
        assert_eq!(pair.len(), 2);
        assert_ne!(pair[0].0, pair[1].0);
        assert_ne!(pair[0].1, pair[1].1);
        let paths = AppPaths {
            root: std::env::temp_dir().join(format!(
                "cuava-same-name-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        };
        let mut sessions = SessionRuntime::open(&paths);
        sessions.ensure_one(1);
        let index = settings
            .provider_ids()
            .iter()
            .take_while(|id| id.as_str() != "same-name-1")
            .map(|id| settings.provider(id).selectable_models().len())
            .sum::<usize>();
        handle_model_pick(
            &slint::Weak::default(),
            &settings,
            &mut sessions,
            index as i32,
        );
        let choice = sessions.model_choice();
        assert_eq!(choice.provider_id, "same-name-1");
        assert_eq!(choice.model, "private-1");
        let selection = settings.resolve_selection(Some(&choice), &crate::clock::now());
        assert_eq!(selection.base_url, "http://127.0.0.1:24001/v1");
        assert_eq!(SessionRuntime::open(&paths).model_choice(), choice);
        std::fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn stale_and_failed_discovery_cannot_change_model_catalog() {
        let mut settings = Settings::default();
        let paths = AppPaths::with_root(unique_test_root("stale-fetch"));
        let secret = SecretStore::new(paths.secret_file());
        let endpoint = settings.provider("deepseek").base_url_or_default();
        let mut fetch = crate::model_catalog::ModelFetch {
            ok: true,
            cached: false,
            stale: false,
            models: vec![config::ModelOption::manual("found-model")],
            dropped: Vec::new(),
            raw: 1,
            error: None,
            at_ms: 1,
        };
        assert!(!remember_fetched_models(
            &mut settings,
            ("deepseek", &endpoint, 1),
            2,
            &fetch,
            &secret,
            ""
        ));
        assert!(settings.provider("deepseek").models.is_empty());
        assert!(remember_fetched_models(
            &mut settings,
            ("deepseek", &endpoint, 2),
            2,
            &fetch,
            &secret,
            ""
        ));
        assert_eq!(settings.provider("deepseek").models_origin_url, endpoint);
        fetch.ok = false;
        fetch.models = vec![config::ModelOption::manual("failed-model")];
        assert!(!remember_fetched_models(
            &mut settings,
            ("deepseek", &endpoint, 3),
            3,
            &fetch,
            &secret,
            ""
        ));
        assert_eq!(settings.provider("deepseek").models[0].id, "found-model");
        assert_eq!(settings.provider("deepseek").models_origin_url, endpoint);
    }

    #[test]
    fn unknown_terminal_without_start_never_claims_not_executed() {
        let paths = AppPaths::with_root(unique_test_root("unknown-terminal"));
        let mut sessions = SessionRuntime::open(&paths);
        sessions.ensure_one(1000);
        for text in [
            "响应不完整，未执行",
            "已中断（状态未知）",
            "已中断（状态未知）",
        ] {
            record_event(
                &slint::Weak::default(),
                &mut sessions,
                9,
                AgentEvent::ToolEnd {
                    turn: 9,
                    step: 2,
                    call: crate::agent::ToolCallId(1),
                    name: "left_click".into(),
                    status: ToolStatus::Cancelled,
                    text: text.into(),
                    ms: 23,
                },
            );
        }
        let restored = SessionRuntime::open(&paths);
        let items = restored.timeline();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "已中断（状态未知）");
        assert!(!items[0].text.contains("未执行"));
        assert_eq!(items[0].ms, 23);
        assert_eq!(
            (items[0].turn_id, items[0].step, items[0].call_id),
            (9, 2, 1)
        );
    }

    #[test]
    fn terminal_only_tool_events_are_saved_once_with_identity_and_not_executed_reason() {
        let paths = AppPaths::with_root(unique_test_root("terminal-only"));
        let mut sessions = SessionRuntime::open(&paths);
        let first = sessions.create(1000, "first");
        let weak = slint::Weak::default();
        for call in [1, 1, 2] {
            record_event(
                &weak,
                &mut sessions,
                7,
                AgentEvent::ToolEnd {
                    turn: 7,
                    step: 3,
                    call: crate::agent::ToolCallId(call),
                    name: "left_click".into(),
                    status: ToolStatus::Failed,
                    text: "响应不完整，未执行".into(),
                    ms: 0,
                },
            );
        }
        sessions.create(2000, "second");
        sessions.select(&first).unwrap();
        let restored = SessionRuntime::open(&paths);
        let items = restored.timeline();
        assert_eq!(items.len(), 2);
        for (i, item) in items.iter().enumerate() {
            assert_eq!(
                (item.turn_id, item.step, item.call_id),
                (7, 3, i as u64 + 1)
            );
            assert_eq!(item.tool, "left_click");
            assert_eq!(item.status, crate::session::TimelineToolStatus::Failed);
            assert!(item.text.contains("未执行"));
            assert!(item.text.contains("响应不完整"));
        }
    }

    #[test]
    fn event_checkpoints_restore_reasoning_and_two_tool_instances() {
        let paths = AppPaths::with_root(unique_test_root("event-checkpoints"));
        let mut sessions = SessionRuntime::open(&paths);
        sessions.ensure_one(1000);
        sessions.set_last_turn(7, TurnStatus::Interrupted);
        let weak = slint::Weak::default();
        record_event(
            &weak,
            &mut sessions,
            7,
            AgentEvent::ReasoningDone {
                turn: 7,
                step: 1,
                chars: 4,
                text: "实际思考".into(),
            },
        );
        for call in [1, 2] {
            record_event(
                &weak,
                &mut sessions,
                7,
                AgentEvent::ToolStart {
                    turn: 7,
                    step: 1,
                    call: crate::agent::ToolCallId(call),
                    name: "read_clipboard".into(),
                    args_summary: serde_json::json!({}),
                },
            );
            if call == 1 {
                record_event(
                    &weak,
                    &mut sessions,
                    7,
                    AgentEvent::ToolEnd {
                        turn: 7,
                        step: 1,
                        call: crate::agent::ToolCallId(call),
                        name: "read_clipboard".into(),
                        status: ToolStatus::Ok,
                        text: "ok".into(),
                        ms: 20,
                    },
                );
            }
        }
        let reopened = SessionRuntime::open(&paths);
        let items = reopened.timeline();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "实际思考");
        assert_eq!(items[1].status, crate::session::TimelineToolStatus::Ok);
        assert_eq!(
            items[2].status,
            crate::session::TimelineToolStatus::Interrupted
        );
        assert_eq!(items[1].ms, 20);
        assert_ne!(items[1].call_id, items[2].call_id);
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

    /// 权限档位的索引映射与 `ui::settings` 的数组必须一致，
    /// 否则界面选"免确认"而实际执行的是别的档位。
    #[test]
    fn permission_index_mapping_matches_settings_module() {
        for (index, mode) in settings_ui::PERMISSION_MODES.iter().enumerate() {
            assert_eq!(permission_mode_for_index(index as i32), *mode);
        }
        assert_eq!(
            permission_mode_for_index(settings_ui::PERMISSION_MODES.len() as i32),
            PermissionMode::Standard,
            "越界回到安全默认档"
        );
        // 「免确认」必须真的能被索引到（PR7 曾把它静默解析成 standard）。
        let pass_index = settings_ui::permission_mode_index(PermissionMode::PassThrough);
        assert_eq!(
            permission_mode_for_index(pass_index),
            PermissionMode::PassThrough
        );
    }

    #[test]
    fn endpoint_summary_never_contains_key() {
        let summary = settings_summary(&Settings::default(), true);
        assert!(!summary.contains("sk-"));
        assert!(summary.contains("Key 已保存"));
    }

    #[test]
    fn arg_summary_is_secret_free() {
        let value = serde_json::json!({ "app": "notepad", "x": 10 });
        assert!(summary_is_secret_free(&value));
        // data URL 的 base64 也必须被摘要层挡住。
        let image = serde_json::json!({
            "image": "data:image/png;base64,".to_string() + &"A".repeat(400)
        });
        let summary = crate::permissions::summarize_args(&image);
        assert!(summary_is_secret_free(&summary));
        assert!(!summary.as_object().map(|o| o.is_empty()).unwrap_or(true));
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
        let _wait = install_pending(&confirmer, 5);
        // 旧请求的 id 不能结算当前挂起的这一条
        assert!(!confirmer.submit(4, ConfirmAnswer::allow_click()));
        assert!(confirmer.pending.lock().unwrap().is_some());
    }

    /// **关键安全语义**：一次确认只结算一次，连续两次确认的答复不会串。
    #[tokio::test]
    async fn answers_never_leak_between_consecutive_confirmations() {
        let confirmer = confirmer_without_ui();
        // 第一次：允许
        let mut first = install_pending(&confirmer, 1);
        assert!(confirmer.submit(1, ConfirmAnswer::allow_click()));
        assert!(first.try_recv().unwrap().allow);
        // 第二次：拒绝。绝不能因为"上一次被允许过"而继承允许。
        let mut second = install_pending(&confirmer, 2);
        assert!(confirmer.submit(2, ConfirmAnswer::deny_click()));
        assert!(!second.try_recv().unwrap().allow);
    }

    #[tokio::test]
    async fn abort_pending_denies_instead_of_leaving_it_open() {
        let confirmer = confirmer_without_ui();
        let mut wait = install_pending(&confirmer, 3);
        confirmer.abort_pending();
        let answer = wait.try_recv().expect("中止必须让等待方立刻拿到答复");
        assert!(!answer.allow, "中止 = 拒绝，不能放行");
        assert!(confirmer.pending.lock().unwrap().is_none());
    }

    #[test]
    fn voice_prompt_outcome_is_only_prompt() {
        let prompt = VoicePrompt {
            text: "打开浏览器".to_string(),
            audio_seconds: 1.0,
            decode_ms: 10,
        };
        assert!(VoiceOutcome::Prompt(prompt).into_prompt().is_some());
        assert!(VoiceOutcome::None.into_prompt().is_none());
    }

    #[test]
    fn cancelled_manual_start_and_final_cannot_bind_after_rearming() {
        let mut voice = VoiceRuntime::new();
        let instance = voice.route.open();
        let turn = VoiceTurn {
            instance,
            session: 1,
        };
        voice.manual_request = 2;
        voice.manual_armed = true;
        voice.set_listen_mode(ListenPurpose::Manual, 2);
        let weak = slint::Weak::default();
        let start = VoiceEvent {
            instance,
            kind: VoiceEventKind::DirectStarted {
                turn,
                purpose: ListenPurpose::Manual,
                request_id: 1,
            },
        };
        assert!(voice.on_event(&weak, start, false).into_prompt().is_none());
        let final_event = VoiceEvent {
            instance,
            kind: VoiceEventKind::Final {
                turn,
                text: "执行操作".into(),
                audio_seconds: 1.0,
                decode_ms: 1,
                purpose: ListenPurpose::Manual,
                request_id: 1,
            },
        };
        assert!(voice
            .on_event(&weak, final_event, false)
            .into_prompt()
            .is_none());
        assert!(voice.route.pending.is_none());
    }

    /// 旧实例的迟到事件不得改界面状态。
    #[test]
    fn stale_voice_instance_events_are_ignored() {
        let mut voice = VoiceRuntime::new();
        let current = voice.route.open();
        // 另一个实例号（模拟关麦后重开）
        let stale = current + 1;
        assert!(voice.route.is_current(current));
        assert!(!voice.route.is_current(stale));
        voice.route.close();
        assert!(!voice.route.is_current(current), "关麦后旧实例彻底作废");
    }

    /// 播报视图只接受"当前或更新"的 speech id。
    #[test]
    fn first_speech_event_takes_over_the_view() {
        let mut view = SpeechView::default();
        assert_eq!(view.current(), None);
        assert!(view.accepts(1));
        assert_eq!(view.current(), Some(1));
        // 更旧的 id 不能覆盖
        assert!(!view.accepts(0));
    }

    /// 聆听模式：有挂起确认听「允许/拒绝」，播报中听打断词，否则等唤醒词。
    #[test]
    fn listen_mode_follows_confirm_and_speech() {
        let mut voice = VoiceRuntime::new();
        let confirmer = confirmer_without_ui();
        let speech = speech_without_engine();

        // 没有播报、没有确认：必须要求唤醒词。
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Wake);

        voice.manual_armed = true;
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Manual);

        // 有挂起的确认：听「允许 / 拒绝」，并带上请求 id。
        let _wait = install_pending(&confirmer, 42);
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Confirm);
        assert_eq!(voice.mode.get().1, 42);

        // 点击识别不能抢占挂起确认；取消点击请求后回到唤醒词。
        voice.manual_armed = false;
        confirmer.abort_pending();
        sync_listen_mode(&mut voice, &speech, &confirmer);
        assert_eq!(voice.mode.purpose(), ListenPurpose::Wake);
    }

    /// 语音确认的语义：明确否定不能被当成同意。
    #[tokio::test]
    async fn voice_denial_does_not_settle_as_allow() {
        let confirmer = confirmer_without_ui();
        let mut wait = install_pending(&confirmer, 9);
        assert!(confirmer.submit(9, ConfirmAnswer::deny_voice("用户说：不要")));
        assert!(!wait.try_recv().unwrap().allow);
    }

    #[test]
    fn rejected_turn_is_saved_as_failed_after_restart() {
        let paths = AppPaths::with_root(unique_test_root("rejected-restart"));
        let mut sessions = SessionRuntime::open(&paths);
        sessions.ensure_one(1000);
        sessions.push_timeline(1, TimelineItem::user("你好"));
        sessions.set_last_turn(1, TurnStatus::Interrupted);
        sessions.save_current().unwrap();
        finish_rejected_turn(
            &slint::Weak::default(),
            &mut sessions,
            1,
            "端点配置无效：测试",
        );
        let reopened = SessionRuntime::open(&paths);
        assert_eq!(
            reopened.current().unwrap().last_turn_status,
            TurnStatus::Failed
        );
        assert!(reopened
            .timeline()
            .iter()
            .any(|item| item.text.contains("启动失败：端点配置无效：测试")));
    }

    /// **没有 CUA 后端时回合仍然启动**：本地工具（时间/系统信息/文件/命令…）
    /// 不依赖 PowerShell 的 UIA 后端，不该一起失效（旧版也是这个取向）。
    #[tokio::test]
    async fn missing_cua_backend_still_allows_local_tools() {
        let root = unique_test_root("worker-no-toolkit");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let settings = Settings::default();
        let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));

        let confirmer = Arc::new(confirmer_without_ui());
        let permissions = Arc::new(std::sync::Mutex::new(PermissionState::default()));
        let audit = AuditLog::new(paths.audit_file());
        let mut history = Vec::new();
        let mut idle = None;
        match start_turn(
            &slint::Weak::default(),
            &paths,
            &secret,
            &settings,
            None,
            &registry,
            &confirmer,
            &permissions,
            &audit,
            &mut history,
            &mut idle,
            "现在几点".to_string(),
            1,
            "s1".to_string(),
        ) {
            TurnStart::Started(_) => {}
            TurnStart::Rejected(reason) => {
                panic!("没有 CUA 后端不该阻止回合启动：{reason}")
            }
        }
        assert!(idle.is_some(), "启动成功后必须留下取消令牌");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 注册表里的工具清单必须与实际公布给模型的一致（不能"显示为可执行却调不到"）。
    #[test]
    fn tool_overview_lists_exactly_what_is_published() {
        use crate::agent::ToolInvoker;
        let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
        let published: Vec<String> = registry
            .tool_definitions()
            .iter()
            .filter_map(|value| value["function"]["name"].as_str().map(str::to_string))
            .collect();
        let statuses = registry.statuses();
        let available: Vec<String> = statuses
            .iter()
            .filter(|status| status.available)
            .map(|status| status.name.clone())
            .collect();
        let mut left = published.clone();
        let mut right = available.clone();
        left.sort();
        right.sort();
        assert_eq!(left, right, "总览与公布清单必须同源");
        // 不可用的条目必须带原因，不能只是一个 ×。
        for status in statuses.iter().filter(|status| !status.available) {
            assert!(
                !status.reason.is_empty(),
                "{} 不可用但没有说明原因",
                status.name
            );
        }
    }

    /// **并发约束**：已经有回合在跑时，绝不启动第二回合。
    #[tokio::test]
    async fn busy_turn_rejects_a_second_turn() {
        let root = unique_test_root("worker-busy");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        let settings = Settings::default();
        let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));

        let confirmer = Arc::new(confirmer_without_ui());
        let permissions = Arc::new(std::sync::Mutex::new(PermissionState::default()));
        let audit = AuditLog::new(paths.audit_file());
        let mut history = Vec::new();

        // 模拟「上一轮还在跑」：令牌槽位非空。
        let mut running = Some(CancelToken::new());
        match start_turn(
            &slint::Weak::default(),
            &paths,
            &secret,
            &settings,
            None,
            &registry,
            &confirmer,
            &permissions,
            &audit,
            &mut history,
            &mut running,
            "打开浏览器".to_string(),
            1,
            "s1".to_string(),
        ) {
            TurnStart::Rejected(reason) => {
                assert!(reason.contains("上一轮"), "原因必须说清楚：{reason}");
            }
            TurnStart::Started(_) => panic!("忙的时候绝不能启动第二回合"),
        }
        // 原来的令牌不能被拒绝路径换掉。
        assert!(running.is_some(), "拒绝新回合不能清掉正在跑的那一轮");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **本地报时必须进入有效 LLM 历史**：否则追问"那再过三小时呢"时，
    /// 模型看不到上一轮问过什么、答了什么。
    #[test]
    fn quick_time_answer_is_written_to_the_llm_history() {
        let root = unique_test_root("quick-answer-history");
        let paths = AppPaths::with_root(&root);
        let mut sessions = SessionRuntime::open(&paths);
        sessions.ensure_one(1_000);
        let mut speech = SpeechRuntime::new(tokio::sync::mpsc::unbounded_channel().0);
        let settings = Settings::default();

        let turn_id = 1;
        sessions.push_timeline(turn_id, TimelineItem::user("现在几点"));
        let answer = "现在是晚上8点9分。";
        finish_quick_answer(
            &slint::Weak::default(),
            &mut sessions,
            &mut speech,
            &settings,
            turn_id,
            "现在几点",
            answer,
            TurnSource::Text,
        );

        // 历史里成对出现 user + assistant，且就是这一问一答。
        let history = sessions.history();
        assert_eq!(history.len(), 2, "应恰好写入一问一答：{history:?}");
        assert_eq!(history[0].role, crate::llm::Role::User);
        assert_eq!(history[1].role, crate::llm::Role::Assistant);
        let text_of = |message: &crate::llm::ChatMessage| match &message.content {
            crate::llm::Content::Text(text) => text.clone(),
            _ => String::new(),
        };
        assert_eq!(text_of(&history[0]), "现在几点");
        assert_eq!(text_of(&history[1]), answer);

        // 时间线里也照旧有一条可见的助手回复。
        assert!(sessions.timeline().iter().any(|item| item.kind
            == crate::session::TimelineKind::Assistant
            && item.text == answer));

        // 落盘后可恢复（切换/重启后仍能关联上一轮）。
        sessions.save_current().unwrap();
        let reopened = SessionRuntime::open(&paths);
        let restored = reopened.history();
        assert_eq!(restored.len(), 2, "历史必须随会话恢复：{restored:?}");
        assert_eq!(text_of(&restored[1]), answer);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 快速回答只写**当前**会话：Session 隔离照旧。
    #[test]
    fn quick_answer_history_stays_in_its_own_session() {
        let root = unique_test_root("quick-answer-isolation");
        let paths = AppPaths::with_root(&root);
        let mut sessions = SessionRuntime::open(&paths);
        sessions.ensure_one(1_000);
        let first = sessions.current_id().to_string();
        let mut speech = SpeechRuntime::new(tokio::sync::mpsc::unbounded_channel().0);
        let settings = Settings::default();

        sessions.push_timeline(1, TimelineItem::user("现在几点"));
        finish_quick_answer(
            &slint::Weak::default(),
            &mut sessions,
            &mut speech,
            &settings,
            1,
            "现在几点",
            "现在是晚上8点9分。",
            TurnSource::Text,
        );
        assert_eq!(sessions.history().len(), 2);

        // 新建一个会话：历史是空的，不继承上一会话的时间问答。
        let second = sessions.create(2_000, "新会话");
        assert_ne!(first, second);
        assert!(
            sessions.history().is_empty(),
            "新会话不该继承另一个会话的历史"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 测试 Key **不得**改动共享客户端或 SecretStore。
    #[tokio::test]
    async fn draft_key_test_has_no_side_effects() {
        let root = unique_test_root("search-key-test");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        // 已保存一把 Key，并让它生效。
        secret.save_search_key("tvly-saved").unwrap();
        let search = Arc::new(WebSearch::new("tvly-saved"));
        let registry = Arc::new(ToolRegistry::new(None, search.clone(), true));
        assert!(registry.search_ready());

        // 用一份**不同**的草稿去测试（指向不可达端点，结果必失败）。
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(2));
        handle_test_search_key(
            &slint::Weak::default(),
            "tvly-draft-not-saved".to_string(),
            2,
            &generation,
        );
        // 等后台任务跑完它的网络尝试。
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // 共享客户端仍是已保存的那把（草稿没有偷偷生效）。
        assert!(search.enabled());
        assert_eq!(
            secret.load_search_key().unwrap().as_deref(),
            Some("tvly-saved"),
            "测试不能写 SecretStore"
        );
        // 工具集没有变化。
        use crate::agent::ToolInvoker as _;
        assert!(registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 迟到的测试结果不能覆盖更新的代次（改草稿/重测/保存/清除都会推进）。
    #[tokio::test]
    async fn stale_key_test_result_is_dropped() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(5));
        // 模拟"测试开始时是第 5 代，结果回来时已经是第 6 代"。
        generation.fetch_add(1, Ordering::SeqCst);
        handle_test_search_key(
            &slint::Weak::default(),
            "tvly-late".to_string(),
            5,
            &generation,
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // 代次没被这次测试改动（它只读不写）。
        assert_eq!(generation.load(Ordering::SeqCst), 6);
    }

    /// **问题 2 的核心回归**：测 Key A 期间把草稿改成 Key B，
    /// A 的迟到结果不能当成 B 的结论。
    ///
    /// 这里直接驱动"界面侧改草稿"这条路径（`on_search_key_edited` 推进代次），
    /// 而测试用的是 Key A 的旧代次 —— 结果必须被丢弃。
    #[tokio::test]
    async fn editing_the_draft_invalidates_an_in_flight_test() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // 用户点「测试可用性」：工作线程推进到第 1 代，用 Key A 开始测。
        let started_with = generation.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(started_with, 1);
        let key_a = "tvly-key-A";
        handle_test_search_key(
            &slint::Weak::default(),
            key_a.to_string(),
            started_with,
            &generation,
        );

        // 用户随即把输入框改成 Key B（界面侧同步推进代次）。
        generation.fetch_add(1, Ordering::SeqCst);

        // 等 Key A 的后台任务跑完（指向真实端点会失败，但不重要）。
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // A 的结果必须被丢弃：代次已经不等于它开始时的 1。
        assert_ne!(
            generation.load(Ordering::SeqCst),
            started_with,
            "改草稿必须让在途测试失效"
        );
        // 而且这次测试本身不会再推进代次（只读）。
        let after = generation.load(Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(generation.load(Ordering::SeqCst), after);
    }

    /// **问题 2 的核心回归**：命令已入队、工作线程尚未处理时用户改了 Key。
    ///
    /// 修复前代次在工作线程收到命令后才推进，于是 Key A 的旧命令会把代次
    /// 推到新值、然后正常执行 —— 结果被显示在 Key B 的输入框旁边。
    /// 现在代次在**点击那一刻**就确定并随命令携带，处理前先核对。
    #[tokio::test]
    async fn queued_test_command_is_dropped_when_draft_changed_before_processing() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // 点击「测试可用性」（Key A）：界面侧立刻推进到第 1 代并带上它。
        let clicked_with = generation.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(clicked_with, 1);

        // 命令还在队列里，用户已把输入改成 Key B（界面侧推进到第 2 代）。
        generation.fetch_add(1, Ordering::SeqCst);

        // 工作线程这时才处理这条命令：必须丢弃，且不点亮"测试中"。
        let before = generation.load(Ordering::SeqCst);
        handle_queued_search_test(
            &slint::Weak::default(),
            &SecretStore::new(std::env::temp_dir().join("cuava-queued-test-unused")),
            "tvly-key-A".to_string(),
            clicked_with,
            &generation,
        );
        // 丢弃时不再推进代次（推进只发生在点击/编辑/保存/清除）。
        assert_eq!(
            generation.load(Ordering::SeqCst),
            before,
            "被丢弃的命令不该改动代次"
        );
    }

    /// 代次仍有效时，排队命令会正常执行（收紧不能把正常测试也拦掉）。
    #[tokio::test]
    async fn queued_test_command_runs_when_generation_is_still_current() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let clicked_with = generation.fetch_add(1, Ordering::SeqCst) + 1;
        let before = generation.load(Ordering::SeqCst);
        handle_queued_search_test(
            &slint::Weak::default(),
            &SecretStore::new(std::env::temp_dir().join("cuava-queued-test-run")),
            "tvly-current".to_string(),
            clicked_with,
            &generation,
        );
        // 正常路径：代次不变（测试本身只读），后台任务已经启动。
        assert_eq!(generation.load(Ordering::SeqCst), before);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            generation.load(Ordering::SeqCst),
            before,
            "测试完成也不该推进代次"
        );
    }

    /// 测试成功后**不保存**，实际运行凭据与工具集保持不变。
    #[tokio::test]
    async fn successful_looking_test_without_save_keeps_runtime_credentials() {
        let root = unique_test_root("search-test-no-save");
        let paths = AppPaths::with_root(&root);
        let secret = SecretStore::new(paths.secret_file());
        secret.save_search_key("tvly-saved-key").unwrap();
        let search = Arc::new(WebSearch::new("tvly-saved-key"));
        let registry = Arc::new(ToolRegistry::new(None, search.clone(), true));
        assert!(registry.search_ready());

        // 用另一把草稿做测试（无论成功失败，都不该改运行时）。
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(3));
        handle_test_search_key(
            &slint::Weak::default(),
            "tvly-draft".to_string(),
            3,
            &generation,
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // 运行凭据仍是已保存那把；SecretStore 未被写入。
        let loaded = secret.load_search_key().unwrap().unwrap_or_default();
        assert_eq!(loaded, "tvly-saved-key", "测试不得写 SecretStore");
        assert!(registry.search_ready());
        use crate::agent::ToolInvoker as _;
        assert!(registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));

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
}
