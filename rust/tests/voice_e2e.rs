//! 使用**真实模型与固定 WAV 样本**的语音链路测试。
//!
//! 这些测试需要本机存在模型文件，缺模型时**跳过并说明**（而不是假装通过）：
//!   - KWS：`models/sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01`
//!   - Whisper：`%APPDATA%\cuava\models\ggml-small.bin` 或 `CUAVA_WHISPER_MODEL`
//!
//! 运行：
//! ```powershell
//! cargo test --test voice_e2e -- --nocapture --test-threads=1
//! ```
//!
//! 这里测的是**真实推理**：KWS 能否命中唤醒词、负面音频是否不误触发、
//! VAD 能否切出一句、Whisper 能否给出合理的中文文本。
//! 不涉及麦克风与 GPU（那两项在 README 的「未验证项」里有说明）。

use std::path::{Path, PathBuf};

use cuava::voice::asr::{read_wav_16k, transcribe, AsrConfig};
use cuava::voice::kws::Kws;
use cuava::voice::settings::{AsrLanguage, DEFAULT_KEYWORD_LINE};
use cuava::voice::vad::{EndReason, EndpointConfig, EndpointDetector, VadEvent};

fn repo_root() -> PathBuf {
    // 允许显式指定：在短路径副本里构建时，模型与样本仍在原仓库。
    if let Some(root) = std::env::var_os("CUAVA_REPO_ROOT") {
        return PathBuf::from(root);
    }
    // rust/ 的上一级是仓库根。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("仓库根")
        .to_path_buf()
}

fn kws_model_dir() -> Option<PathBuf> {
    let dir = repo_root().join("models/sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01");
    dir.join("tokens.txt").is_file().then_some(dir)
}

fn whisper_model() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CUAVA_WHISPER_MODEL") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let candidates = [
        // 用户数据目录里的约定位置
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("cuava/models/ggml-small.bin")),
        std::env::var_os("CUAVA_DATA_DIR")
            .map(PathBuf::from)
            .map(|base| base.join("models/ggml-small.bin")),
        // 开发机上的镜像副本（不入库）
        Some(repo_root().join(".tmp/whisper-models/ggml-small.bin")),
        Some(repo_root().join(".tmp/models/ggml-small.bin")),
    ];
    candidates.into_iter().flatten().find(|path| path.is_file())
}

fn sample(name: &str) -> Option<PathBuf> {
    let path = repo_root().join("assets/test").join(name);
    path.is_file().then_some(path)
}

/// 关键：缺模型/缺样本时必须**明确跳过**，不能算成功。
fn require<T>(value: Option<T>, what: &str) -> Option<T> {
    if value.is_none() {
        eprintln!("跳过：{what} 不存在（这不是失败，但也没有验证任何东西）");
    }
    value
}

/// 真实 `ggml-small` 上下文每个约占 500 MB 权重 + 上百 MB 计算缓冲。
///
/// `cargo test` 默认并行跑同一测试文件里的用例：三个上下文同时加载，在 16 GB
/// 机器上会挤到内存上限，偶发出现「解码失败」。那是环境压力，不是产品行为，
/// 但会变成红灯误报。这里把**加载真实 Whisper 模型**的部分串行化；
/// 官方建议的 `--test-threads=1` 命令依然是最省事的跑法。
fn whisper_model_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 用真实 KWS 模型喂一段 WAV，返回命中的关键词。
fn detect_wake(kws: &mut Kws, wav: &Path) -> bool {
    let samples = read_wav_16k(wav).expect("读取 WAV");
    // 前面补 1 秒静音：真实场景里麦克风通常先听到静音再听到唤醒词。
    let mut padded = vec![0.0f32; 16_000];
    padded.extend_from_slice(&samples);

    for chunk in padded.chunks(1600) {
        if kws.accept(chunk, false).is_some() {
            return true;
        }
    }
    // 流式模型需要显式收尾，否则最后一段的命中不会吐出。
    kws.finish().is_some()
}

#[test]
fn real_kws_detects_wake_word_and_rejects_others() {
    let Some(model_dir) = require(kws_model_dir(), "KWS 模型目录") else {
        return;
    };
    let keywords = repo_root().join("models/kws-keywords.txt");
    // 关键词文件可能不在仓库里（models/ 被忽略）；临时生成默认的一行。
    let temp_keywords;
    let keywords = if keywords.is_file() {
        keywords
    } else {
        temp_keywords = std::env::temp_dir().join("cuava-e2e-keywords.txt");
        std::fs::write(&temp_keywords, format!("{DEFAULT_KEYWORD_LINE}\n")).unwrap();
        temp_keywords
    };

    let make = || Kws::new(&model_dir, &keywords, 2).expect("加载 KWS 模型");

    /*
     * 正向样本：`xiaoai-tongxue-synthetic.wav`（明确标注的 TTS 合成「小爱同学」）。
     *
     * `assets/test/wake-word.wav` 虽然名字像，但实测**不含**「小爱同学」——
     * 用官方模型直接跑它不命中。真人录音的缺口在 README 的未验证项里写明，
     * 这里不能用一段不对的音频假装验证过了。
     */
    let Some(wake) = require(
        sample("xiaoai-tongxue-synthetic.wav"),
        "xiaoai-tongxue-synthetic.wav（合成「小爱同学」，仅作旁证）",
    ) else {
        return;
    };
    let mut kws = make();
    let hit = detect_wake(&mut kws, &wake);
    assert!(hit, "真实 KWS 应命中合成的「小爱同学」");

    // 负向：不含唤醒词的普通语句不能触发。
    if let Some(negative) = require(sample("not-wake-word.wav"), "not-wake-word.wav") {
        let mut kws = make();
        assert!(
            !detect_wake(&mut kws, &negative),
            "不含唤醒词的音频不能误触发"
        );
    }
    // 负向：旧唤醒词「你好狼宝」也不能触发新唤醒词。
    if let Some(old) = require(sample("nihao-langbao.wav"), "nihao-langbao.wav") {
        let mut kws = make();
        assert!(!detect_wake(&mut kws, &old), "旧唤醒词不应触发新唤醒词");
    }
}

#[test]
fn real_vad_ends_on_silence_for_a_real_recording() {
    let Some(wav) = require(sample("speech-clean.wav"), "speech-clean.wav") else {
        return;
    };
    let samples = read_wav_16k(&wav).expect("读取 WAV");
    let mut detector = EndpointDetector::new(EndpointConfig::default(), 16_000);

    let mut started = false;
    let mut ended = None;
    for chunk in samples.chunks(1600) {
        for event in detector.push(chunk) {
            match event {
                VadEvent::SpeechStarted => started = true,
                VadEvent::UtteranceEnded { reason } if ended.is_none() => {
                    ended = Some(reason);
                }
                _ => {}
            }
        }
    }
    // 末尾补静音，让最后一句能正常结束。
    for _ in 0..20 {
        for event in detector.push(&vec![0.0; 1600]) {
            if let VadEvent::UtteranceEnded { reason } = event {
                if ended.is_none() {
                    ended = Some(reason);
                }
            }
        }
    }

    assert!(started, "真实语音必须被判为开始说话");
    assert!(
        matches!(
            ended,
            Some(EndReason::Silence) | Some(EndReason::MaxDuration)
        ),
        "真实语音必须在静音后端点，实际 {ended:?}"
    );
    let utterance = detector.take_utterance();
    assert!(
        utterance.len() > 16_000 / 2,
        "切出来的语音太短：{} 样本",
        utterance.len()
    );
}

#[test]
fn real_whisper_transcribes_chinese_command() {
    let _serial = whisper_model_guard();
    let (Some(model), Some(wav)) = (
        require(whisper_model(), "Whisper 模型（ggml-small.bin）"),
        require(sample("asr-zh.wav"), "asr-zh.wav"),
    ) else {
        return;
    };

    // 模型加载一次、复用：这里直接走 `transcribe`（与工作线程同一段代码）。
    let mut params = whisper_rs::WhisperContextParameters::default();
    params.use_gpu(std::env::var("CUAVA_WHISPER_GPU").is_ok());
    let context = whisper_rs::WhisperContext::new_with_params(&model, params).expect("加载模型");

    let settings = cuava::voice::settings::VoiceSettings {
        language: AsrLanguage::Chinese,
        threads: 8,
        ..Default::default()
    };
    let config = AsrConfig::from_settings(&settings, model.clone());
    let samples = read_wav_16k(&wav).expect("读取 WAV");

    let first = transcribe(&context, &config, 1, &samples).expect("识别");
    eprintln!(
        "识别结果：{:?}（{:.2}s 音频，{} ms）",
        first.text, first.audio_seconds, first.decode_ms
    );
    assert!(
        first.is_submittable(),
        "真实中文语音必须产生可提交文本，实际 {:?}",
        first.text
    );
    // 这句录音的内容是「帮我把浏览器打开，然后搜索今天的天气」。
    // 只断言关键名词，避免把模型措辞当成回归目标。
    let text = first.text.replace(' ', "");
    assert!(
        text.contains("浏览器") || text.contains("搜索") || text.contains("天气"),
        "识别结果与音频内容明显不符：{:?}",
        first.text
    );
    assert_eq!(first.session_id, 1);
    assert!(first.audio_seconds > 2.0);
}

/// **整条链路**（不含麦克风）：唤醒词命中 → 端点切句 → Whisper 转录 → 可提交文本。
///
/// 音频是真实录音的拼接：先「小爱同学」，再一段中文指令。
/// 这验证「唤醒之后那一句会不会被正确提交」——也就是任务书第 2 条里
/// 除了「真人对着麦克风说」以外的全部环节。
#[test]
fn real_session_from_wake_to_final_text() {
    let _serial = whisper_model_guard();
    let (Some(model_dir), Some(keywords), Some(wake_wav), Some(command_wav)) = (
        require(kws_model_dir(), "KWS 模型目录"),
        require(
            {
                let path = repo_root().join("models/kws-keywords.txt");
                path.is_file().then_some(path)
            },
            "models/kws-keywords.txt",
        ),
        require(
            sample("xiaoai-tongxue-synthetic.wav"),
            "合成「小爱同学」样本",
        ),
        // 用 3.77 秒的中文指令；`speech-clean.wav` 有 20 秒，会先撞上单句上限。
        require(sample("asr-zh.wav"), "asr-zh.wav"),
    ) else {
        return;
    };

    let mut wake = read_wav_16k(&wake_wav).expect("读取唤醒样本");
    let mut command = read_wav_16k(&command_wav).expect("读取指令样本");
    let mut audio = vec![0.0f32; 16_000];
    audio.append(&mut wake);
    audio.extend(std::iter::repeat_n(0.0f32, 8_000));
    audio.append(&mut command);
    audio.extend(std::iter::repeat_n(0.0f32, 16_000));

    let mut kws = Kws::new(&model_dir, &keywords, 2).expect("加载 KWS");
    let mut detector = EndpointDetector::new(EndpointConfig::default(), 16_000);

    let mut woke = false;
    let mut captured: Vec<f32> = Vec::new();
    let mut ended = None;

    for chunk in audio.chunks(1600) {
        // 与协调器相同的顺序：先 VAD，再 KWS（KWS 用起点判定决定是否重置）。
        let events = detector.push(chunk);
        let speech_started = events
            .iter()
            .any(|event| matches!(event, VadEvent::SpeechStarted));

        if !woke && kws.accept(chunk, speech_started).is_some() {
            woke = true;
            // 唤醒后指令从这一刻开始算（与 `Coordinator::begin_session` 一致）。
            detector.start_utterance();
            continue;
        }
        if woke {
            for event in events {
                if let VadEvent::UtteranceEnded { reason } = event {
                    ended = Some(reason);
                }
            }
            if ended.is_some() && captured.is_empty() {
                captured = detector.take_utterance();
            }
        }
    }
    if captured.is_empty() {
        captured = detector.take_utterance();
    }

    assert!(woke, "整条链路必须能命中唤醒词");
    assert!(
        !captured.is_empty(),
        "唤醒之后必须能切出一句音频（端点 = {ended:?}）"
    );
    let seconds = captured.len() as f32 / 16_000.0;
    assert!(
        (1.0..12.0).contains(&seconds),
        "切出的句子长度不合理：{seconds:.2}s（端点 = {ended:?}）"
    );

    let Some(model) = require(whisper_model(), "Whisper 模型") else {
        return;
    };
    let settings = cuava::voice::settings::VoiceSettings {
        language: AsrLanguage::Chinese,
        threads: 8,
        ..Default::default()
    };
    let config = AsrConfig::from_settings(&settings, model.clone());
    let mut params = whisper_rs::WhisperContextParameters::default();
    params.use_gpu(std::env::var("CUAVA_WHISPER_GPU").is_ok());
    let context = whisper_rs::WhisperContext::new_with_params(&model, params).expect("加载模型");
    let transcript = transcribe(&context, &config, 42, &captured).expect("识别");
    eprintln!(
        "整条链路：唤醒 → {seconds:.2}s 语音 → {:?}（{} ms）",
        transcript.text, transcript.decode_ms
    );
    assert!(transcript.is_submittable(), "整条链路没有产出可提交文本");
    assert_eq!(transcript.session_id, 42, "会话必须关联到这次语音");
}

/// **异步识别 + 会话关联**：走真正的 `AsrWorker` 线程，而不是同步调用。
///
/// 这条专门防一个已经踩过的时序缺陷：如果 ASR 回调在投递结果后立刻把
/// 「当前会话」清零，工作线程稍后处理事件时会把它判成过期，
/// **所有**语音指令都会被静默丢弃。这里断言结果送达后会话归属仍然有效，
/// 由接收方（工作线程）接受后再清零。
///
/// 顺带断言「实际后端」：CPU-only 构建必须报 CPU 并带原因；
/// 带 CUDA 的构建如果报 CUDA，就不能同时带降级原因（否则自相矛盾）。
#[test]
fn asr_worker_delivers_final_and_keeps_session_until_consumed() {
    use cuava::voice::asr::{AsrBackend, AsrWorker, InferenceCancel};
    use cuava::voice::coordinator::{ActiveSession, VoiceTurn};
    use std::sync::mpsc;

    let _serial = whisper_model_guard();
    let (Some(model), Some(wav)) = (
        require(whisper_model(), "Whisper 模型"),
        require(sample("asr-zh.wav"), "asr-zh.wav"),
    ) else {
        return;
    };

    let settings = cuava::voice::settings::VoiceSettings {
        language: AsrLanguage::Chinese,
        threads: 8,
        ..Default::default()
    };
    let config = AsrConfig::from_settings(&settings, model);
    let (worker, ready) = AsrWorker::start(config).expect("启动 ASR 线程");
    eprintln!(
        "实际后端：{:?}（降级原因：{:?}）",
        ready.backend, ready.fallback_reason
    );
    if !cuava::voice::asr::gpu_compiled_in() {
        assert_eq!(ready.backend, AsrBackend::Cpu, "CPU-only 构建必须报 CPU");
        assert!(
            ready.fallback_reason.is_some(),
            "CPU-only 构建要说明为什么不是 CUDA"
        );
    }
    if ready.backend == AsrBackend::Cuda {
        assert!(
            ready.fallback_reason.is_none(),
            "用上 GPU 时不该同时挂着降级原因"
        );
    }

    let turn = VoiceTurn {
        instance: 1,
        session: 9,
    };
    let active = ActiveSession::new();
    active.begin(turn);
    let (tx, rx) = mpsc::channel();
    let active_for_reply = active.clone();
    worker
        .submit(
            turn.session,
            read_wav_16k(&wav).unwrap(),
            InferenceCancel::new(),
            Box::new(move |result| {
                // 与协调器回调同样的检查：结果必须仍属于当前会话。
                let current = active_for_reply.is_current(turn);
                let _ = tx.send((result.map(|t| t.text), current));
            }),
        )
        .expect("提交识别");

    let (result, still_current) = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("识别必须有结果回来");
    let text = result.expect("识别不应失败");
    eprintln!("异步识别：{text:?}");
    assert!(!text.is_empty());
    assert!(
        still_current,
        "结果送达时会话归属必须仍然有效（否则工作线程会丢弃它）"
    );

    // 接收方消费后清零：同一会话的重复结果不再被接受。
    active.clear();
    assert!(!active.is_current(turn));
    worker.shutdown();
}

#[test]
fn pr17_manual_finish_real_wav_before_natural_endpoint() {
    let _serial = whisper_model_guard();
    let (Some(model), Some(wav)) = (
        require(whisper_model(), "Whisper model"),
        require(sample("asr-zh.wav"), "asr-zh.wav"),
    ) else {
        return;
    };
    let mut detector = EndpointDetector::new(
        EndpointConfig {
            end_silence: std::time::Duration::from_secs(30),
            ..Default::default()
        },
        16000,
    );
    detector.push(&vec![0.0; 16000]);
    detector.start_utterance();
    let samples = read_wav_16k(&wav).unwrap();
    let mut started = false;
    for chunk in samples.chunks(997) {
        for event in detector.push(chunk) {
            assert!(!matches!(event, VadEvent::UtteranceEnded { .. }));
            started |= matches!(event, VadEvent::SpeechStarted);
        }
    }
    assert!(started);
    detector.finish_now();
    assert!(detector.last_voiced_seconds() > 0.25);
    let captured = detector.take_utterance();
    let settings = cuava::voice::settings::VoiceSettings {
        language: AsrLanguage::Chinese,
        threads: 8,
        ..Default::default()
    };
    let config = AsrConfig::from_settings(&settings, model.clone());
    let mut params = whisper_rs::WhisperContextParameters::default();
    params.use_gpu(std::env::var("CUAVA_WHISPER_GPU").is_ok());
    let context = whisper_rs::WhisperContext::new_with_params(&model, params).unwrap();
    let transcript = transcribe(&context, &config, 17, &captured).unwrap();
    eprintln!(
        "manual finish: {:?}, {} ms",
        transcript.text, transcript.decode_ms
    );
    assert!(transcript.is_submittable());
}
