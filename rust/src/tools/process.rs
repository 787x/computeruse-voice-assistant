//! 运行 PowerShell 子进程的公共部分：超时、取消、输出上限、进程树终止。
//!
//! 为什么要单独一层：`run_shell`、`files`（部分动作走原生 API 就不需要）、
//! 进程查询、应用启动索引都要在同一套约束下跑 PowerShell。各写一份的话，
//! "超时到底杀没杀子进程""取消后还能不能读到半截输出"就会各说各话。
//!
//! 三条硬约束（都来自旧版实测）：
//!  1. **从不交互**：`-NoProfile -NonInteractive`，stdin 直接给 null——
//!     否则一条 `Read-Host` 会把回合挂到超时；
//!  2. **超时/取消必须终止整棵进程树**：只杀 `powershell.exe` 会留下它启动的
//!     子进程（`Start-Process`、`npm` 这类），用户看到的是"取消了但还在跑"；
//!  3. **输出有上限**：无限量读取迟早把上下文撑爆，读到的字节要在源头截断，
//!     但仍然继续排空管道（否则子进程会因写满管道而死锁）。

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::cancel::CancelToken;
use crate::tools::error::ToolError;

/// 单次命令的默认超时（与旧版 `SHELL_DEFAULTS.timeoutMs` 一致）。
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 允许模型要求的最大超时（与旧版一致：最多 2 分钟）。
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// 允许模型要求的最小超时。
pub const MIN_TIMEOUT_MS: u64 = 1_000;

/// 一次 PowerShell 执行的结果。
#[derive(Debug, Clone, Default)]
pub struct PsOutcome {
    pub stdout: String,
    pub stderr: String,
    /// 退出码；进程被我们杀掉时是 `-1`（见 `killed`）。
    pub code: i32,
    /// 是否因为超时被我们终止。
    pub killed: bool,
    /// 终止时是否**确认**整棵进程树都已结束。
    ///
    /// `killed = true` 但这里是 `false` 时，只能说明"我们发出了终止请求"，
    /// 可能有子进程还活着——调用方必须如实说"状态可能未知"，
    /// 不能写"已终止整棵树"。
    pub tree_terminated: bool,
    pub elapsed_ms: u64,
    /// 输出是否因为超过上限被截断。
    pub truncated: bool,
    /// 原始总字节数（截断时用来说明"被砍掉了多少"）。
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

/// 把"给模型看的 PowerShell 脚本"包一层：固定 UTF-8 输出编码。
///
/// PowerShell 5.1 默认按控制台代码页（中文系统常为 936）往 stdout 写，
/// 我们用 UTF-8 读回来就会是乱码——中文文件名、`Get-Process` 的窗口标题都会毁掉。
/// 显式设置 `[Console]::OutputEncoding` 是唯一不需要改系统设置的稳定做法。
fn wrap_script(script: &str) -> String {
    format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8;\n{script}")
}

/// 运行一段 PowerShell 脚本并等待结束。
///
/// * `cwd`：工作目录（`None` = 继承当前进程的工作目录）；
/// * `timeout`：到点后终止整棵进程树，结果里 `killed = true`（**不是**错误：
///   命令可能已经产生副作用，调用方必须如实报告）；
/// * `cancel`：回合取消。此时返回 [`ToolError::cancelled`]——副作用状态未知；
/// * `cap`：stdout / stderr 各自最多保留的字符数。
pub async fn run_powershell(
    script: &str,
    cwd: Option<&Path>,
    timeout: Duration,
    cancel: &CancelToken,
    cap: usize,
) -> Result<PsOutcome, ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    let started = Instant::now();
    let mut command = tokio::process::Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(wrap_script(script))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    // 不弹控制台窗口（否则每次工具调用都会闪一下黑框）。
    // `tokio::process::Command` 在 Windows 上自己提供 `creation_flags`，不需要 CommandExt。
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = command
        .spawn()
        .map_err(|error| ToolError::failed(format!("无法启动 powershell.exe：{error}")))?;
    let pid = child.id();

    let stdout_task = child.stdout.take().map(|out| tokio::spawn(drain(out, cap)));
    let stderr_task = child.stderr.take().map(|err| tokio::spawn(drain(err, cap)));

    // 三种结束方式，语义各不相同，不能合并。
    enum Ending {
        Exited(i32),
        TimedOut,
        Cancelled,
    }
    let ending = {
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            status = &mut wait => match status {
                Ok(status) => Ending::Exited(status.code().unwrap_or(-1)),
                Err(error) => {
                    // 进程句柄都拿不到结果：同样不能声称"没执行"。
                    let _ = kill_tree(pid).await;
                    return Err(ToolError::failed(format!(
                        "等待 PowerShell 结束时失败：{error}（命令可能已经执行过）"
                    )));
                }
            },
            _ = tokio::time::sleep(timeout) => Ending::TimedOut,
            _ = cancel.cancelled() => Ending::Cancelled,
        }
    };

    let killed = !matches!(ending, Ending::Exited(_));
    // 终止是否**成功**必须如实记录：`taskkill /T` 也可能失败（拒绝访问、
    // 进程已脱离进程树、目标已是僵尸），那时不能声称"整棵树都结束了"。
    let mut tree_terminated = true;
    if killed {
        // 先杀整棵树，再收尸：否则 `powershell.exe` 退出了、它起的子进程还在跑。
        tree_terminated = kill_tree(pid).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
    }

    let (stdout, stdout_bytes) = collect(stdout_task).await;
    let (stderr, stderr_bytes) = collect(stderr_task).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if matches!(ending, Ending::Cancelled) {
        // 取消是"我们不再等"，不是"什么都没发生"：调用方会写"状态未知"。
        return Err(ToolError::cancelled());
    }

    Ok(PsOutcome {
        truncated: stdout_bytes > cap.saturating_mul(4) || stderr_bytes > cap.saturating_mul(4),
        stdout,
        stderr,
        code: match ending {
            Ending::Exited(code) => code,
            _ => -1,
        },
        killed,
        tree_terminated,
        elapsed_ms,
        stdout_bytes,
        stderr_bytes,
    })
}

/// 排空一个管道，最多保留 `cap` 个字符（仍继续读取，避免子进程写阻塞）。
///
/// 返回 `(保留的文本, 原始总字节数)`。第二个值用来如实说明"被截掉了多少"：
/// 只说"已截断"而不给规模，模型无法判断该不该换更精确的命令。
async fn drain<R>(mut reader: R, cap: usize) -> (String, usize)
where
    R: tokio::io::AsyncRead + Unpin,
{
    // 按字节留足余量（UTF-8 一个字符最多 4 字节），最后再按字符精确截断。
    let byte_cap = cap.saturating_mul(4);
    let mut kept: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut buffer = vec![0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                total += read;
                if kept.len() < byte_cap {
                    let room = byte_cap - kept.len();
                    kept.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
    // 截断可能切在多字节字符中间：`from_utf8_lossy` 会给出替换字符，
    // 再按字符上限收一次就干净了（这几处替换字符只出现在被丢弃的尾部）。
    let text = String::from_utf8_lossy(&kept).to_string();
    let text = if text.chars().count() > cap {
        text.chars().take(cap).collect()
    } else {
        text
    };
    (text, total)
}

async fn collect(task: Option<tokio::task::JoinHandle<(String, usize)>>) -> (String, usize) {
    match task {
        Some(task) => match tokio::time::timeout(Duration::from_secs(2), task).await {
            Ok(Ok(value)) => value,
            // 读取任务没能收尾（进程被杀、管道断了）：如实说明没有完整输出。
            _ => (String::new(), 0),
        },
        None => (String::new(), 0),
    }
}

/// 终止一棵进程树（尽力而为）。
///
/// 用 `taskkill /F /T` 而不是 `Child::kill()`：后者只结束直接子进程，
/// 而 `run_shell` 的命令经常自己再 `Start-Process`。
pub async fn kill_tree(pid: Option<u32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    let mut command = tokio::process::Command::new("taskkill.exe");
    command
        .arg("/F")
        .arg("/T")
        .arg("/PID")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // 同上：`tokio::process::Command` 自带 Windows 的 `creation_flags`。
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    match tokio::time::timeout(Duration::from_secs(5), command.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

/// 把模型给的超时参数夹到安全区间（与旧版 `Math.min(120000, Math.max(1000, …))` 一致）。
///
/// 先在 `i64` 上夹再转 `u64`：负数直接 `as u64` 会变成天文数字，
/// 结果反而落到**最大**超时（把回合挂两分钟），与"给个小超时"的本意相反。
pub fn clamp_timeout_ms(value: Option<i64>) -> u64 {
    match value {
        Some(ms) => ms.clamp(MIN_TIMEOUT_MS as i64, MAX_TIMEOUT_MS as i64) as u64,
        None => DEFAULT_TIMEOUT_MS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_is_clamped_to_legacy_bounds() {
        assert_eq!(clamp_timeout_ms(None), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(Some(1)), MIN_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(Some(10_000)), 10_000);
        assert_eq!(clamp_timeout_ms(Some(9_999_999)), MAX_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(Some(-5)), MIN_TIMEOUT_MS);
    }

    #[test]
    fn script_wrapper_sets_utf8_output_encoding_first() {
        let wrapped = wrap_script("Get-Date");
        assert!(wrapped.starts_with("[Console]::OutputEncoding=[Text.Encoding]::UTF8;"));
        assert!(wrapped.ends_with("Get-Date"));
    }

    #[tokio::test]
    async fn drain_caps_kept_text_but_reports_truncation() {
        let data = "字".repeat(100);
        let (text, bytes) = drain(std::io::Cursor::new(data.clone().into_bytes()), 10).await;
        assert_eq!(text.chars().count(), 10);
        assert!(bytes > 10 * 4, "必须能看出原文更长：{bytes}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_runs_and_reports_exit_code_and_output() {
        let cancel = CancelToken::new();
        let outcome = run_powershell(
            "Write-Output '你好'; exit 3",
            None,
            Duration::from_secs(30),
            &cancel,
            4000,
        )
        .await
        .expect("PowerShell 应该能跑起来");
        assert_eq!(outcome.code, 3, "退出码必须如实返回");
        assert!(
            outcome.stdout.contains("你好"),
            "输出：{:?}",
            outcome.stdout
        );
        assert!(!outcome.killed);
        assert!(!outcome.truncated);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_the_process_and_is_not_reported_as_success() {
        let cancel = CancelToken::new();
        let outcome = run_powershell(
            "Start-Sleep -Seconds 30",
            None,
            Duration::from_millis(600),
            &cancel,
            4000,
        )
        .await
        .expect("超时不应该变成启动失败");
        assert!(outcome.killed, "超时必须标注为被终止");
        assert_eq!(outcome.code, -1);
        assert!(outcome.elapsed_ms < 10_000, "不能真的等满 30 秒");
    }

    /// 终止**成功**时才会声称"整棵树已结束"；子进程还在时不能这么写。
    ///
    /// 这里覆盖的是结果字段本身：`killed` 与 `tree_terminated` 是两个独立事实，
    /// `shell::run` 的报告据此区分"已终止"与"状态可能未知"。
    #[cfg(windows)]
    #[tokio::test]
    async fn successful_timeout_reports_a_terminated_tree() {
        let cancel = CancelToken::new();
        let outcome = run_powershell(
            "Start-Sleep -Seconds 5",
            None,
            Duration::from_millis(500),
            &cancel,
            4000,
        )
        .await
        .unwrap();
        assert!(outcome.killed);
        // 正常情况下 `taskkill /F /T` 能成功，于是可以确认整棵树结束。
        assert!(
            outcome.tree_terminated,
            "成功终止时必须标记树已结束，否则报告会说成状态未知"
        );
    }

    /// `taskkill` 失败时**不能**声称整棵树结束（结果必须允许表达"未知"）。
    #[test]
    fn tree_termination_flag_can_express_unknown_state() {
        let outcome = PsOutcome {
            killed: true,
            tree_terminated: false,
            ..PsOutcome::default()
        };
        assert!(outcome.killed && !outcome.tree_terminated);
        // 这种组合必须能被报表层识别为"状态可能未知"。
        let reported =
            crate::tools::shell::describe_termination(&outcome, Duration::from_millis(500));
        assert!(
            reported.contains("未能确认"),
            "终止未确认时必须如实说明：{reported}"
        );
        let confirmed = PsOutcome {
            killed: true,
            tree_terminated: true,
            ..PsOutcome::default()
        };
        let reported =
            crate::tools::shell::describe_termination(&confirmed, Duration::from_millis(500));
        assert!(reported.contains("被杀"), "{reported}");
        assert!(!reported.contains("未能确认"), "{reported}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancellation_returns_cancelled_error() {
        let cancel = CancelToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            token.cancel();
        });
        let error = run_powershell(
            "Start-Sleep -Seconds 30",
            None,
            Duration::from_secs(60),
            &cancel,
            4000,
        )
        .await
        .unwrap_err();
        assert!(error.is_cancelled());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn already_cancelled_token_never_spawns_a_process() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let error = run_powershell(
            "Write-Output 'should not run'",
            None,
            Duration::from_secs(5),
            &cancel,
            4000,
        )
        .await
        .unwrap_err();
        assert!(error.is_cancelled());
    }
}
