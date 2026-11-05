//! `app_control`（只读查询进程）与 `kill_app`（结束程序）。
//!
//! 语义沿旧版 `packages/tools/procs.mjs`：
//!  - `app_control` 只有 `list`（有窗口的进程，最多 40 个）与
//!    `find`（按名字查**全部**进程，含后台/托盘）。两者都是只读；
//!  - `kill_app` **总是** `taskkill /F /T`（结束整棵进程树），执行后验证真的没了再回话；
//!  - 系统关键进程（`explorer` / `svchost` / `lsass` …）直接拒绝；
//!  - **绝不**结束自己、自己的 CUA 子进程与语音（TTS/ASR）必要进程——
//!    用户说"关掉 python"时不该顺手把助手的嗓子也关掉（旧版真踩过）。
//!
//! 为什么不做"温和关闭"：旧版试过"先发 WM_CLOSE、等 3 秒、还活着就问用户要不要强杀"，
//! 用户的反馈是"优柔寡断"。说关就是关，只有一个选项。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::cancel::CancelToken;
use crate::tools::error::{ArgError, ToolError};
use crate::tools::process::run_powershell;
use crate::tools::spec::{as_args, coerce_string, object_schema, require_string, ToolSpec};
use std::time::Duration;

/// 查询类命令的超时（旧版 15 秒）。
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
/// taskkill 的超时（旧版 15 秒）。
const KILL_TIMEOUT: Duration = Duration::from_secs(15);
/// 关闭后等待进程消失的总时长（旧版 3 秒）。
const GONE_WAIT_MS: u64 = 3_000;

/// 系统关键进程：绝不允许模型去杀（杀错一个会蓝屏或直接注销）。
const PROTECTED: &[&str] = &[
    "csrss",
    "wininit",
    "winlogon",
    "services",
    "lsass",
    "smss",
    "svchost",
    "dwm",
    "explorer",
    "system",
    "registry",
    "memory compression",
    "fontdrvhost",
    "audiodg",
    "sihost",
    "taskhostw",
    "ctfmon",
    "searchhost",
    "startmenuexperiencehost",
    "winlogon",
    "spoolsv",
    "conhost",
];

/// `app_control` 定义。
pub fn control_spec() -> ToolSpec {
    ToolSpec {
        name: "app_control",
        description: "查看正在运行的程序（只读）。action=list 列出有窗口的进程；\
action=find 按名字查**全部**进程（含后台/托盘，用来确认\"到底还在不在\"）。要关程序请用 kill_app。",
        parameters: object_schema(
            json!({
                "action": { "type": "string", "enum": ["list", "find"] },
                "name": { "type": "string", "description": "action=find 时的进程名（不带 .exe）" }
            }),
            &["action"],
        ),
        read_only: true,
        long_timeout: false,
    }
}

/// `kill_app` 定义。
pub fn kill_spec() -> ToolSpec {
    ToolSpec {
        name: "kill_app",
        description: "结束一个程序（总是 taskkill /F /T，结束整棵进程树）。传进程名，如 notepad、chrome、cloudmusic。\
执行后会验证进程真的没了才回话。系统关键进程（explorer、svchost 等）、助手自身与其语音/Computer Use 进程会被拒绝。\
未保存的内容会丢失。",
        parameters: object_schema(
            json!({
                "name": { "type": "string", "description": "进程名（不带 .exe）" }
            }),
            &["name"],
        ),
        read_only: false,
        long_timeout: true,
    }
}

/// 一个进程行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub pid: u32,
    pub name: String,
    pub title: String,
    pub path: String,
}

/// 解析 `ConvertTo-Json` 的输出：单行是对象、多行是数组，两者都要认。
fn parse_rows(raw: &str) -> Vec<ProcessRow> {
    let trimmed = raw.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Vec::new();
    };
    let items = match value {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        other => vec![other],
    };
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let pid = object.get("Id").and_then(Value::as_u64)? as u32;
            Some(ProcessRow {
                pid,
                name: object
                    .get("ProcessName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                title: object
                    .get("MainWindowTitle")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                path: object
                    .get("Path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// 自己的进程与"不该被误杀"的助手子进程。
///
/// 判定用**路径**而不是名字：`python.exe` 既是用户的也可能不是我们的，
/// 但"可执行文件在自己的安装目录/数据目录下"这个条件足够精确。
fn is_own_process(row: &ProcessRow, own_roots: &[String]) -> bool {
    if row.pid == std::process::id() {
        return true;
    }
    let path = row.path.to_lowercase();
    if path.is_empty() {
        // 拿不到路径的进程（提权/系统进程）按"不是自己的"处理，
        // 真正危险的系统进程由 PROTECTED 与权限挡。
        return false;
    }
    own_roots
        .iter()
        .any(|root| !root.is_empty() && path.starts_with(root))
}

/// 助手自己的目录：安装目录（可执行文件所在目录）与数据目录。
fn own_roots() -> Vec<String> {
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_string_lossy().to_lowercase());
        }
    }
    for key in ["CUAVA_DATA_DIR", "CUAVA_ROOT", "CUAVA_RUNTIME_ROOT"] {
        if let Some(value) = std::env::var_os(key) {
            let text = PathBuf::from(value).to_string_lossy().to_lowercase();
            if !text.is_empty() {
                roots.push(text);
            }
        }
    }
    roots
}

fn quote_pwsh(value: &str) -> String {
    value.replace('\'', "''")
}

/// 查同名进程（含后台）。名字里的 `.exe` 会被去掉。
async fn query_by_name(name: &str, cancel: &CancelToken) -> Result<Vec<ProcessRow>, ToolError> {
    let script = format!(
        "Get-Process -Name '{}' -ErrorAction SilentlyContinue | \
         Select-Object Id, ProcessName, MainWindowTitle, Path | ConvertTo-Json -Compress",
        quote_pwsh(name)
    );
    let outcome = run_powershell(&script, None, QUERY_TIMEOUT, cancel, 200_000).await?;
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    Ok(parse_rows(&outcome.stdout))
}

/// 列出有窗口的进程（最多 40 个）。
async fn query_windowed(cancel: &CancelToken) -> Result<Vec<ProcessRow>, ToolError> {
    let script = "Get-Process | Where-Object { $_.MainWindowTitle -ne '' } | \
                  Select-Object -First 40 Id, ProcessName, MainWindowTitle, Path | \
                  ConvertTo-Json -Compress";
    let outcome = run_powershell(script, None, QUERY_TIMEOUT, cancel, 200_000).await?;
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    Ok(parse_rows(&outcome.stdout))
}

/// 执行一次 `app_control`。
pub async fn run_control(args: &Value, cancel: &CancelToken) -> Result<String, ToolError> {
    let object = as_args(args);
    let action = coerce_string(&object, "action")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    match action.as_str() {
        "list" => {
            let rows = query_windowed(cancel).await?;
            if rows.is_empty() {
                return Ok(
                    "没有正在运行的可视窗口程序。（注意：这条只列有窗口的进程，后台进程用 action=find）"
                        .to_string(),
                );
            }
            let lines: Vec<String> = rows
                .iter()
                .map(|row| {
                    let title: String = row.title.chars().take(40).collect();
                    format!("{} (pid {}) — {title}", row.name, row.pid)
                })
                .collect();
            Ok(format!(
                "正在运行（{} 个，只有有窗口的）：\n{}",
                rows.len(),
                lines.join("\n")
            ))
        }
        "find" => {
            let raw = require_string(&object, "name", "app_control action=find")?;
            let name = raw
                .trim_end_matches(".exe")
                .trim_end_matches(".EXE")
                .to_string();
            if name.is_empty() {
                return Err(ToolError::Arg(ArgError::new(
                    "action=find 需要 name（不带 .exe）",
                )));
            }
            let roots = own_roots();
            let mut rows = query_by_name(&name, cancel).await?;
            // 与旧版一致：自己的进程不列出来——否则用户会以为"助手自己还在跑"。
            rows.retain(|row| !is_own_process(row, &roots));
            if rows.is_empty() {
                return Ok(format!(
                    "没有找到名为「{name}」的进程（已查全部进程，不只是有窗口的）。"
                ));
            }
            let lines: Vec<String> = rows
                .iter()
                .map(|row| {
                    if row.title.is_empty() {
                        format!("pid {} · {}（无窗口/后台）", row.pid, row.name)
                    } else {
                        let title: String = row.title.chars().take(30).collect();
                        format!("pid {} · {} — {title}", row.pid, row.name)
                    }
                })
                .collect();
            Ok(format!(
                "「{name}」还有 {} 个进程在跑：\n{}",
                rows.len(),
                lines.join("\n")
            ))
        }
        other => Err(ToolError::Arg(ArgError::new(format!(
            "未知 action：{other}（关程序请用 kill_app）"
        )))),
    }
}

/// 执行一次 `kill_app`。
pub async fn run_kill(args: &Value, cancel: &CancelToken) -> Result<String, ToolError> {
    let object = as_args(args);
    let raw = require_string(&object, "name", "kill_app")?;
    let name = raw
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_string();
    if name.is_empty() {
        return Err(ToolError::Arg(ArgError::new(
            "kill_app 需要 name（不带 .exe）",
        )));
    }
    if PROTECTED.contains(&name.to_lowercase().as_str()) {
        return Err(ToolError::Arg(ArgError::new(format!(
            "「{name}」是系统关键进程，拒绝结束"
        ))));
    }

    let roots = own_roots();
    let all = query_by_name(&name, cancel).await?;
    let targets: Vec<ProcessRow> = all
        .iter()
        .filter(|row| !is_own_process(row, &roots))
        .cloned()
        .collect();
    if all.is_empty() {
        return Err(ToolError::failed(format!(
            "没有找到正在运行的「{name}」（已查全部进程，不只是有窗口的）"
        )));
    }
    if targets.is_empty() {
        // 找得到的只有我们自己的进程：绝不杀，并说明原因。
        return Err(ToolError::failed(format!(
            "「{name}」只匹配到助手自身的进程，已拒绝结束（结束它会连带关掉语音/Computer Use）"
        )));
    }

    let mut killed: Vec<u32> = Vec::new();
    for row in &targets {
        let script = format!("taskkill.exe /F /T /PID {}", row.pid);
        // taskkill 的失败（进程已退出 / 需要管理员）不当作整体失败：
        // 真正的结论由下面的"还在不在"决定。
        let _ = run_powershell(&script, None, KILL_TIMEOUT, cancel, 4_000).await;
        if cancel.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        killed.push(row.pid);
    }

    let alive = wait_until_gone(&name, &roots, cancel).await?;
    if alive.is_empty() {
        return Ok(format!(
            "「{name}」已结束（taskkill /F /T，{} 个进程树）。",
            killed.len()
        ));
    }
    let still: Vec<String> = alive
        .iter()
        .map(|row| format!("{}({})", row.name, row.pid))
        .collect();
    Ok(format!(
        "「{name}」**没关掉**：还剩 {} 个进程（{}）。\
可能是权限不够（需要管理员）或被保护进程挡住了。",
        alive.len(),
        still.join("、")
    ))
}

/// 轮询等待进程消失（最多 `GONE_WAIT_MS`）。返回仍存活的行。
async fn wait_until_gone(
    name: &str,
    roots: &[String],
    cancel: &CancelToken,
) -> Result<Vec<ProcessRow>, ToolError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(GONE_WAIT_MS);
    loop {
        if cancel.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        let rows: Vec<ProcessRow> = query_by_name(name, cancel)
            .await?
            .into_iter()
            .filter(|row| !is_own_process(row, roots))
            .collect();
        if rows.is_empty() {
            return Ok(rows);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(rows);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_object_and_array_payloads_both_parse() {
        let single = r#"{"Id":42,"ProcessName":"notepad","MainWindowTitle":"a","Path":"C:\\Windows\\notepad.exe"}"#;
        let rows = parse_rows(single);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 42);
        assert_eq!(rows[0].name, "notepad");

        let array = r#"[{"Id":1,"ProcessName":"a"},{"Id":2,"ProcessName":"b"}]"#;
        assert_eq!(parse_rows(array).len(), 2);

        assert!(parse_rows("").is_empty());
        assert!(parse_rows("null").is_empty());
        assert!(parse_rows("not json").is_empty());
    }

    #[test]
    fn own_process_is_never_a_kill_target() {
        let me = ProcessRow {
            pid: std::process::id(),
            name: "cuava".into(),
            title: String::new(),
            path: String::new(),
        };
        assert!(is_own_process(&me, &[]));

        let sibling = ProcessRow {
            pid: 999_999,
            name: "cuava".into(),
            title: String::new(),
            path: "D:\\apps\\cuava\\cuava.exe".into(),
        };
        assert!(is_own_process(&sibling, &["d:\\apps\\cuava".to_string()]));
        assert!(!is_own_process(&sibling, &["d:\\other".to_string()]));
    }

    #[tokio::test]
    async fn protected_system_processes_are_refused_before_any_kill() {
        let cancel = CancelToken::new();
        for name in ["explorer", "svchost", "lsass", "csrss"] {
            let error = run_kill(&json!({"name": name}), &cancel).await.unwrap_err();
            assert!(error.is_argument(), "{name} 必须在动作前被拒绝");
            assert!(error.user_message().contains("系统关键进程"));
        }
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let cancel = CancelToken::new();
        let error = run_control(&json!({"action":"stop"}), &cancel)
            .await
            .unwrap_err();
        assert!(error.is_argument());
        assert!(error.user_message().contains("kill_app"));
    }

    #[test]
    fn specs_match_the_legacy_tool_names() {
        assert_eq!(control_spec().name, "app_control");
        assert_eq!(kill_spec().name, "kill_app");
        assert_eq!(control_spec().parameters["required"], json!(["action"]));
        assert_eq!(kill_spec().parameters["required"], json!(["name"]));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn find_reports_a_missing_process_honestly() {
        let cancel = CancelToken::new();
        let text = run_control(
            &json!({"action":"find","name":"cuava-definitely-not-running"}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("没有找到"), "{text}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn list_returns_something_readable() {
        let cancel = CancelToken::new();
        let text = run_control(&json!({"action":"list"}), &cancel)
            .await
            .unwrap();
        assert!(
            text.contains("正在运行") || text.contains("没有正在运行"),
            "{text}"
        );
    }
}
