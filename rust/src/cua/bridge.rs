//! Windows 原生 Computer Use 后端客户端（PowerShell 5.1 + NDJSON）。
//!
//! 直接管理仓库里既有的 `packages/cua/native/cua.ps1`，**不重写**原生 UIA 引擎。
//! 协议与 `packages/cua/bridge.js` 完全一致：
//!  请求：`{"id":"…","cmd":"<method>","args":{…}}\n`（stdin，一行一条）
//!  就绪：`{"id":null,"ok":true,"event":"ready","data":{…}}\n`（stdout 首行）
//!  响应：`{"id":"…","ok":true,"data":{…}}` 或 `{"id":"…","ok":false,"error":{…}}`
//!
//! 三条必须保住的约束（都是旧版实测踩出来的）：
//!  1. **一次只允许一个在途请求**：协议没有流式的请求号多路复用，并发写会把响应配错。
//!  2. **超时 = 进程不可信**：超时后不猜响应归属，直接 kill；下次调用时重启。
//!     否则一条迟到的响应会错配到下一条请求上（比报错危险得多）。
//!  3. **stdout 只能有 NDJSON**：非 JSON 行当作诊断，绝不当作响应。
//!
//! 另外两条与 `cua.ps1` 相关、不能改坏的约束：
//!  - 脚本必须是 UTF-8 **with BOM**（PowerShell 5.1 否则按 ANSI 解析中文注释而语法错乱）；
//!  - 必须以 `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File` 启动。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// 桥接错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum BridgeError {
    /// 后端未就绪 / 已释放 / 重启耗尽。
    #[error("Computer Use 后端不可用：{0}")]
    Unavailable(String),
    /// 后端明确返回了结构化错误（`ok:false`）。
    #[error("{message}")]
    Backend {
        code: String,
        message: String,
        details: Option<Value>,
    },
    /// 调用超时（此时后端进程已被换掉）。
    #[error("后端在 {0}ms 内没有应答，已重启")]
    Timeout(u64),
    /// 响应 id 与请求不匹配（协议被破坏）。
    #[error("协议错配：响应 id {got} 与请求 {want} 不匹配")]
    ProtocolMismatch { got: String, want: String },
    /// 用户取消。
    #[error("已中断")]
    Cancelled,
}

impl BridgeError {
    /// 稳定的错误码（模型/界面据此给出"怎么修"的提示）。
    pub fn code(&self) -> &str {
        match self {
            Self::Unavailable(_) => "BACKEND_UNAVAILABLE",
            Self::Backend { code, .. } => code,
            Self::Timeout(_) => "TIMEOUT",
            Self::ProtocolMismatch { .. } => "PROTOCOL_MISMATCH",
            Self::Cancelled => "CANCELLED",
        }
    }

    /// 供界面显示的说明（不含内部细节噪声）。
    pub fn user_message(&self) -> String {
        match self {
            Self::Unavailable(reason) => format!("Computer Use 后端不可用：{reason}"),
            Self::Backend { message, .. } => message.clone(),
            Self::Timeout(ms) => format!("后端在 {ms}ms 内没有应答，已重启，请重试"),
            Self::ProtocolMismatch { .. } => "后端协议错配，已丢弃该响应".to_string(),
            Self::Cancelled => "已中断".to_string(),
        }
    }
}

/// 后端就绪横幅里的自述。
#[derive(Debug, Clone, Default)]
pub struct ReadyInfo {
    pub protocol: String,
    pub ps_version: String,
    pub dpi_aware: bool,
    pub raw: Value,
}

/// 桥接配置。
#[derive(Debug, Clone)]
pub struct BridgeOptions {
    pub script_path: PathBuf,
    pub executable: String,
    pub default_timeout_ms: u64,
    pub startup_timeout_ms: u64,
    pub max_restarts: u32,
    /// 传给 PowerShell 子进程的 `TEMP` / `TMP`。
    ///
    /// `cua.ps1` 启动时用 `Add-Type` 现场编译，需要可写的临时目录。
    /// `None` 表示沿用调用方的环境变量。
    pub temp_dir: Option<PathBuf>,
}

impl BridgeOptions {
    pub fn new(script_path: impl Into<PathBuf>) -> Self {
        Self {
            script_path: script_path.into(),
            executable: "powershell.exe".to_string(),
            default_timeout_ms: 30_000,
            startup_timeout_ms: 20_000,
            max_restarts: 3,
            temp_dir: None,
        }
    }

    /// 查找仓库里的 `cua.ps1`。
    ///
    /// 依次尝试：`CUAVA_CUA_SCRIPT` 环境变量、可执行文件旁边的 `cua.ps1`、
    /// 从当前目录向上找 `packages/cua/native/cua.ps1`（开发态运行）。
    pub fn resolve_default_script() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CUAVA_CUA_SCRIPT") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                for candidate in [dir.join("cua.ps1"), dir.join("native").join("cua.ps1")] {
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        let mut dir = std::env::current_dir().ok()?;
        loop {
            let candidate = dir
                .join("packages")
                .join("cua")
                .join("native")
                .join("cua.ps1");
            if candidate.is_file() {
                return Some(candidate);
            }
            if !dir.pop() {
                return None;
            }
        }
    }
}

/// 一条已解析的协议行。
#[derive(Debug)]
enum Incoming {
    Ready(Value),
    Response { id: String, ok: bool, body: Value },
    Ignored(String),
}

fn parse_line(line: &str) -> Incoming {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Incoming::Ignored(String::new());
    }
    let Ok(payload) = serde_json::from_str::<Value>(trimmed) else {
        return Incoming::Ignored(format!(
            "非 JSON 行：{}",
            trimmed.chars().take(200).collect::<String>()
        ));
    };
    if payload.get("event").and_then(Value::as_str) == Some("ready") {
        return Incoming::Ready(payload.get("data").cloned().unwrap_or(Value::Null));
    }
    let id = payload
        .get("id")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(false);
    Incoming::Response {
        id,
        ok,
        body: payload,
    }
}

struct Runtime {
    child: Child,
    stdin: ChildStdin,
    /// 后台读线程把解析结果送进来；进程退出时通道关闭。
    lines: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// 与 PowerShell 后端的长连接。
pub struct PwshBridge {
    options: BridgeOptions,
    /// 串行化门：一次只允许一个在途请求。
    gate: Arc<Mutex<()>>,
    runtime: Mutex<Option<Runtime>>,
    ready: Mutex<Option<ReadyInfo>>,
    id_counter: AtomicU64,
    /// 只统计"曾经就绪后意外死亡"的次数。
    restarts: AtomicU64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /*
     * 当前 CUA 子进程的 PID（0 = 没有）。
     *
     * 为什么单独用一个原子量而不是让调用方去锁 `runtime`：
     * `kill_app` 需要在**同步**的候选筛选里判断"这个 powershell 是不是我们的"，
     * 而 `runtime` 是 tokio 异步锁。这个值在 spawn / kill / 退出三处维护，
     * 始终只反映**当前**那一个进程——不做历史累积，避免 PID 被系统复用后
     * 永久保护一个无关程序。
     */
    child_pid: AtomicU32,
}

impl PwshBridge {
    pub fn new(options: BridgeOptions) -> Self {
        Self {
            options,
            gate: Arc::new(Mutex::new(())),
            runtime: Mutex::new(None),
            ready: Mutex::new(None),
            id_counter: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            child_pid: AtomicU32::new(0),
        }
    }

    /// 当前 CUA 子进程的 PID；`None` = 当前没有受管理的子进程。
    ///
    /// 供 `kill_app` 判断"候选进程是不是助手自己的 CUA 后端"。
    /// **只反映这一刻还活着的那个进程**：进程退出/被杀后立刻清零，
    /// 所以 PID 被复用不会得到错误保护。
    pub fn current_child_pid(&self) -> Option<u32> {
        match self.child_pid.load(Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }

    fn set_child_pid(&self, pid: Option<u32>) {
        self.child_pid.store(pid.unwrap_or(0), Ordering::SeqCst);
    }

    /// 测试用：直接登记受管理的子进程 PID（不真的起进程）。
    ///
    /// 生产路径只有 spawn / kill / 启动失败三处调用 `set_child_pid`；
    /// 工具层与 bridge 自己的回归测试需要在不依赖真实 PowerShell 的情况下
    /// 验证"保护跟随当前 PID"，所以留这一个入口。
    #[cfg(test)]
    pub fn set_child_pid_for_test(&self, pid: Option<u32>) {
        self.set_child_pid(pid);
    }

    pub fn script_path(&self) -> &Path {
        &self.options.script_path
    }

    /// 最近一次就绪横幅。
    pub async fn ready_info(&self) -> Option<ReadyInfo> {
        self.ready.lock().await.clone()
    }

    /// 后端诊断尾部（stderr）。
    pub async fn diagnostics(&self) -> String {
        self.stderr_tail
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn next_id(&self) -> String {
        let seq = self.id_counter.fetch_add(1, Ordering::SeqCst);
        format!("{seq:x}")
    }

    /// 确保后端已启动并就绪。并发调用共享同一次启动。
    async fn ensure_started(&self) -> Result<ReadyInfo, BridgeError> {
        {
            let ready = self.ready.lock().await;
            if let Some(info) = ready.as_ref() {
                return Ok(info.clone());
            }
        }
        let restarts = self.restarts.load(Ordering::SeqCst) as u32;
        if restarts > self.options.max_restarts {
            return Err(BridgeError::Unavailable(format!(
                "后端已崩溃 {restarts} 次，不再自动重启{}",
                self.diagnostics_suffix().await
            )));
        }
        self.start().await
    }

    async fn diagnostics_suffix(&self) -> String {
        let text = self.diagnostics().await;
        if text.is_empty() {
            String::new()
        } else {
            format!("\n最近诊断：\n{text}")
        }
    }

    async fn start(&self) -> Result<ReadyInfo, BridgeError> {
        if !self.options.script_path.is_file() {
            return Err(BridgeError::Unavailable(format!(
                "找不到原生脚本：{}",
                self.options.script_path.display()
            )));
        }
        // 先把可能残留的死进程收掉，避免"上一个还没退出"导致永远起不来。
        self.kill().await;

        let mut command = Command::new(&self.options.executable);
        command
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&self.options.script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // `cua.ps1` 启动时用 `Add-Type` 现场编译 UIA 互操作层，会在 `%TEMP%` 下建临时文件。
        // 调用方把 TEMP 指到别处（隔离测试、受限环境）时，子进程必须跟着走——
        // 否则 PowerShell 会去写一个自己没权限的目录，然后整条后端起不来。
        if let Some(temp) = self.options.temp_dir.as_ref() {
            command.env("TEMP", temp).env("TMP", temp);
        }
        #[cfg(windows)]
        {
            // 不弹控制台窗口
            #[allow(unused_imports)]
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }

        let mut child = command.spawn().map_err(|error| {
            BridgeError::Unavailable(format!("无法启动 {}：{error}", self.options.executable))
        })?;
        // 记下当前子进程身份：`kill_app` 据此拒绝结束它。
        // 这是**唯一**登记新进程的地方（每次 spawn 都会覆盖旧值）。
        self.set_child_pid(child.id());

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BridgeError::Unavailable("无法获取后端 stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BridgeError::Unavailable("无法获取后端 stdout".to_string()))?;
        let stderr = child.stderr.take();

        let (sender, lines) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(read_stdout(stdout, sender));

        let stderr_tail = self.stderr_tail.clone();
        if let Some(stderr) = stderr {
            tokio::spawn(read_stderr(stderr, stderr_tail.clone()));
        }
        *stderr_tail.lock().await = VecDeque::new();

        let mut runtime = Runtime {
            child,
            stdin,
            lines,
            stderr_tail: stderr_tail.clone(),
        };

        // 等就绪横幅（约 1s：cua.ps1 启动时要枚举屏幕）。
        let banner = tokio::time::timeout(
            std::time::Duration::from_millis(self.options.startup_timeout_ms),
            wait_ready(&mut runtime),
        )
        .await;

        match banner {
            Ok(Ok(info)) => {
                *self.runtime.lock().await = Some(runtime);
                *self.ready.lock().await = Some(info.clone());
                Ok(info)
            }
            Ok(Err(error)) => {
                let _ = runtime.child.kill().await;
                // 没能就绪 = 这个进程不再受我们管理，撤销保护。
                self.set_child_pid(None);
                Err(error)
            }
            Err(_) => {
                let _ = runtime.child.kill().await;
                self.set_child_pid(None);
                Err(BridgeError::Unavailable(format!(
                    "后端在 {}ms 内没有报告就绪{}",
                    self.options.startup_timeout_ms,
                    self.diagnostics_suffix().await
                )))
            }
        }
    }

    async fn kill(&self) {
        let mut guard = self.runtime.lock().await;
        if let Some(mut runtime) = guard.take() {
            let _ = runtime.stdin.shutdown().await;
            let _ = runtime.child.start_kill();
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), runtime.child.wait()).await;
        }
        // 进程已经不再受我们管理：立刻撤销保护，避免 PID 复用时误保护别的进程。
        self.set_child_pid(None);
        *self.ready.lock().await = None;
    }

    /// 发一条请求并等响应。
    ///
    /// 串行化 + 超时后换进程，保证迟到的响应绝不会被配给后续请求。
    pub async fn call(
        &self,
        cmd: &str,
        args: Value,
        timeout_ms: Option<u64>,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<Value, BridgeError> {
        let guard: OwnedMutexGuard<()> = self.gate.clone().lock_owned().await;
        let result = self.call_inner(cmd, args, timeout_ms, cancel).await;
        drop(guard);
        result
    }

    async fn call_inner(
        &self,
        cmd: &str,
        args: Value,
        timeout_ms: Option<u64>,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<Value, BridgeError> {
        if cancel.is_cancelled() {
            return Err(BridgeError::Cancelled);
        }
        self.ensure_started().await?;
        if cancel.is_cancelled() {
            return Err(BridgeError::Cancelled);
        }

        let id = self.next_id();
        let timeout = timeout_ms.unwrap_or(self.options.default_timeout_ms);
        let request = json!({ "id": id, "cmd": cmd, "args": args });

        // 写入并等待响应；任一环节失败/超时都换掉进程。
        let mut runtime_guard = self.runtime.lock().await;
        let Some(runtime) = runtime_guard.as_mut() else {
            return Err(BridgeError::Unavailable("后端进程不存在".to_string()));
        };

        let line = format!("{}\n", request);
        if let Err(error) = runtime.stdin.write_all(line.as_bytes()).await {
            let reason = format!("写入后端失败：{error}");
            drop(runtime_guard);
            self.bump_restart().await;
            return Err(BridgeError::Unavailable(reason));
        }
        let _ = runtime.stdin.flush().await;

        let outcome = tokio::select! {
            outcome = wait_response(&mut runtime.lines, &id) => outcome,
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout)) => Err(BridgeError::Timeout(timeout)),
            _ = cancel.cancelled() => Err(BridgeError::Cancelled),
        };
        drop(runtime_guard);

        match outcome {
            Ok(value) => Ok(value),
            Err(error) => {
                // 超时/取消/进程死掉：进程状态不可知，直接换掉，迟到的响应随旧进程一起丢弃。
                if !matches!(error, BridgeError::Backend { .. }) {
                    self.bump_restart().await;
                }
                Err(error)
            }
        }
    }

    async fn bump_restart(&self) {
        // 只有"曾经就绪"的进程才算一次重启计数。
        if self.ready.lock().await.is_some() {
            self.restarts.fetch_add(1, Ordering::SeqCst);
        }
        self.kill().await;
    }

    /// 只读探测：后端是否可用（不抛异常）。
    pub async fn probe(
        &self,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<ReadyInfo, BridgeError> {
        let info = self.ensure_started().await?;
        self.call("ping", json!({}), Some(8_000), cancel).await?;
        Ok(info)
    }

    /// 关闭后端并释放资源（幂等）。
    pub async fn dispose(&self) {
        self.kill().await;
    }
}

impl Drop for PwshBridge {
    fn drop(&mut self) {
        // `kill_on_drop(true)` 已在 Command 上设置；这里只兜底标记就绪失效。
        // （Drop 里不能 await，真正的优雅关闭走 `dispose()`。）
    }
}

async fn read_stdout(stdout: ChildStdout, sender: tokio::sync::mpsc::UnboundedSender<Incoming>) {
    let mut reader = BufReader::new(stdout).lines();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                let parsed = parse_line(&line);
                let is_ignored = matches!(&parsed, Incoming::Ignored(text) if text.is_empty());
                if is_ignored {
                    continue;
                }
                if sender.send(parsed).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(_) => return,
        }
    }
}

async fn read_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut guard = tail.lock().await;
        guard.push_back(trimmed.to_string());
        if guard.len() > 60 {
            guard.pop_front();
        }
    }
}

async fn wait_ready(runtime: &mut Runtime) -> Result<ReadyInfo, BridgeError> {
    loop {
        match runtime.lines.recv().await {
            Some(Incoming::Ready(data)) => {
                return Ok(ReadyInfo {
                    protocol: data
                        .get("protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("dsh-cua/1")
                        .to_string(),
                    ps_version: data
                        .get("psVersion")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    dpi_aware: data
                        .get("dpiAware")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    raw: data,
                });
            }
            Some(Incoming::Response { .. }) | Some(Incoming::Ignored(_)) => continue,
            None => {
                let tail = runtime.stderr_tail.lock().await;
                let text = tail.iter().cloned().collect::<Vec<_>>().join("\n");
                return Err(BridgeError::Unavailable(if text.is_empty() {
                    "后端进程在报告就绪前退出".to_string()
                } else {
                    format!("后端进程在报告就绪前退出\n最近诊断：\n{text}")
                }));
            }
        }
    }
}

async fn wait_response(
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    id: &str,
) -> Result<Value, BridgeError> {
    loop {
        match lines.recv().await {
            Some(Incoming::Response { id: got, ok, body }) => {
                if got != id {
                    // 绝不错配：把这条当作协议破坏，调用方会换掉进程。
                    return Err(BridgeError::ProtocolMismatch {
                        got,
                        want: id.to_string(),
                    });
                }
                if ok {
                    return Ok(body.get("data").cloned().unwrap_or(Value::Null));
                }
                let error = body.get("error").cloned().unwrap_or(Value::Null);
                return Err(BridgeError::Backend {
                    code: error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("INTERNAL")
                        .to_string(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("后端报告失败")
                        .to_string(),
                    details: error.get("details").cloned(),
                });
            }
            Some(Incoming::Ready(_)) | Some(Incoming::Ignored(_)) => continue,
            None => {
                return Err(BridgeError::Unavailable("后端进程已退出".to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> PwshBridge {
        PwshBridge::new(BridgeOptions::new("nope.ps1"))
    }

    /// 未启动时没有受管理的子进程：工具层据此不做额外保护。
    #[test]
    fn no_child_pid_before_start() {
        assert_eq!(bridge().current_child_pid(), None);
    }

    /// 登记的 PID 一改，对外暴露的就是新值（重启后保护随之转移）。
    #[test]
    fn child_pid_tracks_the_latest_value() {
        let bridge = bridge();
        bridge.set_child_pid(Some(1234));
        assert_eq!(bridge.current_child_pid(), Some(1234));
        // 模拟重启：新进程换 PID。
        bridge.set_child_pid(Some(5678));
        assert_eq!(bridge.current_child_pid(), Some(5678));
        // 模拟退出/被杀：立刻清零，旧 PID 不再被保护。
        bridge.set_child_pid(None);
        assert_eq!(bridge.current_child_pid(), None);
    }

    /// PID 0 表示"没有子进程"，不会被当成一个真实 PID 暴露出去。
    #[test]
    fn zero_pid_is_reported_as_none() {
        let bridge = bridge();
        bridge.set_child_pid(Some(0));
        assert_eq!(bridge.current_child_pid(), None);
    }
}
