//! 真机语音链路（需要麦克风 + 模型；默认 `#[ignore]`）。
//!
//! 这不是 Mock：它会真的打开麦克风、真的加载 KWS/Whisper、真的进入监听。
//! 运行：
//! ```powershell
//! cargo test --test voice_hardware -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 它**不会**断言「听到了唤醒词」——那需要人在麦克风前说话，
//! 不能由 CI 或自动化假装通过。这里验证的是任务书里可自动化的部分：
//!  - 有可用输入设备，且能拿到合法的采样率/格式；
//!  - 模型能真正加载（KWS 关键词流 + Whisper 上下文）；
//!  - 启动后状态从 Loading 走到 Listening，而不是停在 Error；
//!  - 实际推理后端被如实报告（CPU-only 构建不能显示 CUDA）；
//!  - 停止后设备与线程都能收尾。
//!
//! 若想验证真实唤醒，运行后对着麦克风说「小爱同学」，看状态是否变成
//! 「我在听，请说」——终端会打印每次状态变化。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cuava::voice::capture::Capture;
use cuava::voice::coordinator::{
    self, ActiveSession, CoordinatorConfig, ListenMode, SharedSink, VoiceEvent, VoiceEventKind,
};
use cuava::voice::settings::{VoicePaths, VoiceSettings};

/// 事件 → 一行可读文本（电平事件不记录，避免刷屏）。
fn event_line(event: &VoiceEvent) -> Option<String> {
    let line = match &event.kind {
        VoiceEventKind::StateChanged(state) => format!("state: {}", state.code()),
        VoiceEventKind::Ready(info) => format!(
            "ready: {} @ {} Hz, backend={}, fallback={:?}",
            info.device_name,
            info.sample_rate,
            info.backend.code(),
            info.fallback_reason
        ),
        VoiceEventKind::Woke { keyword, .. } => format!("woke: {keyword}"),
        VoiceEventKind::Final { text, .. } => format!("final: {text}"),
        VoiceEventKind::Rejected { reason, .. } => format!("rejected: {reason:?}"),
        VoiceEventKind::Overflow => "overflow".to_string(),
        VoiceEventKind::DirectStarted { .. } => "direct-started".to_string(),
        VoiceEventKind::Level { .. } => return None,
    };
    Some(format!("[{}] {line}", event.instance))
}

fn repo_root() -> PathBuf {
    if let Some(root) = std::env::var_os("CUAVA_REPO_ROOT") {
        return PathBuf::from(root);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("仓库根")
        .to_path_buf()
}

fn smoke_root() -> PathBuf {
    std::env::var_os("CUAVA_SMOKE_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join(".tmp/smoke-data"))
}

#[test]
#[ignore = "需要真实麦克风与模型"]
fn lists_input_devices() {
    let devices = Capture::input_devices();
    println!("输入设备：{devices:#?}");
    assert!(
        !devices.is_empty(),
        "没有任何输入设备；这台机器无法验收语音采集"
    );
}

#[test]
#[ignore = "需要真实麦克风与模型"]
fn real_models_and_microphone_reach_listening() {
    // 模型从数据目录或仓库找（与运行时同一套解析）。
    let voice = VoiceSettings::default();
    let paths = VoicePaths::resolve(&cuava::config::AppPaths::with_root(smoke_root()));
    let config = match CoordinatorConfig::resolve(&voice, &paths) {
        Ok(config) => config,
        Err(errors) => panic!("模型不可用，无法做真机验收：\n{}", errors.join("\n")),
    };
    println!("KWS 模型：{}", config.kws_model_dir.display());
    println!("关键词：{}", config.keywords_file.display());
    println!("Whisper：{}", config.whisper_model.display());
    println!("麦克风（空 = 默认）：{:?}", config.voice.input_device);

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    let sink = SharedSink::new(move |event| {
        let Some(line) = event_line(&event) else {
            return;
        };
        eprintln!("[voice] {line}");
        sink_events.lock().unwrap().push(line);
    });

    let active = ActiveSession::new();
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let handle = coordinator::spawn(
        config,
        sink,
        active.clone(),
        stop.clone(),
        1,
        ListenMode::new(),
    )
    .expect("启动语音线程");

    // 等到「就绪 + 监听」或超时。
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut listened = false;
    while Instant::now() < deadline {
        {
            let events = events.lock().unwrap();
            if events.iter().any(|line| line.contains("ready: "))
                && events.iter().any(|line| line.contains("state: listening"))
            {
                listened = true;
                break;
            }
            if events.iter().any(|line| line.contains("state: error")) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let snapshot = events.lock().unwrap().clone();
    println!("耗时 {:?}", started.elapsed());
    println!("事件：{snapshot:#?}");

    // 给用户 5 秒对着麦克风说「小爱同学」（自动化只要求进入监听）。
    if listened {
        println!("已进入监听。如果你就在麦克风旁，现在说「小爱同学」——5 秒后停止。");
        std::thread::sleep(Duration::from_secs(5));
    }
    drop(handle);
    stop.store(true, Ordering::SeqCst);

    let ready_line = snapshot
        .iter()
        .find(|line| line.contains("ready: "))
        .unwrap_or_else(|| panic!("模型与设备没有就绪：{snapshot:#?}"));
    println!("就绪行：{ready_line}");
    assert!(
        snapshot
            .iter()
            .any(|line| line.contains("state: listening")),
        "没有进入监听状态"
    );
    assert!(
        !snapshot.iter().any(|line| line.contains("state: error")),
        "启动过程中出现错误状态"
    );
    // CPU-only 构建绝不能报 cuda。
    if !cuava::voice::asr::gpu_compiled_in() {
        assert!(
            ready_line.contains("backend=cpu"),
            "CPU-only 构建不能显示 CUDA：{ready_line}"
        );
    }
}

/// **关麦后立刻重开**：设备与模型能被第二个实例重新拿到，并且都能进入监听。
///
/// 这条在真机上验证「停止是有上限的等待 + 旧实例不再代表当前」：
/// 事件行带实例号，第二个实例的 ready/listening 必须属于它自己。
#[test]
#[ignore = "需要真实麦克风与模型"]
fn quick_close_and_reopen_reaches_listening_again() {
    let voice = VoiceSettings::default();
    let paths = VoicePaths::resolve(&cuava::config::AppPaths::with_root(smoke_root()));
    let config = match CoordinatorConfig::resolve(&voice, &paths) {
        Ok(config) => config,
        Err(errors) => panic!("模型不可用，无法做真机验收：\n{}", errors.join("\n")),
    };

    let mut seen = Vec::new();
    for instance in [1u64, 2] {
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let sink = SharedSink::new(move |event| {
            let Some(line) = event_line(&event) else {
                return;
            };
            eprintln!("[voice] {line}");
            sink_events.lock().unwrap().push(line);
        });
        let stop = Arc::new(AtomicBool::new(false));
        // 每个实例一份自己的活动会话（与 VoiceRuntime 的分配方式一致）。
        let active = ActiveSession::new();
        let handle = coordinator::spawn(
            config.clone(),
            sink,
            active,
            stop.clone(),
            instance,
            ListenMode::new(),
        )
        .expect("启动语音线程");

        let deadline = Instant::now() + Duration::from_secs(30);
        let prefix = format!("[{instance}] ");
        let mut listened = false;
        while Instant::now() < deadline {
            {
                let events = events.lock().unwrap();
                if events
                    .iter()
                    .any(|line| line.starts_with(&prefix) && line.contains("state: listening"))
                {
                    listened = true;
                    break;
                }
                if events
                    .iter()
                    .any(|line| line.starts_with(&prefix) && line.contains("state: error"))
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let snapshot = events.lock().unwrap().clone();
        println!("实例 {instance} 事件：{snapshot:#?}");
        assert!(
            listened,
            "实例 {instance} 没有进入监听（关麦重开后设备/模型应能重新启动）：{snapshot:#?}"
        );
        let ready = snapshot
            .iter()
            .find(|line| line.starts_with(&prefix) && line.contains("ready: "))
            .unwrap_or_else(|| panic!("实例 {instance} 没有就绪事件：{snapshot:#?}"));
        println!("实例 {instance} 就绪：{ready}");
        seen.extend(snapshot);

        // 立刻关闭，不等待：下个实例紧接着启动（模拟用户连续点开关）。
        drop(handle);
        stop.store(true, Ordering::SeqCst);
    }

    // 两个实例的身份必须不同，避免旧事件被当成本实例的。
    assert!(seen.iter().any(|line| line.starts_with("[1] ")));
    assert!(seen.iter().any(|line| line.starts_with("[2] ")));
}
