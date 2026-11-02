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
    self, ActiveSession, CoordinatorConfig, SharedSink, VoiceEventKind,
};
use cuava::voice::settings::{VoicePaths, VoiceSettings};

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
            VoiceEventKind::Level { .. } => return,
        };
        eprintln!("[voice] {line}");
        sink_events.lock().unwrap().push(line);
    });

    let active = ActiveSession::new();
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let handle =
        coordinator::spawn(config, sink, active.clone(), stop.clone(), 1).expect("启动语音线程");

    // 等到「就绪 + 监听」或超时。
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut listened = false;
    while Instant::now() < deadline {
        {
            let events = events.lock().unwrap();
            if events.iter().any(|line| line.starts_with("ready: "))
                && events.iter().any(|line| line == "state: listening")
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
        .find(|line| line.starts_with("ready: "))
        .unwrap_or_else(|| panic!("模型与设备没有就绪：{snapshot:#?}"));
    println!("就绪行：{ready_line}");
    assert!(
        snapshot.iter().any(|line| line == "state: listening"),
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
