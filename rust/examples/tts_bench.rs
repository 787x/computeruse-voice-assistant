//! 本地 TTS 的执行提供者性能对比（CPU vs CUDA）。
//!
//! 用法（模型目录用环境变量指定）：
//! ```text
//! set CUAVA_TTS_MODEL_DIR=D:\path\to\kokoro-multi-lang-v1_0
//! cargo run --release --example tts_bench -- cpu
//! cargo run --release --example tts_bench -- cuda
//! ```
//!
//! 说明：CUDA 需要 shared 版 sherpa-onnx 运行时（`-cuda` 预编译包）与
//! cuDNN 9 / cuBLAS / cudart 的 DLL 在 PATH 里；缺任何一项都会回退 CPU，
//! 这里会打印实际生效的提供者，避免把"回退"当成"CUDA 更快"。
//!
//! 依赖 dev-dependencies 里的 tokio（`SpeechStream::recv` 是 async）。

use std::time::{Duration, Instant};

use cuava::cancel::CancelToken;
use cuava::voice::tts::local::LocalTts;
use cuava::voice::tts::{SynthRequest, TtsBackend, TtsEngine, TtsEvent, TtsSettings};

/// 一句包含中文、数字与中英混排的代表性文本。
const TEXT: &str =
    "已完成。内存占用 1.5 GB，CPU 占用 12%。接下来我会打开记事本并输入 hello world。";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let provider = std::env::args().nth(1).unwrap_or_else(|| "cpu".to_string());
    let model_dir = std::env::var("CUAVA_TTS_MODEL_DIR")
        .unwrap_or_else(|_| "target/models/kokoro-multi-lang-v1_0".to_string());

    let settings = TtsSettings {
        enabled: true,
        backend: TtsBackend::Local,
        local_model_dir: model_dir.clone(),
        local_provider: provider.clone(),
        ..TtsSettings::default()
    };
    let engine = match LocalTts::new(&settings) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("本地 TTS 初始化失败：{error}");
            std::process::exit(1);
        }
    };
    println!("请求的提供者：{provider}");

    // 预热（与运行期 `SpeechRuntime::apply_settings` 之后的后台预热一致）。
    let warm_started = Instant::now();
    engine.warm_up();
    println!(
        "预热耗时 {} ms，实际提供者 {}",
        warm_started.elapsed().as_millis(),
        engine.active_provider()
    );

    // 预热之后的第一次合成：这才是用户点"试音"或第一句回复时的真实延迟。
    let first = run_once(&engine, 1, TEXT).await;
    println!(
        "预热后首句：首音 {} ms，总计 {} ms，音频 {:.2} s，RTF {:.3}",
        first.first_ms,
        first.total_ms,
        first.audio_s,
        first.rtf()
    );

    // 冷启动（不预热时的对照，只用于说明差异）。
    let cold_engine = LocalTts::new(&settings).expect("重新创建引擎");
    let cold = run_once(&cold_engine, 1, TEXT).await;
    println!(
        "冷启动：首音 {} ms，总计 {} ms，音频 {:.2} s，RTF {:.3}，实际提供者 {}",
        cold.first_ms,
        cold.total_ms,
        cold.audio_s,
        cold.rtf(),
        engine.active_provider()
    );

    // 之后是热路径：同一句连续 3 次，取中位数意义更强的平均。
    let mut first_sum = 0.0;
    let mut total_sum = 0.0;
    let mut audio_sum = 0.0;
    const ROUNDS: usize = 3;
    for _ in 0..ROUNDS {
        let warm = run_once(&engine, 2, TEXT).await;
        first_sum += warm.first_ms;
        total_sum += warm.total_ms;
        audio_sum += warm.audio_s;
    }
    let first_avg = first_sum / ROUNDS as f64;
    let total_avg = total_sum / ROUNDS as f64;
    let audio_avg = audio_sum / ROUNDS as f64;
    println!(
        "热路径（{} 次平均）：首音 {:.1} ms，总计 {:.1} ms，音频 {:.2} s，RTF {:.3}",
        ROUNDS,
        first_avg,
        total_avg,
        audio_avg,
        total_avg / (audio_avg * 1000.0)
    );
    println!("实际生效的提供者：{}", engine.active_provider());
}

struct Stats {
    first_ms: f64,
    total_ms: f64,
    audio_s: f64,
}

impl Stats {
    /// 实时率：合成 1 秒音频平均要多少秒算力（越小越快）。
    fn rtf(&self) -> f64 {
        self.total_ms / (self.audio_s * 1000.0)
    }
}

async fn run_once(engine: &LocalTts, turn: u64, text: &str) -> Stats {
    let request = SynthRequest {
        speech_id: turn,
        turn,
        sentence: 0,
        text: text.to_string(),
        style: String::new(),
        cancel: CancelToken::new(),
    };
    let started = Instant::now();
    let mut stream = match engine.start(request) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("合成启动失败：{error}");
            std::process::exit(1);
        }
    };
    let mut first: Option<Duration> = None;
    let mut samples = 0usize;
    let mut rate = 24_000u32;
    while let Some(event) = stream.recv().await {
        match event {
            TtsEvent::Chunk(chunk) => {
                if first.is_none() {
                    first = Some(started.elapsed());
                }
                rate = chunk.sample_rate;
                samples += chunk.samples.len();
            }
            TtsEvent::Finished => break,
            TtsEvent::Failed(message) => {
                eprintln!("合成失败：{message}");
                std::process::exit(1);
            }
        }
    }
    let total = started.elapsed();
    Stats {
        first_ms: first.unwrap_or(total).as_secs_f64() * 1000.0,
        total_ms: total.as_secs_f64() * 1000.0,
        audio_s: samples as f64 / rate as f64,
    }
}
