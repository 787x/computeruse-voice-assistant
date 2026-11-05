//! Computer Use 子系统：原生后端桥接 + 12 个工具。
//!
//! 与 [`crate::tools`] 的分工：这里只管**界面控制**（屏幕/窗口/键鼠/剪贴板），
//! 本地命令、文件、进程、启动、系统信息与联网搜索在 `tools` 模块。
//! 两者通过 [`crate::tools::registry::ToolRegistry`] 组合后交给同一个 Agent。

pub mod bridge;
pub mod invoker;
pub mod tools;

pub use bridge::{BridgeError, BridgeOptions, PwshBridge, ReadyInfo};
pub use tools::{
    tool_specs, tools_for_request, ArgError, CuaToolkit, ImageAttachment, ToolError, ToolSpec,
    DEFAULT_TOOL_NAMES,
};
