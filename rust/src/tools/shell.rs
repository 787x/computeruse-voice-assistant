//! `run_shell`：在本机执行一条 PowerShell 命令。
//!
//! 这是**最危险**的一个工具：一条命令能做任何事。所以语义与旧版严格一致：
//!  - 权限默认 `ask`（能力 `system.shell`，见 `permissions::tool_capability`）；
//!  - Windows PowerShell 5.1、`-NoProfile -NonInteractive`，不读 stdin；
//!  - 默认 30 秒超时、最多 120 秒；到点终止**整棵进程树**；
//!  - 输出截断到 4000 字，并明确写出"原文多少字"；
//!  - 退出码如实返回；被超时杀掉时说"被杀"，**不**说"没执行"。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::cancel::CancelToken;
use crate::tools::error::ToolError;
use crate::tools::process::{clamp_timeout_ms, run_powershell};
use crate::tools::spec::{coerce_int, coerce_string, object_schema, require_string, ToolSpec};

/// 输出上限（与旧版 `SHELL_DEFAULTS.maxOutputChars` 一致）。
pub const MAX_OUTPUT_CHARS: usize = 4_000;

/// 工具定义。
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "run_shell",
        description: "在本机执行一条 PowerShell 命令并返回输出（Windows PowerShell 5.1，非交互）。\
适合查系统状态、跑命令行工具；不可逆的操作（删除、安装、改配置）先跟用户说清楚再执行。\
输出会截断到 4000 字。它不经过 Computer Use 的界面控制，也不该用来绕开本应进行的权限确认。",
        parameters: object_schema(
            json!({
                "command": { "type": "string", "description": "要执行的命令（PowerShell 语法）" },
                "cwd": { "type": "string", "description": "工作目录，默认用户目录" },
                "timeout_ms": { "type": "number", "description": "超时毫秒，默认 30000，最大 120000" },
            }),
            &["command"],
        ),
        read_only: false,
        long_timeout: true,
    }
}

/// 危险命令提示：即使有确认卡片，也让用户看清楚这条命令要做什么。
///
/// 只用于**展示**（结果文本里一行"风险："），不改变权限级别——
/// `system.shell` 本身就是 `ask`，再加一层猜测只会让用户看到互相矛盾的说明。
pub fn assess_risk(command: &str) -> Vec<&'static str> {
    let text = command.to_lowercase();
    let rules: &[(&[&str], &str)] = &[
        (
            &[
                "remove-item",
                "del ",
                "rd ",
                "rmdir",
                "erase ",
                "clear-content",
            ],
            "删除文件",
        ),
        (&["format-volume", "format "], "格式化磁盘"),
        (
            &["shutdown", "restart-computer", "stop-computer"],
            "关机/重启",
        ),
        (
            &[
                "set-executionpolicy",
                "new-localuser",
                "net user",
                "reg add",
            ],
            "修改系统安全设置",
        ),
        (
            &[
                "invoke-webrequest",
                "curl",
                "wget",
                "iwr",
                "invoke-restmethod",
            ],
            "从网络下载内容",
        ),
    ];
    rules
        .iter()
        .filter(|(patterns, _)| patterns.iter().any(|pattern| text.contains(pattern)))
        .map(|(_, why)| *why)
        .collect()
}

/// 执行一次 `run_shell`。
pub async fn run(
    args: &Value,
    cancel: &CancelToken,
    default_cwd: Option<&Path>,
) -> Result<String, ToolError> {
    let object = crate::tools::spec::as_args(args);
    let command = require_string(&object, "command", "run_shell")?;
    let timeout = Duration::from_millis(clamp_timeout_ms(coerce_int(&object, "timeout_ms")));

    // 工作目录：模型给了就用（必须存在），否则用用户目录。
    let cwd: Option<PathBuf> = coerce_string(&object, "cwd")
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .map(PathBuf::from)
        .or_else(|| default_cwd.map(Path::to_path_buf));
    if let Some(dir) = cwd.as_ref() {
        if !dir.is_dir() {
            return Err(ToolError::Arg(crate::tools::error::ArgError::new(format!(
                "工作目录不存在：{}",
                dir.display()
            ))));
        }
    }

    let outcome =
        run_powershell(&command, cwd.as_deref(), timeout, cancel, MAX_OUTPUT_CHARS).await?;

    let risks = assess_risk(&command);
    let mut parts = vec![
        format!("命令：{command}"),
        format!(
            "退出码：{}{}，耗时 {}ms",
            outcome.code,
            if outcome.killed {
                format!(
                    "（超时 {}ms 被杀，可能已有部分副作用）",
                    timeout.as_millis()
                )
            } else {
                String::new()
            },
            outcome.elapsed_ms
        ),
    ];
    if !risks.is_empty() {
        parts.push(format!("风险：{}", risks.join("、")));
    }
    let stdout = outcome.stdout.trim();
    let stderr = outcome.stderr.trim();
    // 截断说明必须带上原始规模：只说"被截断"，模型会以为后面没有内容。
    let trim = |text: &str, bytes: usize, label: &str| -> String {
        let chars = text.chars().count();
        if chars >= MAX_OUTPUT_CHARS || bytes > MAX_OUTPUT_CHARS * 4 {
            format!("{text}…（{label}已截断：保留 {chars} 字，原始约 {bytes} 字节）")
        } else {
            text.to_string()
        }
    };
    if !stdout.is_empty() {
        parts.push(format!(
            "输出：\n{}",
            trim(stdout, outcome.stdout_bytes, "输出")
        ));
    }
    if !stderr.is_empty() {
        parts.push(format!(
            "错误输出：\n{}",
            trim(stderr, outcome.stderr_bytes, "错误输出")
        ));
    }
    if stdout.is_empty() && stderr.is_empty() {
        parts.push("（没有输出）".to_string());
    }
    if outcome.killed {
        // 诚实说明：命令被终止，但它运行期间写下的东西不会自动回滚。
        parts.push(
            "（命令在超时后被终止。终止只保证不再继续执行，已经产生的文件或系统变更不会被撤销；\
需要确认实际状态时，请重新读一次现场。）"
                .to_string(),
        );
    }
    Ok(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn risk_patterns_cover_the_legacy_set() {
        assert!(assess_risk("Remove-Item C:\\tmp\\a.txt").contains(&"删除文件"));
        assert!(assess_risk("shutdown /s").contains(&"关机/重启"));
        assert!(assess_risk("curl https://example.com").contains(&"从网络下载内容"));
        assert!(assess_risk("Get-Date").is_empty());
    }

    #[test]
    fn spec_requires_command_and_forbids_extra_fields() {
        let spec = spec();
        assert_eq!(spec.name, "run_shell");
        assert_eq!(spec.parameters["required"], json!(["command"]));
        assert_eq!(spec.parameters["additionalProperties"], json!(false));
    }

    #[tokio::test]
    async fn missing_command_is_rejected_before_running_anything() {
        let cancel = CancelToken::new();
        let error = run(&json!({}), &cancel, None).await.unwrap_err();
        assert!(error.is_argument());
    }

    #[tokio::test]
    async fn nonexistent_cwd_is_rejected_before_running_anything() {
        let cancel = CancelToken::new();
        let error = run(
            &json!({"command": "Get-Date", "cwd": "C:\\definitely-not-here-xyz"}),
            &cancel,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.is_argument());
        assert!(error.user_message().contains("工作目录不存在"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_command_returns_output_and_exit_code() {
        let cancel = CancelToken::new();
        let text = run(
            &json!({"command": "Write-Output 'hello'; exit 7"}),
            &cancel,
            None,
        )
        .await
        .unwrap();
        assert!(text.contains("退出码：7"), "{text}");
        assert!(text.contains("hello"), "{text}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_is_reported_as_killed_not_as_no_execution() {
        let cancel = CancelToken::new();
        let text = run(
            &json!({"command": "Start-Sleep -Seconds 20", "timeout_ms": 800}),
            &cancel,
            None,
        )
        .await
        .unwrap();
        assert!(text.contains("被杀"), "{text}");
        assert!(
            text.contains("已经产生的文件或系统变更不会被撤销"),
            "{text}"
        );
    }
}
