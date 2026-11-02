//! whisper.cpp 基准用的示例程序（**开发实验**，不是产品路径）。
//!
//! 产品里 Whisper 由 `voice::asr::AsrWorker` 常驻线程调用；这个示例只把同一段
//! `transcribe()` 拿出来，方便和 faster-whisper 对照：
//!
//! ```powershell
//! # CPU 基线
//! cargo build --release --example whisper_bench
//! .\target\release\examples\whisper_bench.exe assets\test\asr-zh.wav 8 3
//!
//! # CUDA（需要 CUDA Toolkit；见 README）
//! cargo build --release --features cuda --example whisper_bench
//! ```
//!
//! 输出是 JSON（一行），字段与 `tools/bench-whisper.mjs` 的期望一致。

use std::path::PathBuf;
use std::time::Instant;

use cuava::voice::asr::{read_wav_16k, transcribe, AsrConfig};
use cuava::voice::settings::{AsrLanguage, VoiceSettings};

fn main() {
    let mut args = std::env::args().skip(1);
    let wav = PathBuf::from(
        args.next()
            .expect("用法: whisper_bench <wav> [threads] [repeats]"),
    );
    let threads: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(8);
    let repeats: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1);

    let model = std::env::var_os("CUAVA_WHISPER_MODEL")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .map(|base| base.join("cuava").join("models").join("ggml-small.bin"))
        })
        .filter(|path| path.is_file())
        .expect("找不到 Whisper 模型：设置 CUAVA_WHISPER_MODEL 或放到 %APPDATA%\\cuava\\models");

    let settings = VoiceSettings {
        language: AsrLanguage::Chinese,
        threads: threads as u32,
        use_gpu: true,
        ..Default::default()
    };
    let config = AsrConfig::from_settings(&settings, model.clone());

    // 冷启动：模型加载单独计时（与「端点后转录延迟」区分开）。
    let load_start = Instant::now();
    let mut params = whisper_rs::WhisperContextParameters::default();
    params.use_gpu(cuava::voice::asr::gpu_compiled_in() && settings.use_gpu);
    let context = whisper_rs::WhisperContext::new_with_params(&model, params).expect("加载模型");
    let load_ms = load_start.elapsed().as_millis();

    let samples = read_wav_16k(&wav).expect("读取 WAV");
    let mut decode_ms = Vec::new();
    let mut text = String::new();
    for _ in 0..repeats.max(1) {
        let result = transcribe(&context, &config, 1, &samples).expect("识别");
        decode_ms.push(result.decode_ms);
        text = result.text;
    }
    decode_ms.sort_unstable();

    let audio_seconds = samples.len() as f64 / 16_000.0;
    let p50 = decode_ms[decode_ms.len() / 2] as f64;
    let p95_index = ((decode_ms.len() as f64 * 0.95) as usize).min(decode_ms.len() - 1);
    let payload = serde_json::json!({
        "engine": "whisper.cpp",
        "model": model.display().to_string(),
        "cuda": cuava::voice::asr::gpu_compiled_in(),
        "loadMs": load_ms,
        "threads": threads,
        "wav": wav.display().to_string(),
        "audioSeconds": (audio_seconds * 100.0).round() / 100.0,
        "decodeMs": decode_ms,
        "p50Ms": p50,
        "p95Ms": decode_ms[p95_index],
        "rtf": (p50 / 1000.0 / audio_seconds * 1000.0).round() / 1000.0,
        "text": text,
    });
    println!("{payload}");
}
