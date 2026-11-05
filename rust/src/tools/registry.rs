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
    pub fn new(cua: Option<Arc<CuaToolkit>>, search: Arc<WebSearch>) -> Self {
        Self {
            cua,
            search,
            app_index: tokio::sync::Mutex::new(launch::AppIndex::new()),
            home: user_home(),
            index_error: std::sync::Mutex::new(None),
        }
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
            search_enabled: self.search.enabled(),
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
        let search_enabled = self.search.enabled();
        out.push(ToolStatus {
            name: web_search::TOOL_NAME.to_string(),
            source: ToolSource::Search,
            read_only: true,
            available: search_enabled,
            reason: if search_enabled {
                String::new()
            } else {
                "没有配置联网搜索的 API Key（不会公布给模型）".to_string()
            },
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
    pub async fn warm_app_index(&self, cancel: &CancelToken) {
        let result = {
            let mut index = self.app_index.lock().await;
            index.ensure(cancel, false).await
        };
        if let Ok(mut slot) = self.index_error.lock() {
            *slot = result.err().map(|error| error.user_message());
        }
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
        // 3) 联网搜索：**没有 Key 就不公布**（旧版 `listTools` 也是这个语义）。
        if self.search.enabled() {
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
            "app_control" => Ok(text_of(procs::run_control(args, cancel).await?)),
            "kill_app" => Ok(text_of(procs::run_kill(args, cancel).await?)),
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
            web_search::TOOL_NAME => Ok(text_of(
                web_search::run(self.search.clone(), args, cancel).await?,
            )),
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
        ToolRegistry::new(None, Arc::new(WebSearch::new("")))
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
        let registry = ToolRegistry::new(None, search.clone());
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
        let registry = ToolRegistry::new(Some(toolkit), search.clone());

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
