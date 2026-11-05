//! Manual release validation with a real model and WAV, in both runtime modes.
use cuava::voice::asr::{read_wav_16k, AsrBackend, AsrConfig, AsrWorker, InferenceCancel};
use cuava::voice::settings::VoiceSettings;
use std::{path::PathBuf, sync::mpsc, time::Duration};

#[test]
#[ignore = "requires a real CUDA GPU, Whisper model and 16 kHz WAV"]
fn real_cuda_and_cpu_in_same_build() {
    let model =
        PathBuf::from(std::env::var_os("CUAVA_WHISPER_MODEL").expect("CUAVA_WHISPER_MODEL"));
    let wav = PathBuf::from(std::env::var_os("CUAVA_ASR_WAV").expect("CUAVA_ASR_WAV"));
    let samples = read_wav_16k(&wav).expect("read WAV");
    for (gpu, expected) in [(true, AsrBackend::Cuda), (false, AsrBackend::Cpu)] {
        let settings = VoiceSettings {
            use_gpu: gpu,
            ..Default::default()
        };
        let (worker, ready) = AsrWorker::start(AsrConfig::from_settings(&settings, model.clone()))
            .expect("load model");
        assert_eq!(
            ready.backend, expected,
            "fallback: {:?}",
            ready.fallback_reason
        );
        let (tx, rx) = mpsc::channel();
        worker
            .submit(
                1,
                samples.clone(),
                InferenceCancel::new(),
                Box::new(move |result| {
                    let _ = tx.send(result);
                }),
            )
            .expect("submit");
        let result = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("timeout")
            .expect("decode");
        eprintln!(
            "Whisper {}: {:?} ({} ms)",
            ready.backend.label(),
            result.text,
            result.decode_ms
        );
        assert!(!result.text.trim().is_empty());
        worker.shutdown();
    }
}
