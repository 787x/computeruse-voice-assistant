//! 工具错误：CUA 与本地工具共用**同一套**错误语义。
//!
//! 为什么统一：Agent 只认 `ToolError`，它对"参数不合法 / 后端报错 / 本地执行失败 /
//! 被中断"的处理方式完全一样（`is_cancelled()` 决定是否按"状态未知"处理）。
//! 如果本地工具另起一套错误类型，取消语义与"已发起/未发起"的判定就会分叉。
//!
//! 三类错误的边界：
//!  - [`ArgError`]：参数不合法，**动作之前**拦下，可以放心说"没有执行"；
//!  - [`LocalError`]：本地工具（PowerShell 子进程 / 文件 / 进程）执行失败；
//!  - `BridgeError`：CUA 后端错误（含超时与协议错配）。

use crate::cua::bridge::BridgeError;

/// 参数错误（与旧版 `invalid arguments: …` 文案同形）。
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid arguments: {0}")]
pub struct ArgError(pub String);

impl ArgError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// 本地工具的错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum LocalError {
    /// 执行失败（含超时、子进程异常退出、路径不存在等），文案直接给模型看。
    #[error("{0}")]
    Failed(String),
    /// 回合被取消。**注意**：本地工具一旦已经启动子进程，
    /// 取消只代表"我们不再等结果"，副作用可能已经发生（调用方据此写"状态未知"）。
    #[error("已中断")]
    Cancelled,
}

impl LocalError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

/// 工具执行错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Arg(#[from] ArgError),
    #[error("{0}")]
    Bridge(#[from] BridgeError),
    #[error("{0}")]
    Local(#[from] LocalError),
}

impl ToolError {
    /// 交给模型/界面的说明：带上"怎么修"的提示，避免裸错误码。
    pub fn user_message(&self) -> String {
        match self {
            Self::Arg(error) => error.to_string(),
            Self::Bridge(error) => match error {
                BridgeError::Backend { code, message, .. } => {
                    format!(
                        "{message}\nHint: {}",
                        crate::cua::tools::action_error_hint(code)
                    )
                }
                other => other.user_message(),
            },
            Self::Local(error) => error.to_string(),
        }
    }

    /// 是否属于"已中断"。Agent 用它区分"工具失败"与"回合取消"。
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Bridge(BridgeError::Cancelled) | Self::Local(LocalError::Cancelled)
        )
    }

    /// 参数问题（可以确定"什么都没做"）。
    pub fn is_argument(&self) -> bool {
        matches!(self, Self::Arg(_))
    }

    /// 便捷构造：本地失败。
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Local(LocalError::Failed(message.into()))
    }

    /// 便捷构造：取消。
    pub fn cancelled() -> Self {
        Self::Local(LocalError::Cancelled)
    }
}

/// 让 `require_string(...)?` 这类返回 `Result<_, String>` 的辅助函数能直接在
/// `Result<_, ToolError>` 里使用：参数问题一律是 [`ArgError`]。
///
/// 只给 `String` 实现（不给任意 `E`）：否则任何错误都能悄悄变成"参数错误"，
/// 而 `is_argument()` 的语义正是"可以确定什么都没做"。
impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::Arg(ArgError::new(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_recognized_for_both_sources() {
        assert!(ToolError::Bridge(BridgeError::Cancelled).is_cancelled());
        assert!(ToolError::Local(LocalError::Cancelled).is_cancelled());
        assert!(!ToolError::failed("磁盘满了").is_cancelled());
        assert!(!ToolError::Arg(ArgError::new("缺少 command")).is_cancelled());
    }

    #[test]
    fn argument_errors_keep_the_legacy_prefix() {
        let error = ToolError::Arg(ArgError::new("run_shell 需要 command"));
        assert_eq!(
            error.user_message(),
            "invalid arguments: run_shell 需要 command"
        );
        assert!(error.is_argument());
        assert!(!ToolError::failed("x").is_argument());
    }

    #[test]
    fn local_failures_surface_their_message_verbatim() {
        let error = ToolError::failed("路径不存在：C:\\nope");
        assert_eq!(error.user_message(), "路径不存在：C:\\nope");
        assert_eq!(ToolError::cancelled().user_message(), "已中断");
    }
}
