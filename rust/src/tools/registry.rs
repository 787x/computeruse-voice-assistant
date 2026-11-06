//! 统一的工具注册表：把 12 个 CUA 工具与 Rust 本地的非 CUA 工具交给**同一个** Agent。
//!
//! 为什么需要它：PR12 要恢复旧版全部非音乐工具，但**不能**再造第二个 Agent、
//! 第二套权限门或第二个确认框。所以只在这一处做组合：
//!
//! ```text
//! Agent ──> ToolInvoker（本注册表）
//!              ├─ CuaToolkit（12 个，PowerShell + UIA，原样保留）
//!              ├─ run_shell / files / app_control / kill_app / launch_app / system_info
//!              ├─ get_current_time（只读）
//!              └─ web_search（仅有 Tavily Key 时公布）
//! ```
//!
//! 边界约束（都很容易在扩展时被破坏，写在这里提醒）：
//!  - **工具定义只在这里汇总**：`tool_definitions()` 是模型能看到什么的唯一来源，
//!    没实现的工具不会出现在里面，因此也不会"显示为可执行"；
//!  - **执行只在这里分发**：名称 → 实现是显式 match，未知名称一律报错，
//!    不存在"名字碰巧对上就执行"的路径；
//!  - **权限仍在 Agent 之前**：本注册表不做任何授权判断，`PermissionGate` 先跑，
//!    被拒绝的调用根本到不了这里（所以这里**不可能**成为绕过权限的旁路）；
//!  - **阻塞 I/O 不在调用方线程**：本地工具自己用 `spawn_blocking` / 子进程，
//!    调用方（Agent 任务、界面工作线程）只 await。

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::cancel::CancelToken;
use crate::cua::tools::CuaToolkit;
use crate::tools::error::ToolError;
use crate::tools::spec::ToolSpec;
use crate::tools::web_search::{self, WebSearch};
use crate::tools::{clock_tool, files, launch, procs, shell, sysinfo};

/// 非音乐、非 CUA 的本地工具名（旧版默认 6 个）。顺序即模型看到顺序。
pub const LOCAL_TOOL_NAMES: &[&str] = &[
    "run_shell",
    "files",
    "app_control",
    "kill_app",
    "launch_app",
    "system_info",
];

/// 只读的本地时间工具（**额外**于上面 6 个，因为它不属于旧版那套工具）。
pub const TIME_TOOL_NAME: &str = clock_tool::TOOL_NAME;

/// 工具来源（界面"可用工具总览"与统计用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// Computer Use（PowerShell + UI Automation）。
    ComputerUse,
    /// Rust 本地实现。
    Local,
    /// 联网搜索（条件启用）。
    Search,
}

impl ToolSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::ComputerUse => "Computer Use",
            Self::Local => "本地",
            Self::Search => "联网",
        }
    }
}

/// 一个工具在**当前配置下**的可用状态。
#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub name: String,
    pub source: ToolSource,
    pub read_only: bool,
    /// 是否真的会发给模型（`false` = 不存在，或缺少前置配置）。
    pub available: bool,
    /// 不可用的原因（可用时为空）。
    pub reason: String,
}

/// 工具总数与条件启用数的统计（Done Criteria 要求区分这两者）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInventory {
    /// Computer Use 工具数（应为 12）。
    pub cua: usize,
    /// 本地非音乐工具数（应为 6）。
    pub local: usize,
    /// 本地时间工具（0 或 1）。
    pub time: usize,
    /// 联网搜索是否启用。
    pub search_enabled: bool,
}

impl ToolInventory {
    /// 无条件可用的工具（CUA + 本地 + 时间）。
    pub fn always(&self) -> usize {
        self.cua + self.local + self.time
    }

    /// 当前实际公布给模型的工具总数。
    pub fn total(&self) -> usize {
        self.always() + usize::from(self.search_enabled)
    }

    /// 条件启用的工具数（目前只有 web_search）。
    pub fn conditional(&self) -> usize {
        usize::from(self.search_enabled)
    }

    /// "12 个 CUA + 6 个本地 + 1 个时间 + 0/1 个搜索"这样的一句话。
    pub fn summary(&self) -> String {
        format!(
            "工具总数 {}：Computer Use {} + 本地 {} + 时间 {} + 条件启用 {}（联网搜索{}）",
            self.total(),
            self.cua,
            self.local,
            self.time,
            self.conditional(),
            if self.search_enabled {
                "已启用"
            } else {
                "未配置 Key，不公布"
            }
        )
    }
}

/// 工具注册表。**克隆它没有意义**：内部索引是有状态的，用 `Arc` 共享一份。
pub struct ToolRegistry {
    /// CUA 后端；`None` = 找不到 `cua.ps1`（此时 CUA 工具不公布，本地工具照常可用）。
    cua: Option<Arc<CuaToolkit>>,
    search: Arc<WebSearch>,
    /// 「启用联网搜索」开关（来自 `Settings::search.enabled`）。
    ///
    /// 用 `AtomicBool` 而不是每轮重建注册表：设置保存后要**立刻**影响下一轮
    /// 的工具定义与运行时调用，而注册表是共享的（应用索引与搜索缓存都在里面）。
    search_enabled: std::sync::atomic::AtomicBool,
    /// 应用索引（`launch_app` 用；带 TTL，跨调用复用）。
    app_index: tokio::sync::Mutex<launch::AppIndex>,
    /// `run_shell` 的默认工作目录（用户目录）。
    home: PathBuf,
    /// 最近一次索引建立失败的说明（界面"可用工具总览"里如实显示）。
    index_error: std::sync::Mutex<Option<String>>,
}

impl ToolRegistry {
    /// 构造注册表。
    ///
    /// `cua` 为 `None` 时**只**公布本地工具：没有 CUA 后端不该让"看时间"""
    /// 查系统状态"一起失效（旧版也是这个取向）。
    ///
    /// `search_enabled` 是用户设置里的开关：**开关关闭时即使有 Key 也不公布**。
    pub fn new(cua: Option<Arc<CuaToolkit>>, search: Arc<WebSearch>, search_enabled: bool) -> Self {
        Self {
            cua,
            search,
            search_enabled: std::sync::atomic::AtomicBool::new(search_enabled),
            app_index: tokio::sync::Mutex::new(launch::AppIndex::new()),
            home: user_home(),
            index_error: std::sync::Mutex::new(None),
        }
    }

    /// 更新「启用联网搜索」开关（保存设置后调用，立即生效）。
    pub fn set_search_enabled(&self, enabled: bool) {
        self.search_enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }
    /// 用户是否启用了联网搜索（不看有没有 Key）。
    pub fn search_setting_enabled(&self) -> bool {
        self.search_enabled
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `web_search` 此刻是否**真的可用**：开关打开**且**有 Key。
    ///
    /// 这是公布、执行、统计、状态提示的**唯一**判据——四者必须一致，
    /// 否则会出现"总览说可用、请求里却没有"或"模型调用了被拒"。
    pub fn search_ready(&self) -> bool {
        self.search_setting_enabled() && self.search.enabled()
    }

    /// 测试/隔离环境用：显式指定默认工作目录。
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = home.into();
        self
    }

    /// 联网搜索客户端（设置页需要它来换 Key / 测试可用性）。
    pub fn search(&self) -> &Arc<WebSearch> {
        &self.search
    }

    /// 当前是否有 CUA 后端。
    pub fn has_cua(&self) -> bool {
        self.cua.is_some()
    }

    /// 本地工具的工具定义（不含 CUA，便于测试与文档生成）。
    pub fn local_specs() -> Vec<ToolSpec> {
        vec![
            shell::spec(),
            files::spec(),
            procs::control_spec(),
            procs::kill_spec(),
            launch::spec(),
            sysinfo::spec(),
            clock_tool::spec(),
        ]
    }

    /// 当前配置下**会发给模型**的工具总数与构成。
    pub fn inventory(&self) -> ToolInventory {
        ToolInventory {
            cua: if self.cua.is_some() {
                crate::cua::tools::DEFAULT_TOOL_NAMES.len()
            } else {
                0
            },
            local: LOCAL_TOOL_NAMES.len(),
            time: 1,
            search_enabled: self.search_ready(),
        }
    }

    /// 全部工具的可用状态（界面"可用工具总览"直接渲染这个）。
    ///
    /// 这里**如实**反映"会不会公布"：CUA 后端缺失时 12 个 CUA 工具标为不可用，
    /// 没有 Key 时 `web_search` 标为不可用并给出原因。
    pub fn statuses(&self) -> Vec<ToolStatus> {
        let mut out = Vec::new();
        let cua_available = self.cua.is_some();
        for name in crate::cua::tools::DEFAULT_TOOL_NAMES {
            out.push(ToolStatus {
                name: (*name).to_string(),
                source: ToolSource::ComputerUse,
                read_only: crate::cua::tools::tool_specs()
                    .iter()
                    .find(|spec| spec.name == *name)
                    .map(|spec| spec.read_only)
                    .unwrap_or(true),
                available: cua_available,
                reason: if cua_available {
                    String::new()
                } else {
                    "找不到 cua.ps1：Computer Use 后端不可用".to_string()
                },
            });
        }
        for spec in Self::local_specs() {
            out.push(ToolStatus {
                name: spec.name.to_string(),
                source: ToolSource::Local,
                read_only: spec.read_only,
                available: true,
                reason: String::new(),
            });
        }
        // 与 `tool_definitions()` **同一个判据**：开关关闭时即使有 Key 也不可用。
        let search_ready = self.search_ready();
        let reason = if search_ready {
            String::new()
        } else if !self.search_setting_enabled() {
            "「启用联网搜索」是关闭的（有 Key 也不公布；打开后立即生效）".to_string()
        } else {
            "没有配置联网搜索的 API Key（不会公布给模型）".to_string()
        };
        out.push(ToolStatus {
            name: web_search::TOOL_NAME.to_string(),
            source: ToolSource::Search,
            read_only: true,
            available: search_ready,
            reason,
        });
        // 应用索引失败不影响工具可用，但要让用户看得见（否则"按名字启动"会莫名失败）。
        if let Ok(error) = self.index_error.lock() {
            if let Some(error) = error.as_ref() {
                if let Some(entry) = out.iter_mut().find(|item| item.name == "launch_app") {
                    entry.reason = error.clone();
                }
            }
        }
        out
    }

    /// 一行汇总（状态栏用）。
    pub fn status_line(&self) -> String {
        self.inventory().summary()
    }

    /// 应用索引的当前状态（设置页/诊断）。
    pub async fn index_status(&self) -> String {
        let index = self.app_index.lock().await;
        launch::describe_index(&index)
    }

    /// 预热应用索引（可选；第一次 `launch_app` 也会自动建）。
    ///
    /// **不要在工作线程的启动路径上直接 await 它**：建索引要跑一次 PowerShell
    /// （约 1~2 秒，冷启动更久），失败时还可能等满超时——那段时间里工作线程
    /// 收不到"打开设置""发消息"等命令，用户看到的是界面没反应。
    /// 调用方应该用 [`Self::spawn_warm_app_index`]。
    pub async fn warm_app_index(&self, cancel: &CancelToken) {
        let result = {
            let mut index = self.app_index.lock().await;
            index.ensure(cancel, false).await
        };
        if let Ok(mut slot) = self.index_error.lock() {
            *slot = result.err().map(|error| error.user_message());
        }
    }

    /// 在**后台任务**里预热应用索引：立即返回，不占用调用方。
    ///
    /// 需要 `Arc<Self>`：任务要活过这次调用。首次 `launch_app` 仍会按需建索引，
    /// 所以预热失败/没跑完都不影响可用性。
    pub fn spawn_warm_app_index(self: &Arc<Self>) {
        let registry = self.clone();
        tokio::spawn(async move {
            // 用独立令牌：预热不该被某个回合的取消连带打断。
            registry.warm_app_index(&CancelToken::new()).await;
        });
    }
}

/// 用户目录（`run_shell` 的默认 cwd，与旧版 `process.env.USERPROFILE` 一致）。
fn user_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_dir())
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

impl crate::agent::ToolInvoker for ToolRegistry {
    fn tool_definitions(&self) -> Vec<Value> {
        let mut out = Vec::new();
        // 1) Computer Use：只有后端在才公布（否则模型会去调一个一定失败的工具）。
        if self.cua.is_some() {
            out.extend(crate::cua::tools::tools_for_request(
                crate::cua::tools::DEFAULT_TOOL_NAMES,
            ));
        }
        // 2) 本地工具（含只读时间）。
        out.extend(Self::local_specs().iter().map(ToolSpec::to_request_value));
        // 3) 联网搜索：**开关打开且配了 Key 才公布**。
        //    关掉开关不清 Key，重新打开立即恢复（Key 仍在 SecretStore 里）。
        if self.search_ready() {
            out.push(web_search::spec().to_request_value());
        }
        out
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a CancelToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::agent::ToolExecution, ToolError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.dispatch(name, args, cancel).await })
    }
}

impl ToolRegistry {
    /// 当前由 bridge 管理的 CUA 后端子进程 PID。
    ///
    /// 没有 CUA 后端（找不到 `cua.ps1`）或后端尚未启动时返回空：
    /// 此时没有任何进程需要额外保护。
    ///
    /// **只返回此刻有效的那一个**：bridge 在 spawn 时写入、在 kill/崩溃/启动
    /// 失败时清零，所以这里不会拿到已经退出的历史 PID（PID 复用不会误保护）。
    pub fn managed_cua_pids(&self) -> Vec<u32> {
        self.cua
            .as_ref()
            .and_then(|toolkit| toolkit.bridge().current_child_pid())
            .into_iter()
            .collect()
    }

    /// 执行一次工具调用。
    ///
    /// 返回 `ToolExecution`（文本 + 可选图像）：截图必须走**真正的**
    /// image content part 才能被多模态模型看见，塞进文本会瞬间撑爆上下文。
    async fn dispatch(
        &self,
        name: &str,
        args: &Value,
        cancel: &CancelToken,
    ) -> Result<crate::agent::ToolExecution, ToolError> {
        match name {
            // ---- 本地工具（纯文本结果） ----
            "run_shell" => Ok(text_of(
                shell::run(args, cancel, Some(self.home.as_path())).await?,
            )),
            "files" => Ok(text_of(files::run(args, cancel).await?)),
            /*
             * `app_control` / `kill_app` 都要知道"当前受管理的 CUA 子进程"。
             *
             * `powershell.exe` 位于 `C:\Windows\System32`，路径判定抓不到它：
             * 只有 bridge 知道自己此刻管着哪个 PID，所以这里**每次执行前**
             * 现取一次（不缓存、不留历史），进程重启后自然拿到新值。
             */
            "app_control" => {
                let managed = self.managed_cua_pids();
                Ok(text_of(procs::run_control(args, cancel, &managed).await?))
            }
            "kill_app" => {
                let managed = self.managed_cua_pids();
                Ok(text_of(procs::run_kill(args, cancel, &managed).await?))
            }
            "launch_app" => {
                let mut index = tokio::select! {
                    guard = self.app_index.lock() => guard,
                    _ = cancel.cancelled() => return Err(ToolError::cancelled()),
                };
                let result = launch::run(&mut index, args, cancel).await;
                drop(index);
                Ok(text_of(result?))
            }
            "system_info" => Ok(text_of(sysinfo::run(args, cancel).await?)),
            TIME_TOOL_NAME => Ok(text_of(clock_tool::run(args, cancel).await?)),
            web_search::TOOL_NAME => {
                // 运行时也要服从同一个判据：关闭开关后即使模型硬调也不能搜。
                if !self.search_ready() {
                    return Err(ToolError::failed(if self.search_setting_enabled() {
                        "联网搜索没有配置 API Key（在设置的「联网与工具」里填写）".to_string()
                    } else {
                        "联网搜索已在设置里关闭（打开后立即生效）".to_string()
                    }));
                }
                Ok(text_of(
                    web_search::run(self.search.clone(), args, cancel).await?,
                ))
            }
            // ---- Computer Use（截图要带回图像） ----
            other => match self.cua.as_ref() {
                Some(toolkit) if crate::cua::tools::DEFAULT_TOOL_NAMES.contains(&other) => {
                    let output = toolkit.invoke(other, args, cancel).await?;
                    Ok(crate::agent::ToolExecution {
                        text: output.text,
                        image: output.image,
                    })
                }
                _ => Err(ToolError::Arg(crate::tools::error::ArgError::new(format!(
                    "未知工具：{other}"
                )))),
            },
        }
    }
}

/// 纯文本结果。
fn text_of(text: String) -> crate::agent::ToolExecution {
    crate::agent::ToolExecution { text, image: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolInvoker;

    fn registry() -> ToolRegistry {
        ToolRegistry::new(None, Arc::new(WebSearch::new("")), true)
    }

    #[test]
    fn local_tool_names_match_the_six_legacy_tools_plus_time() {
        assert_eq!(LOCAL_TOOL_NAMES.len(), 6);
        let specs = ToolRegistry::local_specs();
        assert_eq!(specs.len(), 7, "6 个旧工具 + 1 个只读时间");
        for name in LOCAL_TOOL_NAMES {
            assert!(
                specs.iter().any(|spec| spec.name == *name),
                "缺少工具定义：{name}"
            );
        }
        assert!(specs.iter().any(|spec| spec.name == TIME_TOOL_NAME));
    }

    #[test]
    fn without_cua_only_local_tools_are_published() {
        let registry = registry();
        let definitions = registry.tool_definitions();
        assert_eq!(definitions.len(), 7, "本地 6 + 时间 1");
        let names: Vec<&str> = definitions
            .iter()
            .filter_map(|value| value["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"run_shell"));
        assert!(names.contains(&"get_current_time"));
        assert!(!names.contains(&"web_search"), "没有 Key 不能公布");
        assert!(!names.contains(&"screenshot"), "没有 CUA 后端不能公布");
    }

    #[test]
    fn search_is_published_only_when_a_key_exists() {
        let search = Arc::new(WebSearch::new(""));
        let registry = ToolRegistry::new(None, search.clone(), true);
        assert!(!registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));
        assert_eq!(registry.inventory().conditional(), 0);

        // 运行期换 Key：**不需要重启**，下一次取定义就能看到。
        assert!(search.set_api_key("tvly-test-key"));
        assert!(registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));
        assert_eq!(registry.inventory().conditional(), 1);
        assert!(registry.inventory().summary().contains("已启用"));
    }

    /// **关闭开关时即使有 Key 也不公布**——这是本 PR 修的缺陷之一。
    ///
    /// 公布、统计、状态提示必须与"实际会发给 LLM 的定义"完全一致，
    /// 否则会出现"总览说可用、请求里却没有"。
    #[test]
    fn disabled_setting_hides_search_even_with_a_key() {
        let search = Arc::new(WebSearch::new("tvly-saved-key"));
        let registry = ToolRegistry::new(None, search.clone(), false);
        assert!(search.enabled(), "Key 本身还在");
        assert!(!registry.search_ready(), "开关关闭时不算可用");

        let names: Vec<String> = registry
            .tool_definitions()
            .iter()
            .filter_map(|value| value["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(!names.contains(&"web_search".to_string()), "{names:?}");
        assert_eq!(registry.inventory().conditional(), 0);
        assert_eq!(registry.inventory().total(), 7, "7 = 本地 6 + 时间 1");

        // 工具总览与定义同源：不可用且写明原因。
        let status = registry
            .statuses()
            .into_iter()
            .find(|item| item.name == "web_search")
            .unwrap();
        assert!(!status.available);
        assert!(
            status.reason.contains("关闭"),
            "原因应说明开关是关的：{}",
            status.reason
        );

        // 重新打开**立即**恢复（Key 没被清掉，不需要重启）。
        registry.set_search_enabled(true);
        assert!(registry.search_ready());
        assert!(registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));
        assert_eq!(registry.inventory().conditional(), 1);
    }

    /// 关掉开关不该清 Key：重新打开时仍然用原来那把。
    #[test]
    fn toggling_the_setting_never_touches_the_key() {
        let search = Arc::new(WebSearch::new("tvly-keep-me"));
        let registry = ToolRegistry::new(None, search.clone(), true);
        registry.set_search_enabled(false);
        assert!(search.enabled(), "关开关不能清 Key");
        registry.set_search_enabled(true);
        assert!(search.enabled());
        assert!(registry.search_ready());
    }

    /// 运行时调用同样服从开关：关闭后即使模型硬调也拒绝，且原因可读。
    #[tokio::test]
    async fn disabled_setting_blocks_the_runtime_call() {
        let search = Arc::new(WebSearch::new("tvly-key"));
        let registry = ToolRegistry::new(None, search.clone(), false);
        let error = registry
            .invoke(
                web_search::TOOL_NAME,
                &serde_json::json!({"query": "天气"}),
                &CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(!error.is_cancelled());
        assert!(
            error.user_message().contains("关闭"),
            "{}",
            error.user_message()
        );

        // 打开后不再被"开关"拦下（这里会走到网络并失败，但错误性质不同）。
        registry.set_search_enabled(true);
        let error = registry
            .invoke(
                web_search::TOOL_NAME,
                &serde_json::json!({"query": "天气"}),
                &CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            !error.user_message().contains("已在设置里关闭"),
            "{}",
            error.user_message()
        );
    }

    /// 没有 CUA 后端时没有需要额外保护的子进程。
    #[test]
    fn managed_cua_pids_is_empty_without_a_backend() {
        let registry = registry();
        assert!(registry.managed_cua_pids().is_empty());
    }

    /// 有后端但尚未启动时同样为空——不会凭空保护某个 PID。
    #[test]
    fn managed_cua_pids_is_empty_before_the_backend_starts() {
        let bridge =
            crate::cua::bridge::PwshBridge::new(crate::cua::bridge::BridgeOptions::new("nope.ps1"));
        let toolkit = Arc::new(CuaToolkit::new(bridge, None));
        let registry = ToolRegistry::new(Some(toolkit), Arc::new(WebSearch::new("")), true);
        assert!(
            registry.managed_cua_pids().is_empty(),
            "后端没起来时不该报告任何 PID"
        );
    }

    /// 后端登记了子进程后，registry 必须如实透出**那一个** PID；
    /// 清零后立刻不再透出（PID 复用不会误保护）。
    #[test]
    fn managed_cua_pids_reflects_the_current_backend_child() {
        let bridge =
            crate::cua::bridge::PwshBridge::new(crate::cua::bridge::BridgeOptions::new("nope.ps1"));
        let toolkit = Arc::new(CuaToolkit::new(bridge, None));
        // 直接把这只 toolkit 的 bridge 当成"已启动"，登记一个假 PID。
        toolkit.bridge().set_child_pid_for_test(Some(4321));
        let registry = ToolRegistry::new(Some(toolkit.clone()), Arc::new(WebSearch::new("")), true);
        assert_eq!(registry.managed_cua_pids(), vec![4321]);

        // 进程退出（bridge 清零）：registry 不再报告它。
        toolkit.bridge().set_child_pid_for_test(None);
        assert!(registry.managed_cua_pids().is_empty());
    }

    #[test]
    fn inventory_separates_always_on_from_conditional() {
        let registry = registry();
        let inventory = registry.inventory();
        assert_eq!(inventory.cua, 0, "没有 CUA 后端时不算 12 个");
        assert_eq!(inventory.local, 6);
        assert_eq!(inventory.time, 1);
        assert_eq!(inventory.always(), 7);
        assert_eq!(inventory.total(), 7);
        assert_eq!(inventory.conditional(), 0);
        assert!(inventory.summary().contains("工具总数 7"));
    }

    /// Done Criteria 的完整形态：**12 个 CUA + 6 个本地 + 1 个时间**，
    /// 配上 Key 之后再多 1 个 `web_search`。
    #[test]
    fn full_inventory_matches_the_done_criteria() {
        let bridge =
            crate::cua::bridge::PwshBridge::new(crate::cua::bridge::BridgeOptions::new("nope.ps1"));
        let toolkit = Arc::new(CuaToolkit::new(bridge, None));
        let search = Arc::new(WebSearch::new(""));
        let registry = ToolRegistry::new(Some(toolkit), search.clone(), true);

        // 没有搜索 Key：12 + 6 + 1 = 19。
        let inventory = registry.inventory();
        assert_eq!(inventory.cua, 12);
        assert_eq!(inventory.local, 6);
        assert_eq!(inventory.time, 1);
        assert_eq!(inventory.total(), 19);
        assert_eq!(inventory.conditional(), 0);
        assert_eq!(registry.tool_definitions().len(), 19);
        assert!(!registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));

        // 配上 Key：20，且条件启用数单独计 1。
        search.set_api_key("tvly-test");
        let inventory = registry.inventory();
        assert_eq!(inventory.total(), 20);
        assert_eq!(inventory.conditional(), 1);
        assert_eq!(inventory.always(), 19);
        assert_eq!(registry.tool_definitions().len(), 20);
        assert!(registry
            .tool_definitions()
            .iter()
            .any(|value| value["function"]["name"] == "web_search"));
        assert!(inventory.summary().contains("工具总数 20"));
        assert!(inventory.summary().contains("Computer Use 12"));
        assert!(inventory.summary().contains("本地 6"));
    }

    #[test]
    fn statuses_explain_why_something_is_unavailable() {
        let registry = registry();
        let statuses = registry.statuses();
        assert_eq!(statuses.len(), 12 + 6 + 1 + 1);
        let screenshot = statuses
            .iter()
            .find(|item| item.name == "screenshot")
            .unwrap();
        assert!(!screenshot.available);
        assert!(screenshot.reason.contains("cua.ps1"));
        let search = statuses
            .iter()
            .find(|item| item.name == web_search::TOOL_NAME)
            .unwrap();
        assert!(!search.available);
        assert!(search.reason.contains("API Key"));
        let files = statuses.iter().find(|item| item.name == "files").unwrap();
        assert!(files.available);
        assert!(!files.read_only, "files 可能写文件，不能标成只读");
    }

    #[tokio::test]
    async fn unknown_tool_is_rejected_instead_of_silently_succeeding() {
        let registry = registry();
        let cancel = CancelToken::new();
        let error = registry
            .invoke("definitely_not_a_tool", &serde_json::json!({}), &cancel)
            .await
            .unwrap_err();
        assert!(error.is_argument());
        assert!(error.user_message().contains("未知工具"));
    }

    #[tokio::test]
    async fn time_tool_runs_through_the_registry() {
        let registry = registry();
        let cancel = CancelToken::new();
        let execution = registry
            .invoke(TIME_TOOL_NAME, &serde_json::json!({}), &cancel)
            .await
            .unwrap();
        assert!(execution.text.contains("本地时间："), "{}", execution.text);
        assert!(execution.image.is_none());
    }

    #[tokio::test]
    async fn local_failure_is_reported_not_swallowed() {
        let registry = registry();
        let cancel = CancelToken::new();
        let error = registry
            .invoke(
                "files",
                &serde_json::json!({"action": "read", "path": "C:\\nope\\nope.txt"}),
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(!error.is_cancelled());
        assert!(error.user_message().contains("读取"));
    }

    #[test]
    fn home_defaults_to_a_directory_that_exists() {
        let registry = registry();
        assert!(registry.home.is_dir(), "默认 cwd 必须存在");
    }
}
