//! 模型目录：`/models` 获取、分类、缓存，以及"测试连接"的分类结果。
//!
//! 移植自 `packages/config/{model-catalog,model-discovery}.mjs`，只保留运行期需要的部分。
//!
//! 三条硬规矩（都是用户会实际碰到的）：
//!  1. **凭据按提供商取**：查询哪一家就用哪一家的 Key，绝不借用其它提供商的；
//!  2. **失败不改变用户的选择**：网络失败 / 401 / 空目录都只回错误说明，
//!     上一次成功的列表照样保留（界面仍能选、模型名照样能手填）；
//!  3. **缓存不存 Key**：只缓存"哪些模型存在"，并用短 TTL 避免看不见上新。
//!
//! 分类只做"宁可多留、不可误删"：有元数据按元数据判，没有元数据只剔除
//! 已知的非聊天标识（asr / tts / embedding…），认不出来的标"未验证"而不是丢掉。
//! **`/models` 成功不等于模型能聊天**，所以"测试连接"与"获取模型"是两件事。

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ModelOption, ModelSource, ProviderConfig};
use crate::llm::{ChatMessage, LlmClient, LlmError, ProviderProfile};

/// 模型列表缓存时长：够界面来回切提供商不重复打 API，又不至于看不见新模型。
pub const MODEL_LIST_TTL_MS: u64 = 60_000;

/// 已知不用于 Chat Completions 的模型名标识（按**整段**匹配，避免 `attest` 被 `tts` 误伤）。
const NON_CHAT_SEGMENTS: &[&str] = &[
    "asr",
    "tts",
    "stt",
    "voice",
    "voices",
    "voiceclone",
    "voicedesign",
    "voiceconversion",
    "audio",
    "speech",
    "whisper",
    "transcribe",
    "transcription",
    "parakeet",
    "coqui",
    "embed",
    "embedding",
    "embeddings",
    "rerank",
    "reranker",
];

/// 元数据里表示「能当聊天模型用」的信号。
const CHAT_TASK_TYPES: &[&str] = &[
    "text-generation",
    "chat",
    "chat-completions",
    "conversational",
    "text",
];

/// 元数据里明确"不是聊天模型"的信号。
const NON_CHAT_TASK_TYPES: &[&str] = &[
    "audio-to-text",
    "text-to-audio",
    "speech-recognition",
    "speech-synthesis",
    "automatic-speech-recognition",
    "text-to-speech",
    "text-to-image",
    "image-generation",
    "text-to-video",
    "feature-extraction",
    "sentence-similarity",
    "rerank",
    "fill-mask",
];

/// 一条 `/models` 记录的分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelClassification {
    pub id: String,
    pub chat: bool,
    pub reason: String,
}

fn segments(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|piece| !piece.is_empty())
        .map(str::to_string)
        .collect()
}

fn push_hint(into: &mut Vec<String>, value: Option<&Value>) {
    match value {
        Some(Value::String(text)) => {
            let lower = text.trim().to_ascii_lowercase();
            if !lower.is_empty() {
                into.push(lower);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                push_hint(into, Some(item));
            }
        }
        _ => {}
    }
}

fn metadata_hints(entry: &Value) -> Vec<String> {
    let mut hints = Vec::new();
    if let Some(object) = entry.as_object() {
        for key in ["type", "modelType", "model_type", "task", "task_type"] {
            push_hint(&mut hints, object.get(key));
        }
        if let Some(capabilities) = object.get("capabilities").and_then(Value::as_object) {
            push_hint(&mut hints, capabilities.get("task"));
        }
        for key in ["input_modalities", "output_modalities", "modalities"] {
            push_hint(&mut hints, object.get(key));
        }
    }
    hints
}

/// 这条记录能不能用于 Chat Completions？
pub fn classify_model(entry: &Value) -> ModelClassification {
    let id = entry
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    if id.is_empty() {
        return ModelClassification {
            id,
            chat: false,
            reason: "空 id".to_string(),
        };
    }
    let hints = metadata_hints(entry);
    if let Some(hit) = hints
        .iter()
        .find(|hint| CHAT_TASK_TYPES.contains(&hint.as_str()))
    {
        return ModelClassification {
            id,
            chat: true,
            reason: format!("元数据标为 {hit}"),
        };
    }
    if let Some(hit) = hints
        .iter()
        .find(|hint| NON_CHAT_TASK_TYPES.contains(&hint.as_str()))
    {
        return ModelClassification {
            id,
            chat: false,
            reason: format!("元数据标为 {hit}"),
        };
    }
    // 明确给了 output_modalities 且完全不含 text → 不是聊天模型。
    if let Some(outputs) = entry.get("output_modalities").and_then(Value::as_array) {
        let listed: Vec<String> = outputs
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.to_ascii_lowercase())
            .collect();
        if !listed.is_empty() && !listed.iter().any(|value| value == "text") {
            return ModelClassification {
                id,
                chat: false,
                reason: format!("输出模态 {}", listed.join("/")),
            };
        }
    }
    // 没有可靠元数据：只按已知非聊天标识剔除。
    if let Some(hit) = segments(&id)
        .into_iter()
        .find(|part| NON_CHAT_SEGMENTS.contains(&part.as_str()))
    {
        return ModelClassification {
            id,
            chat: false,
            reason: format!("模型名含 {hit}"),
        };
    }
    ModelClassification {
        id,
        chat: true,
        reason: "未提供分类元数据，保留待验证".to_string(),
    }
}

/// 解析一次 `/models` 响应体。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedModels {
    /// 可用于聊天的模型（含"未验证"）。
    pub chat: Vec<ModelOption>,
    /// 被剔除的非聊天模型（`(id, 原因)`）。
    pub dropped: Vec<(String, String)>,
    /// 服务端返回的条目总数。
    pub raw: usize,
}

impl ParsedModels {
    /// 其中有多少条是"元数据未确认、按名字保守保留"的（界面标记"未验证"）。
    pub fn unverified_count(&self) -> usize {
        self.chat
            .iter()
            .filter(|option| option.source == ModelSource::Unverified)
            .count()
    }
}

/// 解析 `/models` 的响应体（兼容 `{data:[…]}` / `{models:[…]}` / 直接数组）。
pub fn parse_model_response(json: &Value) -> ParsedModels {
    let list = if let Some(items) = json.as_array() {
        items.clone()
    } else if let Some(items) = json.get("data").and_then(Value::as_array) {
        items.clone()
    } else if let Some(items) = json.get("models").and_then(Value::as_array) {
        items.clone()
    } else {
        Vec::new()
    };
    let mut parsed = ParsedModels {
        raw: list.len(),
        ..ParsedModels::default()
    };
    let mut seen = std::collections::BTreeSet::new();
    for item in list {
        let entry = if item.is_object() {
            item.clone()
        } else {
            Value::String(item.as_str().unwrap_or_default().to_string())
        };
        let verdict = classify_model(&entry);
        if verdict.id.is_empty() || !seen.insert(verdict.id.clone()) {
            continue;
        }
        if !verdict.chat {
            parsed.dropped.push((verdict.id, verdict.reason));
            continue;
        }
        // 元数据里没有类型信号时，`classify_model` 的 reason 以「未提供分类元数据」开头：
        // 这类模型照样可选，但要标成"未验证"，不能凭名字断言能力。
        let source = if verdict.reason.starts_with("未提供分类元数据")
            || verdict.reason == "默认保留"
        {
            ModelSource::Unverified
        } else {
            ModelSource::Discovered
        };
        parsed.chat.push(ModelOption {
            id: verdict.id,
            source,
            context_window: read_u64(
                &entry,
                &["context_window", "contextWindow", "max_context_length"],
            ),
            max_output_tokens: read_u64(
                &entry,
                &[
                    "max_output_tokens",
                    "maxOutputTokens",
                    "max_completion_tokens",
                ],
            ),
        });
    }
    parsed
}

fn read_u64(entry: &Value, keys: &[&str]) -> Option<u64> {
    let object = entry.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key) {
            if let Some(number) = value.as_u64() {
                return Some(number);
            }
            if let Some(number) = value.as_f64() {
                if number.is_finite() && number > 0.0 {
                    return Some(number as u64);
                }
            }
        }
    }
    None
}

/// 一次模型列表查询的结果（成功或失败都带可读说明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFetch {
    pub ok: bool,
    /// 是否来自缓存。
    pub cached: bool,
    /// 是否返回了上一次成功的旧列表（本次请求失败）。
    pub stale: bool,
    /// 可用于聊天的模型。
    pub models: Vec<ModelOption>,
    /// 被剔除的非聊天模型。
    pub dropped: Vec<(String, String)>,
    /// 服务端返回的条目总数（0 说明端点没给目录）。
    pub raw: usize,
    pub error: Option<String>,
    pub at_ms: u64,
}

impl ModelFetch {
    /// 给用户看的一句话（含来源与计数）。
    pub fn summary(&self) -> String {
        if !self.ok {
            let reason = self.error.clone().unwrap_or_else(|| "未知原因".to_string());
            if self.stale {
                return format!(
                    "获取模型列表失败：{reason}（仍保留上一次的 {} 个候选；可手填模型名）",
                    self.models.len()
                );
            }
            return format!("获取模型列表失败：{reason}（仍可手填模型名）");
        }
        let source = if self.cached {
            "缓存".to_string()
        } else if self.stale {
            "上一次结果".to_string()
        } else {
            "在线目录".to_string()
        };
        let unverified = self
            .models
            .iter()
            .filter(|option| option.source == ModelSource::Unverified)
            .count();
        let mut text = format!("{source}：{} 个可用聊天模型", self.models.len());
        if !self.dropped.is_empty() {
            text.push_str(&format!("（已排除 {} 个非聊天模型）", self.dropped.len()));
        }
        if unverified > 0 {
            text.push_str(&format!("，其中 {unverified} 个未验证"));
        }
        text
    }

    /// 下拉里显示用的候选（全部聊天模型）。
    pub fn ids(&self) -> Vec<String> {
        self.models.iter().map(|option| option.id.clone()).collect()
    }
}

/// 缓存中的一个条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheEntry {
    at_ms: u64,
    provider_id: String,
    base_url: String,
    /// Key 的短指纹：只用于判断"还是不是同一个账号"，不存明文。
    key_fingerprint: String,
    models: Vec<ModelOption>,
    dropped: Vec<(String, String)>,
    raw: usize,
}

/// 模型目录缓存（内存 + 可选落盘；落盘里**绝不**含明文 Key）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCache {
    #[serde(default)]
    entries: BTreeMap<String, CacheEntry>,
    #[serde(skip)]
    path: Option<std::path::PathBuf>,
}

fn cache_key(provider_id: &str, base_url: &str, credential: &str) -> String {
    format!("{provider_id}|{base_url}|{credential}")
}

/// Key 的短指纹：长度 + 简单散列，只用于"是不是同一个账号"的判断。
pub fn fingerprint(api_key: &str) -> String {
    if api_key.is_empty() {
        return String::new();
    }
    let mut hash: u32 = 0;
    for byte in api_key.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*byte as u32);
    }
    format!("{}:{hash:08x}", api_key.len())
}

impl ModelCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 绑定落盘路径并读取已有缓存（读不出来就当空缓存，绝不影响设置）。
    pub fn with_path(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let mut cache: ModelCache = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        cache.path = Some(path);
        cache
    }

    /// 落盘（失败只记 stderr：缓存坏了不该影响设置保存）。
    pub fn persist(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        if let Err(error) = std::fs::write(path, text) {
            eprintln!("[model-cache] 写入失败：{error}");
        }
    }

    /// 取某个提供商的缓存（仍在 TTL 内、且凭据未变）。
    fn get(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Option<&CacheEntry> {
        let entry = self.for_credentials(provider_id, base_url, api_key)?;
        if now_ms.saturating_sub(entry.at_ms) >= ttl_ms {
            return None;
        }
        if entry.key_fingerprint != fingerprint(api_key) {
            return None;
        }
        Some(entry)
    }

    /// 最近一次（无论是否过期）的缓存：请求失败时用来保留候选。
    #[cfg(test)]
    fn last(&self, provider_id: &str, base_url: &str) -> Option<&CacheEntry> {
        self.entries
            .values()
            .filter(|entry| {
                entry.provider_id == provider_id
                    && entry.base_url.trim_end_matches('/') == base_url.trim_end_matches('/')
            })
            .max_by_key(|entry| entry.at_ms)
    }

    fn put(&mut self, provider_id: &str, base_url: &str, entry: CacheEntry) {
        self.entries.insert(
            cache_key(provider_id, base_url, &entry.key_fingerprint),
            entry,
        );
    }

    fn for_credentials(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: &str,
    ) -> Option<&CacheEntry> {
        let credential = fingerprint(api_key);
        self.entries
            .values()
            .filter(|entry| {
                entry.provider_id == provider_id
                    && entry.base_url.trim().trim_end_matches('/')
                        == base_url.trim().trim_end_matches('/')
                    && entry.key_fingerprint == credential
            })
            .max_by_key(|entry| entry.at_ms)
    }

    /// 恢复目录时同时校验提供商、端点和账户；兼容旧缓存条目的字段布局。
    pub fn models_for(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: &str,
    ) -> Option<&[ModelOption]> {
        self.for_credentials(provider_id, base_url, api_key)
            .map(|entry| entry.models.as_slice())
    }

    pub fn invalidate(&mut self, provider_id: &str, base_url: &str) {
        self.entries.retain(|_, entry| {
            entry.provider_id != provider_id
                || entry.base_url.trim_end_matches('/') != base_url.trim_end_matches('/')
        });
    }

    /// 缓存的条目数（测试用）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 查询一个提供商的在线模型目录。
///
/// `api_key` 必须是**该提供商**的 Key（调用方负责按提供商取，绝不借用）。
/// `force = true` 跳过缓存（用户点了"获取模型"或改了 Key）。
pub async fn fetch_models(
    provider: &ProviderConfig,
    api_key: &str,
    cache: &mut ModelCache,
    force: bool,
) -> ModelFetch {
    let base_url = provider.base_url_or_default();
    let now = now_ms();
    if !force {
        if let Some(entry) = cache.get(&provider.id, &base_url, api_key, now, MODEL_LIST_TTL_MS) {
            return ModelFetch {
                ok: true,
                cached: true,
                stale: false,
                models: entry.models.clone(),
                dropped: entry.dropped.clone(),
                raw: entry.raw,
                error: None,
                at_ms: entry.at_ms,
            };
        }
    }

    let stale_snapshot = cache
        .for_credentials(&provider.id, &base_url, api_key)
        .cloned();
    let fallback = |error: String| ModelFetch {
        ok: false,
        cached: stale_snapshot.is_some(),
        stale: stale_snapshot.is_some(),
        models: stale_snapshot
            .as_ref()
            .map(|entry| entry.models.clone())
            .unwrap_or_default(),
        dropped: stale_snapshot
            .as_ref()
            .map(|entry| entry.dropped.clone())
            .unwrap_or_default(),
        raw: stale_snapshot.as_ref().map(|entry| entry.raw).unwrap_or(0),
        error: Some(error),
        at_ms: stale_snapshot
            .as_ref()
            .map(|entry| entry.at_ms)
            .unwrap_or(0),
    };

    if crate::config::get_provider(&provider.id)
        .map(|info| info.api_key_required)
        .unwrap_or(true)
        && api_key.trim().is_empty()
    {
        return fallback("需要该提供商的 API Key（填好再获取列表）".to_string());
    }

    let profile = ProviderProfile {
        provider_id: provider.id.clone(),
        capability: provider.capability(),
        base_url: base_url.clone(),
        model: if provider.model.trim().is_empty() {
            "models".to_string()
        } else {
            provider.model.clone()
        },
        thinking: provider.thinking,
        reasoning_effort: provider.reasoning_effort,
        model_context_window: None,
    };
    let client = match LlmClient::new(profile, api_key, 0.3, 15_000) {
        Ok(client) => client,
        Err(error) => return fallback(error.to_string()),
    };

    let json = match client.list_models_json().await {
        Ok(json) => json,
        Err(error) => return fallback(describe_llm_error(&error)),
    };
    let parsed = parse_model_response(&json);
    if parsed.chat.is_empty() {
        let reason = if parsed.raw == 0 {
            "该端点没有返回任何模型（可能不支持 /models；可手填模型名）".to_string()
        } else {
            "返回的模型都不是聊天模型（只有 ASR/TTS 等）".to_string()
        };
        return fallback(reason);
    }

    // 网络并行，缓存提交串行：迟到的旧账户响应不能覆盖新账户的目录。
    static CACHE_COMMIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _commit = CACHE_COMMIT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(path) = cache.path.clone() {
        *cache = ModelCache::with_path(path);
    }
    cache.put(
        &provider.id,
        &base_url,
        CacheEntry {
            at_ms: now,
            provider_id: provider.id.clone(),
            base_url: base_url.clone(),
            key_fingerprint: fingerprint(api_key),
            models: parsed.chat.clone(),
            dropped: parsed.dropped.clone(),
            raw: parsed.raw,
        },
    );
    cache.persist();
    ModelFetch {
        ok: true,
        cached: false,
        stale: false,
        models: parsed.chat,
        dropped: parsed.dropped,
        raw: parsed.raw,
        error: None,
        at_ms: now,
    }
}

/// 把 LLM 错误翻译成用户能据此行动的说法（区分授权/模型/网络/不支持）。
pub fn describe_llm_error(error: &LlmError) -> String {
    match error {
        LlmError::Status { status, body } => match *status {
            401 | 403 => format!("认证失败（HTTP {status}）：Key 无效或没有该端点权限"),
            404 => "端点不存在（HTTP 404）：检查地址是否带 /v1".to_string(),
            429 => "被限流（HTTP 429）：稍后再试".to_string(),
            400 => format!("请求被拒绝（HTTP 400）：{body}"),
            500..=599 => format!("服务端错误（HTTP {status}）"),
            other => format!("HTTP {other}：{body}"),
        },
        LlmError::Connect(detail) => format!("连接失败：{detail}"),
        LlmError::Timeout(ms) => format!("请求超时（{ms} ms）"),
        LlmError::Aborted => "已中断".to_string(),
        LlmError::Other(detail) => detail.clone(),
    }
}

/// "测试连接"的结果分类。
///
/// 刻意分成几种**可行动**的结论：用户看到"认证失败"要换 Key，
/// 看到"模型不存在"要换模型名，看到"网络不通"要查代理/地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 真的发出了一次对话请求并拿到正常响应（模型可聊天）。
    ChatOk {
        model: String,
        ms: u64,
        /// 是否返回了 reasoning（用于如实说明思考能力）。
        reasoning_chars: usize,
    },
    /// 认证失败。
    Unauthorized(String),
    /// 模型不存在 / 不被该端点接受。
    ModelRejected(String),
    /// 网络/端点不可达。
    Network(String),
    /// 其它失败（限流、服务端错误、协议异常）。
    Other(String),
}

impl ProbeOutcome {
    pub fn ok(&self) -> bool {
        matches!(self, Self::ChatOk { .. })
    }

    /// 给用户看的一句话（含"测试连接 ≠ 获取模型列表"的边界）。
    pub fn message(&self) -> String {
        match self {
            Self::ChatOk {
                model,
                ms,
                reasoning_chars,
            } => {
                let mut text = format!("连接成功：{model} 完成了 1 次对话请求（{ms} ms）");
                if *reasoning_chars > 0 {
                    text.push_str(&format!("；返回 {reasoning_chars} 字思考内容"));
                }
                text.push_str("。注意：这只是最小对话验证，不代表该模型能完成所有任务。");
                text
            }
            Self::Unauthorized(detail) => format!("认证失败：{detail}"),
            Self::ModelRejected(detail) => format!("模型不可用：{detail}"),
            Self::Network(detail) => format!("网络/端点不可达：{detail}"),
            Self::Other(detail) => format!("测试失败：{detail}"),
        }
    }

    /// 结果分类码（界面配色用）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::ChatOk { .. } => "ok",
            Self::Unauthorized(_) => "auth",
            Self::ModelRejected(_) => "model",
            Self::Network(_) => "network",
            Self::Other(_) => "other",
        }
    }
}

/// 真的发一次最小对话请求来测试端点与模型。
///
/// 为什么不用 `/models` 的成败当"连接测试"：那只能说明目录接口能访问，
/// 模型是否存在、能否在该端点完成对话是另一回事（严格端点会对不存在的模型 404）。
/// 这里只发一条最少的 user 消息，`max_tokens` 很小，不产生费用上的惊喜。
pub async fn probe_chat(profile: &ProviderProfile, api_key: &str, timeout_ms: u64) -> ProbeOutcome {
    let client = match LlmClient::new(profile.clone(), api_key, 0.3, timeout_ms) {
        Ok(client) => client,
        Err(error) => return ProbeOutcome::Other(error.to_string()),
    };
    let cancel = crate::cancel::CancelToken::new();
    let started = std::time::Instant::now();
    let messages = [ChatMessage::user("ping")];
    let result = client.chat(&messages, &[], &cancel, |_| {}, |_| {}).await;
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(result) => {
            if result.interrupted || result.incomplete {
                return ProbeOutcome::Other("响应不完整（端点提前断流）".to_string());
            }
            ProbeOutcome::ChatOk {
                model: profile.model.clone(),
                ms,
                reasoning_chars: result.reasoning.chars().count(),
            }
        }
        Err(LlmError::Status { status, body }) => match status {
            401 | 403 => ProbeOutcome::Unauthorized(format!("HTTP {status}（Key 无效或权限不足）")),
            404 => ProbeOutcome::ModelRejected(format!(
                "HTTP 404（模型或端点不存在）：{}",
                truncate(&body, 160)
            )),
            400 => ProbeOutcome::ModelRejected(format!("HTTP 400：{}", truncate(&body, 160))),
            429 => ProbeOutcome::Other("被限流（HTTP 429），稍后再试".to_string()),
            other => ProbeOutcome::Other(format!("HTTP {other}：{}", truncate(&body, 160))),
        },
        Err(error) => ProbeOutcome::Network(describe_llm_error(&error)),
    }
}

/// 端点是否要求 Key（本地 mock / llama.cpp 不需要）。
pub fn provider_requires_key(provider_id: &str) -> bool {
    crate::config::get_provider(provider_id)
        .map(|info| info.api_key_required)
        .unwrap_or(true)
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let head: String = value.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> Value {
        serde_json::json!({ "id": id })
    }

    #[test]
    fn metadata_wins_over_name_guessing() {
        let chat_meta = serde_json::json!({
            "id": "mimo-v2.5-tts",
            "task": "text-generation"
        });
        let verdict = classify_model(&chat_meta);
        assert!(verdict.chat, "元数据说能聊天时不被名字误杀");

        let speech_meta = serde_json::json!({
            "id": "mimo-v2.6-flash",
            "task": "text-to-speech"
        });
        assert!(!classify_model(&speech_meta).chat);
    }

    #[test]
    fn known_non_chat_names_are_dropped_but_segments_matter() {
        assert!(!classify_model(&entry("mimo-v2.5-asr")).chat);
        assert!(!classify_model(&entry("mimo-v2.5-tts")).chat);
        assert!(!classify_model(&entry("text-embedding-3-small")).chat);
        // `attest` 里没有独立的 `tts` 段，不能被误杀。
        assert!(classify_model(&entry("attest-model")).chat);
        // 认不出来的新模型照样保留（只标未验证）。
        assert!(classify_model(&entry("some-new-chat-model")).chat);
    }

    #[test]
    fn output_modalities_without_text_are_not_chat() {
        let value = serde_json::json!({
            "id": "image-gen",
            "output_modalities": ["image"]
        });
        assert!(!classify_model(&value).chat);
        let with_text = serde_json::json!({
            "id": "multimodal",
            "output_modalities": ["text", "image"]
        });
        assert!(classify_model(&with_text).chat);
    }

    #[test]
    fn parse_response_reads_metadata_and_marks_unverified() {
        let json = serde_json::json!({
            "data": [
                { "id": "mimo-v2.6-flash", "context_window": 1_000_000, "max_output_tokens": 131_072 },
                { "id": "mimo-v2.5-tts" },
                { "id": "mystery-model" },
                { "id": "mimo-v2.6-flash" }
            ]
        });
        let parsed = parse_model_response(&json);
        assert_eq!(parsed.raw, 4);
        assert_eq!(parsed.chat.len(), 2, "去重 + 剔除 TTS");
        assert_eq!(parsed.dropped.len(), 1);
        let flash = parsed
            .chat
            .iter()
            .find(|option| option.id == "mimo-v2.6-flash")
            .unwrap();
        assert_eq!(flash.context_window, Some(1_000_000));
        assert_eq!(flash.max_output_tokens, Some(131_072));
        // 这个条目没有类型元数据：按"宁可多留、不可误删"保留，但**不能**
        // 凭模型名断言它一定能聊天，所以标「未验证」而不是「已验证」。
        assert_eq!(flash.source, ModelSource::Unverified);
        let mystery = parsed
            .chat
            .iter()
            .find(|option| option.id == "mystery-model")
            .unwrap();
        assert_eq!(mystery.source, ModelSource::Unverified);
        assert_eq!(parsed.unverified_count(), 2);
    }

    #[test]
    fn empty_or_unsupported_response_is_reported_clearly() {
        let parsed = parse_model_response(&serde_json::json!({ "object": "list" }));
        assert!(parsed.chat.is_empty());
        assert_eq!(parsed.raw, 0);
        let fetch = ModelFetch {
            ok: false,
            cached: false,
            stale: false,
            models: Vec::new(),
            dropped: Vec::new(),
            raw: 0,
            error: Some("该端点没有返回任何模型（可能不支持 /models；可手填模型名）".to_string()),
            at_ms: 0,
        };
        assert!(fetch.summary().contains("可手填"));
    }

    #[test]
    fn cache_expires_and_is_bound_to_credentials() {
        let mut cache = ModelCache::new();
        let now = 1_000_000u64;
        cache.put(
            "mimo",
            "https://api.example/v1",
            CacheEntry {
                at_ms: now,
                provider_id: "mimo".to_string(),
                base_url: "https://api.example/v1".to_string(),
                key_fingerprint: fingerprint("sk-key-a"),
                models: vec![ModelOption::manual("m1")],
                dropped: Vec::new(),
                raw: 1,
            },
        );
        // 同一把 Key、TTL 内命中。
        assert!(cache
            .get(
                "mimo",
                "https://api.example/v1",
                "sk-key-a",
                now + 10,
                MODEL_LIST_TTL_MS
            )
            .is_some());
        // 超过 TTL 不命中。
        assert!(cache
            .get(
                "mimo",
                "https://api.example/v1",
                "sk-key-a",
                now + MODEL_LIST_TTL_MS + 1,
                MODEL_LIST_TTL_MS
            )
            .is_none());
        // 换了 Key 不能复用旧账号的目录。
        assert!(cache
            .get(
                "mimo",
                "https://api.example/v1",
                "sk-key-b",
                now + 10,
                MODEL_LIST_TTL_MS
            )
            .is_none());
        // 不同地址是两套目录。
        assert!(cache
            .get(
                "mimo",
                "https://other/v1",
                "sk-key-a",
                now + 10,
                MODEL_LIST_TTL_MS
            )
            .is_none());
        // 但"最近一次"仍可用于失败时保留候选。
        assert!(cache.last("mimo", "https://api.example/v1").is_some());
    }

    #[test]
    fn cache_persists_without_plaintext_keys() {
        let dir = unique_root("model-cache");
        let path = dir.join("model-cache.json");
        let mut cache = ModelCache::with_path(&path);
        cache.put(
            "deepseek",
            "https://api.deepseek.com/v1",
            CacheEntry {
                at_ms: 5,
                provider_id: "deepseek".to_string(),
                base_url: "https://api.deepseek.com/v1".to_string(),
                key_fingerprint: fingerprint("sk-super-secret-value"),
                models: vec![ModelOption::manual("deepseek-flash")],
                dropped: Vec::new(),
                raw: 1,
            },
        );
        cache.persist();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("sk-super-secret-value"),
            "缓存里不能有明文 Key"
        );
        let reloaded = ModelCache::with_path(&path);
        assert_eq!(reloaded.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_outcome_messages_explain_the_boundary() {
        let ok = ProbeOutcome::ChatOk {
            model: "m".to_string(),
            ms: 12,
            reasoning_chars: 3,
        };
        assert!(ok.ok());
        assert!(ok.message().contains("不代表该模型能完成所有任务"));
        assert!(!ProbeOutcome::Unauthorized("HTTP 401".to_string()).ok());
        assert_eq!(ProbeOutcome::ModelRejected("x".to_string()).code(), "model");
    }

    fn unique_root(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("{prefix}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
