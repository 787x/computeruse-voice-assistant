//! PowerShell 桥接的异常恢复测试。
//!
//! 用**假的 NDJSON 后端**（临时 .bat/.ps1 脚本）而不是真的 PowerShell，
//! 这样能在几秒内复现那些"真机上偶发、CI 上必然复现"的故障：
//!  - 超时后迟到的响应绝不能被配给下一条请求；
//!  - 进程崩溃后能自愈，且重启计数用尽后给出明确不可用原因；
//!  - 一次只允许一个在途请求（并发调用必须串行化）。
//!
//! 真机 PowerShell 链路由 `#[ignore]` 的只读用例覆盖（需要 Windows 桌面会话）。

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use cuava::cancel::CancelToken;
use cuava::cua::bridge::{BridgeError, BridgeOptions, PwshBridge};

/// 写一个假后端脚本。
///
/// 行为：读一行 JSON，按 `cmd` 回应。支持的"故障注入"命令：
///  - `hang`：永远不回（用于超时）
///  - `late`：延迟 800ms 才回（用于"迟到响应错配"）
///  - `die`：直接退出进程（用于崩溃自愈）
///  - 其它：原样回声
fn write_fake_backend(name: &str) -> PathBuf {
    let dir = scratch_dir(name);
    let script = dir.join("fake.ps1");
    let content = r#"
$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
[Console]::InputEncoding = $utf8
function Send($obj) {
  $json = $obj | ConvertTo-Json -Depth 8 -Compress
  [Console]::Out.Write($json)
  [Console]::Out.Write("`n")
  [Console]::Out.Flush()
}
Send ([ordered]@{ id = $null; ok = $true; event = 'ready'; data = [ordered]@{ protocol = 'dsh-cua/1'; psVersion = '5.1'; dpiAware = $false } })
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
  if ($line.Trim().Length -eq 0) { continue }
  $req = ConvertFrom-Json $line
  $cmd = [string]$req.cmd
  if ($cmd -eq 'hang') { continue }
  if ($cmd -eq 'die') { exit 3 }
  if ($cmd -eq 'late') { [System.Threading.Thread]::Sleep(800) }
  if ($cmd -eq 'fail') {
    Send ([ordered]@{ id = [string]$req.id; ok = $false; error = [ordered]@{ code = 'BAD_REQUEST'; message = 'nope'; details = @{} } })
    continue
  }
  Send ([ordered]@{ id = [string]$req.id; ok = $true; data = [ordered]@{ echoed = $cmd } })
}
"#;
    std::fs::write(&script, content).unwrap();
    script
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bridge_for(script: PathBuf, timeout_ms: u64, startup_ms: u64) -> PwshBridge {
    let mut options = BridgeOptions::new(script);
    options.default_timeout_ms = timeout_ms;
    options.startup_timeout_ms = startup_ms;
    options.max_restarts = 2;
    // `cua.ps1` 启动时要用临时目录编译 UIA 互操作层；某些受限环境里
    // 系统 `%TEMP%` 不可写，所以统一指到构建目录下。
    let temp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join("pstemp");
    let _ = std::fs::create_dir_all(&temp);
    options.temp_dir = Some(temp);
    PwshBridge::new(options)
}

#[tokio::test]
async fn roundtrip_through_ready_banner_and_echo() {
    let script = write_fake_backend("bridge-roundtrip");
    let bridge = bridge_for(script, 5_000, 10_000);
    let cancel = CancelToken::new();

    let info = bridge.probe(&cancel).await.expect("后端应当就绪");
    assert_eq!(info.protocol, "dsh-cua/1");
    assert_eq!(info.ps_version, "5.1");

    let data = bridge
        .call("screen_info", serde_json::json!({}), None, &cancel)
        .await
        .unwrap();
    assert_eq!(data["echoed"], serde_json::json!("screen_info"));

    bridge.dispose().await;
}

#[tokio::test]
async fn backend_error_is_surfaced_with_code() {
    let script = write_fake_backend("bridge-error");
    let bridge = bridge_for(script, 5_000, 10_000);
    let cancel = CancelToken::new();

    let error = bridge
        .call("fail", serde_json::json!({}), None, &cancel)
        .await
        .unwrap_err();
    match error {
        BridgeError::Backend { code, message, .. } => {
            assert_eq!(code, "BAD_REQUEST");
            assert_eq!(message, "nope");
        }
        other => panic!("应当是结构化后端错误，实际是 {other:?}"),
    }
    bridge.dispose().await;
}

/// 超时后进程被换掉：迟到的响应绝不能配给下一条请求。
#[tokio::test]
async fn timeout_restarts_backend_and_late_reply_is_discarded() {
    let script = write_fake_backend("bridge-timeout");
    let bridge = bridge_for(script, 200, 10_000);
    let cancel = CancelToken::new();

    // 1) 一条必然超时的请求
    let error = bridge
        .call("hang", serde_json::json!({}), Some(200), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(error, BridgeError::Timeout(_)),
        "应当是超时：{error:?}"
    );

    // 2) 下一条请求必须正常返回，而且**不能**拿到上一条的响应
    let data = bridge
        .call("screen_info", serde_json::json!({}), None, &cancel)
        .await
        .expect("超时后应能自动重启并继续工作");
    assert_eq!(
        data["echoed"],
        serde_json::json!("screen_info"),
        "迟到的 hang 响应不得错配到后续请求上"
    );

    bridge.dispose().await;
}

/// 进程崩溃后能自愈。
#[tokio::test]
async fn crash_is_recovered_by_restart() {
    let script = write_fake_backend("bridge-crash");
    let bridge = bridge_for(script, 5_000, 10_000);
    let cancel = CancelToken::new();

    let _ = bridge.probe(&cancel).await.expect("先就绪");
    let error = bridge
        .call("die", serde_json::json!({}), None, &cancel)
        .await
        .unwrap_err();
    assert!(
        !matches!(error, BridgeError::Backend { .. }),
        "崩溃应当报进程级错误"
    );

    let data = bridge
        .call("screen_info", serde_json::json!({}), None, &cancel)
        .await
        .expect("崩溃后应自动重启");
    assert_eq!(data["echoed"], serde_json::json!("screen_info"));
    bridge.dispose().await;
}

/// 重启次数用尽后必须给出明确的不可用原因，而不是无限重试。
#[tokio::test]
async fn restart_exhaustion_reports_clear_reason() {
    let script = write_fake_backend("bridge-exhaust");
    let mut options = BridgeOptions::new(script);
    options.default_timeout_ms = 5_000;
    options.startup_timeout_ms = 10_000;
    options.max_restarts = 1;
    let bridge = PwshBridge::new(options);
    let cancel = CancelToken::new();

    // 连撞两次崩溃（第一次就让重启计数+1，第二次超过上限）
    let _ = bridge
        .call("die", serde_json::json!({}), None, &cancel)
        .await;
    let _ = bridge
        .call("die", serde_json::json!({}), None, &cancel)
        .await;
    let error = bridge
        .call("screen_info", serde_json::json!({}), None, &cancel)
        .await
        .unwrap_err();
    let message = error.user_message();
    assert!(
        message.contains("不可用") || message.contains("崩溃"),
        "应给出明确的不可用原因：{message}"
    );
    bridge.dispose().await;
}

/// 协议串行化：并发调用不会把响应配错。
#[tokio::test]
async fn concurrent_calls_are_serialized() {
    let script = write_fake_backend("bridge-serialize");
    let bridge = std::sync::Arc::new(bridge_for(script, 5_000, 10_000));
    let cancel = CancelToken::new();
    bridge.probe(&cancel).await.unwrap();

    let mut handles = Vec::new();
    for index in 0..8 {
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            let cmd = format!("cmd{index}");
            let data = bridge
                .call(&cmd, serde_json::json!({}), None, &cancel)
                .await
                .unwrap();
            // 每条响应都必须与自己的请求配对
            assert_eq!(data["echoed"], serde_json::json!(cmd));
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    bridge.dispose().await;
}

/// 取消正在等待的请求：立刻返回取消错误，不留下悬挂的 pending。
#[tokio::test]
async fn cancel_interrupts_pending_request() {
    let script = write_fake_backend("bridge-cancel");
    let bridge = bridge_for(script, 30_000, 10_000);
    let cancel = CancelToken::new();
    bridge.probe(&cancel).await.unwrap();

    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let error = bridge
        .call("hang", serde_json::json!({}), Some(30_000), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(error, BridgeError::Cancelled),
        "应当被取消：{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "取消不能被超时兜底（否则用户点取消后要干等）"
    );
    bridge.dispose().await;
}

/// 找不到脚本时给出可读的不可用原因，而不是 panic。
#[tokio::test]
async fn missing_script_is_reported_clearly() {
    let bridge = bridge_for(PathBuf::from("definitely-not-here.ps1"), 1_000, 1_000);
    let cancel = CancelToken::new();
    let error = bridge
        .call("ping", serde_json::json!({}), None, &cancel)
        .await
        .unwrap_err();
    let message = error.user_message();
    assert!(
        message.contains("不可用") && message.contains("找不到"),
        "应说明脚本缺失：{message}"
    );
}

/// 非 JSON 的 stdout 不得被当成响应（否则会读到半截诊断信息）。
#[tokio::test]
async fn non_json_stdout_is_ignored() {
    let dir = scratch_dir("bridge-noisy");
    let script = dir.join("noisy.ps1");
    let mut file = std::fs::File::create(&script).unwrap();
    writeln!(
        file,
        r#"
$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
[Console]::InputEncoding = $utf8
function Send($obj) {{
  $json = $obj | ConvertTo-Json -Depth 8 -Compress
  [Console]::Out.Write($json)
  [Console]::Out.Write("`n")
  [Console]::Out.Flush()
}}
Send ([ordered]@{{ id = $null; ok = $true; event = 'ready'; data = [ordered]@{{ protocol = 'dsh-cua/1' }} }})
while ($true) {{
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) {{ break }}
  $req = ConvertFrom-Json $line
  [Console]::Out.Write("this is not json")
  [Console]::Out.Write("`n")
  [Console]::Out.Flush()
  Send ([ordered]@{{ id = [string]$req.id; ok = $true; data = [ordered]@{{ echoed = 'still-works' }} }})
}}
"#
    )
    .unwrap();
    drop(file);

    let bridge = bridge_for(script, 5_000, 10_000);
    let cancel = CancelToken::new();
    let data = bridge
        .call("ping", serde_json::json!({}), None, &cancel)
        .await
        .expect("非 JSON 行应被忽略，真正的响应仍然可用");
    assert_eq!(data["echoed"], serde_json::json!("still-works"));
    bridge.dispose().await;
}

/// 真机只读链路：列出窗口并读取界面状态。
///
/// 默认 `#[ignore]`：需要真实 Windows 桌面会话与仓库里的 `cua.ps1`。
/// 运行方式：`cargo test --test bridge -- --ignored --nocapture`
#[tokio::test]
#[ignore = "需要真实 Windows 桌面会话"]
async fn real_cua_lists_windows() {
    let Some(script) = BridgeOptions::resolve_default_script() else {
        panic!("找不到 cua.ps1（可用 CUAVA_CUA_SCRIPT 指定）");
    };
    println!("使用脚本：{}", script.display());
    let bridge = bridge_for(script, 30_000, 20_000);
    let cancel = CancelToken::new();

    let info = bridge.probe(&cancel).await.expect("后端应就绪");
    println!("就绪：protocol={} ps={}", info.protocol, info.ps_version);

    let windows = bridge
        .call("list_windows", serde_json::json!({}), None, &cancel)
        .await
        .expect("list_windows 应成功");
    let count = windows["count"].as_i64().unwrap_or(0);
    println!("窗口数量：{count}");
    assert!(count > 0, "桌面会话里至少应有一个窗口");
    let first = &windows["windows"][0];
    println!("第一个窗口：{first}");

    bridge.dispose().await;
}

/// 真实工具层只读闭环：`CuaToolkit` → `list_windows` / `get_app_state`。
///
/// 这是 PR5 的"实机可执行至少一个无破坏性的 Windows CUA 闭环"。
/// 需要真实桌面会话，默认 `#[ignore]`。
#[tokio::test]
#[ignore = "需要真实 Windows 桌面会话"]
async fn real_toolkit_reads_window_state() {
    use cuava::cua::tools::CuaToolkit;

    let Some(script) = BridgeOptions::resolve_default_script() else {
        panic!("找不到 cua.ps1");
    };
    let bridge = bridge_for(script, 30_000, 20_000);
    let toolkit = CuaToolkit::new(bridge, None);
    let cancel = CancelToken::new();

    if let Err(error) = toolkit.warmup(&cancel).await {
        panic!(
            "后端应就绪：{error}
诊断：{}",
            toolkit.bridge().diagnostics().await
        );
    }

    // 1) list_windows：模型面信封必须是可读的结构化文本
    let listed = toolkit
        .invoke("list_windows", &serde_json::json!({}), &cancel)
        .await
        .expect("list_windows 应成功");
    println!("--- list_windows ---\n{}", listed.text);
    assert!(listed.text.contains("<windows count="));

    // 2) get_app_state：必须带 state_id 与元素坐标规则说明
    let windows = listed.data["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!windows.is_empty(), "桌面会话里至少应有一个窗口");
    let hwnd = windows[0]["hwnd"].as_str().unwrap().to_string();
    let state = toolkit
        .invoke(
            "get_app_state",
            &serde_json::json!({ "window": hwnd }),
            &cancel,
        )
        .await
        .expect("get_app_state 应成功");
    println!(
        "--- get_app_state（前 800 字）---\n{}",
        state.text.chars().take(800).collect::<String>()
    );
    assert!(state.text.contains("state_id=\""), "必须给出 state_id");
    assert!(
        state.data["state_id"].as_str().is_some(),
        "结构化输出里必须有 state_id"
    );

    toolkit.bridge().dispose().await;
}

/// 真实截图：图像按真实 media type 返回，且信封里写明栅格坐标规则。
#[tokio::test]
#[ignore = "需要真实 Windows 桌面会话"]
async fn real_toolkit_captures_screenshot() {
    use cuava::cua::tools::CuaToolkit;

    let Some(script) = BridgeOptions::resolve_default_script() else {
        panic!("找不到 cua.ps1");
    };
    let bridge = bridge_for(script, 30_000, 20_000);
    let toolkit = CuaToolkit::new(bridge, None);
    let cancel = CancelToken::new();
    if let Err(error) = toolkit.warmup(&cancel).await {
        panic!(
            "后端应就绪：{error}
诊断：{}",
            toolkit.bridge().diagnostics().await
        );
    }

    let shot = toolkit
        .invoke(
            "screenshot",
            &serde_json::json!({ "max_width": 640, "format": "jpeg" }),
            &cancel,
        )
        .await
        .expect("screenshot 应成功");

    println!("--- screenshot envelope ---\n{}", shot.text);
    let image = shot.image.expect("应带回一张图");
    assert_eq!(image.media_type, "image/jpeg");
    assert!(!image.base64.is_empty());
    assert!(shot.text.contains("raster pixels inside this image"));
    assert!(
        shot.data["frame_id"].as_str().is_some(),
        "必须给出 frame_id"
    );
    println!(
        "图像 {}x{} media={} bytes≈{}",
        image.width.unwrap_or(0),
        image.height.unwrap_or(0),
        image.media_type,
        image.base64.len() * 3 / 4
    );

    toolkit.bridge().dispose().await;
}
/// Read-only diagnostic for real DPI changes; only logs our HUD bounds and backend DPI data.
#[tokio::test]
#[ignore = "requires a manually visible HUD on an interactive Windows desktop"]
async fn real_hud_geometry() {
    let script = BridgeOptions::resolve_default_script().expect("cua.ps1");
    let bridge = bridge_for(script, 30_000, 20_000);
    let cancel = CancelToken::new();
    let info = bridge.probe(&cancel).await.expect("ready");
    println!("DPI probe: {info:?}");
    let windows = bridge
        .call("list_windows", serde_json::json!({}), None, &cancel)
        .await
        .expect("list windows");
    let hud = windows["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["title"] == "CUAVA Voice HUD");
    println!(
        "HUD bounds: {}",
        hud.map(|w| &w["bounds"])
            .unwrap_or(&serde_json::Value::Null)
    );
    bridge.dispose().await;
    let bounds = &hud.expect("manual HUD must be visible")["bounds"];
    let x = bounds["x"].as_i64().unwrap();
    let y = bounds["y"].as_i64().unwrap();
    let width = bounds["width"].as_i64().unwrap();
    let height = bounds["height"].as_i64().unwrap();
    let monitors = info.raw["screen"]["monitors"].as_array().unwrap();
    let monitor = monitors
        .iter()
        .find(|m| {
            let b = &m["bounds"];
            let left = b["x"].as_i64().unwrap();
            let top = b["y"].as_i64().unwrap();
            x + width / 2 >= left
                && x + width / 2 < left + b["width"].as_i64().unwrap()
                && y + height / 2 >= top
                && y + height / 2 < top + b["height"].as_i64().unwrap()
        })
        .expect("HUD monitor");
    let scale = monitor["dpi"].as_f64().unwrap() / 96.0;
    assert!(
        (width - (380.0 * scale).round() as i64).abs() <= 2,
        "HUD width must preserve logical layout at DPI {scale}"
    );
    assert!(
        (height - (290.0 * scale).round() as i64).abs() <= 2,
        "HUD height must preserve logical layout at DPI {scale}"
    );
    let area = &monitor["working"];
    assert!(x >= area["x"].as_i64().unwrap() && y >= area["y"].as_i64().unwrap());
    assert!(x + width <= area["x"].as_i64().unwrap() + area["width"].as_i64().unwrap());
    assert!(y + height <= area["y"].as_i64().unwrap() + area["height"].as_i64().unwrap());
}
