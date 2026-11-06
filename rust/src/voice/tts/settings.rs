//! 语音合成（TTS）设置。
//!
//! 兼容性要求（任务书）：这是对 PR6 `settings.json` 的**增量扩展**，
//! 全部字段带 `#[serde(default)]`——PR5/PR6 写下的配置文件在新版里照常可读，
//! 新安装默认 **不开启** 云端播报（`enabled = false`），不会悄悄把回复发到服务端。
//!
//! "用哪个后端""是否自动播报哪类输入"都显式写在配置里，不靠猜：
//!  - `backend`: `off` / `mimo` / `local`
//!  - `speakVoiceTurns` / `speakTextTurns`: 语音提问与手动输入是否自动播报

use serde::{Deserialize, Serialize};

/// 正式支持的两种后端（不引入 GPT-SoVITS / Python worker / 插件体系）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TtsBackend {
    /// 关闭（默认）：不合成、不联网、不占用音频设备。
    #[default]
    Off,
    /// MiMo-V2.5-TTS 标准预置音色（云端，会把**回复文本**发到 MiMo）。
    Mimo,
    /// 本地 ONNX（sherpa-onnx OfflineTts / Kokoro 中英多语言），不联网。
    Local,
}

impl TtsBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Mimo => "mimo",
            Self::Local => "local",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "mimo" => Self::Mimo,
            "local" => Self::Local,
            _ => Self::Off,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Mimo => "MiMo 云端",
            Self::Local => "本地 ONNX",
        }
    }
}

fn default_mimo_model() -> String {
    super::mimo::DEFAULT_MODEL.to_string()
}
fn default_speed() -> f32 {
    1.0
}
/// 本地推理默认走 CUDA（GPU 版 sherpa-onnx 运行时可用时）；拿不到就回退 CPU。
fn default_local_provider() -> String {
    "cuda".to_string()
}
fn default_timeout_ms() -> u64 {
    30_000
}
fn default_true() -> bool {
    true
}

/// 落盘的 TTS 设置。**全部带 `#[serde(default)]`**，缺字段等于默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsSettings {
    /// 总开关。默认 false：新安装不会自动联网播报。
    #[serde(default)]
    pub enabled: bool,
    /// 后端。`enabled = true` 且 `backend = off` 时视为"没有可用后端"。
    #[serde(default)]
    pub backend: TtsBackend,
    /// 语音提问的回复是否自动播报。
    #[serde(default = "default_true")]
    pub speak_voice_turns: bool,
    /// 手动输入的回复是否自动播报。
    #[serde(default = "default_true")]
    pub speak_text_turns: bool,
    /// 输出设备名；空 = 系统默认输出设备。
    #[serde(default)]
    pub output_device: String,
    /// MiMo TTS 模型（标准预置音色模型）。
    #[serde(default = "default_mimo_model")]
    pub mimo_model: String,
    /// 预置音色名；空 = 不发送 `audio.voice`，由服务端默认音色决定。
    #[serde(default)]
    pub mimo_voice: String,
    /// 简短风格指令（放进 `user` 消息）；空 = 不发送。
    #[serde(default)]
    pub mimo_style: String,
    #[serde(default)]
    pub mimo_tags: Vec<String>,
    #[serde(default = "default_true")]
    pub mimo_streaming: bool,
    #[serde(default = "default_true")]
    pub mimo_reuse_key: bool,
    #[serde(default = "default_speed")]
    pub mimo_speed: f32,
    #[serde(default = "default_speed")]
    pub volume: f32,
    /// 本地模型目录（含 model.onnx / voices.bin / tokens.txt 等）。
    #[serde(default)]
    pub local_model_dir: String,
    /// 本地模型音色编号（Kokoro 的 sid）。
    #[serde(default)]
    pub local_sid: i32,
    /// 本地合成语速。
    #[serde(default = "default_speed")]
    pub local_speed: f32,
    /// 本地推理执行提供者：`cuda`（默认，需要 GPU 版 sherpa-onnx 运行时）
    /// 或 `cpu`。选 `cuda` 但运行时拿不到 CUDA 时会**自动回退 CPU**，
    /// 并把实际使用的提供者写进引擎标签。
    #[serde(default = "default_local_provider")]
    pub local_provider: String,
    /// 单句合成超时（毫秒）。
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for TtsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: TtsBackend::Off,
            speak_voice_turns: default_true(),
            speak_text_turns: default_true(),
            output_device: String::new(),
            mimo_model: default_mimo_model(),
            mimo_voice: String::new(),
            mimo_style: String::new(),
            mimo_tags: Vec::new(),
            mimo_streaming: true,
            mimo_reuse_key: true,
            mimo_speed: 1.0,
            volume: 1.0,
            local_model_dir: String::new(),
            local_sid: 0,
            local_speed: default_speed(),
            local_provider: default_local_provider(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

impl TtsSettings {
    /// 规整到安全范围。只夹**无效值**，不猜用户意图；返回 `Err` 时不落盘。
    pub fn normalized(mut self) -> Result<Self, String> {
        self.output_device = self.output_device.trim().to_string();
        self.mimo_model = self.mimo_model.trim().to_string();
        self.mimo_voice = self.mimo_voice.trim().to_string();
        if self.mimo_model.encode_utf16().count() > 200 {
            return Err("MiMo 模型名最多200字符".into());
        }
        super::styles::validate(&self.mimo_tags)?;
        if !self.mimo_speed.is_finite() || !(0.5..=2.0).contains(&self.mimo_speed) {
            return Err("MiMo 语速必须为 0.5–2.0".into());
        }
        if !self.volume.is_finite() || !(0.0..=1.0).contains(&self.volume) {
            return Err("音量必须为 0–1".into());
        }
        self.local_model_dir = self.local_model_dir.trim().to_string();
        // 提供者只认 cuda/cpu；写错就按默认（cuda）处理，不因为一个拼写
        // 错误让整个设置无法保存。
        self.local_provider = match self.local_provider.trim().to_ascii_lowercase().as_str() {
            "cpu" => "cpu".to_string(),
            _ => "cuda".to_string(),
        };
        if self.mimo_model.is_empty() {
            self.mimo_model = default_mimo_model();
        }
        if !self.local_speed.is_finite() || !(0.5..=2.0).contains(&self.local_speed) {
            return Err(format!(
                "本地语速必须在 0.5~2.0 之间，收到 {}",
                self.local_speed
            ));
        }
        if self.local_sid < 0 {
            return Err(format!("音色编号不能为负：{}", self.local_sid));
        }
        if self.local_sid > 500 {
            return Err(format!("音色编号超出合理范围：{}", self.local_sid));
        }
        self.timeout_ms = self.timeout_ms.clamp(5_000, 120_000);
        if self.mimo_style.encode_utf16().count() > 1000 {
            return Err("自然语言说明最多 1000 字符".into());
        }
        Ok(self)
    }

    /// 是否真的会播报（总开关 + 有效后端）。
    pub fn active_backend(&self) -> TtsBackend {
        if !self.enabled {
            return TtsBackend::Off;
        }
        self.backend
    }

    /// 该来源的回复是否应当自动播报。
    pub fn speaks(&self, from_voice: bool) -> bool {
        // 后端为 off 时,"启用"只是空开关：不合成、不联网、不占设备。
        if !self.enabled || self.backend == TtsBackend::Off {
            return false;
        }
        if from_voice {
            self.speak_voice_turns
        } else {
            self.speak_text_turns
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **兼容性核心**：PR6 的 settings.json 没有 tts 字段，必须按默认值读出来，
    /// 而且默认**不联网播报**。
    #[test]
    fn missing_tts_fields_default_to_off() {
        let legacy = r#"{
            "schemaVersion": 1,
            "providerId": "mimo",
            "baseUrl": "https://api.xiaomimimo.com/v1",
            "model": "mimo-v2.6-flash",
            "voice": { "enabled": true }
        }"#;
        let parsed: crate::config::Settings = serde_json::from_str(legacy).unwrap();
        assert!(!parsed.tts.enabled, "新装/旧配置默认不播报");
        assert_eq!(parsed.tts.backend, TtsBackend::Off);
        assert_eq!(parsed.tts.mimo_model, "mimo-v2.5-tts");
        assert_eq!(parsed.tts.local_speed, 1.0);
        // 即便总开关被打开，两类输入仍然按各自的开关播报。
        let mut settings = parsed.tts.clone();
        settings.enabled = true;
        settings.backend = TtsBackend::Mimo;
        assert!(settings.speaks(true));
        assert!(settings.speaks(false));
        assert_eq!(settings.active_backend(), TtsBackend::Mimo);
    }

    #[test]
    fn backend_off_wins_over_enabled_flag() {
        let settings = TtsSettings {
            enabled: true,
            backend: TtsBackend::Off,
            ..TtsSettings::default()
        };
        assert_eq!(settings.active_backend(), TtsBackend::Off);
        assert!(!settings.speaks(true));
    }

    #[test]
    fn normalization_rejects_impossible_values() {
        let bad_speed = TtsSettings {
            local_speed: 3.0,
            ..TtsSettings::default()
        };
        assert!(bad_speed.normalized().is_err());
        let bad_sid = TtsSettings {
            local_sid: -1,
            ..TtsSettings::default()
        };
        assert!(bad_sid.normalized().is_err());
        let ok = TtsSettings {
            local_speed: 1.5,
            timeout_ms: 1,
            mimo_style: "x".repeat(200),
            ..TtsSettings::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(ok.local_speed, 1.5);
        assert_eq!(ok.timeout_ms, 5_000);
        assert_eq!(ok.mimo_style.chars().count(), 200);
    }

    #[test]
    fn pr14_playback_ranges_roundtrip_and_reject_invalid_values() {
        for speed in [0.5, 1.0, 2.0] {
            for volume in [0.0, 0.5, 1.0] {
                let settings = TtsSettings {
                    mimo_speed: speed,
                    volume,
                    ..Default::default()
                }
                .normalized()
                .unwrap();
                let loaded: TtsSettings =
                    serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
                assert_eq!(loaded.mimo_speed, speed);
                assert_eq!(loaded.volume, volume);
            }
        }
        for speed in [0.49, 2.01, f32::NAN, f32::INFINITY] {
            assert!(TtsSettings {
                mimo_speed: speed,
                ..Default::default()
            }
            .normalized()
            .is_err());
        }
        for volume in [-0.01, 1.01, f32::NAN, f32::INFINITY] {
            assert!(TtsSettings {
                volume,
                ..Default::default()
            }
            .normalized()
            .is_err());
        }
    }

    #[test]
    fn roundtrip_keeps_selected_backend() {
        let settings = TtsSettings {
            enabled: true,
            backend: TtsBackend::Local,
            local_model_dir: "D:/models/kokoro".to_string(),
            local_sid: 3,
            ..TtsSettings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: TtsSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend, TtsBackend::Local);
        assert_eq!(back.local_model_dir, "D:/models/kokoro");
        assert_eq!(back.local_sid, 3);
        // 云端音色与本地音色互不串用：本地模式不会带出 MiMo 的音色字段。
        assert!(back.mimo_voice.is_empty());
    }
}
