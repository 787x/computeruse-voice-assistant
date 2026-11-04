//! 配置：独立用户数据目录、多提供商档案、模型方案与时段切换、默认值。
//!
//! 位置：`%APPDATA%\cuava\`（可用环境变量 `CUAVA_DATA_DIR` 覆盖，测试用）。
//! **绝不**读写旧版 Electron 的 `.config/`：那是另一套产品仍在使用的目录，
//! 它的 `agent.json` 里还有 Chromium safeStorage 加密的 `enc:` 值，本程序既不能
//! 假设它可解，也不该覆盖它。旧配置的正式迁移留给后续发布任务。
//!
//! 配置本身**不含任何密钥**：密钥走 `secret.rs` 的 DPAPI 存储（按提供商隔离）。
//!
//! PR8 的几处结构变化（全部**向后兼容**，带 `serde(default)`）：
//!  - `providers`：每个提供商自己的端点 / 首选模型 / 推理档位 / 已知模型列表；
//!    旧配置（只有顶层 providerId/baseUrl/model）会在读取时按顶层值补一条，
//!    已有 Key、语音与播报设置**原样保留**；
//!  - `modelPlans` + `modelSchedule`：命名模型方案与按本地时段切换（默认关闭）；
//!  - `permissionOverrides`：逐能力 allow/ask/deny/跟随档位（默认全跟随）；
//!  - `schemaVersion` 升到 2，只用于识别结构，不做破坏性改写。

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
/// 当前配置结构版本。
pub const CONFIG_SCHEMA_VERSION: u32 = 2;

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
    /// 是否必须配置 Key 才能用（本地 mock 端点不需要）。
    pub api_key_required: bool,
    /// 该提供商是否有可查询的在线 `/models` 目录。
    pub model_discovery: bool,
}

/// 与 `packages/config/registry.mjs` 对齐的内置提供商。
///
/// 只保留真正会走的三类：MiMo（仅 thinking 开关）、DeepSeek（reasoning_effort）、
/// 以及任意 OpenAI 兼容端点。本地 llama.cpp / mock 归入标准兼容端点。
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "mimo",
        name: "Xiaomi MiMo",
        base_url: "https://api.xiaomimimo.com/v1",
        default_model: "mimo-v2.6-flash",
        capability: Capability::Mimo,
        context_window: Some(1_000_000),
        api_key_required: true,
        model_discovery: true,
    },
    ProviderInfo {
        id: "deepseek",
        name: "DeepSeek 官方",
        base_url: "https://api.deepseek.com/v1",
        default_model: "deepseek-flash",
        capability: Capability::DeepSeek,
        context_window: Some(1_000_000),
        api_key_required: true,
        model_discovery: true,
    },
    ProviderInfo {
        id: "openai-compatible",
        name: "OpenAI 兼容端点（自定义）",
        base_url: "https://api.openai.com/v1",
        default_model: "",
        capability: Capability::Plain,
        context_window: None,
        api_key_required: true,
        model_discovery: true,
    },
];

pub const DEFAULT_PROVIDER_ID: &str = "mimo";

pub fn get_provider(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

/// 自定义端点上的协议提示（只对注册表里没有声明协议的提供商生效）。
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

    /// 该提示是否给端点注入了厂商专属协议字段。
    pub fn injects_vendor_fields(&self) -> bool {
        !matches!(self, Self::Auto)
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

    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "默认（跟随服务端）",
            Self::Enabled => "开启思考",
            Self::Disabled => "关闭思考",
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

    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "默认（跟随服务端）",
            Self::None => "不推理",
            Self::Low => "低",
            Self::High => "高",
            Self::Max => "最大",
        }
    }
}

/// 权限档位：保守 / 标准 / 宽松 / 免确认。
///
/// `PassThrough`（免确认，旧版叫 `unattended`）只放开**已知且可授权**的工具；
/// 未知工具、全局暂停与显式覆盖仍然优先（详见 [`crate::permissions`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    Standard,
    Conservative,
    Permissive,
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

    /// 解析档位。
    ///
    /// 修复点（PR8）：之前只认 conservative / permissive，`"pass-through"`
    /// 会被静默解析成 Standard——界面显示的"免确认"与实际执行的策略不一致。
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "conservative" => Self::Conservative,
            "permissive" => Self::Permissive,
            // 旧版叫 unattended；两种写法都认。
            "pass-through" | "passthrough" | "unattended" => Self::PassThrough,
            _ => Self::Standard,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Conservative => "保守",
            Self::Standard => "标准",
            Self::Permissive => "宽松",
            Self::PassThrough => "免确认",
        }
    }
}

/// 逐能力覆盖：`Follow` = 跟随档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionOverride {
    #[default]
    Follow,
    Allow,
    Ask,
    Deny,
}

impl PermissionOverride {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" => Self::Allow,
            "ask" => Self::Ask,
            "deny" => Self::Deny,
            _ => Self::Follow,
        }
    }
}

/// 模型的来源：决定界面上的"已验证 / 未验证"标示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModelSource {
    /// 用户手填（在线目录里没有，照样能用）。
    #[default]
    Manual,
    /// 从 `/models` 拿到，服务端元数据确认是聊天模型。
    Discovered,
    /// 从 `/models` 拿到，但**元数据未能确认**是聊天模型（标"未验证"）。
    Unverified,
}

impl ModelSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Manual => "手填",
            Self::Discovered => "目录",
            Self::Unverified => "未验证",
        }
    }
}

/// 一个已知模型（手动添加或从在线目录获取）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    #[serde(default)]
    pub source: ModelSource,
    /// 服务端元数据声明的上下文窗口（`None` = 未知，按配置值）。
    #[serde(default)]
    pub context_window: Option<u64>,
    /// 服务端元数据声明的最大输出 tokens（仅展示）。
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

impl ModelOption {
    pub fn manual(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: ModelSource::Manual,
            context_window: None,
            max_output_tokens: None,
        }
    }
}

/// 一个提供商的持久配置（端点、首选模型、推理档位、已知模型）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub id: String,
    /// 自定义端点显示名（内置提供商留空，用注册表名字）。
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider_hint: ProviderHint,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    /// 已知模型（手动添加 + 在线目录缓存）。有界：见 [`Self::max_models`]。
    #[serde(default)]
    pub models: Vec<ModelOption>,
}

impl ProviderConfig {
    /// 每个提供商最多保留的模型条目：在线目录可能很长，但设置文件不该无限膨胀。
    pub const MAX_MODELS: usize = 80;

    pub fn display_name(&self) -> String {
        if let Some(info) = get_provider(&self.id) {
            return info.name.to_string();
        }
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        self.id.clone()
    }

    /// 该提供商的协议能力（注册表声明优先，其次用户提示）。
    pub fn capability(&self) -> Capability {
        match get_provider(&self.id) {
            Some(info) if info.capability != Capability::Plain => info.capability,
            _ => match self.provider_hint {
                ProviderHint::DeepSeek => Capability::DeepSeek,
                ProviderHint::Mimo => Capability::Mimo,
                ProviderHint::Auto => Capability::Plain,
            },
        }
    }

    /// 有没有可用的推理档位（自定义通用端点没有）。
    pub fn has_reasoning_controls(&self) -> bool {
        !matches!(self.capability(), Capability::Plain)
    }

    pub fn base_url_or_default(&self) -> String {
        if self.base_url.trim().is_empty() {
            get_provider(&self.id)
                .map(|info| info.base_url.to_string())
                .unwrap_or_default()
        } else {
            self.base_url.trim().trim_end_matches('/').to_string()
        }
    }

    /// 该提供商下某个模型的可信元数据（上下文窗口）。
    pub fn model_meta(&self, model: &str) -> Option<&ModelOption> {
        self.models.iter().find(|option| option.id == model)
    }

    /// 记下/更新一个模型（保留手动添加的模型，清掉过期的目录缓存）。
    pub fn remember_models(&mut self, discovered: &[ModelOption]) {
        let manual: Vec<ModelOption> = self
            .models
            .iter()
            .filter(|option| option.source == ModelSource::Manual)
            .cloned()
            .collect();
        let mut merged = Vec::with_capacity(manual.len() + discovered.len());
        merged.extend(manual);
        for option in discovered {
            if let Some(existing) = merged.iter_mut().find(|item| item.id == option.id) {
                existing.source = option.source;
                existing.context_window = option.context_window;
                existing.max_output_tokens = option.max_output_tokens;
            } else {
                merged.push(option.clone());
            }
        }
        if merged.len() > Self::MAX_MODELS {
            merged.truncate(Self::MAX_MODELS);
        }
        self.models = merged;
    }

    /// 手填一个模型（已存在就原样保留其元数据）。
    pub fn remember_manual_model(&mut self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        if self.models.iter().any(|option| option.id == model) {
            return;
        }
        self.models.push(ModelOption::manual(model));
        if self.models.len() > Self::MAX_MODELS {
            self.models.drain(0..self.models.len() - Self::MAX_MODELS);
        }
    }
}

/// 一套命名模型方案：「提供商 + 模型 + 推理档位」。
///
/// 不含 Key：Key 永远按提供商从 `SecretStore` 取，方案只描述"用哪家哪套参数"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPlan {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub model: String,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

/// 一条时段规则。跨午夜（22:00 → 06:00）与全天（from == to）都支持。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRule {
    pub from: String,
    pub to: String,
    /// ISO 星期（1 = 周一 … 7 = 周日）；空 = 每天。
    #[serde(default)]
    pub days: Vec<u8>,
    /// 命中的方案 id。
    pub plan_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 时段切换设置（默认关闭，不引入任何后台任务）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 不在任何时段内时用哪套方案；空 = 用全局默认（顶层 provider/model）。
    #[serde(default)]
    pub default_plan_id: String,
    #[serde(default)]
    pub rules: Vec<ScheduleRule>,
}

/// 时段匹配结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleMatch {
    /// 命中了某条规则（`index` 是规则在数组里的下标）。
    Rule { index: usize, plan_id: String },
    /// 没命中，落到默认方案（`plan_id` 可能为空 = 用全局默认）。
    Fallback { plan_id: String },
}

impl ScheduleSettings {
    /// 校验规则数组；返回可读错误。
    pub fn validate(&self) -> Result<(), String> {
        for (index, rule) in self.rules.iter().enumerate() {
            let where_ = format!("第 {} 条", index + 1);
            if crate::clock::parse_hhmm(&rule.from).is_none() {
                return Err(format!("{where_}的开始时间格式不对（要 HH:mm）"));
            }
            if crate::clock::parse_hhmm(&rule.to).is_none() {
                return Err(format!("{where_}的结束时间格式不对（要 HH:mm）"));
            }
            if rule.plan_id.trim().is_empty() {
                return Err(format!("{where_}没有选择方案"));
            }
            for day in &rule.days {
                if !(1..=7).contains(day) {
                    return Err(format!("{where_}的星期必须是 1-7（1 = 周一）"));
                }
            }
        }
        if self.enabled && self.default_plan_id.trim().is_empty() && self.rules.is_empty() {
            return Err("已启用按时间段切换，但没有任何规则".to_string());
        }
        Ok(())
    }

    /// 按本地时间匹配第一条命中的规则（先命中先用）。
    ///
    /// 规则语义与旧版 `schedule.mjs` 一致：
    ///  - `from == to` 视为全天；
    ///  - `from < to` 时左闭右开（`to` 不含在该段内）；
    ///  - `from > to` 是跨午夜；
    ///  - `days` 为空或全 7 天视为每天。
    pub fn match_at(&self, now: &crate::clock::LocalTime) -> Option<ScheduleMatch> {
        if !self.enabled {
            return None;
        }
        let current = now.minutes_of_day();
        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }
            let Some(from) = crate::clock::parse_hhmm(&rule.from) else {
                continue;
            };
            let Some(to) = crate::clock::parse_hhmm(&rule.to) else {
                continue;
            };
            let days: Vec<u8> = rule
                .days
                .iter()
                .copied()
                .filter(|d| (1..=7).contains(d))
                .collect();
            if !days.is_empty() && days.len() < 7 && !days.contains(&now.weekday) {
                continue;
            }
            let hit = if from == to {
                true
            } else if from < to {
                current >= from && current < to
            } else {
                current >= from || current < to
            };
            if hit {
                return Some(ScheduleMatch::Rule {
                    index,
                    plan_id: rule.plan_id.clone(),
                });
            }
        }
        Some(ScheduleMatch::Fallback {
            plan_id: self.default_plan_id.clone(),
        })
    }
}

/// 一次请求实际使用的模型选择（已解析端点与推理档位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
    pub provider_hint: ProviderHint,
    pub thinking: ThinkingLevel,
    pub reasoning_effort: ReasoningEffort,
    /// 选择来源，用于界面如实说明。
    pub source: SelectionSource,
}

/// 当前选择从哪来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// 会话里的手动选择。
    Session,
    /// 时段规则。
    Schedule,
    /// 默认方案。
    Plan,
    /// 全局默认（设置页顶部的模型）。
    Global,
}

impl SelectionSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Session => "会话选择",
            Self::Schedule => "时段方案",
            Self::Plan => "默认方案",
            Self::Global => "全局默认",
        }
    }
}

/// 会话级的模型选择（可只覆盖提供商/模型，也可覆盖推理档位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelChoice {
    /// 空 = 跟随计划/全局。
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
    /// `None` = 跟随方案/全局的推理档位。
    #[serde(default)]
    pub thinking: Option<ThinkingLevel>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl SessionModelChoice {
    pub fn is_empty(&self) -> bool {
        self.provider_id.trim().is_empty() && self.model.trim().is_empty()
    }
}

/// 落盘的用户设置。**不含密钥**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// 当前生效的提供商（与旧版单提供商字段保持一致）。
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
    /// 逐能力覆盖（键是能力 id，见 [`crate::permissions::capabilities`]）。
    #[serde(default)]
    pub permission_overrides: std::collections::BTreeMap<String, PermissionOverride>,
    /// 自定义系统提示词；`None` = 用内置默认。
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// 多提供商配置。空 = 旧配置，读取时由顶层字段迁移出一条。
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// 命名模型方案（供时段切换引用）。
    #[serde(default)]
    pub model_plans: Vec<ModelPlan>,
    /// 按本地时段切换方案（默认关闭）。
    #[serde(default)]
    pub model_schedule: ScheduleSettings,
    /// 语音输入设置（PR6）。**带 `serde(default)`**：旧配置照常可读。
    #[serde(default)]
    pub voice: crate::voice::settings::VoiceSettings,
    /// 语音合成设置（PR7）。同样带 `serde(default)`，并且**默认关闭**。
    #[serde(default)]
    pub tts: crate::voice::tts::TtsSettings,
}

fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
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
            schema_version: CONFIG_SCHEMA_VERSION,
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
            permission_overrides: Default::default(),
            system_prompt: None,
            providers: Vec::new(),
            model_plans: Vec::new(),
            model_schedule: ScheduleSettings::default(),
            voice: crate::voice::settings::VoiceSettings::default(),
            tts: crate::voice::tts::TtsSettings::default(),
        }
    }
}

impl Settings {
    /// 校验并规整：空白地址/模型退回该提供商默认值，数值夹到安全区间。
    /// 返回 `Err` 时不落盘（保存失败而不是写入半个配置）。
    pub fn normalized(mut self) -> Result<Self, String> {
        // 先把顶层（当前生效）提供商同步进多提供商列表，再逐条校验。
        self.sync_active_provider();
        if self.base_url.trim().is_empty() {
            self.base_url = get_provider(&self.provider_id)
                .map(|provider| provider.base_url.to_string())
                .unwrap_or_default();
        }
        if !self.base_url.trim().is_empty() && !is_http_url(&self.base_url) {
            return Err(format!(
                "模型地址必须以 http:// 或 https:// 开头：{}",
                self.base_url
            ));
        }
        if self.base_url.trim().is_empty() {
            return Err(format!("提供商 {} 的模型地址不能为空", self.provider_id));
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
        self.context_window = self.context_window.min(10_000_000);
        self.context_compress_percent = self.context_compress_percent.clamp(10, 99);
        if let Some(prompt) = &self.system_prompt {
            if prompt.trim().is_empty() {
                self.system_prompt = None;
            }
        }

        // 多提供商条目：地址/模型合法化，非法条目**报错而不是静默删除**，
        // 否则用户会看到"我配的端点不见了"。
        for provider in &mut self.providers {
            provider.id = provider.id.trim().to_string();
            if provider.id.is_empty() {
                return Err("提供商 id 不能为空".to_string());
            }
            let address = provider.base_url_or_default();
            if address.is_empty() {
                return Err(format!("提供商 {} 的模型地址不能为空", provider.id));
            }
            if !is_http_url(&address) {
                return Err(format!(
                    "提供商 {} 的地址必须以 http:// 或 https:// 开头：{address}",
                    provider.id
                ));
            }
            provider.base_url = address;
            provider.model = provider.model.trim().to_string();
            let model = provider.model.clone();
            provider.remember_manual_model(&model);
            if provider.models.len() > ProviderConfig::MAX_MODELS {
                provider.models.truncate(ProviderConfig::MAX_MODELS);
            }
        }
        // 去重（同一 id 只保留第一条），保证"按 id 查配置"结果确定。
        let mut seen = std::collections::BTreeSet::new();
        self.providers
            .retain(|provider| seen.insert(provider.id.clone()));

        // 模型方案：必须引用存在的提供商与非空模型。
        for plan in &mut self.model_plans {
            plan.id = plan.id.trim().to_string();
            plan.name = plan.name.trim().to_string();
            if plan.id.is_empty() {
                return Err("模型方案缺少 id".to_string());
            }
            if plan.name.is_empty() {
                return Err("模型方案名称不能为空".to_string());
            }
            if !self.providers.iter().any(|p| p.id == plan.provider_id) {
                return Err(format!(
                    "模型方案「{}」引用了不存在的提供商：{}",
                    plan.name, plan.provider_id
                ));
            }
            if plan.model.trim().is_empty() {
                return Err(format!("模型方案「{}」的模型名不能为空", plan.name));
            }
            plan.model = plan.model.trim().to_string();
        }
        let plan_ids: Vec<String> = self
            .model_plans
            .iter()
            .map(|plan| plan.id.clone())
            .collect();
        let mut seen_plans = std::collections::BTreeSet::new();
        self.model_plans
            .retain(|plan| seen_plans.insert(plan.id.clone()));
        // 规则引用必须存在（只允许删掉已经失效的引用，不静默保留）。
        self.model_schedule.validate()?;
        for rule in &mut self.model_schedule.rules {
            if !plan_ids.contains(&rule.plan_id) {
                return Err(format!(
                    "时段规则引用了不存在的方案：{}（开始 {}）",
                    rule.plan_id, rule.from
                ));
            }
        }
        if !self.model_schedule.default_plan_id.trim().is_empty()
            && !plan_ids.contains(&self.model_schedule.default_plan_id)
        {
            return Err("时段默认方案指向了不存在的方案".to_string());
        }

        // 语音播报设置同样只做"夹到能工作的范围"，非法值拒绝落盘。
        self.tts = std::mem::take(&mut self.tts).normalized()?;
        self.schema_version = CONFIG_SCHEMA_VERSION;
        Ok(self)
    }

    /// 把顶层（当前生效）字段写回多提供商列表；不覆盖其它提供商。
    fn sync_active_provider(&mut self) {
        let provider_id = self.provider_id.trim().to_string();
        if provider_id.is_empty() {
            return;
        }
        let entry = ProviderConfig {
            id: provider_id.clone(),
            name: get_provider(&provider_id)
                .map(|info| info.name.to_string())
                .unwrap_or_default(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            provider_hint: self.provider_hint,
            thinking: self.thinking,
            reasoning_effort: self.reasoning_effort,
            models: Vec::new(),
        };
        match self.providers.iter_mut().find(|p| p.id == provider_id) {
            Some(existing) => {
                existing.base_url = entry.base_url;
                existing.model = entry.model;
                existing.provider_hint = entry.provider_hint;
                existing.thinking = entry.thinking;
                existing.reasoning_effort = entry.reasoning_effort;
            }
            None => self.providers.push(entry),
        }
    }

    /// 取某个提供商的持久配置。
    ///
    /// 两个来源按优先级：
    ///  1. 已保存的 `providers` 条目；
    ///  2. 没有条目时按注册表默认值合成 —— **但当前生效的提供商**还要带上
    ///     顶层的 hint / 推理档位（旧配置与运行时覆盖都写在那里），
    ///     否则"用户显式填的兼容协议"会在取档案时被悄悄丢掉。
    pub fn provider(&self, provider_id: &str) -> ProviderConfig {
        if let Some(config) = self.providers.iter().find(|p| p.id == provider_id) {
            let mut copy = config.clone();
            if copy.base_url.trim().is_empty() {
                copy.base_url = copy.base_url_or_default();
            }
            return copy;
        }
        let info = get_provider(provider_id);
        let is_active = provider_id == self.provider_id;
        ProviderConfig {
            id: provider_id.to_string(),
            name: info.map(|i| i.name.to_string()).unwrap_or_default(),
            base_url: if is_active && !self.base_url.trim().is_empty() {
                self.base_url.clone()
            } else {
                info.map(|i| i.base_url.to_string()).unwrap_or_default()
            },
            model: if is_active && !self.model.trim().is_empty() {
                self.model.clone()
            } else {
                info.map(|i| i.default_model.to_string())
                    .unwrap_or_default()
            },
            provider_hint: if is_active {
                self.provider_hint
            } else {
                ProviderHint::Auto
            },
            thinking: if is_active {
                self.thinking
            } else {
                ThinkingLevel::Default
            },
            reasoning_effort: if is_active {
                self.reasoning_effort
            } else {
                ReasoningEffort::Default
            },
            models: Vec::new(),
        }
    }

    /// 生成模型端点的档案（能力判定只在这里做一次）。
    ///
    /// 判定顺序（与旧版 `resolveCapability` 等价）：
    ///  1. 内置提供商**声明了具体协议**（MiMo / DeepSeek）→ 以注册表为准，
    ///     旧配置里残留的 hint 不得把官方端点带偏；
    ///  2. 注册表里的「通用 OpenAI 兼容」条目本身没有协议声明 → 认用户显式填的 hint；
    ///  3. 注册表里没有的自定义 id → 同样认 hint；
    ///  4. 以上都没有 → 标准兼容协议（一个专属字段都不发）。
    pub fn profile(&self) -> ProviderProfile {
        self.profile_of(
            &self.provider_id,
            &self.model,
            self.thinking,
            self.reasoning_effort,
        )
    }

    /// 用指定提供商/模型/推理档位生成档案（会话级选择与方案切换都走这里）。
    pub fn profile_of(
        &self,
        provider_id: &str,
        model: &str,
        thinking: ThinkingLevel,
        reasoning_effort: ReasoningEffort,
    ) -> ProviderProfile {
        let provider = self.provider(provider_id);
        ProviderProfile {
            provider_id: provider_id.to_string(),
            capability: provider.capability(),
            base_url: provider.base_url_or_default(),
            model: model.to_string(),
            thinking,
            reasoning_effort,
            model_context_window: self.model_context_window(provider_id, model),
        }
    }

    /// 该端点/模型**可信的**上下文窗口（模型元数据只用来收紧，不用来放宽）。
    pub fn model_context_window(&self, provider_id: &str, model: &str) -> Option<u64> {
        let registry = get_provider(provider_id).and_then(|info| info.context_window);
        let meta = self
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .and_then(|p| p.model_meta(model))
            .and_then(|option| option.context_window);
        match (registry, meta) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// 该端点**可信的**上下文窗口（当前生效的提供商/模型）。
    pub fn effective_context_window(&self) -> u64 {
        context_window_with(
            self.context_window,
            self.model_context_window(&self.provider_id, &self.model),
        )
    }

    /// 指定提供商/模型的有效窗口（显示与预算共用同一个口径）。
    pub fn effective_context_window_of(&self, provider_id: &str, model: &str) -> u64 {
        context_window_with(
            self.context_window,
            self.model_context_window(provider_id, model),
        )
    }

    /// 找到某套方案。
    pub fn plan(&self, plan_id: &str) -> Option<&ModelPlan> {
        self.model_plans.iter().find(|plan| plan.id == plan_id)
    }

    /// 解析"这一轮实际用哪套模型"。
    ///
    /// 优先级（任务书第 2 条）：
    ///  1. **会话级手动选择**最高——不受时段规则影响；
    ///  2. 时段规则（第一条命中）→ 它引用的方案；
    ///  3. 默认方案（`defaultPlanId`）→ 方案；
    ///  4. 全局默认（顶层 provider/model）。
    ///
    /// 已经开始的回合不受影响：调用方只在**发请求前**解析一次并做快照。
    pub fn resolve_selection(
        &self,
        session_choice: Option<&SessionModelChoice>,
        now: &crate::clock::LocalTime,
    ) -> ModelSelection {
        if let Some(choice) = session_choice.filter(|choice| !choice.is_empty()) {
            let provider = self.provider(&choice.provider_id);
            let model = if choice.model.trim().is_empty() {
                provider.model.clone()
            } else {
                choice.model.clone()
            };
            return ModelSelection {
                provider_id: provider.id.clone(),
                base_url: provider.base_url_or_default(),
                model,
                provider_hint: provider.provider_hint,
                thinking: choice.thinking.unwrap_or(provider.thinking),
                reasoning_effort: choice.reasoning_effort.unwrap_or(provider.reasoning_effort),
                source: SelectionSource::Session,
            };
        }

        let mut plan_id = String::new();
        let mut source = SelectionSource::Global;
        if let Some(hit) = self.model_schedule.match_at(now) {
            match hit {
                ScheduleMatch::Rule { plan_id: id, .. } => {
                    plan_id = id;
                    source = SelectionSource::Schedule;
                }
                ScheduleMatch::Fallback { plan_id: id } => {
                    plan_id = id;
                    source = SelectionSource::Plan;
                }
            }
        }
        if let Some(plan) = self.plan(&plan_id) {
            let provider = self.provider(&plan.provider_id);
            return ModelSelection {
                provider_id: provider.id.clone(),
                base_url: provider.base_url_or_default(),
                model: plan.model.clone(),
                provider_hint: provider.provider_hint,
                thinking: plan.thinking,
                reasoning_effort: plan.reasoning_effort,
                source,
            };
        }

        ModelSelection {
            provider_id: self.provider_id.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            provider_hint: self.provider_hint,
            thinking: self.thinking,
            reasoning_effort: self.reasoning_effort,
            source: SelectionSource::Global,
        }
    }

    /// 与 `resolve_selection` 对应的端点档案。
    pub fn profile_for_selection(&self, selection: &ModelSelection) -> ProviderProfile {
        ProviderProfile {
            provider_id: selection.provider_id.clone(),
            capability: self.provider(&selection.provider_id).capability(),
            base_url: selection.base_url.clone(),
            model: selection.model.clone(),
            thinking: selection.thinking,
            reasoning_effort: selection.reasoning_effort,
            model_context_window: self
                .model_context_window(&selection.provider_id, &selection.model),
        }
    }

    /// 所有可选提供商 id（内置在前，自定义按配置顺序）。
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = PROVIDERS.iter().map(|info| info.id.to_string()).collect();
        for provider in &self.providers {
            if !ids.contains(&provider.id) {
                ids.push(provider.id.clone());
            }
        }
        ids
    }
}

/// 有效窗口：模型元数据只能收紧，不能放宽用户配置。
pub fn context_window_with(configured: u64, trusted: Option<u64>) -> u64 {
    match trusted.filter(|value| *value > 0) {
        Some(meta) => configured.min(meta),
        None => configured,
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

    /// 会话存储目录（与全局设置、密钥分开）。
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// 会话索引（当前选中项 + 列表元数据）。
    pub fn session_index_file(&self) -> PathBuf {
        self.sessions_dir().join("index.json")
    }

    /// 某个会话文件。
    pub fn session_file(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{session_id}.json"))
    }

    /// 权限审计日志。
    pub fn audit_file(&self) -> PathBuf {
        self.root.join("audit.jsonl")
    }

    /// 模型目录缓存（与设置分开，损坏时不影响设置）。
    pub fn model_cache_file(&self) -> PathBuf {
        self.root.join("model-cache.json")
    }

    /// 审计导出目录。
    pub fn exports_dir(&self) -> PathBuf {
        self.root.join("exports")
    }

    /// 截图落盘目录（不参与模型输入，仅供排查）。
    pub fn frames_dir(&self) -> PathBuf {
        self.root.join("frames")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)
    }
}

/// 读取设置的失败原因。
///
/// 为什么要把"没有文件"和"读不了"分开：这两种情况对调用方的决定完全不同。
///  - 没有文件 → 全新安装，用默认配置继续，**可以**做旧 Key 迁移；
///  - 读不了（损坏/权限/IO 错误）→ 用户本来有配置，绝不能悄悄换成默认值，
///    更不能拿默认的 provider_id 去给旧 Key 定归属。
#[derive(Debug)]
pub enum LoadError {
    /// 配置文件存在，但读不出来或解析不了。**不要回退默认配置**。
    Corrupt(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(message) => formatter.write_str(message),
        }
    }
}

/// 读取设置：文件不存在 → 默认值（全新安装）。
///
/// **解析/读取失败不回退默认**，而是返回 `LoadError::Corrupt`：
/// 用默认配置继续跑，会一边把请求打到用户没配过的端点，一边在下次保存时
/// 覆盖掉那份（可能只是暂时读不了的）真实配置。
///
/// PR7 的单提供商配置在这里升级：顶层字段自动补成 `providers` 的一条，
/// 已有 Key / 语音 / 播报设置原样保留（Key 不经过这里，按 provider id 存）。
pub fn load_settings(paths: &AppPaths) -> Result<Settings, LoadError> {
    let path = paths.settings_file();
    if !path.is_file() {
        return Ok(Settings::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        LoadError::Corrupt(format!(
            "读取配置失败（{}）：{error}。\n\
             为避免覆盖你的设置，程序不会用默认配置继续运行。\
             可以修复或删除该文件后重启。",
            path.display()
        ))
    })?;
    let mut settings: Settings = serde_json::from_str(&raw).map_err(|error| {
        LoadError::Corrupt(format!(
            "配置文件无法解析（{}）：{error}。\n\
             为避免覆盖你的设置，程序不会用默认配置继续运行。\
             可以修复或删除该文件后重启。",
            path.display()
        ))
    })?;
    // 旧配置（没有 providers）：按顶层字段补一条，**不改内存以外的任何东西**。
    if settings.providers.is_empty() {
        settings.sync_active_provider();
        if let Some(entry) = settings
            .providers
            .iter_mut()
            .find(|p| p.id == settings.provider_id)
        {
            entry.remember_manual_model(&settings.model);
        }
    }
    Ok(settings)
}

/// 把"拒绝访问"这类系统错误翻译成**用户能照做**的说明。
///
/// 实测过的真实原因：`cuava.exe` 被打了低完整性标签
/// （`Mandatory Label\Low Mandatory Level`，例如在受限沙箱里构建过），
/// 于是进程以 Low 完整性运行，Windows 的强制完整性控制（MIC）会禁止它
/// 写入 Medium 完整性的用户目录 —— 表现就是 `os error 5 拒绝访问`。
/// 这时只说一句"拒绝访问"没有意义：用户需要的是一条能立刻照做的路。
pub fn describe_write_error(context: &str, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "{context}：拒绝访问。\
             常见原因是本程序的可执行文件带有「低完整性」标记\
             （在受限沙箱里构建过就会这样），\
             Windows 不允许低完整性进程写入用户目录。\
             解决办法：把 cuava.exe 复制到别的目录后再运行\
             （复制不会继承该标记），或在属性里解除「解除锁定」。\
             原始错误：{error}"
        ),
        _ => format!("{context}：{error}"),
    }
}

/// 启动自检：数据目录到底能不能写。
///
/// 为什么要在启动时做：程序可以被正常启动、能聊天、能调工具，却在用户
/// 第一次点「保存」时才暴露"写不进去"。那时用户已经改了一堆设置，
/// 失败还发生在最不该出错的位置。提前一次探测（写一个临时文件再删掉）
/// 能让界面在**开始**就说清楚，而不是等到保存时才报错。
///
/// 只探测，不创建任何长期文件；失败也不影响启动（只影响可写能力）。
pub fn check_writable(paths: &AppPaths) -> Result<(), String> {
    paths.ensure().map_err(|error| {
        describe_write_error(
            &format!("无法创建数据目录（{}）", paths.root.display()),
            &error,
        )
    })?;
    let probe = paths.root.join(".write-probe");
    std::fs::write(&probe, b"probe").map_err(|error| {
        describe_write_error(
            &format!("数据目录不可写（{}）", paths.root.display()),
            &error,
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// 原子写入设置。
pub fn save_settings(paths: &AppPaths, settings: &Settings) -> Result<(), String> {
    paths
        .ensure()
        .map_err(|error| format!("无法创建数据目录（{}）：{error}", paths.root.display()))?;
    let path = paths.settings_file();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, text).map_err(|error| {
        describe_write_error(&format!("写入配置失败（{}）", tmp.display()), &error)
    })?;

    /*
     * 提交：**绝不先删除唯一有效的配置文件**。
     *
     * Windows 的 rename 不能覆盖已有目标，之前的实现是"删旧 → 改名"；
     * 改名失败时用户的配置文件就没了。现在先把旧文件降级为 `.bak`，
     * 新文件上位成功后再删备份；失败则用备份还原。
     */
    if !path.exists() {
        return std::fs::rename(&tmp, &path).map_err(|error| format!("提交配置失败：{error}"));
    }

    let backup = path.with_extension("json.bak");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&path, &backup).map_err(|error| format!("提交配置失败：{error}"))?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup);
            Ok(())
        }
        Err(error) => {
            // 新配置没能上位：还原旧配置，用户原来的设置仍然有效。
            let _ = std::fs::rename(&backup, &path);
            let _ = std::fs::remove_file(&tmp);
            Err(format!("提交配置失败：{error}"))
        }
    }
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
    settings.sync_active_provider();
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
        assert_eq!(auto.profile().capability, Capability::Plain);

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

    /// 模型元数据只能收紧，不能放宽。
    #[test]
    fn model_metadata_only_tightens_effective_window() {
        let mut settings = Settings::default();
        settings.context_window = 1_000_000;
        settings.providers = vec![ProviderConfig {
            id: "mimo".to_string(),
            name: String::new(),
            base_url: "https://api.xiaomimimo.com/v1".to_string(),
            model: "mimo-v2.6-flash".to_string(),
            provider_hint: ProviderHint::Auto,
            thinking: ThinkingLevel::Default,
            reasoning_effort: ReasoningEffort::Default,
            models: vec![ModelOption {
                id: "mimo-v2.6-flash".to_string(),
                source: ModelSource::Discovered,
                context_window: Some(131_072),
                max_output_tokens: None,
            }],
        }];
        assert_eq!(
            settings.model_context_window("mimo", "mimo-v2.6-flash"),
            Some(131_072),
            "注册表 1M 与模型 128K 取更小的"
        );
        assert_eq!(
            settings.effective_context_window_of("mimo", "mimo-v2.6-flash"),
            131_072
        );

        // 用户自己把窗口调到 200K：仍然按更小的模型元数据算。
        settings.context_window = 200_000;
        assert_eq!(
            settings.effective_context_window_of("mimo", "mimo-v2.6-flash"),
            131_072
        );

        // 元数据比注册表宽（不应该发生，但按更小者）→ 注册表 1M 生效。
        settings.providers[0].models[0].context_window = Some(4_000_000);
        assert_eq!(
            settings.model_context_window("mimo", "mimo-v2.6-flash"),
            Some(1_000_000)
        );
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

    /// PR7 的单提供商配置可以被读取：现有 Key / 语音 / 播报设置不丢，
    /// 并且会补出一条 providers 记录。
    #[test]
    fn legacy_single_provider_config_migrates_without_losing_anything() {
        let legacy = serde_json::json!({
            "schemaVersion": 1,
            "providerId": "deepseek",
            "baseUrl": "https://api.deepseek.com/v1",
            "model": "deepseek-v4-pro",
            "providerHint": "auto",
            "temperature": 0.4,
            "maxSteps": 12,
            "timeoutMs": 30000,
            "contextWindow": 1000000,
            "contextCompressPercent": 90,
            "thinking": "default",
            "reasoningEffort": "high",
            "permissionMode": "conservative",
            "systemPrompt": "自定义人设",
            "voice": { "enabled": true },
            "tts": { "enabled": true, "backend": "mimo" }
        });
        let settings: Settings = serde_json::from_value(legacy).unwrap();
        assert_eq!(settings.provider_id, "deepseek");
        assert_eq!(settings.model, "deepseek-v4-pro");
        assert_eq!(settings.reasoning_effort, ReasoningEffort::High);
        assert_eq!(settings.permission_mode, PermissionMode::Conservative);
        assert_eq!(settings.system_prompt.as_deref(), Some("自定义人设"));
        assert!(settings.voice.enabled);
        assert!(settings.tts.enabled);
        // 迁移只影响内存结构，不改写磁盘。
        let mut migrated = settings.clone();
        migrated.sync_active_provider();
        let entry = migrated
            .providers
            .iter()
            .find(|p| p.id == "deepseek")
            .unwrap();
        assert_eq!(entry.base_url, "https://api.deepseek.com/v1");
        assert_eq!(entry.model, "deepseek-v4-pro");
    }

    /// 权限档位解析必须认 `pass-through`（PR7 会把它静默当成 standard）。
    #[test]
    fn permission_mode_parsing_is_complete() {
        assert_eq!(
            PermissionMode::parse("pass-through"),
            PermissionMode::PassThrough
        );
        assert_eq!(
            PermissionMode::parse("PassThrough"),
            PermissionMode::PassThrough
        );
        assert_eq!(
            PermissionMode::parse("unattended"),
            PermissionMode::PassThrough
        );
        assert_eq!(
            PermissionMode::parse("permissive"),
            PermissionMode::Permissive
        );
        assert_eq!(
            PermissionMode::parse("conservative"),
            PermissionMode::Conservative
        );
        assert_eq!(PermissionMode::parse("standard"), PermissionMode::Standard);
        assert_eq!(PermissionMode::parse("unknown"), PermissionMode::Standard);
        for mode in [
            PermissionMode::Standard,
            PermissionMode::Conservative,
            PermissionMode::Permissive,
            PermissionMode::PassThrough,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_str()), mode, "必须能往返");
        }
    }

    /// 会话级手动选择永远优先于时段规则与全局默认。
    #[test]
    fn session_choice_beats_schedule_and_global_default() {
        let mut settings = Settings::default();
        settings.providers = vec![
            ProviderConfig {
                id: "mimo".to_string(),
                name: String::new(),
                base_url: "https://api.xiaomimimo.com/v1".to_string(),
                model: "mimo-v2.6-flash".to_string(),
                provider_hint: ProviderHint::Auto,
                thinking: ThinkingLevel::Default,
                reasoning_effort: ReasoningEffort::Default,
                models: Vec::new(),
            },
            ProviderConfig {
                id: "deepseek".to_string(),
                name: String::new(),
                base_url: "https://api.deepseek.com/v1".to_string(),
                model: "deepseek-flash".to_string(),
                provider_hint: ProviderHint::Auto,
                thinking: ThinkingLevel::Default,
                reasoning_effort: ReasoningEffort::High,
                models: Vec::new(),
            },
        ];
        settings.model_plans = vec![ModelPlan {
            id: "plan-day".to_string(),
            name: "白天".to_string(),
            provider_id: "deepseek".to_string(),
            model: "deepseek-v4-pro".to_string(),
            thinking: ThinkingLevel::Default,
            reasoning_effort: ReasoningEffort::Max,
        }];
        settings.model_schedule = ScheduleSettings {
            enabled: true,
            default_plan_id: String::new(),
            rules: vec![ScheduleRule {
                from: "09:00".to_string(),
                to: "18:00".to_string(),
                days: Vec::new(),
                plan_id: "plan-day".to_string(),
                enabled: true,
            }],
        };
        let noon = crate::clock::LocalTime {
            year: 2026,
            month: 1,
            day: 5,
            weekday: 1,
            hour: 12,
            minute: 0,
            second: 0,
            unix_ms: 0,
        };
        let scheduled = settings.resolve_selection(None, &noon);
        assert_eq!(scheduled.provider_id, "deepseek");
        assert_eq!(scheduled.model, "deepseek-v4-pro");
        assert_eq!(scheduled.reasoning_effort, ReasoningEffort::Max);
        assert_eq!(scheduled.source, SelectionSource::Schedule);

        // 会话选了 MiMo：时段规则不得覆盖它。
        let choice = SessionModelChoice {
            provider_id: "mimo".to_string(),
            model: "mimo-v2.6-pro".to_string(),
            thinking: Some(ThinkingLevel::Disabled),
            reasoning_effort: None,
        };
        let pinned = settings.resolve_selection(Some(&choice), &noon);
        assert_eq!(pinned.provider_id, "mimo");
        assert_eq!(pinned.model, "mimo-v2.6-pro");
        assert_eq!(pinned.thinking, ThinkingLevel::Disabled);
        assert_eq!(pinned.source, SelectionSource::Session);

        // 时段外 → 全局默认。
        let outside =
            settings.resolve_selection(None, &crate::clock::LocalTime { hour: 20, ..noon });
        assert_eq!(outside.source, SelectionSource::Global);
        assert_eq!(outside.model, settings.model);
    }

    /// 时段匹配：跨午夜、全天、星期过滤与"先命中先用"。
    #[test]
    fn schedule_matching_matches_legacy_semantics() {
        let make = |rules: Vec<ScheduleRule>, enabled: bool| ScheduleSettings {
            enabled,
            default_plan_id: String::new(),
            rules,
        };
        let rule = |from: &str, to: &str, days: Vec<u8>, plan: &str| ScheduleRule {
            from: from.to_string(),
            to: to.to_string(),
            days,
            plan_id: plan.to_string(),
            enabled: true,
        };
        let at = |weekday: u8, hour: u8, minute: u8| crate::clock::LocalTime {
            year: 2026,
            month: 3,
            day: 1,
            weekday,
            hour,
            minute,
            second: 0,
            unix_ms: 0,
        };

        let cross_midnight = make(vec![rule("22:00", "06:00", vec![], "night")], true);
        assert!(matches!(
            cross_midnight.match_at(&at(3, 23, 0)),
            Some(ScheduleMatch::Rule { .. })
        ));
        assert!(matches!(
            cross_midnight.match_at(&at(4, 1, 0)),
            Some(ScheduleMatch::Rule { .. })
        ));
        assert!(matches!(
            cross_midnight.match_at(&at(4, 12, 0)),
            Some(ScheduleMatch::Fallback { .. })
        ));

        let all_day = make(vec![rule("09:00", "09:00", vec![], "always")], true);
        assert!(matches!(
            all_day.match_at(&at(1, 3, 0)),
            Some(ScheduleMatch::Rule { .. })
        ));

        // 左闭右开：18:00 不含在 09:00→18:00 之间。
        let daytime = make(vec![rule("09:00", "18:00", vec![], "day")], true);
        assert!(matches!(
            daytime.match_at(&at(2, 17, 59)),
            Some(ScheduleMatch::Rule { .. })
        ));
        assert!(matches!(
            daytime.match_at(&at(2, 18, 0)),
            Some(ScheduleMatch::Fallback { .. })
        ));

        // 星期过滤：只周一到周五。
        let weekday_only = make(
            vec![rule("09:00", "18:00", vec![1, 2, 3, 4, 5], "weekday")],
            true,
        );
        assert!(matches!(
            weekday_only.match_at(&at(6, 12, 0)),
            Some(ScheduleMatch::Fallback { .. })
        ));
        assert!(matches!(
            weekday_only.match_at(&at(3, 12, 0)),
            Some(ScheduleMatch::Rule { .. })
        ));

        // 第一条命中先用。
        let two = make(
            vec![
                rule("08:00", "12:00", vec![], "first"),
                rule("00:00", "23:59", vec![], "second"),
            ],
            true,
        );
        match two.match_at(&at(2, 9, 0)) {
            Some(ScheduleMatch::Rule { index, plan_id }) => {
                assert_eq!(index, 0);
                assert_eq!(plan_id, "first");
            }
            other => panic!("应命中第一条：{other:?}"),
        }

        // 关闭时不匹配任何规则。
        let disabled = make(vec![rule("00:00", "23:59", vec![], "x")], false);
        assert!(disabled.match_at(&at(2, 9, 0)).is_none());
    }

    /// 自定义提供商（不在注册表里）可以保存并参与方案。
    #[test]
    fn custom_provider_round_trips_through_normalization() {
        let settings = Settings {
            provider_id: "custom-work".to_string(),
            base_url: "http://127.0.0.1:9000/v1".to_string(),
            model: "qwen3".to_string(),
            provider_hint: ProviderHint::Auto,
            ..Settings::default()
        };
        let normalized = settings.normalized().unwrap();
        assert_eq!(normalized.provider_id, "custom-work");
        let entry = normalized.provider("custom-work");
        assert_eq!(entry.model, "qwen3");
        assert_eq!(entry.capability(), Capability::Plain);
        assert!(normalized
            .provider_ids()
            .contains(&"custom-work".to_string()));
    }

    /// 非法的方案/规则引用必须报错，不能静默引用一个不存在的方案。
    #[test]
    fn plans_and_rules_are_validated() {
        let mut settings = Settings::default();
        settings.model_plans = vec![ModelPlan {
            id: "p1".to_string(),
            name: "方案一".to_string(),
            provider_id: "mimo".to_string(),
            model: "mimo-v2.6-flash".to_string(),
            thinking: ThinkingLevel::Default,
            reasoning_effort: ReasoningEffort::Default,
        }];
        settings.model_schedule = ScheduleSettings {
            enabled: true,
            default_plan_id: String::new(),
            rules: vec![ScheduleRule {
                from: "09:00".to_string(),
                to: "18:00".to_string(),
                days: vec![],
                plan_id: "missing".to_string(),
                enabled: true,
            }],
        };
        let error = settings.normalized().unwrap_err();
        assert!(error.contains("不存在的方案"), "{error}");

        let mut bad_time = Settings::default();
        bad_time.model_schedule = ScheduleSettings {
            enabled: true,
            default_plan_id: String::new(),
            rules: vec![ScheduleRule {
                from: "25:00".to_string(),
                to: "18:00".to_string(),
                days: vec![],
                plan_id: "p".to_string(),
                enabled: true,
            }],
        };
        assert!(bad_time.normalized().is_err());
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
