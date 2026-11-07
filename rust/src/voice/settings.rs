//! 语音设置：采集、KWS、VAD、Whisper。
//!
//! 兼容性要求（任务书）：这是对 PR5 `settings.json` 的**增量扩展**，
//! 缺省字段必须能用默认值补齐——PR5 写下的配置在新版里照常可读，
//! 新版写下的配置在 PR5 里最多是「多出几个不认识的字段」，不会破坏它。
//!
//! 路径解析刻意**不**读取旧版 Electron 数据目录（`.config/`）：
//! - 模型文件：`CUAVA_VOICE_MODEL_DIR` → 数据目录 `models/` → 仓库 `../models/`
//!   （只读探测，找到就用）；
//! - 关键词文件：显式配置 → 数据目录 `kws-keywords.txt`（没有则写入默认「小爱同学」）；
//! - 音频**不落盘**：设置里没有任何录音路径字段。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::AppPaths;

/// 默认唤醒词与 sherpa 官方 keywords.txt 里的写法完全一致。
pub const DEFAULT_WAKE_WORD: &str = "小爱同学";
pub const DEFAULT_KEYWORD_LINE: &str = "x iǎo ài t óng x ué @小爱同学";

/// 默认 KWS 模型目录名（与旧版使用同一个 Zipformer 模型，文件可按名兼容）。
pub const DEFAULT_KWS_MODEL_DIR: &str = "sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01";

/// 默认 Whisper 模型文件名。
pub const DEFAULT_WHISPER_MODEL: &str = "ggml-small.bin";

/// 识别语言：自动 / 明确指定。默认中文——这个产品的使用场景是中文短指令，
/// 而 Whisper 对中文的「自动检测」在 1~3 秒短句上并不可靠（容易判成粤语/日语）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AsrLanguage {
    /// 由 whisper 自动检测（短句容易误判，不推荐）。
    Auto,
    #[default]
    Chinese,
    English,
}

impl AsrLanguage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Chinese => "zh",
            Self::English => "en",
        }
    }

    /// 给 whisper 的 `set_language`：`None` 表示自动检测。
    pub fn whisper_language(&self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Chinese => Some("zh"),
            Self::English => Some("en"),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Chinese => "中文",
            Self::English => "英文",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "en" | "english" => Self::English,
            _ => Self::Chinese,
        }
    }
}

/// 落盘的语音设置。**全部带 `#[serde(default)]`**，缺字段就等于默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSettings {
    /// 用户是否启用过监听。**默认 false**：新安装启动时不开麦。
    ///
    /// 这里保存的是「用户上次明确保存的偏好」，不是「现在一定在跑」——
    /// 程序启动时是否真的开始采集，由界面上的开关决定（见 worker）。
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_use_gpu")]
    pub continuous_conversation: bool,
    #[serde(default = "default_followup_seconds")]
    pub followup_seconds: u32,
    /// 麦克风设备名；空 = 系统默认输入设备。
    #[serde(default)]
    pub input_device: String,
    /// KWS 模型目录（空 = 按路径解析规则自动找）。
    #[serde(default)]
    pub kws_model_dir: String,
    /// 关键词文件（空 = 按路径解析规则自动找/生成）。
    #[serde(default)]
    pub keywords_file: String,
    /// Whisper 模型文件（空 = 按路径解析规则自动找）。
    #[serde(default)]
    pub whisper_model: String,
    #[serde(default)]
    pub language: AsrLanguage,
    /// 是否让 whisper 用 GPU。默认 true：CUDA 构建 + 可用 GPU 时延迟从秒级降到百毫秒级。
    ///
    /// 这只是**用户的期望**：实际后端由 `voice::asr` 决定并如实上报——
    /// CPU-only 构建、没有可用设备、CUDA 初始化失败都会退回 CPU 并说明原因。
    #[serde(default = "default_use_gpu")]
    pub use_gpu: bool,
    /// 推理线程数。0 = 自动（min(硬件线程, 8)）。
    #[serde(default)]
    pub threads: u32,
    /// VAD 起判阈值（高于噪声底多少 dB）。
    #[serde(default = "default_vad_on_db")]
    pub vad_on_db: f32,
    /// VAD 收尾阈值（迟滞，必须小于 `vad_on_db`）。
    #[serde(default = "default_vad_off_db")]
    pub vad_off_db: f32,
    /// 最短语音（毫秒）：低于它视为短噪声，不提交。
    #[serde(default = "default_min_speech_ms")]
    pub min_speech_ms: u32,
    /// 静音多久算说完（毫秒）。
    #[serde(default = "default_end_silence_ms")]
    pub end_silence_ms: u32,
    /// 一句最长多少毫秒：到点强制结束，防止无限积累。
    #[serde(default = "default_max_utterance_ms")]
    pub max_utterance_ms: u32,
    /// 唤醒后多久没人说话就回到监听（毫秒）。
    #[serde(default = "default_no_speech_timeout_ms")]
    pub no_speech_timeout_ms: u32,
}

fn default_followup_seconds() -> u32 {
    8
}

fn default_use_gpu() -> bool {
    true
}
fn default_vad_on_db() -> f32 {
    12.0
}
fn default_vad_off_db() -> f32 {
    7.0
}
fn default_min_speech_ms() -> u32 {
    250
}
fn default_end_silence_ms() -> u32 {
    700
}
fn default_max_utterance_ms() -> u32 {
    15_000
}
fn default_no_speech_timeout_ms() -> u32 {
    8_000
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            continuous_conversation: true,
            followup_seconds: 8,
            input_device: String::new(),
            kws_model_dir: String::new(),
            keywords_file: String::new(),
            whisper_model: String::new(),
            language: AsrLanguage::default(),
            use_gpu: default_use_gpu(),
            threads: 0,
            vad_on_db: default_vad_on_db(),
            vad_off_db: default_vad_off_db(),
            min_speech_ms: default_min_speech_ms(),
            end_silence_ms: default_end_silence_ms(),
            max_utterance_ms: default_max_utterance_ms(),
            no_speech_timeout_ms: default_no_speech_timeout_ms(),
        }
    }
}

impl VoiceSettings {
    /// 规整到安全范围。返回 `Err` 表示不落盘。
    ///
    /// 不做「猜用户想说什么」的修正：只把**无效值**夹到能工作的范围，
    /// 并在 `warnings()` 里如实说明改了哪里。
    pub fn normalized(mut self) -> Result<Self, String> {
        self.followup_seconds = self.followup_seconds.clamp(1, 30);
        self.input_device = self.input_device.trim().to_string();
        self.kws_model_dir = self.kws_model_dir.trim().to_string();
        self.keywords_file = self.keywords_file.trim().to_string();
        self.whisper_model = self.whisper_model.trim().to_string();

        if !self.vad_on_db.is_finite() || !(3.0..=40.0).contains(&self.vad_on_db) {
            return Err(format!(
                "VAD 起始阈值必须在 3~40 dB，收到 {}",
                self.vad_on_db
            ));
        }
        if !self.vad_off_db.is_finite() || self.vad_off_db >= self.vad_on_db {
            return Err("VAD 收尾阈值必须小于起始阈值（否则无法形成迟滞）".to_string());
        }
        self.vad_off_db = self.vad_off_db.max(0.5);
        // 时长上下限：太短会把正常一句话切碎，太长会让内存与等待失控。
        self.min_speech_ms = self.min_speech_ms.clamp(100, 10_000);
        self.end_silence_ms = self.end_silence_ms.clamp(200, 5_000);
        self.max_utterance_ms = self.max_utterance_ms.clamp(1_000, 60_000);
        self.no_speech_timeout_ms = self.no_speech_timeout_ms.clamp(1_000, 60_000);
        if self.max_utterance_ms <= self.end_silence_ms {
            return Err("最长语句时长必须大于静音收尾时长".to_string());
        }
        self.threads = self.threads.min(64);
        Ok(self)
    }

    /// 规整时被改写的项（给界面提示「你的值不合法，已按 X 处理」）。
    pub fn warnings(&self, original: &VoiceSettings) -> Vec<String> {
        let mut out = Vec::new();
        if original.threads != self.threads {
            out.push(format!("线程数已限制为 {}", self.threads));
        }
        if original.min_speech_ms != self.min_speech_ms {
            out.push(format!("最短语音已限制为 {} ms", self.min_speech_ms));
        }
        if original.end_silence_ms != self.end_silence_ms {
            out.push(format!("静音收尾已限制为 {} ms", self.end_silence_ms));
        }
        if original.max_utterance_ms != self.max_utterance_ms {
            out.push(format!("最长语句已限制为 {} ms", self.max_utterance_ms));
        }
        if original.no_speech_timeout_ms != self.no_speech_timeout_ms {
            out.push(format!("无声超时已限制为 {} ms", self.no_speech_timeout_ms));
        }
        out
    }

    /// 实际使用的推理线程数。
    pub fn resolved_threads(&self) -> usize {
        if self.threads > 0 {
            return self.threads as usize;
        }
        let fallback = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        fallback.clamp(1, 8)
    }

    /// VAD 参数（毫秒直接换成 `Duration`）。
    pub fn endpoints(&self) -> crate::voice::vad::EndpointConfig {
        crate::voice::vad::EndpointConfig {
            on_db: self.vad_on_db,
            off_db: self.vad_off_db,
            min_speech: std::time::Duration::from_millis(self.min_speech_ms as u64),
            end_silence: std::time::Duration::from_millis(self.end_silence_ms as u64),
            max_utterance: std::time::Duration::from_millis(self.max_utterance_ms as u64),
            no_speech_timeout: std::time::Duration::from_millis(self.no_speech_timeout_ms as u64),
            ..Default::default()
        }
    }
}

/// 语音相关文件的解析结果（找不到时给出**可读的**原因，而不是静默失败）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Found(PathBuf),
    Missing(String),
}

impl Resolved {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Found(path) => Some(path),
            Self::Missing(_) => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Found(_) => None,
            Self::Missing(message) => Some(message),
        }
    }
}

/// 语音文件的路径解析器。
///
/// 查找顺序刻意简单且可预测：**显式配置 → 数据目录 → 仓库 `models/`**。
/// 仓库目录只是「开发机上能找到就用」，不会写入，也不读旧版 Electron 目录。
pub struct VoicePaths {
    root: PathBuf,
    /// 仓库根下的 `models/`（开发期）；`None` 表示找不到。
    repo_models: Option<PathBuf>,
}

impl VoicePaths {
    pub fn resolve(paths: &AppPaths) -> Self {
        Self {
            root: paths.root.clone(),
            repo_models: find_repo_models(),
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            repo_models: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 数据目录下的关键词文件（用户可写的那份）。
    pub fn keywords_file(&self) -> PathBuf {
        self.root.join("kws-keywords.txt")
    }

    /// 数据目录下的 KWS 模型目录约定。
    pub fn kws_model_home(&self) -> PathBuf {
        self.root.join("models").join(DEFAULT_KWS_MODEL_DIR)
    }

    /// 数据目录下的 Whisper 模型约定位置。
    pub fn whisper_model_home(&self) -> PathBuf {
        self.root.join("models").join(DEFAULT_WHISPER_MODEL)
    }

    fn candidates_dir(&self, name: &str) -> Vec<PathBuf> {
        let mut out = vec![self.root.join("models").join(name)];
        if let Some(models) = &self.repo_models {
            out.push(models.join(name));
        }
        out
    }

    /// 解析 KWS 模型目录，并校验必需文件都在。
    pub fn resolve_kws_model(&self, configured: &str) -> Resolved {
        let configured = configured.trim();
        if !configured.is_empty() {
            let dir = PathBuf::from(configured);
            return validate_kws_dir(&dir).map_or_else(
                |reason| Resolved::Missing(format!("KWS 模型目录不可用：{reason}")),
                |_| Resolved::Found(dir),
            );
        }
        let mut tried = Vec::new();
        for dir in self.candidates_dir(DEFAULT_KWS_MODEL_DIR) {
            tried.push(dir.display().to_string());
            if validate_kws_dir(&dir).is_ok() {
                return Resolved::Found(dir);
            }
        }
        Resolved::Missing(format!(
            "找不到 KWS 模型目录（需要 encoder/decoder/joiner/tokens）。\n\
             已尝试：\n{}\n\
             可以把它放到 {}，或在设置里指定目录。",
            tried.join("\n"),
            self.kws_model_home().display()
        ))
    }

    /// 解析 Whisper 模型文件。
    pub fn resolve_whisper_model(&self, configured: &str) -> Resolved {
        let configured = configured.trim();
        if !configured.is_empty() {
            let path = PathBuf::from(configured);
            return validate_whisper_model(&path).map_or_else(
                |reason| Resolved::Missing(format!("Whisper 模型不可用：{reason}")),
                |_| Resolved::Found(path),
            );
        }
        let mut tried = Vec::new();
        for directory in self.candidates_dir("") {
            let path = directory.join(DEFAULT_WHISPER_MODEL);
            tried.push(path.display().to_string());
            if validate_whisper_model(&path).is_ok() {
                return Resolved::Found(path);
            }
        }
        Resolved::Missing(format!(
            "找不到 Whisper 模型 {}。\n\
             已尝试：\n{}\n\
             把 ggml 模型放到 {}，或在设置里指定文件。",
            DEFAULT_WHISPER_MODEL,
            tried.join("\n"),
            self.whisper_model_home().display()
        ))
    }

    /// 解析关键词文件；不存在时**生成默认「小爱同学」**。
    ///
    /// 只在用户没有显式指定路径时生成（显式路径写错要报错，不能悄悄换一个文件）。
    pub fn resolve_keywords(&self, configured: &str) -> Result<Resolved, String> {
        let configured = configured.trim();
        if !configured.is_empty() {
            let path = PathBuf::from(configured);
            return match validate_keywords_file(&path) {
                Ok(()) => Ok(Resolved::Found(path)),
                Err(reason) => Ok(Resolved::Missing(format!("关键词文件不可用：{reason}"))),
            };
        }

        let data_file = self.keywords_file();
        if data_file.is_file() {
            return match validate_keywords_file(&data_file) {
                Ok(()) => Ok(Resolved::Found(data_file)),
                Err(reason) => Err(format!(
                    "关键词文件读不出来，为避免覆盖你的配置已中止：{}（{reason}）",
                    data_file.display()
                )),
            };
        }

        // 数据目录里没有：先看仓库是否已有（开发机），再生成默认。
        if let Some(repo) = &self.repo_models {
            let candidate = repo.join("kws-keywords.txt");
            if candidate.is_file() && validate_keywords_file(&candidate).is_ok() {
                return Ok(Resolved::Found(candidate));
            }
        }

        std::fs::create_dir_all(&self.root)
            .map_err(|error| format!("无法创建数据目录 {}：{error}", self.root.display()))?;
        std::fs::write(&data_file, format!("{DEFAULT_KEYWORD_LINE}\n"))
            .map_err(|error| format!("写入默认唤醒词失败 {}：{error}", data_file.display()))?;
        Ok(Resolved::Found(data_file))
    }
}

/// KWS 模型目录必须含有的文件。文件名与旧版使用的模型一致（int8 优先）。
fn validate_kws_dir(dir: &Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("目录不存在：{}", dir.display()));
    }
    let required = [
        "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        "tokens.txt",
    ];
    for name in required {
        if !dir.join(name).is_file() {
            return Err(format!("缺少 {name}"));
        }
    }
    Ok(())
}

/// Whisper 模型：存在、非空、开头是 GGML magic。
///
/// 只读前 4 字节做快速判断，不把几百 MB 读进来；
/// 真正的兼容性仍由 whisper.cpp 加载时决定（这里只挡「明显错了」的情况）。
fn validate_whisper_model(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("文件不存在：{}", path.display()));
    }
    let meta = std::fs::metadata(path)
        .map_err(|error| format!("读不到文件信息 {}：{error}", path.display()))?;
    if meta.len() < 1024 * 1024 {
        return Err(format!(
            "文件只有 {} 字节，不像 whisper ggml 模型：{}",
            meta.len(),
            path.display()
        ));
    }
    use std::io::Read;
    let mut head = [0u8; 4];
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("打不开模型 {}：{error}", path.display()))?;
    file.read_exact(&mut head)
        .map_err(|error| format!("读模型头失败 {}：{error}", path.display()))?;
    // ggml 的 magic 是小端 `ggml`（字节序为 'l','m','g','g'）。
    if &head != b"lmgg" {
        return Err(format!(
            "不是 whisper.cpp 的 ggml 模型（文件头 {:?}）：{}",
            String::from_utf8_lossy(&head),
            path.display()
        ));
    }
    Ok(())
}

/// 关键词文件校验：至少一行，每行有 `@标签` 且音素非空。
fn validate_keywords_file(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读取失败 {}：{error}", path.display()))?;
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return Err("文件里没有任何关键词".to_string());
    }
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with('#') {
            return Err(format!(
                "第 {} 行是注释；sherpa 关键词文件不支持注释",
                index + 1
            ));
        }
        let Some(at) = line.rfind('@') else {
            return Err(format!("第 {} 行缺少 `@标签`", index + 1));
        };
        if line[..at].trim().is_empty() {
            return Err(format!("第 {} 行缺少音素", index + 1));
        }
        if line[at + 1..].trim().is_empty() {
            return Err(format!("第 {} 行 `@` 后缺少展示名", index + 1));
        }
    }
    Ok(())
}

/// 关键词文件里的展示名（KWS 命中时返回的就是它）。
pub fn keyword_labels(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(at) = line.rfind('@') {
            let label = line[at + 1..].trim().to_string();
            if !label.is_empty() && !out.contains(&label) {
                out.push(label);
            }
        }
    }
    out
}

/// 开发机上找仓库根：当前目录或 exe 目录逐级向上，直到看到 `models/` 与 `packages/`。
///
/// 供语音输入（KWS/Whisper）与本地 TTS 共用：两边的模型都遵循
/// 「显式配置 → 数据目录 → 仓库 `models/`」这一条查找顺序，不各写一份。
pub(crate) fn find_repo_models() -> Option<PathBuf> {
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            starts.push(dir.to_path_buf());
        }
    }
    for start in starts {
        let mut dir = start;
        for _ in 0..8 {
            let candidate = dir.join("models");
            if candidate.is_dir() && dir.join("packages").is_dir() {
                return Some(candidate);
            }
            if !dir.pop() {
                break;
            }
        }
    }
    None
}

/// 测试辅助：临时目录（只在 `cfg(test)` 下存在，供其它模块的测试复用）。
#[cfg(test)]
pub mod tests_support {
    use std::path::PathBuf;

    pub fn temp_root(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("voice-{prefix}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("voice-settings-{name}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// **兼容性核心**：PR5 的 settings.json 里没有 voice 字段，必须能用默认值读出来。
    #[test]
    fn missing_voice_fields_default_cleanly() {
        let legacy = r#"{
            "schemaVersion": 1,
            "providerId": "mimo",
            "baseUrl": "https://api.xiaomimimo.com/v1",
            "model": "mimo-v2.6-flash"
        }"#;
        let parsed: crate::config::Settings = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.provider_id, "mimo");
        assert!(!parsed.voice.enabled, "新装/旧配置默认不开麦");
        assert_eq!(parsed.voice.language, AsrLanguage::Chinese);
        assert!(parsed.voice.use_gpu);
        assert_eq!(parsed.voice.min_speech_ms, 250);
        assert_eq!(parsed.voice.end_silence_ms, 700);
    }

    #[test]
    fn defaults_are_off_by_default() {
        let settings = VoiceSettings::default();
        assert!(!settings.enabled, "新安装启动时不意外开麦");
    }

    #[test]
    fn normalization_rejects_inverted_hysteresis() {
        let bad = VoiceSettings {
            vad_on_db: 7.0,
            vad_off_db: 12.0,
            ..VoiceSettings::default()
        };
        assert!(bad.normalized().is_err(), "收尾阈值高于起始阈值必须拒绝");
    }

    #[test]
    fn normalization_clamps_durations() {
        let settings = VoiceSettings {
            min_speech_ms: 1,
            end_silence_ms: 10_000,
            max_utterance_ms: 100,
            no_speech_timeout_ms: 0,
            threads: 999,
            ..VoiceSettings::default()
        }
        .normalized();
        // max_utterance 必须大于 end_silence，否则报错而不是偷偷改
        assert!(settings.is_err());

        let ok = VoiceSettings {
            min_speech_ms: 1,
            end_silence_ms: 10_000,
            max_utterance_ms: 60_000,
            no_speech_timeout_ms: 0,
            threads: 999,
            ..VoiceSettings::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(ok.min_speech_ms, 100);
        assert_eq!(ok.end_silence_ms, 5_000);
        assert!(ok.max_utterance_ms > ok.end_silence_ms);
        assert_eq!(ok.threads, 64);
    }

    #[test]
    fn validation_errors_are_rejected_not_clamped() {
        let bad = VoiceSettings {
            vad_on_db: f32::NAN,
            ..VoiceSettings::default()
        };
        assert!(bad.normalized().is_err());
    }

    #[test]
    fn language_mapping() {
        assert_eq!(AsrLanguage::Chinese.whisper_language(), Some("zh"));
        assert_eq!(AsrLanguage::Auto.whisper_language(), None);
        assert_eq!(AsrLanguage::parse("en"), AsrLanguage::English);
        assert_eq!(AsrLanguage::parse("随便"), AsrLanguage::Chinese);
    }

    #[test]
    fn default_keywords_are_generated_once() {
        let root = temp_root("keywords");
        let paths = VoicePaths::with_root(&root);
        let resolved = paths.resolve_keywords("").unwrap();
        let Resolved::Found(file) = resolved else {
            panic!("应当生成默认关键词文件");
        };
        assert_eq!(file, paths.keywords_file());
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.trim(), DEFAULT_KEYWORD_LINE);
        assert_eq!(keyword_labels(&file), vec![DEFAULT_WAKE_WORD.to_string()]);

        // 第二次不再覆盖（用户可能已经改过）。
        std::fs::write(&file, "x iǎo ài t óng x ué @自定义\n").unwrap();
        let again = paths.resolve_keywords("").unwrap();
        assert_eq!(again.path(), Some(file.as_path()));
        assert_eq!(keyword_labels(&file), vec!["自定义".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_keyword_file_is_an_error_not_overwritten() {
        let root = temp_root("keywords-bad");
        let paths = VoicePaths::with_root(&root);
        std::fs::write(paths.keywords_file(), b"\xff\xfe\x00invalid").unwrap();
        let result = paths.resolve_keywords("");
        // 能读成字符串但内容非法 → 报错；不能读成字符串 → 也报错。
        assert!(result.is_err() || matches!(result.unwrap(), Resolved::Missing(_)));
        // 关键：原文件不能被覆盖。
        assert!(paths.keywords_file().exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_keyword_path_is_not_replaced() {
        let root = temp_root("keywords-explicit");
        let paths = VoicePaths::with_root(&root);
        let resolved = paths
            .resolve_keywords("/definitely/not/here/keywords.txt")
            .unwrap();
        assert!(matches!(resolved, Resolved::Missing(_)));
        assert!(
            !paths.keywords_file().exists(),
            "显式路径错误时不能悄悄生成另一个文件顶替"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keyword_validation_catches_bad_lines() {
        let root = temp_root("keyword-validate");
        let file = root.join("kw.txt");
        for (content, ok) in [
            ("x iǎo ài t óng x ué @小爱同学\n", true),
            ("", false),
            ("# 注释\n", false),
            ("x iǎo ài t óng x ué\n", false),
            ("@只有标签\n", false),
            ("x iǎo ài t óng x ué @\n", false),
        ] {
            std::fs::write(&file, content).unwrap();
            assert_eq!(
                validate_keywords_file(&file).is_ok(),
                ok,
                "内容 {content:?} 的判定不符合预期"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn whisper_model_must_have_ggml_magic() {
        let root = temp_root("whisper-validate");
        let fake = root.join("ggml-fake.bin");
        // 够大但不是 ggml 头
        std::fs::write(&fake, vec![0u8; 2 * 1024 * 1024]).unwrap();
        assert!(validate_whisper_model(&fake).is_err());

        let good = root.join("ggml-good.bin");
        let mut bytes = b"lmgg".to_vec();
        bytes.resize(2 * 1024 * 1024, 0);
        std::fs::write(&good, bytes).unwrap();
        assert!(validate_whisper_model(&good).is_ok());

        // 太小
        let small = root.join("small.bin");
        std::fs::write(&small, b"lmgg").unwrap();
        assert!(validate_whisper_model(&small).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kws_dir_requires_all_model_files() {
        let root = temp_root("kws-validate");
        assert!(validate_kws_dir(&root).is_err());
        for name in [
            "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
            "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
            "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
            "tokens.txt",
        ] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        assert!(validate_kws_dir(&root).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_model_messages_are_actionable() {
        let root = temp_root("missing");
        let paths = VoicePaths::with_root(&root);
        let kws = paths.resolve_kws_model("");
        let message = kws.error().unwrap();
        assert!(message.contains("KWS"), "{message}");
        assert!(message.contains("设置"), "必须告诉用户怎么办：{message}");

        let whisper = paths.resolve_whisper_model("");
        let message = whisper.error().unwrap();
        assert!(message.contains("ggml-small.bin"), "{message}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn voice_settings_roundtrip() {
        let settings = VoiceSettings {
            enabled: true,
            input_device: "Microphone".to_string(),
            language: AsrLanguage::English,
            ..VoiceSettings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: VoiceSettings = serde_json::from_str(&json).unwrap();
        assert!(back.enabled);
        assert_eq!(back.input_device, "Microphone");
        assert_eq!(back.language, AsrLanguage::English);
    }
}
