//! Rust 本地工具（非音乐、非 CUA）与统一工具注册表。
//!
//! 模块边界（与 `cua/` 平级，互不依赖内部实现）：
//!  - [`spec`]：工具定义与参数宽化（CUA 与本地共用同一套 schema 形态）；
//!  - [`error`]：`ArgError` / `LocalError` / `ToolError`（取消语义统一）；
//!  - [`process`]：PowerShell 子进程的公共部分（超时、取消、进程树、输出上限）；
//!  - [`shell`]：`run_shell`；
//!  - [`files`]：`files`（list / read / write / copy / move / delete / info）；
//!  - [`procs`]：`app_control` / `kill_app`；
//!  - [`launch`]：`launch_app`（开始菜单级名字解析）；
//!  - [`sysinfo`]：`system_info`（直接 Win32，不跑 PowerShell）；
//!  - [`clock_tool`]：`get_current_time` 与"报时间"的直接回答路径；
//!  - [`web_search`]：Tavily 联网搜索（独立 Key，条件启用）；
//!  - [`registry`]：把 CUA 与本地工具组合起来交给同一个 Agent。
//!
//! 这些工具**都不**经过 `cua.ps1`：CUA 引擎只负责界面控制，
//! 把文件/命令塞进那条协议会同时污染两边的语义与取消路径。

pub mod clock_tool;
pub mod error;
pub mod files;
pub mod launch;
pub mod process;
pub mod procs;
pub mod registry;
pub mod shell;
pub mod spec;
pub mod sysinfo;
pub mod web_search;

pub use error::{ArgError, LocalError, ToolError};
pub use registry::{
    ToolInventory, ToolRegistry, ToolSource, ToolStatus, LOCAL_TOOL_NAMES, TIME_TOOL_NAME,
};
pub use spec::ToolSpec;
pub use web_search::WebSearch;
