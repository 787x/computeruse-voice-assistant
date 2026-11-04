//! OpenAI 兼容 Chat Completions 客户端（流式 SSE）。
//!
//! 移植自 `packages/agent/{llm,ll-params}.mjs`，只保留 PR5 闭环必需的协议行为：
//!  - 提供商专属推理字段（MiMo 的 `thinking` 开关 / DeepSeek 的 `reasoning_effort`）
//!    由**能力标记**决定，绝不按模型名猜；标准兼容端点一个专属字段都不发。
//!  - `reasoning_content` 与可见回复分离：思考内容只用于协议回传，不进用户可见文本。
//!  - 流式 `tool_calls` 按 `index` 归并（id / name / arguments 都是分片到达的）。
//!  - 响应不完整（截断 / 无结束信号 / 错误事件 / finish_reason 不允许）时，
//!    工具调用一律标记为不可执行，绝不拿半截参数去点鼠标、敲键盘。

use std::collections::BTreeMap;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{ReasoningEffort, ThinkingLevel};

/// 端点协议能力。判定依据是注册表声明或用户显式 hint，**不是**模型名字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Xiaomi MiMo：Chat Completions 只有 `thinking: {type}`，没有强度档位。
    Mimo,
    /// DeepSeek：只用 `reasoning_effort` 一个字段表达开关与强度。
    DeepSeek,
    /// 其它 OpenAI 兼容端点：不发送任何提供商专属字段。
    Plain,
}

impl Capability {
    /// 该提供商是否要求把 `reasoning_content` 原样回传（漏了会 400）。
    pub fn requires_reasoning_echo(self) -> bool {
        matches!(self, Self::Mimo | Self::DeepSeek)
    }
}

/// 一次请求的端点档案（**回合快照**：一整轮用同一套）。
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    pub provider_id: String,
    pub capability: Capability,
    pub base_url: String,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub reasoning_effort: ReasoningEffort,
    pub model_context_window: Option<u64>,
}

/// 组装后的请求字段（`fields` 合进请求体，`temperature` 为 `None` 表示别发）。
#[derive(Debug, Clone, Default)]
pub struct RequestFields {
    pub fields: serde_json::Map<String, Value>,
    pub temperature: Option<f64>,
    pub thinking_active: bool,
    /// 官方会静默忽略/强制覆盖的采样参数（用于界面如实说明）。
    pub ignored_sampling: Vec<&'static str>,
}

fn clamp_number(value: f64, min: f64, max: f64) -> Option<f64> {
    if !value.is_finite() {
        return None;
    }
    Some(value.clamp(min, max))
}

/// MiMo 思考模式下官方强制覆盖采样参数的型号系列（`mimo-v2.6*`）。
fn mimo_forces_sampling(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower == "mimo-v2.6" || lower.starts_with("mimo-v2.6-") || lower.starts_with("mimo-v2.6.")
}

/// 由档案 + 采样参数算出要发的推理字段。与 `ll-params.mjs` 的 `buildRequestFields` 对齐。
pub fn build_request_fields(profile: &ProviderProfile, temperature: f64) -> RequestFields {
    let mut out = RequestFields::default();
    match profile.capability {
        Capability::Mimo => {
            // 「默认」不传 thinking → 服务端决定。MiMo V2.6 官方默认**开**思考，
            // 所以把 default 当成 disabled 是错的。
            out.thinking_active = profile.thinking != ThinkingLevel::Disabled;
            if profile.thinking != ThinkingLevel::Default {
                out.fields.insert(
                    "thinking".to_string(),
                    json!({ "type": profile.thinking.as_str() }),
                );
            }
            if out.thinking_active && mimo_forces_sampling(&profile.model) {
                // 传了也会被强制覆盖 → 不发，并如实记录。
                out.ignored_sampling = vec!["temperature", "top_p"];
            } else {
                // MiMo 官方区间：temperature [0, 1.5]
                out.temperature = clamp_number(temperature, 0.0, 1.5);
            }
        }
        Capability::DeepSeek => {
            // 官方协议：省略 thinking 时由 reasoning_effort 决定模式；
            // 两个都发只会引入优先级歧义，所以这里**只发一个字段**。
            match profile.reasoning_effort {
                ReasoningEffort::Default => {
                    out.thinking_active = true;
                    out.ignored_sampling = vec!["temperature", "top_p"];
                }
                ReasoningEffort::None => {
                    out.fields
                        .insert("reasoning_effort".to_string(), json!("none"));
                    out.thinking_active = false;
                    out.temperature = clamp_number(temperature, 0.0, 2.0);
                }
                level => {
                    out.fields
                        .insert("reasoning_effort".to_string(), json!(level.as_str()));
                    out.thinking_active = true;
                    // 思考模式不支持 temperature；发个无效的不如不发。
                    out.ignored_sampling = vec!["temperature", "top_p"];
                }
            }
        }
        Capability::Plain => {
            out.thinking_active = false;
            out.temperature = clamp_number(temperature, 0.0, 2.0);
        }
    }
    out
}

/// 角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// 消息内容：纯文本，或 OpenAI 多段内容（截图附件必须走数组）。
///
/// 会话持久化时按"可恢复且有界"的形态保存：
///  - 文本原样保存；
///  - 多段内容里的截图**不存 base64**（会撑爆会话文件），只保留说明文字，
///    并标明"附件未保留"。这不会把消息结构改坏：下次请求时它只是没有图片的文本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Content {
    Text(String),
    Parts(Vec<Value>),
}

impl Content {
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Text(text) => Value::String(text.clone()),
            Self::Parts(parts) => Value::Array(parts.clone()),
        }
    }
}

/// 一个工具调用（模型产出）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 原始 JSON 参数文本（可能不合法——见 `valid`）。
    pub arguments: String,
    /// `false` 表示参数不是合法 JSON 或缺函数名，调用方**不得**执行。
    pub valid: bool,
    pub invalid_reason: Option<String>,
}

/// 对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: Role,
    pub content: Content,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// 思考内容。只用于协议回传，绝不进用户可见回复。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 程序生成的内部提示（"参数不完整，请重新生成"）。**绝不发往模型**的标记位，
    /// 但内容本身仍然发出去（模型要看得到这条提示）。
    #[serde(default)]
    pub internal: bool,
    /// 产生这条消息的提供商 id（`None` = 未知/用户消息）。
    ///
    /// 用来解决跨提供商的私有字段问题：MiMo / DeepSeek 的 `reasoning_content`
    /// 是各自端点的私有协议上下文，发给另一家可能直接 400。历史里保留普通对话
    /// 与完整的工具调用链，只在提供商**不同**时剥掉 `reasoning_content`。
    #[serde(default)]
    pub provider_id: Option<String>,
}

impl ChatMessage {
    /// 会话持久化用的有界副本：截图 base64 不进会话文件。
    ///
    /// 只处理 `Content::Parts`：保留说明文字，把每个 `image_url` 段换成一条
    /// 说明（"附件未保留"）。**结构仍是数组**，所以恢复后依然是一条合法的
    /// 多段消息，不会让后续请求或压缩逻辑把它误当成普通用户文本。
    pub fn for_storage(&self) -> Self {
        let mut copy = self.clone();
        if let Content::Parts(parts) = &self.content {
            let mut kept = Vec::with_capacity(parts.len());
            let mut dropped = 0usize;
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("image_url") {
                    dropped += 1;
                    continue;
                }
                kept.push(part.clone());
            }
            if dropped > 0 {
                kept.push(json!({
                    "type": "text",
                    "text": format!("（这条消息原本附带 {dropped} 张截图；恢复会话时不保留图片，仅保留文字说明）")
                }));
            }
            copy.content = Content::Parts(kept);
        }
        copy
    }

    /// 这条消息里有没有附件（截图）。
    pub fn has_attachment(&self) -> bool {
        matches!(&self.content, Content::Parts(parts)
            if parts.iter().any(|part| part.get("type").and_then(Value::as_str) == Some("image_url")))
    }

    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Content::Text(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            internal: false,
            provider_id: None,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::text(Role::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    pub fn internal_note(text: impl Into<String>) -> Self {
        let mut message = Self::text(Role::User, text);
        message.internal = true;
        message
    }

    pub fn tool_result(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Content::Text(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            internal: false,
            provider_id: None,
        }
    }

    /// 截图附件：`content` 必须是数组，协议只允许这个形态。
    pub fn image_attachment(text: impl Into<String>, media_type: &str, base64: &str) -> Self {
        let mut message = Self {
            role: Role::User,
            content: Content::Parts(vec![
                json!({ "type": "text", "text": text.into() }),
                json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{base64}") }
                }),
            ]),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            internal: true,
            provider_id: None,
        };
        // 附件属于工具调用片段，不是新的用户回合。
        message.internal = true;
        message
    }

    /// 序列化成请求体里的消息对象。内部标记与本地字段一律剥掉。
    ///
    /// `current_provider` 是本次请求要打的提供商。历史里来自**别的**提供商的
    /// `reasoning_content` 不发送——它是那家的私有协议上下文，发给这家可能直接 400。
    /// 普通正文、`tool_calls` 与 `tool` 结果都照常保留，所以对话记忆与工具链不受影响。
    pub fn to_request_value(&self, echo_reasoning: bool, current_provider: &str) -> Value {
        let from_other_provider = match (&self.provider_id, current_provider) {
            (Some(origin), current) => !current.is_empty() && origin != current,
            // 没标记来源的（用户消息、旧历史）按本家处理，保持原样。
            (None, _) => false,
        };
        let mut object = serde_json::Map::new();
        object.insert("role".to_string(), json!(self.role.as_str()));
        if self.role == Role::Assistant && !self.tool_calls.is_empty() {
            // 有 tool_calls 时 content 允许为 null，但字段必须存在。
            let text = self.content.as_text();
            object.insert(
                "content".to_string(),
                if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                },
            );
            object.insert(
                "tool_calls".to_string(),
                Value::Array(
                    self.tool_calls
                        .iter()
                        .filter(|call| call.valid)
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.name, "arguments": call.arguments }
                            })
                        })
                        .collect(),
                ),
            );
        } else {
            object.insert("content".to_string(), self.content.to_value());
        }
        if let Some(id) = &self.tool_call_id {
            object.insert("tool_call_id".to_string(), json!(id));
        }
        if echo_reasoning && !from_other_provider {
            if let Some(reasoning) = &self.reasoning_content {
                // 空串与缺省在协议上不是一回事：没想过就别回一个空字段。
                if !reasoning.is_empty() {
                    object.insert("reasoning_content".to_string(), json!(reasoning));
                }
            }
        }
        Value::Object(object)
    }
}

/// 由一次响应收口出 assistant 消息（`assistantMessageFrom` 的对应实现）。
///
/// `provider_id` 记下这条消息是**哪家**端点产生的，供后续切换提供商时
/// 判断要不要剥掉私有 `reasoning_content`。
pub fn assistant_message_from(
    result: &ChatResult,
    echo_reasoning: bool,
    provider_id: &str,
) -> ChatMessage {
    let mut message = ChatMessage {
        role: Role::Assistant,
        content: Content::Text(result.text.clone()),
        tool_calls: result
            .tool_calls
            .iter()
            .filter(|call| call.valid)
            .cloned()
            .collect(),
        tool_call_id: None,
        reasoning_content: None,
        internal: false,
        provider_id: Some(provider_id.to_string()),
    };
    if echo_reasoning && !result.reasoning.is_empty() {
        message.reasoning_content = Some(result.reasoning.clone());
    }
    message
}

/// 一次请求的结果。
#[derive(Debug, Clone, Default)]
pub struct ChatResult {
    pub text: String,
    pub reasoning: String,
    /// 可执行的调用（只有响应完整且 `finish_reason == "tool_calls"` 才非空）。
    pub tool_calls: Vec<ToolCall>,
    /// 残缺/不可执行的调用（参数坏、被截断、协议不一致）。
    pub broken_tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: Option<Value>,
    /// 本次响应不能作为可执行指令的依据（Agent 据此一个工具都不执行）。
    pub tool_calls_incomplete: bool,
    pub interrupted: bool,
    pub incomplete: bool,
}

/// LLM 错误。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("无法连接模型服务：{0}")]
    Connect(String),
    #[error("模型服务返回 {status}：{body}")]
    Status { status: u16, body: String },
    #[error("模型响应超时（{0}ms），已中止")]
    Timeout(u64),
    #[error("已中断")]
    Aborted,
    #[error("请求失败：{0}")]
    Other(String),
}

/// 流式工具调用归并器。
///
/// OpenAI 协议里 `id` / `name` / `arguments` 都是分片到达的（arguments 常拆成几十片），
/// 按 `index` 归并并保留到达顺序（`BTreeMap` 保证 index 升序）。
#[derive(Debug, Default)]
struct ToolCallAssembler {
    calls: BTreeMap<i64, PartialCall>,
}

#[derive(Debug, Default, Clone)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAssembler {
    fn feed(&mut self, list: &Value) {
        let Some(items) = list.as_array() else { return };
        for part in items {
            if !part.is_object() {
                continue;
            }
            let index = part
                .get("index")
                .and_then(Value::as_i64)
                .unwrap_or(self.calls.len() as i64);
            let entry = self.calls.entry(index).or_default();
            if let Some(id) = part.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    entry.id = id.to_string();
                }
            }
            if let Some(function) = part.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    entry.name.push_str(name);
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(arguments);
                }
            }
        }
    }

    /// 收尾：把分片变成可执行判定。
    ///
    /// 参数不是合法 JSON、或函数名缺失的，标记 `valid: false` 而不是硬执行——
    /// 半截参数喂给 CUA（点击、键盘）比不调用危险得多。
    fn done(&self) -> Vec<ToolCall> {
        let mut out = Vec::with_capacity(self.calls.len());
        for (position, (_, call)) in self.calls.iter().enumerate() {
            let id = if call.id.is_empty() {
                format!("call_{position}")
            } else {
                call.id.clone()
            };
            let name = call.name.trim().to_string();
            let mut record = ToolCall {
                id,
                name,
                arguments: call.arguments.clone(),
                valid: true,
                invalid_reason: None,
            };
            if record.name.is_empty() {
                record.valid = false;
                record.invalid_reason = Some("缺少函数名".to_string());
            } else if record.arguments.trim().is_empty() {
                // 空参数按 {} 处理是各家 SDK 的通行做法（无参工具就是空对象）。
                record.arguments = "{}".to_string();
            } else if !is_json_object(&record.arguments) {
                record.valid = false;
                record.invalid_reason = Some("参数不是合法 JSON 对象".to_string());
            }
            out.push(record);
        }
        out
    }
}

fn is_json_object(raw: &str) -> bool {
    matches!(serde_json::from_str::<Value>(raw), Ok(Value::Object(_)))
}

/// 该 finish_reason 是否允许工具调用被视为可执行。
fn finish_allows_tool_execution(reason: &str) -> bool {
    // 只有 tool_calls / stop 是"模型明确结束本轮"。
    // length / content_filter / repetition_truncation / abort / cancel / error 一律不行。
    reason == "tool_calls" || reason == "stop"
}

/// 收口一次请求的结果（与 `llm.mjs` 的 `finalize` 语义一致）。
struct FinalizeInput {
    text: String,
    reasoning: String,
    assembler: ToolCallAssembler,
    finish_reason: String,
    usage: Option<Value>,
    aborted: bool,
    incomplete: bool,
    saw_error: bool,
}

fn finalize(input: FinalizeInput) -> ChatResult {
    let all = input.assembler.done();
    let invalid: Vec<ToolCall> = all.iter().filter(|c| !c.valid).cloned().collect();
    let reliably_complete = !input.aborted && !input.incomplete && !input.saw_error;
    let bad_finish =
        input.finish_reason.is_empty() || !finish_allows_tool_execution(&input.finish_reason);
    let truncated = !reliably_complete || bad_finish;
    let allows_tools = input.finish_reason == "tool_calls";

    let valid: Vec<ToolCall> = if reliably_complete && !truncated && allows_tools {
        all.iter().filter(|c| c.valid).cloned().collect()
    } else {
        Vec::new()
    };

    let mut broken = invalid;
    if truncated || (reliably_complete && !allows_tools && !all.is_empty()) {
        for call in all.iter().filter(|c| c.valid) {
            let mut call = call.clone();
            call.valid = false;
            call.invalid_reason = Some(match input.finish_reason.as_str() {
                "length" | "content_filter" | "repetition_truncation" => {
                    format!("响应被终止（finish_reason={}）", input.finish_reason)
                }
                _ => "响应未完整结束（缺少可靠结束信号或结束状态不允许执行）".to_string(),
            });
            broken.push(call);
        }
    }

    let finish_reason = if input.aborted && input.finish_reason == "stop" {
        "interrupted".to_string()
    } else if truncated && input.finish_reason == "stop" {
        "incomplete".to_string()
    } else {
        input.finish_reason.clone()
    };

    let tool_calls_incomplete = truncated
        || (input.finish_reason == "tool_calls" && valid.is_empty() && !broken.is_empty())
        || (reliably_complete && input.finish_reason != "tool_calls" && !all.is_empty())
        || (input.aborted && !all.is_empty());

    ChatResult {
        text: input.text,
        reasoning: input.reasoning,
        tool_calls: valid,
        broken_tool_calls: broken,
        finish_reason,
        usage: input.usage,
        tool_calls_incomplete,
        interrupted: input.aborted,
        incomplete: input.incomplete && !input.aborted,
    }
}

/// 客户端。字段全部是"此刻生效的端点快照"，Agent 每轮取一份克隆。
#[derive(Debug, Clone)]
pub struct LlmClient {
    pub profile: ProviderProfile,
    pub temperature: f64,
    pub timeout_ms: u64,
    http: reqwest::Client,
    api_key: String,
}

impl LlmClient {
    pub fn new(
        profile: ProviderProfile,
        api_key: impl Into<String>,
        temperature: f64,
        timeout_ms: u64,
    ) -> Result<Self, LlmError> {
        if profile.base_url.trim().is_empty() {
            return Err(LlmError::Other("模型地址为空".to_string()));
        }
        if profile.model.trim().is_empty() {
            return Err(LlmError::Other("模型名为空".to_string()));
        }
        let http = reqwest::Client::builder()
            .user_agent("cuava/0.1 (computer-use desktop)")
            .build()
            .map_err(|error| LlmError::Other(error.to_string()))?;
        Ok(Self {
            profile,
            temperature,
            timeout_ms,
            http,
            api_key: api_key.into(),
        })
    }

    /// 本次请求的提供商 id（用于给 assistant 消息打来源标记）。
    pub fn provider_id(&self) -> String {
        self.profile.provider_id.clone()
    }

    /// 组装请求体（导出给测试断言"界面测的就是实际发的"）。
    pub fn build_body(&self, messages: &[ChatMessage], tools: &[Value], stream: bool) -> Value {
        let echo = self.profile.capability.requires_reasoning_echo();
        let built = build_request_fields(&self.profile, self.temperature);
        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(self.profile.model));
        body.insert(
            "messages".to_string(),
            Value::Array(
                messages
                    .iter()
                    .map(|m| m.to_request_value(echo, &self.profile.provider_id))
                    .collect(),
            ),
        );
        body.insert("stream".to_string(), json!(stream));
        if let Some(temperature) = built.temperature {
            body.insert("temperature".to_string(), json!(temperature));
        }
        if !tools.is_empty() {
            // 空数组**不能**当 tools 发：多家实现会把 `tools: []` 当非法参数（400）。
            body.insert("tools".to_string(), Value::Array(tools.to_vec()));
            // MiMo 官方：非 auto 的 tool_choice 会被剥掉；DeepSeek 思考模式下
            // required/指定工具直接 400。所以只用 auto，两家都安全。
            body.insert("tool_choice".to_string(), json!("auto"));
        }
        for (key, value) in built.fields {
            body.insert(key, value);
        }
        Value::Object(body)
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.profile.base_url.trim_end_matches('/')
        )
    }

    /// 发一次流式对话请求。
    ///
    /// `on_delta` 只收到**可见正文**；思考内容永远走 `on_reasoning`（默认不接）。
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        signal: &crate::cancel::CancelToken,
        mut on_delta: impl FnMut(&str) + Send,
        mut on_reasoning: impl FnMut(&str) + Send,
    ) -> Result<ChatResult, LlmError> {
        let body = self.build_body(messages, tools, true);
        let mut request = self
            .http
            .post(self.endpoint())
            .header("content-type", "application/json")
            .json(&body);
        if !self.api_key.trim().is_empty() {
            request = request.header("authorization", format!("Bearer {}", self.api_key));
        }

        let timeout = std::time::Duration::from_millis(self.timeout_ms);
        let response = tokio::select! {
            result = request.send() => result,
            _ = signal.cancelled() => return Err(LlmError::Aborted),
            _ = tokio::time::sleep(timeout) => return Err(LlmError::Timeout(self.timeout_ms)),
        };

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if signal.is_cancelled() {
                    return Err(LlmError::Aborted);
                }
                return Err(LlmError::Connect(error.to_string()));
            }
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status,
                body: truncate(&body, 400),
            });
        }

        self.consume_stream(response, signal, &mut on_delta, &mut on_reasoning)
            .await
    }

    async fn consume_stream(
        &self,
        response: reqwest::Response,
        signal: &crate::cancel::CancelToken,
        on_delta: &mut impl FnMut(&str),
        on_reasoning: &mut impl FnMut(&str),
    ) -> Result<ChatResult, LlmError> {
        let mut stream = response.bytes_stream();
        /*
         * 网络分片是**字节**级的：一个中文字符（3 字节）或一个 `\r\n` 都可能被拆在
         * 两次 `bytes_stream()` 之间。所以这里按字节缓冲、按字节找事件边界，
         * 只在拿到一整段**完整 UTF-8** 之后才解码。
         *
         * 之前逐块 `String::from_utf8_lossy` 会在字符中间插入 U+FFFD，
         * 把中文正文和工具参数里的 JSON 一起毁掉。
         */
        let mut buffer: Vec<u8> = Vec::new();
        let mut assembler = ToolCallAssembler::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut finish_reason = "stop".to_string();
        let mut usage: Option<Value> = None;
        let mut saw_finish = false;
        let mut saw_done = false;
        let mut saw_error = false;
        let mut interrupted = false;

        'outer: loop {
            /*
             * 读超时：连接建立后如果服务端长时间不吐字节（思考卡住、中间层挂起），
             * 不能无限等下去。每收到一块就重新计时，所以持续输出的长推理
             * **不会**被误杀——它一直在重置这个计时器。
             */
            let idle = std::time::Duration::from_millis(self.timeout_ms);
            let chunk = tokio::select! {
                chunk = tokio::time::timeout(idle, stream.next()) => chunk,
                _ = signal.cancelled() => { interrupted = true; break 'outer; }
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    if signal.is_cancelled() {
                        interrupted = true;
                        break 'outer;
                    }
                    return Err(LlmError::Timeout(self.timeout_ms));
                }
            };
            let Some(chunk) = chunk else { break };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    if signal.is_cancelled() {
                        interrupted = true;
                        break 'outer;
                    }
                    return Err(LlmError::Other(error.to_string()));
                }
            };
            buffer.extend_from_slice(&bytes);

            // 按字节切出完整事件；只有完整 UTF-8 的片段才会被解码。
            while let Some(boundary) = find_event_boundary(&buffer) {
                let raw: Vec<u8> = buffer.drain(..boundary.0).collect();
                // boundary.1 是分隔符长度（`\n\n` 或 `\r\n\r\n`），一并丢弃。
                buffer.drain(..boundary.1);
                if let Ok(raw) = String::from_utf8(raw) {
                    if pump_event(
                        &raw,
                        &mut assembler,
                        &mut text,
                        &mut reasoning,
                        &mut finish_reason,
                        &mut usage,
                        &mut saw_finish,
                        &mut saw_done,
                        &mut saw_error,
                        on_delta,
                        on_reasoning,
                    ) {
                        // 收到 [DONE]：协议已经结束，不必再等连接关闭。
                        saw_done = true;
                        break 'outer;
                    }
                }
            }
            if signal.is_cancelled() {
                interrupted = true;
                break;
            }
        }

        // 收尾：把缓冲区里最后一个没有空行结尾的事件也处理掉。
        if !buffer.is_empty() && !interrupted {
            let raw: Vec<u8> = std::mem::take(&mut buffer);
            if let Ok(raw) = String::from_utf8(raw) {
                let _ = pump_event(
                    &raw,
                    &mut assembler,
                    &mut text,
                    &mut reasoning,
                    &mut finish_reason,
                    &mut usage,
                    &mut saw_finish,
                    &mut saw_done,
                    &mut saw_error,
                    on_delta,
                    on_reasoning,
                );
            }
        }

        Ok(finalize(FinalizeInput {
            text,
            reasoning,
            assembler,
            finish_reason,
            usage,
            aborted: interrupted || signal.is_cancelled(),
            incomplete: !saw_finish && !saw_done,
            saw_error,
        }))
    }

    /// 拉取可用模型列表（只要 id，保留给既有测试与简单场景）。
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let json = self.list_models_json().await?;
        Ok(json
            .get("data")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// 拉取 `/models` 的原始 JSON（分类与元数据解析在 `model_catalog` 里做）。
    ///
    /// 超时取 `min(timeout_ms, 15s)`：目录接口不该把界面卡住。
    pub async fn list_models_json(&self) -> Result<Value, LlmError> {
        let url = format!("{}/models", self.profile.base_url.trim_end_matches('/'));
        let mut request = self.http.get(url);
        if !self.api_key.trim().is_empty() {
            request = request.header("authorization", format!("Bearer {}", self.api_key));
        }
        let response = request
            .timeout(std::time::Duration::from_millis(
                self.timeout_ms.min(15_000),
            ))
            .send()
            .await
            .map_err(|error| LlmError::Connect(error.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = truncate(&response.text().await.unwrap_or_default(), 240);
            return Err(LlmError::Status { status, body });
        }
        response
            .json::<Value>()
            .await
            .map_err(|error| LlmError::Other(format!("模型列表不是合法 JSON：{error}")))
    }

    /// 发一次**最小**对话请求（连接测试用，不产生工具调用、不带历史）。
    ///
    /// 与正常 `chat` 的区别只有两点：请求体带 `max_tokens: 16`（省费用），
    /// 以及不传工具定义。推理字段仍按能力发送，所以测试结果与真实请求同源。
    pub async fn probe(
        &self,
        prompt: &str,
        signal: &crate::cancel::CancelToken,
    ) -> Result<ChatResult, LlmError> {
        let messages = [ChatMessage::user(prompt)];
        let body = {
            let mut body = self.build_body(&messages, &[], true);
            if let Some(object) = body.as_object_mut() {
                object.insert("max_tokens".to_string(), json!(16));
            }
            body
        };
        let mut request = self
            .http
            .post(self.endpoint())
            .header("content-type", "application/json")
            .json(&body);
        if !self.api_key.trim().is_empty() {
            request = request.header("authorization", format!("Bearer {}", self.api_key));
        }
        let timeout = std::time::Duration::from_millis(self.timeout_ms);
        let response = tokio::select! {
            result = request.send() => result,
            _ = signal.cancelled() => return Err(LlmError::Aborted),
            _ = tokio::time::sleep(timeout) => return Err(LlmError::Timeout(self.timeout_ms)),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if signal.is_cancelled() {
                    return Err(LlmError::Aborted);
                }
                return Err(LlmError::Connect(error.to_string()));
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = truncate(&response.text().await.unwrap_or_default(), 400);
            return Err(LlmError::Status { status, body });
        }
        self.consume_stream(response, signal, &mut |_| {}, &mut |_| {})
            .await
    }
}

/// 在字节缓冲里找下一个 SSE 事件边界。
///
/// 返回 `(事件结束位置, 分隔符长度)`。SSE 允许 `\n\n`、`\r\n\r\n` 以及混合写法，
/// 这里按字节匹配，因此即使分隔符本身被拆在两个网络块之间也能正确识别。
pub(crate) fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
        // \n\n
        if buffer[index] == b'\n' && buffer.get(index + 1) == Some(&b'\n') {
            return Some((index, 2));
        }
        // \r\n\r\n
        if buffer[index] == b'\r'
            && buffer.get(index + 1) == Some(&b'\n')
            && buffer.get(index + 2) == Some(&b'\r')
            && buffer.get(index + 3) == Some(&b'\n')
        {
            return Some((index, 4));
        }
        // \n\r\n（首行以 LF 结尾、次行以 CRLF 结尾的混写）
        if buffer[index] == b'\n'
            && buffer.get(index + 1) == Some(&b'\r')
            && buffer.get(index + 2) == Some(&b'\n')
        {
            return Some((index, 3));
        }
    }
    None
}

/// 解析单个 SSE 事件。返回 `true` 表示收到 `[DONE]`。
#[allow(clippy::too_many_arguments)]
fn pump_event(
    raw_event: &str,
    assembler: &mut ToolCallAssembler,
    text: &mut String,
    reasoning: &mut String,
    finish_reason: &mut String,
    usage: &mut Option<Value>,
    saw_finish: &mut bool,
    saw_done: &mut bool,
    saw_error: &mut bool,
    on_delta: &mut impl FnMut(&str),
    on_reasoning: &mut impl FnMut(&str),
) -> bool {
    let mut event_name = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    // 事件内部也可能用 CRLF 换行（`\r\n\r\n` 分隔时，事件体里就是 `\r\n`）。
    for line in raw_event.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(index) => (
                &line[..index],
                line[index + 1..]
                    .strip_prefix(' ')
                    .unwrap_or(&line[index + 1..]),
            ),
            None => (line, ""),
        };
        let field = field.trim().to_ascii_lowercase();
        if field == "event" {
            event_name = value.trim().to_ascii_lowercase();
        } else if field == "data" {
            data_lines.push(value);
        }
    }
    let event_is_error = event_name == "error";
    let payload = data_lines.join("\n").trim().to_string();
    if payload.is_empty() {
        if event_is_error {
            *saw_error = true;
        }
        return false;
    }
    if payload == "[DONE]" {
        *saw_done = true;
        return true;
    }
    // 服务端错误事件：`event: error` 不管负载长什么样；负载里带 error 字段也算。
    if event_is_error || (payload.starts_with('{') && payload.contains("\"error\"")) {
        *saw_error = true;
        return false;
    }
    let Ok(json) = serde_json::from_str::<Value>(&payload) else {
        return false;
    };
    if json.get("error").map(|e| !e.is_null()).unwrap_or(false) {
        *saw_error = true;
        return false;
    }
    if let Some(value) = json.get("usage") {
        if !value.is_null() {
            *usage = Some(value.clone());
        }
    }
    // 有些端点在最后一个 chunk 里把 choices 置空，只带 usage。
    let Some(choice) = json.get("choices").and_then(|c| c.get(0)) else {
        return false;
    };
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        if !reason.is_empty() {
            *finish_reason = reason.to_string();
            *saw_finish = true;
        }
    }
    let delta = choice
        .get("delta")
        .filter(|value| value.is_object())
        .or_else(|| choice.get("message"))
        .cloned()
        .unwrap_or(Value::Null);
    if delta.get("error").map(|e| !e.is_null()).unwrap_or(false) {
        *saw_error = true;
        return false;
    }
    if let Some(chunk) = delta.get("reasoning_content").and_then(Value::as_str) {
        if !chunk.is_empty() {
            reasoning.push_str(chunk);
            // 思考内容**只**走 on_reasoning，绝不混进可见正文。
            on_reasoning(chunk);
        }
    }
    if let Some(chunk) = delta.get("content").and_then(Value::as_str) {
        if !chunk.is_empty() {
            text.push_str(chunk);
            on_delta(chunk);
        }
    }
    if let Some(calls) = delta.get("tool_calls") {
        assembler.feed(calls);
    }
    false
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let clipped: String = value.chars().take(max).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(capability: Capability, model: &str) -> ProviderProfile {
        ProviderProfile {
            provider_id: "test".to_string(),
            capability,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: model.to_string(),
            thinking: ThinkingLevel::Default,
            reasoning_effort: ReasoningEffort::Default,
            model_context_window: None,
        }
    }

    #[test]
    fn plain_endpoint_sends_no_vendor_fields() {
        let built = build_request_fields(&profile(Capability::Plain, "gpt-4o-mini"), 0.3);
        assert!(built.fields.is_empty());
        assert_eq!(built.temperature, Some(0.3));
        assert!(!built.thinking_active);
    }

    #[test]
    fn mimo_default_sends_no_thinking_field() {
        let built = build_request_fields(&profile(Capability::Mimo, "mimo-v2.5-flash"), 0.3);
        assert!(!built.fields.contains_key("thinking"));
        assert!(built.thinking_active, "默认不是关闭思考");
        assert_eq!(built.temperature, Some(0.3));
    }

    #[test]
    fn mimo_v26_thinking_drops_temperature() {
        let mut p = profile(Capability::Mimo, "mimo-v2.6-flash");
        p.thinking = ThinkingLevel::Enabled;
        let built = build_request_fields(&p, 0.3);
        assert_eq!(
            built.fields.get("thinking").unwrap(),
            &json!({"type":"enabled"})
        );
        assert_eq!(built.temperature, None);
        assert!(built.ignored_sampling.contains(&"temperature"));
    }

    #[test]
    fn mimo_disabled_sends_explicit_switch_and_temperature() {
        let mut p = profile(Capability::Mimo, "mimo-v2.6-flash");
        p.thinking = ThinkingLevel::Disabled;
        let built = build_request_fields(&p, 0.4);
        assert_eq!(
            built.fields.get("thinking").unwrap(),
            &json!({"type":"disabled"})
        );
        assert!(!built.thinking_active);
        assert_eq!(built.temperature, Some(0.4));
    }

    #[test]
    fn deepseek_sends_only_reasoning_effort() {
        let mut p = profile(Capability::DeepSeek, "deepseek-flash");
        p.reasoning_effort = ReasoningEffort::High;
        let built = build_request_fields(&p, 0.3);
        assert_eq!(
            built.fields.get("reasoning_effort").unwrap(),
            &json!("high")
        );
        assert!(!built.fields.contains_key("thinking"), "两家字段不能混发");
        assert_eq!(built.temperature, None);
    }

    #[test]
    fn deepseek_default_sends_nothing() {
        let built = build_request_fields(&profile(Capability::DeepSeek, "deepseek-flash"), 0.3);
        assert!(built.fields.is_empty());
        assert!(built.thinking_active);
    }

    #[test]
    fn deepseek_none_disables_and_keeps_temperature() {
        let mut p = profile(Capability::DeepSeek, "deepseek-flash");
        p.reasoning_effort = ReasoningEffort::None;
        let built = build_request_fields(&p, 0.7);
        assert_eq!(
            built.fields.get("reasoning_effort").unwrap(),
            &json!("none")
        );
        assert!(!built.thinking_active);
        assert_eq!(built.temperature, Some(0.7));
    }

    #[test]
    fn assembler_merges_fragments_by_index() {
        let mut assembler = ToolCallAssembler::default();
        assembler.feed(&json!([{ "index": 0, "id": "call_a", "function": { "name": "list_windows", "arguments": "{\"ap" } }]));
        assembler.feed(&json!([{ "index": 0, "function": { "arguments": "p\":\"x\"}" } }]));
        assembler.feed(&json!([{ "index": 1, "id": "call_b", "function": { "name": "type", "arguments": "{\"text\":\"hi\"}" } }]));
        let calls = assembler.done();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "list_windows");
        assert_eq!(calls[0].arguments, "{\"app\":\"x\"}");
        assert!(calls[0].valid);
        assert_eq!(calls[1].name, "type");
    }

    #[test]
    fn assembler_marks_broken_arguments_invalid() {
        let mut assembler = ToolCallAssembler::default();
        assembler.feed(&json!([{ "index": 0, "id": "c", "function": { "name": "type", "arguments": "{oops" } }]));
        let calls = assembler.done();
        assert!(!calls[0].valid);
        assert!(calls[0].invalid_reason.is_some());
    }

    #[test]
    fn assembler_treats_empty_arguments_as_empty_object() {
        let mut assembler = ToolCallAssembler::default();
        assembler.feed(&json!([{ "index": 0, "id": "c", "function": { "name": "screenshot", "arguments": "" } }]));
        let calls = assembler.done();
        assert!(calls[0].valid);
        assert_eq!(calls[0].arguments, "{}");
    }

    fn feed_all(text: &str, finish: &str) -> ToolCallAssembler {
        let mut assembler = ToolCallAssembler::default();
        let mut sink = String::new();
        let mut reason = String::new();
        let mut fr = "stop".to_string();
        let mut usage: Option<Value> = None;
        let (mut a, mut b, mut c, mut d, mut e) = (false, false, false, None::<Value>, false);
        let _ = (
            &mut sink,
            &mut reason,
            &mut fr,
            &mut usage,
            &mut a,
            &mut b,
            &mut c,
            &mut d,
            &mut e,
            finish,
        );
        assembler.feed(
            &json!([{ "index":0, "id":"c1", "function": { "name":"x", "arguments": text } }]),
        );
        assembler
    }

    #[test]
    fn truncated_stream_never_yields_executable_calls() {
        let assembler = feed_all("{\"a\":1}", "length");
        let result = finalize(FinalizeInput {
            text: String::new(),
            reasoning: String::new(),
            assembler,
            finish_reason: "length".to_string(),
            usage: None,
            aborted: false,
            incomplete: false,
            saw_error: false,
        });
        assert!(result.tool_calls.is_empty(), "被截断的响应不得执行工具");
        assert_eq!(result.broken_tool_calls.len(), 1);
        assert!(result.tool_calls_incomplete);
    }

    #[test]
    fn stop_with_tool_calls_is_protocol_mismatch() {
        let assembler = feed_all("{\"a\":1}", "stop");
        let result = finalize(FinalizeInput {
            text: String::new(),
            reasoning: String::new(),
            assembler,
            finish_reason: "stop".to_string(),
            usage: None,
            aborted: false,
            incomplete: false,
            saw_error: false,
        });
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.broken_tool_calls.len(), 1);
        assert!(result.tool_calls_incomplete);
    }

    #[test]
    fn tool_calls_finish_yields_executable_calls() {
        let assembler = feed_all("{\"a\":1}", "tool_calls");
        let result = finalize(FinalizeInput {
            text: String::new(),
            reasoning: String::new(),
            assembler,
            finish_reason: "tool_calls".to_string(),
            usage: None,
            aborted: false,
            incomplete: false,
            saw_error: false,
        });
        assert_eq!(result.tool_calls.len(), 1);
        assert!(!result.tool_calls_incomplete);
    }

    #[test]
    fn missing_finish_signal_is_incomplete() {
        let assembler = feed_all("{\"a\":1}", "tool_calls");
        let result = finalize(FinalizeInput {
            text: String::new(),
            reasoning: String::new(),
            assembler,
            finish_reason: "tool_calls".to_string(),
            usage: None,
            aborted: false,
            incomplete: true,
            saw_error: false,
        });
        assert!(result.tool_calls.is_empty());
        assert!(result.tool_calls_incomplete);
    }

    #[test]
    fn error_event_blocks_execution() {
        let assembler = feed_all("{\"a\":1}", "tool_calls");
        let result = finalize(FinalizeInput {
            text: String::new(),
            reasoning: String::new(),
            assembler,
            finish_reason: "tool_calls".to_string(),
            usage: None,
            aborted: false,
            incomplete: false,
            saw_error: true,
        });
        assert!(result.tool_calls.is_empty());
    }

    #[test]
    fn sse_parser_reads_text_reasoning_and_tool_fragments() {
        let mut assembler = ToolCallAssembler::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut finish = "stop".to_string();
        let mut usage: Option<Value> = None;
        let (mut saw_finish, mut saw_done, mut saw_error) = (false, false, false);
        let mut visible = String::new();
        let mut hidden = String::new();

        let events = [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"}}]}",
            "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"type\",\"arguments\":\"{\\\"te\"}}]}}]}",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"xt\\\":\\\"hi\\\"}\"}}]}}]}",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
            "data: [DONE]",
        ];
        let mut delta_sink = |value: &str| visible.push_str(value);
        let mut reason_sink = |value: &str| hidden.push_str(value);
        for event in events {
            pump_event(
                event,
                &mut assembler,
                &mut text,
                &mut reasoning,
                &mut finish,
                &mut usage,
                &mut saw_finish,
                &mut saw_done,
                &mut saw_error,
                &mut delta_sink,
                &mut reason_sink,
            );
        }
        assert_eq!(text, "你好");
        assert_eq!(reasoning, "想");
        assert_eq!(visible, "你好");
        assert_eq!(hidden, "想", "思考内容不得混进可见正文");
        assert_eq!(finish, "tool_calls");
        assert!(saw_finish && saw_done && !saw_error);
        let calls = assembler.done();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, "{\"text\":\"hi\"}");
    }

    #[test]
    fn error_event_is_detected_before_json_parse() {
        let mut assembler = ToolCallAssembler::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut finish = "stop".to_string();
        let mut usage: Option<Value> = None;
        let (mut saw_finish, mut saw_done, mut saw_error) = (false, false, false);
        let mut delta_sink = |_: &str| {};
        let mut reason_sink = |_: &str| {};
        pump_event(
            "event: error\ndata: {\"message\":\"quota exceeded\"}",
            &mut assembler,
            &mut text,
            &mut reasoning,
            &mut finish,
            &mut usage,
            &mut saw_finish,
            &mut saw_done,
            &mut saw_error,
            &mut delta_sink,
            &mut reason_sink,
        );
        assert!(
            saw_error,
            "event: error 即使负载里没有 error 字段也必须识别"
        );
    }

    #[test]
    fn request_body_omits_empty_tools() {
        let client = LlmClient::new(profile(Capability::Plain, "m"), "", 0.3, 1000).unwrap();
        let body = client.build_body(&[ChatMessage::user("hi")], &[], true);
        assert!(
            body.get("tools").is_none(),
            "tools: [] 会被多家实现判为 400"
        );
    }

    #[test]
    fn request_body_sets_tool_choice_auto() {
        let client = LlmClient::new(profile(Capability::Plain, "m"), "", 0.3, 1000).unwrap();
        let tools = vec![json!({"type":"function","function":{"name":"t","parameters":{}}})];
        let body = client.build_body(&[ChatMessage::user("hi")], &tools, true);
        assert_eq!(body.get("tool_choice").unwrap(), &json!("auto"));
    }

    #[test]
    fn internal_messages_are_never_serialized_as_internal() {
        let message = ChatMessage::internal_note("（程序生成的提示）");
        let value = message.to_request_value(true, "test");
        assert!(value.get("internal").is_none());
        assert_eq!(value.get("role").unwrap(), &json!("user"));
    }

    #[test]
    fn reasoning_is_echoed_only_for_vendor_capabilities() {
        let mut message = ChatMessage::text(Role::Assistant, "答");
        message.reasoning_content = Some("想".to_string());
        let echoed = message.to_request_value(true, "test");
        assert_eq!(echoed.get("reasoning_content").unwrap(), &json!("想"));
        let plain = message.to_request_value(false, "test");
        assert!(
            plain.get("reasoning_content").is_none(),
            "标准端点带上私有字段可能被判非法"
        );
    }

    #[test]
    fn assistant_message_with_tools_uses_null_content() {
        let result = ChatResult {
            text: String::new(),
            reasoning: "想".to_string(),
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "list_windows".to_string(),
                arguments: "{}".to_string(),
                valid: true,
                invalid_reason: None,
            }],
            ..ChatResult::default()
        };
        let message = assistant_message_from(&result, true, "test");
        let value = message.to_request_value(true, "test");
        assert_eq!(value.get("content").unwrap(), &Value::Null);
        assert_eq!(value.get("reasoning_content").unwrap(), &json!("想"));
        assert!(value.get("tool_calls").is_some());
    }
}
