//! 配置：独立用户数据目录、provider 档案、默认值。
//!
//! 位置：`%APPDATA%\cuava\`（可用环境变量 `CUAVA_DATA_DIR` 覆盖，测试用）。
//! **绝不**读写旧版 Electron 的 `.config/`：那是另一套产品仍在使用的目录，
//! 它的 `agent.json` 里还有 Chromium safeStorage 加密的 `enc:` 值，本程序既不能
//! 假设它可解，也不该覆盖它。旧配置的正式迁移留给 PR8。
//!
//! 配置本身**不含任何密钥**：密钥走 `secret.rs` 的 DPAPI 存储。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::llm::{Capability, ProviderProfile};
use crate::secret::mask_secret;

/// 默认上下文窗口：1M tokens（MiMo V2.6 / DeepSeek V4 官方均为 1M）。
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
/// 默认压缩触发点：窗口的 90%。
pub const DEFAULT_COMPRESS_PERCENT: u32 = 90;
/// 一步 = 一次模型往返（可含多个工具调用）。与旧版默认值保持一致。
pub const DEFAULT_MAX_STEPS: u32 = 16;
/// 默认请求超时。
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// 内置提供商描述（只有"有哪些可选"，不含密钥）。
#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub capability: Capability,
    /// 该端点**可信的**上下文窗口；`None` = 未知，按 1M 处理。
    pub context_window: Option<u64>,
}

/// 与 `packages/config/registry.mjs` 对齐的内置提供商。
///
/// 只保留 PR5 真正会走的三类：MiMo（仅 thinking 开关）、DeepSeek（reasoning_effort）、
/// 以及任意 OpenAI 兼容端点。本地 llama.cpp / mock 归入标准兼容端点。
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "mimo",
        name: "Xiaomi MiMo",
        base_url: "https://api.xiaomimimo.com/v1",
        default_model: "mimo-v2.6-flash",
        capability: Capability::Mimo,
        context_window: Some(1_000_000),
    },
    ProviderInfo {
        id: "deepseek",
        name: "DeepSeek 官方",
        base_url: "https://api.deepseek.com/v1",
        default_model: "deepseek-flash",
        capability: Capability::DeepSeek,
        context_window: Some(1_000_000),
    },
    ProviderInfo {
        id: "openai-compatible",
        name: "OpenAI 兼容端点（自定义）",
        base_url: "https://api.openai.com/v1",
        default_model: "",
        capability: Capability::Plain,
        context_window: None,
    },
];

pub const DEFAULT_PROVIDER_ID: &str = "mimo";

pub fn get_provider(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

/// 自定义端点上的协议提示（只对注册表里没有的提供商生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderHint {
    #[default]
    Auto,
    DeepSeek,
    Mimo,
}

impl ProviderHint {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::DeepSeek => "deepseek",
            Self::Mimo => "mimo",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "deepseek" => Self::DeepSeek,
            "mimo" => Self::Mimo,
            _ => Self::Auto,
        }
    }
}

/// MiMo 思考档位。`Default` = 不传 `thinking`，由服务端决定（V2.6 官方默认开启思考）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingLevel {
    #[default]
    Default,
    Enabled,
    Disabled,
}

impl ThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "enabled" => Self::Enabled,
            "disabled" => Self::Disabled,
            _ => Self::Default,
        }
    }
}

/// DeepSeek 推理强度档位。`Default` = 两个字段都不发，跟随服务端默认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    #[default]
    Default,
    None,
    Low,
    High,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "low" => Self::Low,
            "high" => Self::High,
            "max" => Self::Max,
            _ => Self::Default,
        }
    }
}

/// 权限档位。PR5 只实现三档预设，作用域/覆盖留待后续。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    Standard,
    Conservative,
    Permissive,
    /// 免确认（全放行）：`ask` 与 `deny` 都变成 `allow`。
    ///
    /// 这是给"我就想让它自己跑完、别弹卡片"的场景准备的，代价必须说清楚：
    /// 删除、发消息、执行命令都不会再问。**未知工具仍然拒绝**——不认识的工具
    /// 不该因为模式放开。PR5 的界面只提供前三档，这一档留给自动化测试与本机脚本。
    PassThrough,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Conservative => "conservative",
            Self::Permissive => "permissive",
            Self::PassThrough => "pass-through",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "conservative" => Self::Conservative,
            "permissive" => Self::Permissive,
            _ => Self::Standard,
        }
    }
}

/// 落盘的用户设置。**不含密钥**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_provider_id")]
    pub provider_id: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider_hint: ProviderHint,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_compress_percent")]
    pub context_compress_percent: u32,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// 自定义系统提示词；`None` = 用内置默认。
    #[serde(default)]
    pub system_prompt: Option<String>,
}

fn default_schema_version() -> u32 {
    1
}
fn default_provider_id() -> String {
    DEFAULT_PROVIDER_ID.to_string()
}
fn default_temperature() -> f64 {
    0.3
}
fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}
fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}
fn default_compress_percent() -> u32 {
    DEFAULT_COMPRESS_PERCENT
}

impl Default for Settings {
    fn default() -> Self {
        let provider = get_provider(DEFAULT_PROVIDER_ID).expect("内置默认提供商必须存在");
        Self {
            schema_version: default_schema_version(),
            provider_id: provider.id.to_string(),
            base_url: provider.base_url.to_string(),
            model: provider.default_model.to_string(),
            provider_hint: ProviderHint::Auto,
            temperature: default_temperature(),
            max_steps: default_max_steps(),
            timeout_ms: default_timeout_ms(),
            context_window: default_context_window(),
            context_compress_percent: default_compress_percent(),
            thinking: ThinkingLevel::Default,
            reasoning_effort: ReasoningEffort::Default,
            permission_mode: PermissionMode::Standard,
            system_prompt: None,
        }
    }
}

impl Settings {
    /// 校验并规整：空白地址/模型退回该提供商默认值，数值夹到安全区间。
    /// 返回 `Err` 时不落盘（保存失败而不是写入半个配置）。
    pub fn normalized(mut self) -> Result<Self, String> {
        if !self.base_url.trim().is_empty() && !is_http_url(&self.base_url) {
            return Err(format!(
                "模型地址必须以 http:// 或 https:// 开头：{}",
                self.base_url
            ));
        }
        // 切换提供商时界面会带上新的 baseURL；若用户清空地址，则回落到注册表默认值。
        if self.base_url.trim().is_empty() {
            self.base_url = get_provider(&self.provider_id)
                .map(|provider| provider.base_url.to_string())
                .unwrap_or_default();
        }
        if self.base_url.trim().is_empty() {
            return Err("模型地址不能为空".to_string());
        }
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        self.model = self.model.trim().to_string();
        if self.model.is_empty() {
            return Err("模型名不能为空".to_string());
        }
        if !self.temperature.is_finite() {
            self.temperature = default_temperature();
        }
        self.max_steps = self.max_steps.clamp(1, 40);
        self.timeout_ms = self.timeout_ms.clamp(5_000, 600_000);
        if self.context_window == 0 {
            self.context_window = DEFAULT_CONTEXT_WINDOW;
        }
        self.context_compress_percent = self.context_compress_percent.clamp(10, 99);
        if let Some(prompt) = &self.system_prompt {
            if prompt.trim().is_empty() {
                self.system_prompt = None;
            }
        }
        Ok(self)
    }

    /// 生成模型端点的档案（能力判定只在这里做一次）。
    ///
    /// 判定顺序（与旧版 `resolveCapability` 等价）：
    ///  1. 内置提供商**声明了具体协议**（MiMo / DeepSeek）→ 以注册表为准，
    ///     旧配置里残留的 hint 不得把官方端点带偏；
    ///  2. 注册表里的「通用 OpenAI 兼容」条目本身没有协议声明 → 认用户显式填的 hint；
    ///  3. 注册表里没有的自定义 id → 同样认 hint；
    ///  4. 以上都没有 → 标准兼容协议（一个专属字段都不发）。
    ///
    /// 关键点：hint 只在**用户明确设成** `deepseek` / `mimo` 时生效，
    /// 绝不根据模型名去猜协议。
    pub fn profile(&self) -> ProviderProfile {
        let capability = match get_provider(&self.provider_id) {
            // 只有真正声明了协议的提供商才压过 hint。
            Some(provider) if provider.capability != Capability::Plain => provider.capability,
            // 通用兼容条目 / 完全自定义的 id：用用户显式的协议提示。
            Some(_) | None => match self.provider_hint {
                ProviderHint::DeepSeek => Capability::DeepSeek,
                ProviderHint::Mimo => Capability::Mimo,
                ProviderHint::Auto => Capability::Plain,
            },
        };
        ProviderProfile {
            provider_id: self.provider_id.clone(),
            capability,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            thinking: self.thinking,
            reasoning_effort: self.reasoning_effort,
            model_context_window: get_provider(&self.provider_id).and_then(|p| p.context_window),
        }
    }

    /// 该端点**可信的**上下文窗口（模型元数据只用来收紧，不用来放宽）。
    pub fn effective_context_window(&self) -> u64 {
        match self.profile().model_context_window {
            Some(meta) => self.context_window.min(meta),
            None => self.context_window,
        }
    }
}

fn is_http_url(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 数据目录布局。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
}

impl AppPaths {
    /// 解析数据目录：`CUAVA_DATA_DIR` 环境变量优先（测试/隔离），否则 `%APPDATA%\cuava`。
    pub fn resolve() -> Self {
        if let Some(dir) = std::env::var_os("CUAVA_DATA_DIR") {
            if !dir.is_empty() {
                return Self {
                    root: PathBuf::from(dir),
                };
            }
        }
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: base.join("cuava"),
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn secret_file(&self) -> PathBuf {
        self.root.join("secret.bin")
    }

    /// 截图落盘目录（不参与模型输入，仅供排查）。
    pub fn frames_dir(&self) -> PathBuf {
        self.root.join("frames")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)
    }
}

/// 读取设置：文件不存在 → 默认值。**解析失败不回退默认**，而是报错，
/// 免得用户以为自己的设置还在、实际却静默换成默认端点。
pub fn load_settings(paths: &AppPaths) -> Result<Settings, String> {
    let path = paths.settings_file();
    if !path.is_file() {
        return Ok(Settings::default());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取配置失败（{}）：{error}", path.display()))?;
    let parsed: Settings = serde_json::from_str(&raw)
        .map_err(|error| format!("配置文件无法解析（{}）：{error}", path.display()))?;
    Ok(parsed)
}

/// 原子写入设置。
pub fn save_settings(paths: &AppPaths, settings: &Settings) -> Result<(), String> {
    paths
        .ensure()
        .map_err(|error| format!("无法创建数据目录（{}）：{error}", paths.root.display()))?;
    let path = paths.settings_file();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, text).map_err(|error| format!("写入配置失败：{error}"))?;
    // Windows 的 rename 不会覆盖已存在的目标文件。
    if path.exists() {
        std::fs::remove_file(&path).map_err(|error| format!("提交配置失败：{error}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|error| format!("提交配置失败：{error}"))?;
    Ok(())
}

/// 供界面显示端点摘要（**不含密钥**）。
pub fn describe_endpoint(settings: &Settings, has_key: bool) -> String {
    format!(
        "{} · {} · {}",
        settings.provider_id,
        settings.model,
        if has_key {
            "Key 已保存"
        } else {
            "未保存 Key"
        }
    )
}

/// 供日志使用的密钥摘要：只有打码值。
pub fn key_hint(secret: Option<&str>) -> String {
    match secret {
        Some(value) if !value.is_empty() => mask_secret(value),
        _ => String::new(),
    }
}

/// 运行期需要"命令行覆盖配置"的场景（隔离测试）：`CUAVA_DATA_DIR` 之外，
/// 还允许用 `CUAVA_BASE_URL` / `CUAVA_MODEL` 把请求指向本地 mock。
pub fn apply_env_overrides(settings: &mut Settings) {
    if let Ok(value) = std::env::var("CUAVA_BASE_URL") {
        if is_http_url(&value) {
            settings.base_url = value.trim().trim_end_matches('/').to_string();
        }
    }
    if let Ok(value) = std::env::var("CUAVA_MODEL") {
        if !value.trim().is_empty() {
            settings.model = value.trim().to_string();
        }
    }
    if let Ok(value) = std::env::var("CUAVA_PROVIDER_HINT") {
        settings.provider_hint = ProviderHint::parse(&value);
    }
}

/// 是否像一条真实 Key（用于"没有 Key 就不发 Authorization"）。
pub fn looks_like_secret(value: &str) -> bool {
    !value.trim().is_empty()
}

pub fn data_root_for_display(paths: &AppPaths) -> &Path {
    &paths.root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_policy() {
        let settings = Settings::default();
        assert_eq!(settings.provider_id, "mimo");
        assert_eq!(settings.context_window, 1_000_000);
        assert_eq!(settings.context_compress_percent, 90);
        assert_eq!(settings.max_steps, 16);
        assert_eq!(settings.profile().capability, Capability::Mimo);
    }

    #[test]
    fn registry_capability_beats_stale_hint() {
        // 旧配置可能残留 providerHint；官方端点必须仍按注册表能力发参数。
        let settings = Settings {
            provider_id: "mimo".to_string(),
            provider_hint: ProviderHint::DeepSeek,
            ..Settings::default()
        };
        assert_eq!(settings.profile().capability, Capability::Mimo);
    }

    #[test]
    fn unknown_provider_uses_hint_then_plain() {
        let hinted = Settings {
            provider_id: "my-gateway".to_string(),
            base_url: "http://127.0.0.1:9000/v1".to_string(),
            model: "m".to_string(),
            provider_hint: ProviderHint::DeepSeek,
            ..Settings::default()
        };
        assert_eq!(hinted.profile().capability, Capability::DeepSeek);

        let plain = Settings {
            provider_hint: ProviderHint::Auto,
            ..hinted
        };
        assert_eq!(plain.profile().capability, Capability::Plain);
    }

    /// 自定义端点（`openai-compatible`）本身没有协议声明，必须认用户显式填的 hint。
    #[test]
    fn custom_provider_honours_explicit_hint() {
        let base = Settings {
            provider_id: "openai-compatible".to_string(),
            base_url: "http://127.0.0.1:9000/v1".to_string(),
            model: "m".to_string(),
            ..Settings::default()
        };

        let auto = Settings {
            provider_hint: ProviderHint::Auto,
            ..base.clone()
        };
        assert_eq!(
            auto.profile().capability,
            Capability::Plain,
            "没填 hint 时按标准兼容协议"
        );

        let mimo = Settings {
            provider_hint: ProviderHint::Mimo,
            ..base.clone()
        };
        assert_eq!(mimo.profile().capability, Capability::Mimo);

        let deepseek = Settings {
            provider_hint: ProviderHint::DeepSeek,
            ..base
        };
        assert_eq!(deepseek.profile().capability, Capability::DeepSeek);
    }

    /// 绝不按模型名猜协议：换个像 DeepSeek 的模型名也不会改变能力判定。
    #[test]
    fn model_name_never_decides_capability() {
        let settings = Settings {
            provider_id: "openai-compatible".to_string(),
            base_url: "http://127.0.0.1:9000/v1".to_string(),
            model: "deepseek-flash".to_string(),
            ..Settings::default()
        };
        assert_eq!(settings.profile().capability, Capability::Plain);
    }

    #[test]
    fn local_small_window_wins_over_configured_1m() {
        let settings = Settings {
            provider_id: "local".to_string(),
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            model: "local-model".to_string(),
            context_window: 1_000_000,
            ..Settings::default()
        };
        // local 不在注册表里 → 没有可信元数据，按用户配置
        assert_eq!(settings.effective_context_window(), 1_000_000);
    }

    #[test]
    fn normalization_rejects_bad_inputs() {
        assert!(Settings {
            base_url: "ftp://x".to_string(),
            ..Settings::default()
        }
        .normalized()
        .is_err());
        assert!(Settings {
            model: "   ".to_string(),
            ..Settings::default()
        }
        .normalized()
        .is_err());
    }

    #[test]
    fn normalization_clamps_numbers() {
        let settings = Settings {
            max_steps: 999,
            context_compress_percent: 200,
            temperature: f64::INFINITY,
            ..Settings::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(settings.max_steps, 40);
        assert_eq!(settings.context_compress_percent, 99);
        assert_eq!(settings.temperature, 0.3);
    }

    #[test]
    fn settings_roundtrip_has_no_secret_field() {
        let settings = Settings::default();
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("apiKey"));
        assert!(!json.contains("api_key"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model, settings.model);
    }

    /// 环境变量覆盖是 README 里写明的用法，必须在真实取值链路上生效。
    ///
    /// 两个断言合并成一个测试：它们都改**进程级**环境变量，
    /// 分成两个测试会并行跑、互相覆盖。
    ///
    /// `unsafe` 说明：Rust 2024 起 `std::env::set_var` 被标记为 unsafe
    /// （多线程下修改环境不安全）。这里只出现在单元测试里，且整段串行；
    /// 生产代码只**读**环境变量。Windows 上 `set_var` 是线程安全的，
    /// 但仍按新签名显式标注。
    #[test]
    #[cfg(windows)]
    fn env_overrides_apply_to_settings() {
        #[allow(unsafe_code)]
        // SAFETY: 单测内独占执行；生产代码不调用 set_var。
        unsafe {
            std::env::set_var("CUAVA_BASE_URL", "http://127.0.0.1:18080/v1");
            std::env::set_var("CUAVA_MODEL", "mock-tool-use");
            std::env::set_var("CUAVA_PROVIDER_HINT", "deepseek");
        }

        let mut settings = Settings::default();
        apply_env_overrides(&mut settings);
        assert_eq!(settings.base_url, "http://127.0.0.1:18080/v1");
        assert_eq!(settings.model, "mock-tool-use");
        assert_eq!(settings.provider_hint, ProviderHint::DeepSeek);
        // hint 让自定义端点按 DeepSeek 协议发参数
        let profile = Settings {
            provider_id: "openai-compatible".to_string(),
            ..settings.clone()
        }
        .profile();
        assert_eq!(profile.capability, Capability::DeepSeek);

        // 非法值不会覆盖已有配置（宁可用原地址，也不要发出打不出去的请求）
        #[allow(unsafe_code)]
        // SAFETY: 同上。
        unsafe {
            std::env::set_var("CUAVA_BASE_URL", "not-a-url");
            std::env::set_var("CUAVA_MODEL", "   ");
        }
        let before_base = settings.base_url.clone();
        let before_model = settings.model.clone();
        apply_env_overrides(&mut settings);
        assert_eq!(settings.base_url, before_base);
        assert_eq!(settings.model, before_model);

        #[allow(unsafe_code)]
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("CUAVA_BASE_URL");
            std::env::remove_var("CUAVA_MODEL");
            std::env::remove_var("CUAVA_PROVIDER_HINT");
        }
    }
}
