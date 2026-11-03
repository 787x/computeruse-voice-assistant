//! 本地 ONNX 语音合成（sherpa-onnx `OfflineTts`，Kokoro 中英多语言模型）。
//!
//! 选型与边界（见 `docs/12-PR7-TTS与语音播报架构决策.md`）：
//!  - 只支持**一个**模型族：Kokoro 多语言（中英混读），用上游要求的完整
//!    词典/voices/tokens/文本前端资源，不自造残缺的中文拼音替代品；
//!  - 模型权重**不随源码提交**，也不自动下载；路径由用户显式指定或放在
//!    数据目录的约定位置；
//!  - 合成是 CPU 推理，跑在独立线程里，模型进程内复用（不每句重载）；
//!  - 断网可用，不发任何网络请求。
//!
//! 取消：把取消令牌接到 `OfflineTts` 的进度回调上，回调返回 `false` 即停止生成。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig,
};

use crate::cancel::CancelToken;
use crate::config::AppPaths;

use super::{
    AudioFormat, SpeechChunk, SpeechStream, SynthRequest, TtsEngine, TtsEvent, TtsSettings,
};

/// 数据目录下约定使用的模型目录名（与 sherpa-onnx 上游发布包同名）。
pub const DEFAULT_MODEL_DIR: &str = "kokoro-multi-lang-v1_0";
/// Kokoro 的上游发布包体积约 350 MB，这里不做下载，只校验文件在不在。
pub const MODEL_FILES: &[&str] = &["model.onnx", "voices.bin", "tokens.txt"];
/// 可选的中英词典（有就带上，中英混读质量依赖它们）。
pub const LEXICON_FILES: &[&str] = &["lexicon-us-en.txt", "lexicon-zh.txt"];

/// 校验通过后的模型文件布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelLayout {
    pub dir: PathBuf,
    pub model: PathBuf,
    pub voices: PathBuf,
    pub tokens: PathBuf,
    /// 逗号分隔的词典列表（上游协议如此）。
    pub lexicon: Option<String>,
    pub espeak_data: Option<PathBuf>,
}

impl ModelLayout {
    pub fn describe(&self) -> String {
        format!("Kokoro 本地模型（{}）", self.dir.display())
    }
}

/// 解析并校验本地模型目录。
///
/// 顺序：显式配置 → 数据目录 `models/<name>` → 仓库 `models/<name>`（开发机）。
/// 都找不到时返回**可读原因**（要告诉用户放哪里），而不是静默失败。
pub fn resolve_model_dir(configured: &str, paths: &AppPaths) -> Result<ModelLayout, String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return validate_model_dir(Path::new(configured));
    }
    let mut tried: Vec<PathBuf> = vec![paths.root.join("models").join(DEFAULT_MODEL_DIR)];
    if let Some(repo) = crate::voice::settings::find_repo_models() {
        tried.push(repo.join(DEFAULT_MODEL_DIR));
    }
    for candidate in &tried {
        if let Ok(layout) = validate_model_dir(candidate) {
            return Ok(layout);
        }
    }
    Err(format!(
        "找不到本地 TTS 模型目录（需要 {}）。\n已尝试：\n{}\n\
         把 Kokoro 多语言模型放到 {}，或在设置里指定目录。",
        MODEL_FILES.join(" / "),
        tried
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        paths.root.join("models").join(DEFAULT_MODEL_DIR).display()
    ))
}

/// 目录里必须有的文件都在，才认为模型可用。
pub fn validate_model_dir(dir: &Path) -> Result<ModelLayout, String> {
    if !dir.is_dir() {
        return Err(format!("模型目录不存在：{}", dir.display()));
    }
    for name in MODEL_FILES {
        if !dir.join(name).is_file() {
            return Err(format!(
                "模型目录缺少 {name}：{}（需要完整的 ONNX 模型与词典/voices 资源）",
                dir.display()
            ));
        }
    }
    let lexicons: Vec<String> = LEXICON_FILES
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string())
        .collect();
    let espeak = dir.join("espeak-ng-data");
    Ok(ModelLayout {
        dir: dir.to_path_buf(),
        model: dir.join("model.onnx"),
        voices: dir.join("voices.bin"),
        tokens: dir.join("tokens.txt"),
        lexicon: if lexicons.is_empty() {
            None
        } else {
            Some(lexicons.join(","))
        },
        espeak_data: espeak.is_dir().then_some(espeak),
    })
}

/// 本地 TTS 后端。
///
/// `engine` 是进程内复用的模型句柄：第一次合成时在合成线程里加载，
/// 之后所有句子共用（不每句重载）。退出时随 `LocalTts` 一起释放。
#[derive(Clone)]
pub struct LocalTts {
    layout: ModelLayout,
    sid: i32,
    speed: f32,
    threads: i32,
    /// 首选执行提供者：`cuda`（默认）或 `cpu`。
    provider: String,
    /// 实际生效的提供者（CUDA 不可用时会变成 CPU，标签里如实显示）。
    active_provider: Arc<Mutex<String>>,
    engine: Arc<Mutex<Option<Arc<OfflineTts>>>>,
}

/// 这份二进制是否链接了 **GPU 版** sherpa-onnx。
///
/// 运行期没有可靠的探测手段（`OfflineTts::create` 在 CPU 版库里会自己
/// "Fallback to cpu!" 并返回成功），所以用编译期 feature 表达。
const CUDA_TTS_BUILD: bool = cfg!(feature = "tts-cuda");

impl LocalTts {
    pub fn new(settings: &TtsSettings) -> Result<Self, String> {
        let paths = AppPaths::resolve();
        let layout = resolve_model_dir(&settings.local_model_dir, &paths)?;
        let wants_cuda = !settings.local_provider.eq_ignore_ascii_case("cpu");
        let provider = if wants_cuda && CUDA_TTS_BUILD {
            "cuda".to_string()
        } else {
            "cpu".to_string()
        };
        // 用户要 CUDA 但这份二进制没带：如实写在界面上，不假装。
        let effective = if wants_cuda && !CUDA_TTS_BUILD {
            "cpu（本构建未启用 CUDA TTS）".to_string()
        } else {
            provider.clone()
        };
        Ok(Self {
            layout,
            sid: settings.local_sid,
            speed: settings.local_speed.clamp(0.5, 2.0),
            threads: settings_threads(),
            active_provider: Arc::new(Mutex::new(effective)),
            provider,
            engine: Arc::new(Mutex::new(None)),
        })
    }

    /// 供测试：用已校验的布局直接构造。
    pub fn with_layout(layout: ModelLayout, sid: i32, speed: f32) -> Self {
        let provider = if CUDA_TTS_BUILD { "cuda" } else { "cpu" };
        Self {
            layout,
            sid,
            speed,
            threads: settings_threads(),
            active_provider: Arc::new(Mutex::new(provider.to_string())),
            provider: provider.to_string(),
            engine: Arc::new(Mutex::new(None)),
        }
    }

    pub fn layout(&self) -> &ModelLayout {
        &self.layout
    }

    /// 实际生效的执行提供者（`cuda` / `cpu` / `cpu（CUDA 不可用，已回退）`）。
    pub fn active_provider(&self) -> String {
        self.active_provider
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn config_for(&self, provider: &str) -> OfflineTtsConfig {
        OfflineTtsConfig {
            model: OfflineTtsModelConfig {
                kokoro: OfflineTtsKokoroModelConfig {
                    model: Some(self.layout.model.display().to_string()),
                    voices: Some(self.layout.voices.display().to_string()),
                    tokens: Some(self.layout.tokens.display().to_string()),
                    lexicon: self.layout.lexicon.clone(),
                    // 2.0 起 `data_dir` 已弃用；仍按上游示例带上，便于旧模型包工作。
                    data_dir: self
                        .layout
                        .espeak_data
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    length_scale: 1.0,
                    ..Default::default()
                },
                num_threads: self.threads,
                provider: Some(provider.to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// 加载模型（幂等）。返回可读错误：调用方**只**禁用播报，不影响文本。
    ///
    /// 执行提供者默认是 **CUDA**；运行时拿不到 GPU（链接的是 CPU 版
    /// sherpa-onnx、缺 cuDNN 9、显存被占）时**回退 CPU** 并把实际提供者
    /// 记下来给人看——绝不假装 CUDA 生效。
    fn ensure_loaded(&self) -> Result<Arc<OfflineTts>, String> {
        let mut guard = self
            .engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(engine) = guard.as_ref() {
            return Ok(engine.clone());
        }
        let started = std::time::Instant::now();
        let (engine, used) = match OfflineTts::create(&self.config_for(&self.provider)) {
            Some(engine) => (engine, self.provider.clone()),
            None if self.provider != "cpu" => {
                eprintln!(
                    "[tts] 本地模型用 {} 加载失败，回退 CPU（需要 GPU 版 sherpa-onnx 运行时与 cuDNN 9）",
                    self.provider
                );
                let Some(engine) = OfflineTts::create(&self.config_for("cpu")) else {
                    return Err(format!(
                        "本地 TTS 模型加载失败：{}（文件不完整或与当前 sherpa-onnx 版本不兼容）",
                        self.layout.dir.display()
                    ));
                };
                (engine, "cpu（CUDA 不可用，已回退）".to_string())
            }
            None => {
                return Err(format!(
                    "本地 TTS 模型加载失败：{}（文件不完整或与当前 sherpa-onnx 版本不兼容）",
                    self.layout.dir.display()
                ))
            }
        };
        let speakers = engine.num_speakers();
        if self.sid >= speakers {
            return Err(format!(
                "音色编号 {} 超出模型支持范围（共 {} 个音色，编号 0~{}）",
                self.sid,
                speakers,
                speakers - 1
            ));
        }
        eprintln!(
            "[tts] 本地模型加载完成：{} Hz，{} 个音色，{}，耗时 {} ms",
            engine.sample_rate(),
            speakers,
            used,
            started.elapsed().as_millis()
        );
        *self
            .active_provider
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = used;
        let engine = Arc::new(engine);
        *guard = Some(engine.clone());
        Ok(engine)
    }

    /// 在**当前线程**上阻塞合成（由 `start` 里的独立线程调用）。
    fn synthesize_blocking(
        &self,
        request: &SynthRequest,
        tx: &tokio::sync::mpsc::UnboundedSender<TtsEvent>,
    ) -> Result<(), String> {
        let engine = self.ensure_loaded()?;
        if request.cancel.is_cancelled() {
            return Ok(());
        }
        let sample_rate = engine.sample_rate();
        if sample_rate <= 0 {
            return Err("本地 TTS 报告了无效的采样率".to_string());
        }
        let config = GenerationConfig {
            sid: self.sid,
            speed: self.speed,
            ..Default::default()
        };
        let cancel: CancelToken = request.cancel.clone();
        let sink = tx.clone();
        let speech_id = request.speech_id;
        let turn = request.turn;
        let sentence = request.sentence;
        // 回调是 move 闭包，计数要能被回调写、被这里读，所以用共享原子。
        let produced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let produced_in_cb = produced.clone();
        let audio = engine.generate_with_config(
            &request.text,
            &config,
            Some(move |samples: &[f32], _progress: f32| {
                // 取消：回调返回 false 让 sherpa-onnx 立刻停下（这是唯一
                // 能在推理中途打断它的手段，否则要等整句算完）。
                if cancel.is_cancelled() {
                    return false;
                }
                if samples.is_empty() {
                    return true;
                }
                produced_in_cb.fetch_add(samples.len(), std::sync::atomic::Ordering::Relaxed);
                let _ = sink.send(TtsEvent::Chunk(SpeechChunk {
                    speech_id,
                    turn,
                    sentence,
                    sample_rate: sample_rate as u32,
                    channels: 1,
                    format: AudioFormat::Pcm16Le,
                    samples: samples.to_vec(),
                }));
                true
            }),
        );
        if request.cancel.is_cancelled() {
            return Ok(());
        }
        let produced = produced.load(std::sync::atomic::Ordering::Relaxed);
        match audio {
            Some(_) if produced > 0 => Ok(()),
            Some(_) => Err("本地 TTS 没有生成有效音频（文本可能只含无法发音的符号）".to_string()),
            None => Err(format!(
                "本地 TTS 合成失败：{}（模型不支持这段文本或推理出错）",
                self.layout.dir.display()
            )),
        }
    }
}

fn settings_threads() -> i32 {
    let fallback = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4);
    fallback.clamp(1, 4) as i32
}

impl TtsEngine for LocalTts {
    fn label(&self) -> String {
        format!(
            "{} · 音色 {} · 语速 {:.1} · {}",
            self.layout.describe(),
            self.sid,
            self.speed,
            self.active_provider()
        )
    }

    fn start(&self, request: SynthRequest) -> Result<SpeechStream, String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TtsEvent>();
        let engine = self.clone();
        // 本地推理是阻塞的：放独立线程，绝不占用 tokio 工作线程或界面线程。
        std::thread::Builder::new()
            .name("cuava-tts-local".to_string())
            .spawn(move || {
                let result = engine.synthesize_blocking(&request, &tx);
                match result {
                    Ok(()) => {
                        if !request.cancel.is_cancelled() {
                            let _ = tx.send(TtsEvent::Finished);
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(TtsEvent::Failed(error));
                    }
                }
            })
            .map_err(|error| format!("无法启动本地 TTS 线程：{error}"))?;
        Ok(SpeechStream::new(rx))
    }

    /// 预热：加载模型 + 合成一个很短的句子（只为了让 CUDA 的首次初始化和
    /// 显存分配提前发生），音频直接丢弃。
    fn warm_up(&self) {
        let started = std::time::Instant::now();
        let engine = match self.ensure_loaded() {
            Ok(engine) => engine,
            Err(error) => {
                eprintln!("[tts] 预热失败：{error}");
                return;
            }
        };
        let config = GenerationConfig {
            sid: self.sid,
            speed: self.speed,
            ..Default::default()
        };
        let _ = engine.generate_with_config("好", &config, None::<fn(&[f32], f32) -> bool>);
        eprintln!(
            "[tts] 本地模型预热完成（{}），耗时 {} ms",
            self.active_provider(),
            started.elapsed().as_millis()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("tts-local-{name}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 缺文件时要指出**缺哪个**，而不是只说"加载失败"。
    #[test]
    fn missing_files_are_named() {
        let dir = temp_root("missing");
        let error = validate_model_dir(&dir).unwrap_err();
        assert!(error.contains("model.onnx"), "{error}");

        std::fs::write(dir.join("model.onnx"), b"x").unwrap();
        let error = validate_model_dir(&dir).unwrap_err();
        assert!(error.contains("voices.bin"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_lexicons_are_joined_and_espeak_detected() {
        let dir = temp_root("layout");
        for name in MODEL_FILES {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let layout = validate_model_dir(&dir).unwrap();
        assert!(layout.lexicon.is_none());
        assert!(layout.espeak_data.is_none());

        std::fs::write(dir.join("lexicon-us-en.txt"), b"x").unwrap();
        std::fs::write(dir.join("lexicon-zh.txt"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("espeak-ng-data")).unwrap();
        let layout = validate_model_dir(&dir).unwrap();
        let lexicon = layout.lexicon.clone().unwrap();
        assert!(lexicon.contains("lexicon-us-en.txt"));
        assert!(lexicon.contains("lexicon-zh.txt"));
        assert_eq!(lexicon.matches(',').count(), 1);
        assert!(layout.espeak_data.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 显式配置的目录写错时必须报错，不能悄悄换一个目录顶替。
    #[test]
    fn explicit_dir_is_not_replaced_by_defaults() {
        let paths = AppPaths::with_root(temp_root("explicit-root"));
        let error = resolve_model_dir("Z:/definitely/not/here", &paths).unwrap_err();
        assert!(error.contains("不存在"), "{error}");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// 找不到模型时的提示必须告诉用户放哪里。
    #[test]
    fn missing_model_message_is_actionable() {
        let paths = AppPaths::with_root(temp_root("missing-root"));
        let error = resolve_model_dir("", &paths).unwrap_err();
        assert!(error.contains("Kokoro"), "{error}");
        assert!(error.contains("设置"), "{error}");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// 文件齐了但内容是垃圾：必须报"加载失败"，不能假装合成成功。
    #[tokio::test]
    async fn corrupt_model_reports_load_failure() {
        let dir = temp_root("corrupt");
        for name in MODEL_FILES {
            std::fs::write(dir.join(name), b"not a real onnx model").unwrap();
        }
        let layout = validate_model_dir(&dir).unwrap();
        let engine = LocalTts::with_layout(layout, 0, 1.0);
        let mut stream = engine
            .start(SynthRequest {
                speech_id: 1,
                turn: 1,
                sentence: 0,
                text: "你好".to_string(),
                style: String::new(),
                cancel: CancelToken::new(),
            })
            .unwrap();
        let mut failed = None;
        while let Some(event) = stream.recv().await {
            match event {
                TtsEvent::Failed(message) => failed = Some(message),
                TtsEvent::Chunk(_) => panic!("损坏的模型不可能产出真实音频"),
                TtsEvent::Finished => {}
            }
        }
        let message = failed.expect("必须如实报告加载失败");
        assert!(message.contains("加载失败"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
