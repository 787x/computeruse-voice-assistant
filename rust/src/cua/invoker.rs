//! `CuaToolkit` 的 `ToolInvoker` 实现：把 Agent 的工具调用接到 PowerShell 后端。
//!
//! 单独一个文件是因为这条边界要能被测试替换：Agent 只认 `ToolInvoker`，
//! 不知道底下是 PowerShell 还是假后端。

use serde_json::Value;

use crate::agent::{ToolExecution, ToolInvoker};
use crate::cancel::CancelToken;
use crate::cua::tools::{tool_specs, CuaToolkit, ToolError};

impl ToolInvoker for CuaToolkit {
    fn tool_definitions(&self) -> Vec<Value> {
        tool_specs()
            .iter()
            .map(|spec| spec.to_request_value())
            .collect()
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a CancelToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolExecution, ToolError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let output = CuaToolkit::invoke(self, name, args, cancel).await?;
            Ok(ToolExecution {
                text: output.text,
                image: output.image,
            })
        })
    }
}
