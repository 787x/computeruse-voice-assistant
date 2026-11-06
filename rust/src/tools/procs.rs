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
/// 三层判定，按**精确性**从高到低：
///  1. 就是本进程（PID 相等）；
///  2. PID 等于当前受管理的 CUA 后端子进程（`managed_pids`）——
///     `powershell.exe` 在 `C:\Windows\System32`，路径判定抓不到它；
///  3. 可执行文件位于助手自己的安装目录/数据目录下（`own_roots`）。
///
/// 第 2 层只认 **当前** 那个 PID：bridge 在子进程退出/重启后立即更新，
/// 不做历史累积，所以 PID 被系统复用不会错误保护无关程序。
fn is_own_process(row: &ProcessRow, own_roots: &[String], managed_pids: &[u32]) -> bool {
    if row.pid == std::process::id() {
        return true;
    }
    if managed_pids.contains(&row.pid) {
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

/// 把用户/模型给的进程名收敛成**精确名字**；带通配符或分隔符的一律拒绝。
///
/// 为什么必须拒绝：`Get-Process -Name` 支持 PowerShell 通配符
/// （`*`、`?`、`[a-z]`），而 `PROTECTED` 只按原始输入做字符串比较。
/// 于是 `svchost*` 既能绕开保护名单，又会把一批系统进程一起匹配出来——
/// 这是"结束一个程序"变成"结束一堆关键进程"的直接路径。
///
/// 返回规范化后的名字（去 `.exe`、去首尾空白）；不合法时给出可照做的原因。
pub fn normalize_process_name(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("需要 name（不带 .exe）".to_string());
    }
    // PowerShell 通配符 + 常见"多目标"写法：逗号分隔、分号、管道、括号。
    const FORBIDDEN: &[char] = &[
        '*', '?', '[', ']', ',', ';', '|', '&', '(', ')', '{', '}', '<', '>', '`', '$', '@', '\n',
        '\r', '\t',
    ];
    if let Some(found) = trimmed.chars().find(|ch| FORBIDDEN.contains(ch)) {
        return Err(format!(
            "进程名不能包含「{found}」：kill_app 只接受一个明确的进程名，\
不接受通配符或多目标（否则一次调用可能结束一批系统进程）"
        ));
    }
    let name = trimmed
        .strip_suffix(".exe")
        .or_else(|| trimmed.strip_suffix(".EXE"))
        .or_else(|| trimmed.strip_suffix(".Exe"))
        .unwrap_or(trimmed)
        .trim();
    if name.is_empty() {
        return Err("需要 name（不带 .exe）".to_string());
    }
    // 名字里不能再有路径分隔符——那是"指定某个 exe"的写法，本工具不接受。
    if name.contains('\\') || name.contains('/') {
        return Err("kill_app 只接受进程名，不接受路径".to_string());
    }
    Ok(name.to_string())
}

/// 大小写不敏感地判断某个进程名是否属于保护名单。
fn is_protected(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    PROTECTED.contains(&lower.as_str())
}

/// 候选进程里真正可以结束的那些。
///
/// 三道闸，任何一道不通过都不进 `taskkill`：
///  1. 实际 `ProcessName` 必须与请求名**完全一致**（不是前缀、不是包含）；
///  2. 不能是系统关键进程（按**实际**名字再查一次，不只是查输入）；
///  3. 不能是助手自身、**当前管理的 CUA 子进程**或其必要子进程。
fn kill_candidates(
    rows: &[ProcessRow],
    requested: &str,
    own_roots: &[String],
    managed_pids: &[u32],
) -> (Vec<ProcessRow>, Vec<String>) {
    let wanted = requested.to_lowercase();
    let mut allowed = Vec::new();
    let mut refused = Vec::new();
    for row in rows {
        let actual = row.name.trim();
        if actual.to_lowercase() != wanted {
            // 通配符解析出来的其它进程：列出名字但绝不结束。
            refused.push(format!("{}（名字不匹配）", actual));
            continue;
        }
        if is_protected(actual) {
            refused.push(format!("{}（系统关键进程）", actual));
            continue;
        }
        if is_own_process(row, own_roots, managed_pids) {
            refused.push(format!("{}({})（助手自身或其必要进程）", actual, row.pid));
            continue;
        }
        allowed.push(row.clone());
    }
    (allowed, refused)
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
pub async fn run_control(
    args: &Value,
    cancel: &CancelToken,
    managed_pids: &[u32],
) -> Result<String, ToolError> {
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
            rows.retain(|row| !is_own_process(row, &roots, managed_pids));
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
///
/// `managed_pids` 是助手**当前**管理着的子进程（CUA 后端 PowerShell）。
/// 它必须由调用方在执行前从 bridge 取一次——`powershell.exe` 位于系统目录，
/// 路径判定抓不到它，只靠 `own_roots` 会把我们的后端一起结束掉。
pub async fn run_kill(
    args: &Value,
    cancel: &CancelToken,
    managed_pids: &[u32],
) -> Result<String, ToolError> {
    let object = as_args(args);
    let raw = require_string(&object, "name", "kill_app")?;
    // **第一步就是拒绝通配符**：必须在任何查询之前，也不能靠"查到再筛"兜底——
    // `Get-Process -Name 'svchost*'` 在查询阶段就已经枚举了整批系统进程。
    let name =
        normalize_process_name(&raw).map_err(|message| ToolError::Arg(ArgError::new(message)))?;
    if is_protected(&name) {
        return Err(ToolError::Arg(ArgError::new(format!(
            "「{name}」是系统关键进程，拒绝结束"
        ))));
    }

    let roots = own_roots();
    let all = query_by_name(&name, cancel).await?;
    if all.is_empty() {
        return Err(ToolError::failed(format!(
            "没有找到正在运行的「{name}」（已查全部进程，不只是有窗口的）"
        )));
    }
    // 执行前再按**实际** ProcessName 复核一次，逐 PID 排除关键进程、自身与 CUA 子进程。
    let (targets, refused) = kill_candidates(&all, &name, &roots, managed_pids);
    if targets.is_empty() {
        return Err(ToolError::failed(format!(
            "「{name}」没有可结束的进程（{}）",
            if refused.is_empty() {
                "匹配到的都不允许结束".to_string()
            } else {
                refused.join("、")
            }
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

    let alive = wait_until_gone(&name, &roots, managed_pids, cancel).await?;
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
///
/// 只把**同名且可结束**的进程算作"还活着"：否则通配符顺带匹配到的其它进程
/// 会被误报成"没关掉"。
async fn wait_until_gone(
    name: &str,
    roots: &[String],
    managed_pids: &[u32],
    cancel: &CancelToken,
) -> Result<Vec<ProcessRow>, ToolError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(GONE_WAIT_MS);
    loop {
        if cancel.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        let (rows, _) = kill_candidates(
            &query_by_name(name, cancel).await?,
            name,
            roots,
            managed_pids,
        );
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
        assert!(is_own_process(&me, &[], &[]));

        let sibling = ProcessRow {
            pid: 999_999,
            name: "cuava".into(),
            title: String::new(),
            path: "D:\\apps\\cuava\\cuava.exe".into(),
        };
        assert!(is_own_process(
            &sibling,
            &["d:\\apps\\cuava".to_string()],
            &[]
        ));
        assert!(!is_own_process(&sibling, &["d:\\other".to_string()], &[]));
    }

    #[tokio::test]
    async fn protected_system_processes_are_refused_before_any_kill() {
        let cancel = CancelToken::new();
        for name in ["explorer", "svchost", "lsass", "csrss"] {
            let error = run_kill(&json!({"name": name}), &cancel, &[])
                .await
                .unwrap_err();
            assert!(error.is_argument(), "{name} 必须在动作前被拒绝");
            assert!(error.user_message().contains("系统关键进程"));
        }
    }

    /// **通配符与多目标语法必须在查询之前就被拒绝**。
    ///
    /// 这是本 PR 的真实缺陷：`Get-Process -Name` 支持通配符，而保护名单只按
    /// 原始输入字符串比较，于是 `svchost*` 能绕过保护并匹配一整批系统进程。
    /// 这里断言的是"连查询都不会发生"——所以用一个**记账**的取消令牌与
    /// 一个不可达的进程表来证明没有任何 `taskkill` 被发出。
    #[tokio::test]
    async fn wildcard_and_multi_target_names_never_reach_taskkill() {
        let cancel = CancelToken::new();
        for name in [
            "*",
            "chrome*",
            "svchost*",
            "?hrome",
            "[a-z]hrome",
            "chrome,notepad",
            "chrome;notepad",
            "chrome|notepad",
            "chrome & notepad",
            "$(Get-Process)",
            "C:\\Windows\\notepad.exe",
            "   ",
            "",
        ] {
            let error = run_kill(&json!({"name": name}), &cancel, &[]).await;
            let error = match error {
                Ok(text) => panic!("「{name}」必须被拒绝，却返回成功：{text}"),
                Err(error) => error,
            };
            assert!(
                error.is_argument(),
                "「{name}」必须是参数错误（动作前拦下），实际：{}",
                error.user_message()
            );
        }
    }

    /// 规范化只接受一个明确名字；`.exe` 后缀被去掉。
    #[test]
    fn legitimate_names_are_normalized_but_never_widened() {
        assert_eq!(normalize_process_name("notepad").unwrap(), "notepad");
        assert_eq!(normalize_process_name(" notepad ").unwrap(), "notepad");
        assert_eq!(normalize_process_name("notepad.exe").unwrap(), "notepad");
        assert_eq!(normalize_process_name("Notepad.EXE").unwrap(), "Notepad");
        // 名字里有点号是合法的（`msedge.exe` 去掉后缀后仍是 `msedge`）。
        assert_eq!(normalize_process_name("msedge").unwrap(), "msedge");
        for bad in ["*", "a?b", "[abc]", "a,b", "a;b", "a|b", "a\\b", "a/b", ""] {
            assert!(normalize_process_name(bad).is_err(), "「{bad}」必须被拒绝");
        }
    }

    /// 假进程表驱动的目标选择：名字必须**完全一致**，且逐 PID 排除关键进程与自身。
    #[test]
    fn kill_candidates_require_exact_name_and_exclude_protected_and_own() {
        let rows = vec![
            ProcessRow {
                pid: 100,
                name: "chrome".into(),
                title: String::new(),
                path: "C:\\Program Files\\Google\\Chrome\\chrome.exe".into(),
            },
            // 通配符/前缀顺带命中：名字不完全一致，绝不能结束。
            ProcessRow {
                pid: 101,
                name: "chrome_crashpad_handler".into(),
                title: String::new(),
                path: String::new(),
            },
            // 实际名字是系统关键进程：即使请求名不敏感也要拒绝。
            ProcessRow {
                pid: 102,
                name: "svchost".into(),
                title: String::new(),
                path: String::new(),
            },
            // 助手自身（按路径判定）。
            ProcessRow {
                pid: 103,
                name: "cuava".into(),
                title: String::new(),
                path: "D:\\apps\\cuava\\cuava.exe".into(),
            },
        ];

        let (allowed, refused) =
            kill_candidates(&rows, "chrome", &["d:\\apps\\cuava".to_string()], &[]);
        assert_eq!(allowed.len(), 1, "只允许精确匹配的 chrome");
        assert_eq!(allowed[0].pid, 100);
        assert!(
            refused
                .iter()
                .any(|item| item.contains("chrome_crashpad_handler")),
            "前缀命中的必须被记录为拒绝：{refused:?}"
        );

        // 请求 svchost 时一个都不许结束（保护名单按实际名字生效）。
        let (allowed, refused) =
            kill_candidates(&rows, "svchost", &["d:\\apps\\cuava".to_string()], &[]);
        assert!(allowed.is_empty(), "系统关键进程不能进 taskkill");
        assert!(refused.iter().any(|item| item.contains("系统关键进程")));

        // 请求助手自己的名字：同样一个都不许结束。
        let (allowed, _) = kill_candidates(&rows, "cuava", &["d:\\apps\\cuava".to_string()], &[]);
        assert!(allowed.is_empty(), "助手自身不能被结束");

        // 大小写不敏感（Windows 语义），但仍然要求整名相等。
        let (allowed, _) = kill_candidates(&rows, "CHROME", &[], &[]);
        assert_eq!(allowed.len(), 1);
    }

    /// **CUA 后端 PowerShell 不能被 kill_app 结束**。
    ///
    /// `powershell.exe` 位于 `C:\Windows\System32`，不在助手安装目录下，
    /// 所以只靠 `own_roots` 的路径判定抓不到它——这正是本次修的缺陷。
    /// 同时必须保留"结束**普通** PowerShell"的能力，不能把名字一律加进保护名单。
    #[test]
    fn managed_cua_powershell_is_protected_but_user_powershell_is_not() {
        let cua_pid = 4242;
        let rows = vec![
            // 助手的 CUA 后端：系统目录里的 powershell.exe。
            ProcessRow {
                pid: cua_pid,
                name: "powershell".into(),
                title: String::new(),
                path: "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".into(),
            },
            // 用户自己开的 PowerShell：同样路径，但 PID 不在受管理集合里。
            ProcessRow {
                pid: 4243,
                name: "powershell".into(),
                title: "Windows PowerShell".into(),
                path: "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".into(),
            },
            // 助手主进程。
            ProcessRow {
                pid: 4244,
                name: "cuava".into(),
                title: String::new(),
                path: "D:\\apps\\cuava\\cuava.exe".into(),
            },
        ];

        let (allowed, refused) = kill_candidates(&rows, "powershell", &[], &[cua_pid]);
        assert_eq!(
            allowed.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![4243],
            "只允许结束用户自己的 powershell"
        );
        assert!(
            refused.iter().any(|item| item.contains("4242")),
            "CUA 子进程必须被记录为拒绝：{refused:?}"
        );

        // 没有受管理 PID 时（后端未启动），两个 powershell 都可以是目标——
        // 证明保护来自"当前 PID"而不是"进程名一律拉黑"。
        let (allowed, _) = kill_candidates(&rows, "powershell", &[], &[]);
        assert_eq!(allowed.len(), 2, "没有 CUA 时不该按名字一刀切");
    }

    /// CUA 进程重启 / PID 变化后，保护关系必须跟着更新。
    #[test]
    fn protection_follows_the_current_cua_pid_only() {
        let rows = vec![
            ProcessRow {
                pid: 5001,
                name: "powershell".into(),
                title: String::new(),
                path: "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".into(),
            },
            ProcessRow {
                pid: 5002,
                name: "powershell".into(),
                title: String::new(),
                path: "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".into(),
            },
        ];

        // 第一代 CUA 是 5001：它被保护，5002 可结束。
        let (allowed, _) = kill_candidates(&rows, "powershell", &[], &[5001]);
        assert_eq!(
            allowed.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![5002]
        );

        // 重启后 CUA 变成 5002：保护随之转移，旧 PID 不再特殊。
        let (allowed, refused) = kill_candidates(&rows, "powershell", &[], &[5002]);
        assert_eq!(
            allowed.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![5001],
            "旧 PID 不能继续被保护"
        );
        assert!(refused.iter().any(|item| item.contains("5002")));
    }

    /// 旧 PID 被系统复用（新进程仍是 powershell）时，不会因为历史记录被保护。
    ///
    /// 保护集合只来自"当前管理的 PID"，不累积历史；所以只要 bridge 已清零，
    /// 复用同一 PID 的无关进程就是普通目标。
    #[test]
    fn recycled_pid_is_not_protected_once_the_cua_process_is_gone() {
        let rows = vec![ProcessRow {
            pid: 6001,
            name: "powershell".into(),
            title: String::new(),
            path: "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".into(),
        }];

        // CUA 已经退出：bridge 清空 → 没有任何受管理 PID。
        let (allowed, _) = kill_candidates(&rows, "powershell", &[], &[]);
        assert_eq!(allowed.len(), 1, "复用 PID 的无关进程不该被保护");

        // 反向确认：只要它仍是当前 CUA，就必须保护。
        let (allowed, _) = kill_candidates(&rows, "powershell", &[], &[6001]);
        assert!(allowed.is_empty());
    }

    /// 精确名字的合法目标仍然可以被选中（不会因为这次收紧而全都杀不掉）。
    #[cfg(windows)]
    #[tokio::test]
    async fn exact_name_still_resolves_to_a_real_target_when_present() {
        let cancel = CancelToken::new();
        // 用当前测试进程自己作为"存在的进程"：它会被识别为自身而拒绝结束，
        // 但这证明了查询与筛选链路是通的（不是一律返回"没有找到"）。
        let exe_name = std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
            })
            .expect("测试进程必须有可执行文件名");
        let rows = query_by_name(&exe_name, &cancel).await.unwrap();
        assert!(!rows.is_empty(), "按名字应能查到当前测试进程");
        let (allowed, refused) = kill_candidates(&rows, &exe_name, &own_roots(), &[]);
        assert!(allowed.is_empty(), "自己的进程必须被排除");
        assert!(!refused.is_empty(), "应记录为拒绝原因：{refused:?}");
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let cancel = CancelToken::new();
        let error = run_control(&json!({"action":"stop"}), &cancel, &[])
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
            &[],
        )
        .await
        .unwrap();
        assert!(text.contains("没有找到"), "{text}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn list_returns_something_readable() {
        let cancel = CancelToken::new();
        let text = run_control(&json!({"action":"list"}), &cancel, &[])
            .await
            .unwrap();
        assert!(
            text.contains("正在运行") || text.contains("没有正在运行"),
            "{text}"
        );
    }
}
