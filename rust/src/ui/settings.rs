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

/// 界面「保存」提交的全部内容。
#[derive(Debug, Clone)]
pub struct SaveRequest {
    pub model_plans_json: String,
    pub model_schedule_json: String,
    // ---- 当前生效的提供商（模型页顶部） ----
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
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
    // ---- 播报 ----
    pub tts_enabled: bool,
    pub tts_backend_index: i32,
    pub tts_mimo_voice: String,
    pub tts_mimo_style: String,
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
    next.providers = providers;

    // ---- 播报 ----
    next.tts.enabled = request.tts_enabled;
    next.tts.backend = tts_backend_for_index(request.tts_backend_index);
    next.tts.mimo_voice = request.tts_mimo_voice.clone();
    next.tts.mimo_style = request.tts_mimo_style.clone();
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
            existing.base_url = base_url.to_string();
            existing.model = model_text.clone();
            existing.provider_hint = hint;
            existing.thinking = thinking;
            existing.reasoning_effort = effort;
            existing.remember_manual_model(&model_text);
        }
        None => {
            let mut entry = ProviderConfig {
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
    a.enabled != b.enabled
        || a.backend != b.backend
        || a.mimo_model != b.mimo_model
        || a.mimo_voice != b.mimo_voice
        || a.mimo_style != b.mimo_style
        || a.local_model_dir != b.local_model_dir
        || a.local_sid != b.local_sid
        || (a.local_speed - b.local_speed).abs() > f32::EPSILON
        || a.local_provider != b.local_provider
        || a.output_device != b.output_device
        || a.timeout_ms != b.timeout_ms
        || key_changed_for_active_backend
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
    let next = match build_candidate(&previous, request) {
        Ok(next) => next,
        Err(error) => return SaveOutcome::NotSaved(error),
    };

    // 只写一次盘。失败时内存配置**不动**，界面保留草稿。
    if let Err(error) = config::save_settings(paths, &next) {
        return SaveOutcome::NotSaved(format!("写入配置文件失败：{error}"));
    }
    *settings = next;

    // 再写 Key：只写到目标提供商名下，留空 = 不动。
    let key_provider = settings.provider_id.clone();
    let new_key = request.api_key.trim().to_string();
    let key_error = if new_key.is_empty() {
        None
    } else {
        secret.save(&key_provider, &new_key).err()
    };

    // 只有"给当前启用的云端后端"写入了新 Key 才算凭据变化。
    let key_changed_for_active_backend = key_error.is_none()
        && !new_key.is_empty()
        && key_provider == crate::ui::worker::MIMO_PROVIDER_ID
        && settings.tts.active_backend() == TtsBackend::Mimo;
    let tts_changed =
        tts_runtime_config_changed(&previous, settings, key_changed_for_active_backend);
    let speech_warning = if tts_changed {
        speech.apply_settings(&settings.tts, secret).err()
    } else {
        None
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
            model_plans_json: serde_json::to_string(&settings.model_plans).unwrap(),
            model_schedule_json: serde_json::to_string(&settings.model_schedule).unwrap(),
            provider_id: settings.provider_id.clone(),
            base_url: settings.base_url.clone(),
            model: settings.model.clone(),
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
            tts_enabled: false,
            tts_backend_index: 0,
            tts_mimo_voice: String::new(),
            tts_mimo_style: String::new(),
            tts_local_dir: String::new(),
            tts_local_sid: settings.tts.local_sid.to_string(),
            tts_local_speed: format!("{:.1}", settings.tts.local_speed),
            speak_mode: 0,
        }
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
}
