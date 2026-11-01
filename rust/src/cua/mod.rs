//! Computer Use 子系统：原生后端桥接 + 12 个工具。

pub mod bridge;
pub mod invoker;
pub mod tools;

pub use bridge::{BridgeError, BridgeOptions, PwshBridge, ReadyInfo};
pub use tools::{
    tool_specs, tools_for_request, CuaToolkit, ImageAttachment, ToolError, ToolOutput, ToolSpec,
    DEFAULT_TOOL_NAMES,
};
