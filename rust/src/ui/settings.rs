//! 设置页的一次保存：把界面上的全部字段组装成一份新配置，校验、落盘、回填。
//!
//! PR8 的字段变多（多提供商、推理档位、提示词、权限覆盖、语音播报），
//! 这里用"一次提交"的形态把它们放在**同一个事务**里：
//!  1. 先在内存里算出一份候选配置；
//!  2. 整体校验（任何一项不合法 → 全部不落盘，界面保留草稿）；
//!  3. 只写一次盘；
//!  4. 再写 Key（只写目标提供商名下，留空 = 不动）；
//!  5. 最后按需重建 TTS 引擎。
//!
//! 这样"模型设置写失败但播报设置写成功"的旧问题不会再出现。

use std::collections::BTreeMap;

use crate::config::{
    self, AppPaths, PermissionMode, PermissionOverride, ProviderConfig, ProviderHint,
    ReasoningEffort, Settings, ThinkingLevel,
};
use crate::prompt;
use crate::secret::SecretStore;
use crate::voice::speech::SpeechRuntime;
use crate::voice::tts::{TtsBackend, TtsSettings};

/// 单个提供商在设置页中的未保存字段。
#[derive(Clone, Default)]
pub struct ProviderDraft {
    pub base_url: String,
    pub model: String,
    pub hint: i32,
    pub thinking: i32,
    pub effort: i32,
    pub api_key: String,
    pub rows: Vec<(String, bool)>,
}

impl std::fmt::Debug for ProviderDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderDraft")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// 界面「保存」提交的全部内容。
#[derive(Debug, Clone)]
pub struct SaveRequest {
    pub kws_dir: String,
    pub whisper_file: String,
    pub keywords_file: String,
    pub model_plans_json: String,
    pub model_schedule_json: String,
    // ---- 当前生效的提供商（模型页顶部） ----
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
    pub enabled_models: Vec<String>,
    pub provider_enabled_models: BTreeMap<String, Vec<String>>,
    pub provider_drafts: BTreeMap<String, ProviderDraft>,
    pub provider_hint: i32,
    pub api_key: String,
    // ---- 采样与预算 ----
    pub temperature: String,
    pub max_steps: String,
    pub timeout_ms: String,
    pub context_window: String,
    pub compress_percent: String,
    // ---- 推理档位 ----
    pub thinking: i32,
    pub reasoning_effort: i32,
    // ---- 提示词 ----
    pub custom_prompt: bool,
    pub system_prompt: String,
    // ---- 权限 ----
    pub permission_index: i32,
    pub permission_overrides: Vec<i32>,
    // ---- 联网搜索（PR12）----
    /// Tavily API Key 草稿（留空 = 不改动已有的 Key；"清除"是单独的动作）。
    pub search_api_key: String,
    /// 「启用联网搜索」开关（写进 `Settings::search.enabled`）。
    pub search_enabled: bool,
    // ---- 播报 ----
    pub tts_key: String,
    pub tts_clear_key: bool,
    pub tts_enabled: bool,
    pub tts_backend_index: i32,
    pub tts_mimo_voice: String,
    pub tts_mimo_style: String,
    pub tts_mimo_model: String,
    pub tts_mimo_tags: String,
    pub tts_mimo_streaming: bool,
    pub tts_mimo_reuse_key: bool,
    pub tts_mimo_speed: String,
    pub tts_volume: String,
    pub tts_output_device: String,

    pub tts_local_dir: String,
    pub tts_local_sid: String,
    pub tts_local_speed: String,
    pub speak_mode: i32,
}

/// 权限档位的下拉顺序（与界面数组一致）。
pub const PERMISSION_MODES: &[PermissionMode] = &[
    PermissionMode::Standard,
    PermissionMode::Conservative,
    PermissionMode::Permissive,
    PermissionMode::PassThrough,
];

/// 权限档位在界面上的标签（顺序与 [`PERMISSION_MODES`] 严格对应）。
pub const PERMISSION_LABELS: [&str; 4] = ["标准", "保守", "宽松", "免确认（不再询问）"];

pub fn permission_mode_for_index(index: i32) -> PermissionMode {
    usize::try_from(index)
        .ok()
        .and_then(|index| PERMISSION_MODES.get(index).copied())
        .unwrap_or(PermissionMode::Standard)
}

pub fn permission_mode_index(mode: PermissionMode) -> i32 {
    PERMISSION_MODES
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0) as i32
}

/// 推理档位的下拉顺序（与界面数组一致）。
pub const THINKING_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Default,
    ThinkingLevel::Enabled,
    ThinkingLevel::Disabled,
];
pub const REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Default,
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::High,
    ReasoningEffort::Max,
];

/// 界面下拉里显示的标签（与上面的顺序严格一一对应）。
pub const THINKING_LABELS: [&str; 3] = ["默认（跟随服务端）", "开启思考", "关闭思考"];
pub const EFFORT_LABELS: [&str; 5] = ["默认（跟随服务端）", "不推理", "低", "高", "最大"];

pub fn thinking_for_index(index: i32) -> ThinkingLevel {
    usize::try_from(index)
        .ok()
        .and_then(|index| THINKING_LEVELS.get(index).copied())
        .unwrap_or_default()
}

pub fn thinking_index(level: ThinkingLevel) -> i32 {
    THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
        .unwrap_or(0) as i32
}

pub fn reasoning_effort_for_index(index: i32) -> ReasoningEffort {
    usize::try_from(index)
        .ok()
        .and_then(|index| REASONING_EFFORTS.get(index).copied())
        .unwrap_or_default()
}

pub fn reasoning_effort_index(effort: ReasoningEffort) -> i32 {
    REASONING_EFFORTS
        .iter()
        .position(|candidate| *candidate == effort)
        .unwrap_or(0) as i32
}

/// 协议提示的下拉顺序（与界面数组一致）：auto / deepseek / mimo。
pub fn provider_hint_for_index(index: i32) -> ProviderHint {
    match index {
        1 => ProviderHint::DeepSeek,
        2 => ProviderHint::Mimo,
        _ => ProviderHint::Auto,
    }
}

pub fn provider_hint_index(hint: ProviderHint) -> i32 {
    match hint {
        ProviderHint::Auto => 0,
        ProviderHint::DeepSeek => 1,
        ProviderHint::Mimo => 2,
    }
}

/// 播报后端下拉顺序（与界面数组一致）。
pub const MIMO_VOICES: &[&str] = &[
    "",
    "mimo_default",
    "冰糖",
    "茉莉",
    "苏打",
    "白桦",
    "Mia",
    "Chloe",
    "Milo",
    "Dean",
];
pub fn mimo_voice_index(voice: &str) -> i32 {
    MIMO_VOICES
        .iter()
        .position(|v| *v == voice)
        .map(|i| i as i32)
        .unwrap_or(10)
}
pub fn mimo_voice_for_index(index: i32) -> &'static str {
    MIMO_VOICES.get(index as usize).copied().unwrap_or("")
}

pub const TTS_BACKENDS: &[TtsBackend] = &[TtsBackend::Off, TtsBackend::Mimo, TtsBackend::Local];

/// 自动播报选项的界面标签（与 `speak_flags` 的顺序严格对应）。
pub const SPEAK_MODE_LABELS: [&str; 4] = ["语音与文本", "只播语音", "只播文本", "都不自动"];

/// 逐能力覆盖的下拉标签（与 `PermissionOverride` 的索引对应）。
pub const OVERRIDE_LABELS: [&str; 4] = ["跟随档位", "允许", "询问", "拒绝"];

pub fn tts_backend_for_index(index: i32) -> TtsBackend {
    usize::try_from(index)
        .ok()
        .and_then(|index| TTS_BACKENDS.get(index).copied())
        .unwrap_or(TtsBackend::Off)
}

pub fn tts_backend_index(backend: TtsBackend) -> i32 {
    TTS_BACKENDS
        .iter()
        .position(|candidate| *candidate == backend)
        .unwrap_or(0) as i32
}

/// 「自动播报」：0 = 语音与文本都播，1 = 只播语音，2 = 只播文本，3 = 都不播。
pub fn speak_flags(mode: i32) -> (bool, bool) {
    match mode {
        1 => (true, false),
        2 => (false, true),
        3 => (false, false),
        _ => (true, true),
    }
}

pub fn speak_mode_code(voice: bool, text: bool) -> i32 {
    match (voice, text) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

/// 一次保存的结果分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// 配置已写入，一切正常。
    Saved,
    /// 配置已写入，但播报当前不可用（缺 Key / 缺模型 / 设备问题）。
    SavedSpeechUnavailable(String),
    /// 配置已写入，但 Key 没有写进去。
    SavedKeyFailed(String),
    /// 配置**没有**写入：校验失败或落盘失败。界面必须保留草稿。
    NotSaved(String),
}

impl SaveOutcome {
    pub fn config_written(&self) -> bool {
        !matches!(self, Self::NotSaved(_))
    }

    pub fn message(&self) -> String {
        match self {
            Self::Saved => "设置已保存".to_string(),
            Self::SavedSpeechUnavailable(detail) => {
                format!("设置已保存，但播报暂不可用：{detail}")
            }
            Self::SavedKeyFailed(detail) => {
                format!("设置已保存，但 Key 没有写入：{detail}")
            }
            Self::NotSaved(detail) => detail.clone(),
        }
    }
}

/// 解析出错时带上字段名，让用户知道该改哪一个。
fn parse_u32(field: &str, text: &str, default: u32) -> Result<u32, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<u32>()
        .map_err(|_| format!("{field}必须是整数，收到「{trimmed}」"))
}

fn parse_u64(field: &str, text: &str, default: u64) -> Result<u64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<u64>()
        .map_err(|_| format!("{field}必须是整数，收到「{trimmed}」"))
}

fn parse_f64(field: &str, text: &str, default: f64) -> Result<f64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<f64>()
        .map_err(|_| format!("{field}必须是数字，收到「{trimmed}」"))
}

fn parse_f32(field: &str, text: &str, default: f32) -> Result<f32, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<f32>()
        .map_err(|_| format!("{field}必须是数字，收到「{trimmed}」"))
}

fn parse_i32(field: &str, text: &str, default: i32) -> Result<i32, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<i32>()
        .map_err(|_| format!("{field}必须是整数，收到「{trimmed}」"))
}

/// 把界面提交组装成候选配置（**不碰磁盘**）。
pub fn build_candidate(current: &Settings, request: &SaveRequest) -> Result<Settings, String> {
    let mut next = current.clone();
    next.model_plans = serde_json::from_str(&request.model_plans_json)
        .map_err(|e| format!("模型方案格式错误：{e}"))?;
    next.model_schedule = serde_json::from_str(&request.model_schedule_json)
        .map_err(|e| format!("时段规则格式错误：{e}"))?;

    // ---- 当前生效的提供商 ----
    next.provider_id = request.provider_id.trim().to_string();
    if next.provider_id.is_empty() {
        return Err("提供商不能为空".to_string());
    }
    next.base_url = request.base_url.clone();
    next.model = request.model.clone();
    next.provider_hint = provider_hint_for_index(request.provider_hint);
    next.thinking = thinking_for_index(request.thinking);
    next.reasoning_effort = reasoning_effort_for_index(request.reasoning_effort);

    // ---- 数值字段（非法输入要**如实报错**，不能静默改默认值） ----
    next.temperature = parse_f64("温度", &request.temperature, current.temperature)?;
    next.max_steps = parse_u32("最大步数", &request.max_steps, current.max_steps)?;
    next.timeout_ms = parse_u64("超时（毫秒）", &request.timeout_ms, current.timeout_ms)?;
    next.context_window = parse_u64(
        "上下文窗口",
        &request.context_window,
        current.context_window,
    )?;
    next.context_compress_percent = parse_u32(
        "压缩触发点",
        &request.compress_percent,
        current.context_compress_percent,
    )?;

    // ---- 提示词：自定义为空 = 恢复默认（不偷偷拼回人设） ----
    next.system_prompt = if request.custom_prompt {
        prompt::normalize_system_prompt_for_storage(Some(&request.system_prompt))?
    } else {
        None
    };

    // ---- 权限 ----
    next.permission_mode = permission_mode_for_index(request.permission_index);
    let mut overrides: BTreeMap<String, PermissionOverride> = BTreeMap::new();
    for (index, capability) in crate::permissions::capabilities().iter().enumerate() {
        let choice = request
            .permission_overrides
            .get(index)
            .copied()
            .unwrap_or(0);
        let parsed = match choice {
            1 => PermissionOverride::Allow,
            2 => PermissionOverride::Ask,
            3 => PermissionOverride::Deny,
            _ => PermissionOverride::Follow,
        };
        if parsed != PermissionOverride::Follow {
            overrides.insert(capability.id.to_string(), parsed);
        }
    }
    next.permission_overrides = overrides;

    // ---- 多提供商（保留已知模型列表与其他提供商的条目） ----
    let mut providers: Vec<ProviderConfig> = current.providers.clone();
    for provider in &mut providers {
        provider.base_url = provider.base_url_or_default();
    }
    upsert_provider(
        &mut providers,
        &next.provider_id,
        &next.base_url,
        &next.model,
        next.provider_hint,
        next.thinking,
        next.reasoning_effort,
    );
    if let Some(provider) = providers.iter_mut().find(|p| p.id == next.provider_id) {
        provider.enabled_models = Some(request.enabled_models.clone());
    }
    for (id, enabled) in &request.provider_enabled_models {
        if id != &next.provider_id {
            if let Some(provider) = providers.iter_mut().find(|p| &p.id == id) {
                provider.enabled_models = Some(enabled.clone());
            } else {
                let mut provider = current.provider(id);
                provider.enabled_models = Some(enabled.clone());
                providers.push(provider);
            }
        }
    }
    for (id, draft) in &request.provider_drafts {
        if id == &next.provider_id {
            continue;
        }
        if draft.model.trim().is_empty() {
            return Err(format!("提供商 {id} 的模型名不能为空"));
        }
        upsert_provider(
            &mut providers,
            id,
            &draft.base_url,
            &draft.model,
            provider_hint_for_index(draft.hint),
            thinking_for_index(draft.thinking),
            reasoning_effort_for_index(draft.effort),
        );
        if let Some(provider) = providers.iter_mut().find(|p| &p.id == id) {
            provider.enabled_models = Some(
                draft
                    .rows
                    .iter()
                    .filter(|(_, enabled)| *enabled)
                    .map(|(name, _)| name.clone())
                    .collect(),
            );
        }
    }
    next.providers = providers;

    next.voice.kws_model_dir = request.kws_dir.clone();
    next.voice.whisper_model = request.whisper_file.clone();
    next.voice.keywords_file = request.keywords_file.clone();
    // ---- 联网搜索（Key 不在这里：它只写 SecretStore 的独立槽位） ----
    next.search.enabled = request.search_enabled;
    // ---- 播报 ----
    next.tts.enabled = request.tts_enabled;
    next.tts.backend = tts_backend_for_index(request.tts_backend_index);
    next.tts.mimo_voice = request.tts_mimo_voice.clone();
    next.tts.mimo_style = request.tts_mimo_style.clone();
    next.tts.mimo_model = request.tts_mimo_model.clone();
    next.tts.mimo_tags = crate::voice::tts::styles::decode(&request.tts_mimo_tags);
    next.tts.mimo_streaming = request.tts_mimo_streaming;
    next.tts.mimo_reuse_key = request.tts_mimo_reuse_key;
    next.tts.mimo_speed = parse_f32("MiMo语速", &request.tts_mimo_speed, current.tts.mimo_speed)?;
    next.tts.volume = parse_f32("音量", &request.tts_volume, current.tts.volume)?;
    next.tts.output_device = request.tts_output_device.clone();

    next.tts.local_model_dir = request.tts_local_dir.clone();
    next.tts.local_sid = parse_i32("音色编号", &request.tts_local_sid, current.tts.local_sid)?;
    next.tts.local_speed = parse_f32("语速", &request.tts_local_speed, current.tts.local_speed)?;
    let (voice, text) = speak_flags(request.speak_mode);
    next.tts.speak_voice_turns = voice;
    next.tts.speak_text_turns = text;

    next.normalized()
}

/// 把当前生效的那条提供商写回列表（不存在就补一条），保留其它字段。
fn upsert_provider(
    providers: &mut Vec<ProviderConfig>,
    id: &str,
    base_url: &str,
    model: &str,
    hint: ProviderHint,
    thinking: ThinkingLevel,
    effort: ReasoningEffort,
) {
    let model_text = model.to_string();
    match providers.iter_mut().find(|provider| provider.id == id) {
        Some(existing) => {
            if existing.base_url_or_default().trim_end_matches('/')
                != base_url.trim().trim_end_matches('/')
                && existing.models_origin_url.is_empty()
            {
                existing.models_origin_url = existing.base_url_or_default();
            }
            // 目录保留；地址变化只让旧服务端元数据失效。
            existing.base_url = base_url.to_string();
            existing.invalidate_catalog_for_url();
            existing.model = model_text.clone();
            existing.provider_hint = hint;
            existing.thinking = thinking;
            existing.reasoning_effort = effort;
            existing.remember_manual_model(&model_text);
        }
        None => {
            let mut entry = ProviderConfig {
                protocol_kind: None,
                enabled_models: None,
                models_origin_url: String::new(),
                id: id.to_string(),
                name: config::get_provider(id)
                    .map(|info| info.name.to_string())
                    .unwrap_or_default(),
                base_url: base_url.to_string(),
                model: model_text.clone(),
                provider_hint: hint,
                thinking,
                reasoning_effort: effort,
                models: Vec::new(),
            };
            entry.remember_manual_model(&model_text);
            providers.push(entry);
        }
    }
}

/// 这次保存是否真的改变了**播报运行时**的配置。
///
/// 只有这些变化才值得重建引擎（会 `stop()` 当前播报、重载本地模型并重新预热）：
/// TTS 开关 / 后端 / 音色 / 本地模型路径 / 设备 / 超时；
/// 以及"给**当前启用的云端后端**写入了新 Key"。
pub fn tts_runtime_config_changed(
    previous: &Settings,
    next: &Settings,
    key_changed_for_active_backend: bool,
) -> bool {
    let a = &previous.tts;
    let b = &next.tts;
    if a.active_backend() != b.active_backend() {
        return true;
    }
    if a.active_backend() == TtsBackend::Off {
        return false;
    }
    a.volume != b.volume
        || a.output_device != b.output_device
        || a.timeout_ms != b.timeout_ms
        || match a.active_backend() {
            TtsBackend::Mimo => {
                a.mimo_model != b.mimo_model
                    || a.mimo_voice != b.mimo_voice
                    || a.mimo_style != b.mimo_style
                    || a.mimo_tags != b.mimo_tags
                    || a.mimo_streaming != b.mimo_streaming
                    || a.mimo_reuse_key != b.mimo_reuse_key
                    || a.mimo_speed != b.mimo_speed
                    || key_changed_for_active_backend
            }
            TtsBackend::Local => {
                a.local_model_dir != b.local_model_dir
                    || a.local_sid != b.local_sid
                    || a.local_speed != b.local_speed
                    || a.local_provider != b.local_provider
            }
            TtsBackend::Off => false,
        }
}

/// 持久目录优先；仅为旧配置、缺失目录或更换端点补充匹配缓存，不覆盖已有条目。
pub fn restore_model_catalogs(
    settings: &mut Settings,
    paths: &AppPaths,
    secret: &SecretStore,
    pending_key: Option<(&str, &str)>,
) {
    let cache = crate::model_catalog::ModelCache::with_path(paths.model_cache_file());
    for id in settings.provider_ids() {
        let mut provider = settings.provider(&id);
        let legacy = provider.enabled_models.is_none();
        if legacy {
            provider.enabled_models = Some(provider.selectable_models());
        }
        let endpoint = provider.base_url_or_default();
        let recover = legacy
            || provider.models.is_empty()
            || provider.models_origin_url.trim().trim_end_matches('/') != endpoint;
        provider.invalidate_catalog_for_url();
        if !recover {
            continue;
        }
        let saved_key = secret.load(&id).ok().flatten().unwrap_or_default();
        let key = pending_key
            .filter(|(target, _)| *target == id)
            .map(|(_, key)| key)
            .unwrap_or(&saved_key);
        if let Some(models) = cache.models_for(&id, &provider.base_url_or_default(), key) {
            // 缓存只补充缺失候选；用户删除/修改过的持久目录不重新由缓存主导。
            for model in models {
                if provider.models.len() < ProviderConfig::MAX_MODELS
                    && !provider
                        .models
                        .iter()
                        .any(|existing| existing.id == model.id)
                {
                    provider.models.push(model.clone());
                }
            }
            provider.models_origin_url = endpoint;
            if legacy {
                let enabled = provider
                    .enabled_models
                    .as_mut()
                    .expect("legacy selection initialized");
                for model in models {
                    if !enabled.contains(&model.id) {
                        enabled.push(model.id.clone());
                    }
                }
            }
        }
        if let Some(existing) = settings.providers.iter_mut().find(|p| p.id == id) {
            *existing = provider;
        } else if !provider.models.is_empty() {
            settings.providers.push(provider);
        }
    }
}

/// **一次保存的全部逻辑**。
pub fn apply_save(
    paths: &AppPaths,
    secret: &SecretStore,
    settings: &mut Settings,
    speech: &mut SpeechRuntime,
    request: &SaveRequest,
) -> SaveOutcome {
    let previous = settings.clone();
    let mut next = match build_candidate(&previous, request) {
        Ok(next) => next,
        Err(error) => return SaveOutcome::NotSaved(error),
    };

    // 仅校验本次明确更改的路径；缺省模型不阻止文字 Agent 或其它设置保存。
    let voice_paths = crate::voice::settings::VoicePaths::resolve(paths);
    if next.voice.kws_model_dir != previous.voice.kws_model_dir
        && !next.voice.kws_model_dir.trim().is_empty()
    {
        if let Some(error) = voice_paths
            .resolve_kws_model(&next.voice.kws_model_dir)
            .error()
        {
            return SaveOutcome::NotSaved(error.into());
        }
    }
    if next.voice.whisper_model != previous.voice.whisper_model
        && !next.voice.whisper_model.trim().is_empty()
    {
        if let Some(error) = voice_paths
            .resolve_whisper_model(&next.voice.whisper_model)
            .error()
        {
            return SaveOutcome::NotSaved(error.into());
        }
    }
    if next.voice.keywords_file != previous.voice.keywords_file
        && !next.voice.keywords_file.trim().is_empty()
        && (!std::path::Path::new(&next.voice.keywords_file).is_file()
            || crate::voice::settings::keyword_labels(std::path::Path::new(
                &next.voice.keywords_file,
            ))
            .is_empty())
    {
        return SaveOutcome::NotSaved("关键词文件不存在或没有有效关键词".into());
    }
    if next.tts.local_model_dir != previous.tts.local_model_dir
        && !next.tts.local_model_dir.trim().is_empty()
    {
        if let Err(error) =
            crate::voice::tts::local::resolve_model_dir(&next.tts.local_model_dir, paths)
        {
            return SaveOutcome::NotSaved(error);
        }
    }

    let pending_key = request.api_key.trim();
    restore_model_catalogs(
        &mut next,
        paths,
        secret,
        if pending_key.is_empty() {
            None
        } else {
            Some((&request.provider_id, pending_key))
        },
    );

    // 只写一次盘。失败时内存配置**不动**，界面保留草稿。
    if let Err(error) = config::save_settings(paths, &next) {
        return SaveOutcome::NotSaved(format!("写入配置文件失败：{error}"));
    }
    *settings = next;

    // Key 仅写入各自提供商；单项失败保留表单供用户重试。
    let key_provider = settings.provider_id.clone();
    let new_key = request.api_key.trim().to_string();
    let mut errors = Vec::new();
    let effective_key = |tts: &TtsSettings| {
        secret.load("mimo-tts").ok().flatten().or_else(|| {
            if tts.mimo_reuse_key {
                secret.load("mimo").ok().flatten()
            } else {
                None
            }
        })
    };
    let previous_key = effective_key(&previous.tts);
    for (id, key) in request
        .provider_drafts
        .iter()
        .filter(|(id, _)| *id != &key_provider)
        .map(|(id, draft)| (id.as_str(), draft.api_key.trim()))
        .chain(std::iter::once((key_provider.as_str(), new_key.as_str())))
    {
        if key.is_empty() {
            continue;
        }
        match secret.save(id, key) {
            Ok(()) => {}
            Err(error) => errors.push(format!("{id}: {error}")),
        }
    }
    if request.tts_clear_key {
        match secret.clear("mimo-tts") {
            Ok(()) => {}
            Err(e) => errors.push(e.to_string()),
        }
    } else if !request.tts_key.trim().is_empty() {
        match secret.save("mimo-tts", request.tts_key.trim()) {
            Ok(()) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    let key_error = if errors.is_empty() {
        None
    } else {
        Some(errors.join("；"))
    };

    /*
     * 联网搜索的 Key 写进**独立槽位**（`SEARCH_SECRET_ID`）。
     *
     * 三条与模型 Key 相同的语义：
     *  - 留空 = 不改动已有 Key（"清除"是设置页的单独按钮）；
     *  - 写失败不静默：作为 `SavedKeyFailed` 的一部分如实告诉用户；
     *  - 绝不借用任何模型提供商的 Key（两条写入路径完全独立）。
     */
    let mut search_key_error: Option<String> = None;
    let search_key = request.search_api_key.trim();
    let mut search_key_written = false;
    if !search_key.is_empty() {
        match secret.save_search_key(search_key) {
            Ok(()) => search_key_written = true,
            Err(error) => search_key_error = Some(format!("联网搜索 Key 没有写入：{error}")),
        }
    }

    restore_model_catalogs(settings, paths, secret, None);

    // 只有"给当前启用的云端后端"写入了新 Key 才算凭据变化。
    let key_changed_for_active_backend = previous_key != effective_key(&settings.tts)
        && settings.tts.active_backend() == TtsBackend::Mimo;
    let tts_changed =
        tts_runtime_config_changed(&previous, settings, key_changed_for_active_backend);
    let speech_warning = if tts_changed {
        speech.apply_settings(&settings.tts, secret).err()
    } else {
        None
    };
    let _ = search_key_written;

    let key_error = match (key_error, search_key_error) {
        (Some(left), Some(right)) => Some(format!("{left}；{right}")),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    };

    match (key_error, speech_warning) {
        (Some(error), warning) => {
            let mut detail = error.to_string();
            if let Some(warning) = warning {
                detail.push('；');
                detail.push_str(&warning);
            }
            SaveOutcome::SavedKeyFailed(detail)
        }
        (None, Some(warning)) => SaveOutcome::SavedSpeechUnavailable(warning),
        (None, None) => SaveOutcome::Saved,
    }
}

/// 重置 TTS 设置为默认（设置页「恢复默认播报」用）。
pub fn default_tts() -> TtsSettings {
    TtsSettings::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request(settings: &Settings) -> SaveRequest {
        SaveRequest {
            kws_dir: settings.voice.kws_model_dir.clone(),
            whisper_file: settings.voice.whisper_model.clone(),
            keywords_file: settings.voice.keywords_file.clone(),
            model_plans_json: serde_json::to_string(&settings.model_plans).unwrap(),
            model_schedule_json: serde_json::to_string(&settings.model_schedule).unwrap(),
            provider_id: settings.provider_id.clone(),
            base_url: settings.base_url.clone(),
            model: settings.model.clone(),
            enabled_models: settings.provider(&settings.provider_id).selectable_models(),
            provider_enabled_models: BTreeMap::new(),
            provider_drafts: BTreeMap::new(),
            provider_hint: provider_hint_index(settings.provider_hint),
            api_key: String::new(),
            temperature: format!("{:.1}", settings.temperature),
            max_steps: settings.max_steps.to_string(),
            timeout_ms: settings.timeout_ms.to_string(),
            context_window: settings.context_window.to_string(),
            compress_percent: settings.context_compress_percent.to_string(),
            thinking: thinking_index(settings.thinking),
            reasoning_effort: reasoning_effort_index(settings.reasoning_effort),
            custom_prompt: false,
            system_prompt: String::new(),
            permission_index: permission_mode_index(settings.permission_mode),
            permission_overrides: vec![0; crate::permissions::capabilities().len()],
            search_api_key: String::new(),
            search_enabled: settings.search.enabled,
            tts_key: String::new(),
            tts_clear_key: false,
            tts_enabled: false,
            tts_backend_index: 0,
            tts_mimo_voice: String::new(),
            tts_mimo_style: String::new(),
            tts_mimo_model: String::new(),
            tts_mimo_tags: String::new(),
            tts_mimo_streaming: true,
            tts_mimo_reuse_key: true,
            tts_mimo_speed: "1.0".into(),
            tts_volume: "1.0".into(),
            tts_output_device: String::new(),

            tts_local_dir: String::new(),
            tts_local_sid: settings.tts.local_sid.to_string(),
            tts_local_speed: format!("{:.1}", settings.tts.local_speed),
            speak_mode: 0,
        }
    }

    #[test]
    fn card_catalog_survives_unrelated_save_key_clear_and_restart_without_cache() {
        let paths = AppPaths::with_root(std::env::temp_dir().join(format!(
                "cuava-catalog-authority-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        for id in ["mimo", "deepseek"] {
            let mut provider = settings.provider(id);
            let mut remote = config::ModelOption::manual("server-model");
            remote.source = config::ModelSource::Discovered;
            remote.display_name = "用户显示名".into();
            remote.context_window = Some(64000);
            remote.max_output_tokens = Some(4000);
            remote.image_input = Some(true);
            let mut unknown = config::ModelOption::manual("unknown");
            unknown.source = config::ModelSource::Unverified;
            provider.model = remote.id.clone();
            provider.models = vec![remote, unknown, config::ModelOption::manual("private")];
            provider.enabled_models = Some(vec!["server-model".into(), "unknown".into()]);
            provider.models_origin_url = provider.base_url_or_default();
            settings = super::super::endpoint_cards::save_provider(
                &paths,
                &secret,
                &settings,
                &provider,
                "dummy-key",
                "",
            )
            .unwrap();
        }
        let before = settings.providers.clone();
        assert!(!paths.model_cache_file().exists());
        let mut request = base_request(&settings);
        request.custom_prompt = true;
        request.system_prompt = "保留模型目录的提示词".into();
        request.temperature = "0.7".into();
        request.context_window = "32000".into();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        assert!(apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written());
        assert_eq!(settings.providers, before);
        secret.clear("deepseek").unwrap();
        restore_model_catalogs(&mut settings, &paths, &secret, None);
        assert_eq!(settings.providers, before);
        let mut restarted = config::load_settings(&paths).unwrap();
        restore_model_catalogs(&mut restarted, &paths, &secret, None);
        assert_eq!(restarted.providers, before);
        // 旧缓存即使含相同 ID 的过时元数据，也不能更新或复活用户的持久目录。
        let p = restarted.provider("mimo");
        let cache = serde_json::json!({"entries":{"old":{"atMs":1,"providerId":"mimo","baseUrl":p.base_url,"keyFingerprint":crate::model_catalog::fingerprint("dummy-key"),"models":[{"id":"server-model","displayName":"过时名称","source":"discovered","contextWindow":8},{"id":"deleted-cached-model","source":"discovered"}],"dropped":[],"raw":2}}});
        std::fs::write(
            paths.model_cache_file(),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        restore_model_catalogs(&mut restarted, &paths, &secret, None);
        assert_eq!(restarted.providers, before);
        let new_url = "http://127.0.0.1:24999/v1";
        restarted.base_url = new_url.into();
        restarted
            .providers
            .iter_mut()
            .find(|p| p.id == "mimo")
            .unwrap()
            .base_url = new_url.into();
        restore_model_catalogs(&mut restarted, &paths, &secret, None);
        let changed = restarted.provider("mimo");
        let stale = changed
            .models
            .iter()
            .find(|m| m.id == "server-model")
            .unwrap();
        assert_eq!(stale.source, config::ModelSource::Unverified);
        assert_eq!(stale.context_window, None);
        assert_eq!(stale.image_input, None);
        assert_eq!(changed.enabled_models, p.enabled_models);
        assert!(changed.models.iter().any(|m| m.id == "private"));
        std::fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn invalid_other_provider_draft_does_not_write_any_keys_or_settings() {
        let root = std::env::temp_dir().join(format!(
            "cuava-invalid-drafts-{}",
            crate::clock::now().unix_ms
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut request = base_request(&settings);
        request.api_key = "test-current-key".into();
        request.provider_drafts.insert(
            "deepseek".into(),
            ProviderDraft {
                base_url: "ftp://invalid".into(),
                model: "test".into(),
                api_key: "test-other-key".into(),
                ..Default::default()
            },
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        assert!(
            !apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written()
        );
        assert!(!paths.settings_file().exists());
        assert!(!secret.has("mimo"));
        assert!(!secret.has("deepseek"));
        assert_eq!(
            request.provider_drafts["deepseek"].api_key,
            "test-other-key"
        );
    }

    #[test]
    fn provider_fields_and_keys_are_saved_to_their_own_accounts() {
        let root = std::env::temp_dir().join(format!(
            "cuava-multi-drafts-{}",
            crate::clock::now().unix_ms
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut request = base_request(&settings);
        request.provider_id = "deepseek".into();
        request.base_url = "https://deepseek.example/v1".into();
        request.model = "deepseek-test".into();
        request.api_key = "test-deepseek-key".into();
        request.enabled_models = vec!["deepseek-test".into()];
        request.provider_drafts.insert(
            "mimo".into(),
            ProviderDraft {
                base_url: "https://mimo.example/v1".into(),
                model: "mimo-test".into(),
                thinking: 1,
                effort: 1,
                api_key: "test-mimo-key".into(),
                rows: vec![("mimo-test".into(), true)],
                ..Default::default()
            },
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        assert!(apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written());
        let loaded = config::load_settings(&paths).unwrap();
        assert_eq!(loaded.provider("mimo").base_url, "https://mimo.example/v1");
        assert_eq!(loaded.provider("mimo").model, "mimo-test");
        assert_eq!(
            loaded.provider("deepseek").base_url,
            "https://deepseek.example/v1"
        );
        assert_eq!(loaded.provider("deepseek").model, "deepseek-test");
        assert_eq!(
            secret.load("mimo").unwrap().as_deref(),
            Some("test-mimo-key")
        );
        assert_eq!(
            secret.load("deepseek").unwrap().as_deref(),
            Some("test-deepseek-key")
        );
        assert!(!std::fs::read_to_string(paths.settings_file())
            .unwrap()
            .contains("test-mimo-key"));
    }

    #[test]
    fn provider_drafts_are_saved_together_and_reload() {
        let root = std::env::temp_dir().join(format!(
            "cuava-provider-drafts-{}",
            crate::clock::now().unix_ms
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(root);
        let settings = Settings::default();
        let mut request = base_request(&settings);
        request.provider_id = "deepseek".into();
        let deepseek = settings.provider("deepseek");
        request.base_url = deepseek.base_url_or_default();
        request.model = deepseek.model;
        request.enabled_models = vec!["deepseek-choice".into()];
        request
            .provider_enabled_models
            .insert("mimo".into(), vec!["mimo-choice".into()]);
        let next = build_candidate(&settings, &request).unwrap();
        assert_ne!(
            settings.provider("mimo").selectable_models(),
            vec!["mimo-choice"]
        );
        config::save_settings(&paths, &next).unwrap();
        let loaded = config::load_settings(&paths).unwrap();
        assert_eq!(
            loaded.provider("mimo").enabled_models,
            Some(vec!["mimo-choice".into()])
        );
        assert_eq!(
            loaded.provider("deepseek").enabled_models,
            Some(vec!["deepseek-choice".into()])
        );
    }

    #[test]
    fn enabled_models_and_multiline_prompt_survive_save_and_catalog_refresh() {
        let root = std::env::temp_dir().join(format!("cuava-pr11-{}", crate::clock::now().unix_ms));
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(root);
        let mut settings = Settings::default();
        let endpoint = settings.provider("mimo").base_url_or_default();
        settings.remember_discovered_models(
            "mimo",
            &endpoint,
            &[
                config::ModelOption::manual("one"),
                config::ModelOption::manual("two"),
            ],
        );
        let mut request = base_request(&settings);
        request.enabled_models = vec!["one".into(), "two".into()];
        request.custom_prompt = true;
        request.system_prompt = (0..60).map(|i| format!("第{i}行：中文提示词\n")).collect();
        let next = build_candidate(&settings, &request).unwrap();
        config::save_settings(&paths, &next).unwrap();
        let mut restored = config::load_settings(&paths).unwrap();
        assert_eq!(restored.system_prompt, Some(request.system_prompt));
        restored.remember_discovered_models(
            "mimo",
            &endpoint,
            &[config::ModelOption::manual("three")],
        );
        let models = restored.provider("mimo").selectable_models();
        assert!(models.contains(&"one".into()) && models.contains(&"two".into()));
        assert!(!models.contains(&"three".into()));
        assert!(restored
            .provider("mimo")
            .models
            .iter()
            .any(|m| m.id == "three"));
        request = base_request(&restored);
        request.enabled_models = vec!["one".into()];
        let disabled = build_candidate(&restored, &request).unwrap();
        assert!(disabled
            .provider("mimo")
            .models
            .iter()
            .any(|m| m.id == "two"));
        assert!(!disabled
            .provider("mimo")
            .selectable_models()
            .contains(&"two".into()));
    }

    #[test]
    fn saving_changed_endpoint_uses_its_cached_models_and_preserves_other_providers() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/save-discovered-endpoint");
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(&root);
        let mut settings = Settings::default();
        let old = settings.provider("mimo").base_url_or_default();
        let old_models = vec![crate::config::ModelOption::manual("old-model")];
        assert!(settings.remember_discovered_models("mimo", &old, &old_models));
        let endpoint = "http://127.0.0.1:12345/v1";
        assert!(!settings.remember_discovered_models(
            "mimo",
            endpoint,
            &[crate::config::ModelOption::manual("new-model")]
        ));
        let cache = serde_json::json!({"entries": {format!("mimo|{endpoint}"): {"atMs": 1, "providerId": "mimo", "baseUrl": endpoint, "keyFingerprint": "", "models": [{"id": "new-model"}], "dropped": [], "raw": 1}}});
        std::fs::write(
            paths.model_cache_file(),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        let mut request = base_request(&settings);
        request.base_url = endpoint.into();
        request.model = "new-model".into();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        let secret = SecretStore::new(paths.secret_file());
        assert!(apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written());
        let restored = config::load_settings(&paths).unwrap();
        let provider = restored.provider("mimo");
        assert_eq!(provider.base_url, endpoint);
        assert!(provider.models.iter().any(|m| m.id == "new-model"));
        assert!(provider.models.iter().any(|m| m.id == "old-model"));
        let cached = crate::model_catalog::ModelCache::with_path(paths.model_cache_file());
        assert_eq!(
            cached.models_for("mimo", endpoint, "").unwrap()[0].id,
            "new-model"
        );
    }

    #[test]
    fn selectors_round_trip_with_every_value() {
        for mode in PERMISSION_MODES {
            assert_eq!(
                permission_mode_for_index(permission_mode_index(*mode)),
                *mode
            );
        }
        for level in THINKING_LEVELS {
            assert_eq!(thinking_for_index(thinking_index(*level)), *level);
        }
        for effort in REASONING_EFFORTS {
            assert_eq!(
                reasoning_effort_for_index(reasoning_effort_index(*effort)),
                *effort
            );
        }
        for backend in TTS_BACKENDS {
            assert_eq!(tts_backend_for_index(tts_backend_index(*backend)), *backend);
        }
        for hint in [
            ProviderHint::Auto,
            ProviderHint::DeepSeek,
            ProviderHint::Mimo,
        ] {
            assert_eq!(provider_hint_for_index(provider_hint_index(hint)), hint);
        }
        // 越界不 panic，落到安全默认值。
        assert_eq!(permission_mode_for_index(-1), PermissionMode::Standard);
        assert_eq!(tts_backend_for_index(99), TtsBackend::Off);
        assert_eq!(speak_flags(9), (true, true));
        assert_eq!(speak_mode_code(true, true), 0);
        // 标签数量必须与枚举数量一致，否则下拉会错位。
        assert_eq!(PERMISSION_LABELS.len(), PERMISSION_MODES.len());
        assert_eq!(THINKING_LABELS.len(), THINKING_LEVELS.len());
        assert_eq!(EFFORT_LABELS.len(), REASONING_EFFORTS.len());
        assert_eq!(SPEAK_MODE_LABELS.len(), 4);
        assert_eq!(OVERRIDE_LABELS.len(), 4);
    }

    #[test]
    fn invalid_numbers_are_reported_without_silent_defaults() {
        let settings = Settings::default();
        let mut request = base_request(&settings);
        request.max_steps = "很多".to_string();
        let error = build_candidate(&settings, &request).unwrap_err();
        assert!(error.contains("最大步数"), "{error}");
        assert!(error.contains("很多"), "错误要带上用户输入：{error}");

        let mut request = base_request(&settings);
        request.tts_local_speed = "快".to_string();
        let error = build_candidate(&settings, &request).unwrap_err();
        assert!(error.contains("语速"), "{error}");

        let mut request = base_request(&settings);
        request.tts_local_sid = "abc".to_string();
        assert!(build_candidate(&settings, &request)
            .unwrap_err()
            .contains("音色编号"));
    }

    #[test]
    fn custom_prompt_is_used_verbatim_and_blank_recovers_default() {
        let settings = Settings::default();
        let mut request = base_request(&settings);
        request.custom_prompt = true;
        request.system_prompt = "  自定义人设  ".to_string();
        let candidate = build_candidate(&settings, &request).unwrap();
        assert_eq!(candidate.system_prompt.as_deref(), Some("  自定义人设  "));

        let mut request = base_request(&settings);
        request.custom_prompt = true;
        request.system_prompt = "   ".to_string();
        assert!(build_candidate(&settings, &request).is_err());

        // 关掉自定义 → None（恢复内置默认，不是拼回人设）。
        let mut request = base_request(&settings);
        request.custom_prompt = false;
        request.system_prompt = "旧的自定义".to_string();
        let candidate = build_candidate(&settings, &request).unwrap();
        assert_eq!(candidate.system_prompt, None);
    }

    #[test]
    fn overrides_only_record_non_follow_capabilities() {
        let settings = Settings::default();
        let mut request = base_request(&settings);
        let index = crate::permissions::capabilities()
            .iter()
            .position(|cap| cap.id == "clipboard.read")
            .unwrap();
        request.permission_overrides[index] = 3; // deny
        request.permission_index = permission_mode_index(PermissionMode::PassThrough);
        let candidate = build_candidate(&settings, &request).unwrap();
        assert_eq!(candidate.permission_mode, PermissionMode::PassThrough);
        assert_eq!(
            candidate.permission_overrides.get("clipboard.read"),
            Some(&PermissionOverride::Deny)
        );
        assert!(
            !candidate.permission_overrides.contains_key("input.pointer"),
            "跟随档位的项不写进配置"
        );
    }

    #[test]
    fn timeout_and_context_window_are_clamped_by_normalization() {
        let settings = Settings::default();
        let mut request = base_request(&settings);
        request.timeout_ms = "1".to_string();
        request.context_window = "0".to_string();
        request.compress_percent = "5".to_string();
        let candidate = build_candidate(&settings, &request).unwrap();
        assert_eq!(candidate.timeout_ms, 5_000);
        assert_eq!(candidate.context_window, 1_000_000);
        assert_eq!(candidate.context_compress_percent, 10);
    }

    #[test]
    fn pr14_voice_mapping_preserves_defaults_presets_and_custom() {
        for (i, voice) in MIMO_VOICES.iter().enumerate() {
            assert_eq!(mimo_voice_index(voice), i as i32);
            assert_eq!(mimo_voice_for_index(i as i32), *voice);
        }
        assert_ne!(mimo_voice_index(""), mimo_voice_index("mimo_default"));
        assert_eq!(mimo_voice_index("legacy-custom"), 10);
        for voice in ["茉莉", "", "mimo_default", "legacy-custom"] {
            let mut settings = Settings::default();
            settings.tts.mimo_voice = voice.into();
            let json = serde_json::to_string(&settings).unwrap();
            let loaded: Settings = serde_json::from_str(&json).unwrap();
            assert_eq!(loaded.tts.mimo_voice, voice);
            assert_eq!(
                mimo_voice_index(&loaded.tts.mimo_voice),
                mimo_voice_index(voice)
            );
        }
    }

    #[test]
    fn pr14_save_restart_and_secret_isolation() {
        let root = std::env::temp_dir().join(format!("cuava-pr14-{}", crate::clock::now().unix_ms));
        let paths = AppPaths::with_root(root);
        let secret = SecretStore::new(paths.secret_file());
        secret.save("mimo", "official-test").unwrap();
        secret.save("deepseek", "deepseek-test").unwrap();
        let mut settings = Settings::default();
        let mut request = base_request(&settings);
        request.tts_key = "tts-test".into();
        request.tts_mimo_model = "mimo-v2.5-tts".into();
        request.tts_mimo_tags =
            crate::voice::tts::styles::encode(&["温柔".into(), "含|分隔符".into()]);
        request.tts_mimo_style = "  自然\n 保留  ".into();
        request.tts_mimo_streaming = false;
        request.tts_mimo_speed = "1.5".into();
        request.tts_volume = "0.4".into();
        request.tts_output_device = "测试扬声器".into();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        assert!(apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written());
        let loaded = config::load_settings(&paths).unwrap();
        assert_eq!(loaded.tts.mimo_tags, vec!["温柔", "含|分隔符"]);
        assert_eq!(loaded.tts.mimo_style, request.tts_mimo_style);
        assert_eq!(loaded.tts.volume, 0.4);
        assert_eq!(loaded.tts.mimo_speed, 1.5);
        assert!(!loaded.tts.mimo_streaming);
        assert_eq!(loaded.tts.output_device, "测试扬声器");
        assert_eq!(
            secret.load("mimo-tts").unwrap().as_deref(),
            Some("tts-test")
        );
        request.tts_key.clear();
        request.tts_clear_key = true;
        assert!(apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written());
        assert!(!secret.has("mimo-tts"));
        assert_eq!(
            secret.load("mimo").unwrap().as_deref(),
            Some("official-test")
        );
        assert_eq!(
            secret.load("deepseek").unwrap().as_deref(),
            Some("deepseek-test")
        );
    }

    /// 只改模型端点时，TTS 运行时配置不算变化 → 不重建引擎（不会中断播报）。
    #[test]
    fn llm_only_change_does_not_require_tts_rebuild() {
        let mut previous = Settings::default();
        previous.tts.enabled = true;
        previous.tts.backend = TtsBackend::Mimo;

        let mut next = previous.clone();
        next.provider_id = "deepseek".to_string();
        next.model = "deepseek-v4-pro".to_string();
        next.base_url = "https://api.deepseek.com/v1".to_string();
        next.permission_mode = PermissionMode::Conservative;

        assert!(
            !tts_runtime_config_changed(&previous, &next, false),
            "只改 LLM 端点 / 权限时不该重建 TTS"
        );

        next.tts.mimo_voice = "nova".to_string();
        assert!(tts_runtime_config_changed(&previous, &next, false));

        let mut third = previous.clone();
        third.tts.backend = TtsBackend::Local;
        assert!(tts_runtime_config_changed(&previous, &third, false));

        let mut fourth = previous.clone();
        fourth.tts.enabled = false;
        assert!(tts_runtime_config_changed(&previous, &fourth, false));

        // 自动播报开关只影响"要不要播"，不要求重建引擎。
        let mut fifth = previous.clone();
        fifth.tts.speak_text_turns = false;
        assert!(
            !tts_runtime_config_changed(&previous, &fifth, false),
            "只改自动播报开关不该重建引擎"
        );

        // 给当前启用的 MiMo 换了新 Key：凭据变了，必须重建。
        assert!(
            tts_runtime_config_changed(&previous, &previous.clone(), true),
            "换了当前后端的 Key 必须重建引擎"
        );
    }

    // ---------------------------------------------------------------
    // PR12：联网搜索设置与独立 Key
    // ---------------------------------------------------------------

    /// 「启用联网搜索」开关落进配置，但不影响模型提供商。
    #[test]
    fn search_settings_are_saved_without_touching_providers() {
        let root = std::env::temp_dir().join(format!(
            "cuava-search-settings-{}",
            crate::clock::now().unix_ms
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let before_provider = settings.provider_id.clone();
        let mut request = base_request(&settings);
        request.search_enabled = false;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        assert!(apply_save(&paths, &secret, &mut settings, &mut speech, &request).config_written());

        let loaded = config::load_settings(&paths).unwrap();
        assert!(!loaded.search.enabled);
        assert_eq!(loaded.provider_id, before_provider, "搜索开关不能改提供商");
        // 开关本身不需要 Key。
        assert!(!secret.has_search_key());
    }

    /// 搜索 Key 写进**独立槽位**，与模型提供商的 Key 互不影响。
    #[cfg(windows)]
    #[test]
    fn search_key_is_written_to_its_own_slot_only() {
        let root =
            std::env::temp_dir().join(format!("cuava-search-key-{}", crate::clock::now().unix_ms));
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::with_root(root);
        let secret = SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        let mut request = base_request(&settings);
        request.api_key = "test-model-key".into();
        request.search_api_key = "tvly-search-key".into();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut speech = SpeechRuntime::new(tx);
        let outcome = apply_save(&paths, &secret, &mut settings, &mut speech, &request);
        assert!(outcome.config_written(), "{}", outcome.message());

        assert_eq!(
            secret.load_search_key().unwrap().as_deref(),
            Some("tvly-search-key")
        );
        assert_eq!(
            secret.load(&settings.provider_id).unwrap().as_deref(),
            Some("test-model-key"),
            "模型 Key 必须写到它自己的提供商名下"
        );
        // 配置文件里不出现任何一个 Key。
        let text = std::fs::read_to_string(paths.settings_file()).unwrap();
        assert!(!text.contains("tvly-search-key"));
        assert!(!text.contains("test-model-key"));

        // 留空 = 不改动已保存的搜索 Key。
        let mut second = base_request(&settings);
        second.search_api_key = String::new();
        let _ = apply_save(&paths, &secret, &mut settings, &mut speech, &second);
        assert_eq!(
            secret.load_search_key().unwrap().as_deref(),
            Some("tvly-search-key"),
            "留空不能把已有 Key 抹掉"
        );

        // 清除只影响搜索这一格。
        secret.clear_search_key().unwrap();
        assert!(!secret.has_search_key());
        assert_eq!(
            secret.load(&settings.provider_id).unwrap().as_deref(),
            Some("test-model-key"),
            "清搜索 Key 不能顺手清模型 Key"
        );
    }
}
