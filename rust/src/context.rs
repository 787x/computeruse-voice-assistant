//! 上下文预算与压缩（对应 `packages/agent/context.mjs`）。
//!
//! 规则只有一条：**按 token 预算算，到 90% 才压**。
//!  - 默认窗口 1,000,000 tokens；模型有可信且更小的窗口元数据时取 min；
//!  - 90% 是默认触发点，可由用户改；
//!  - 低于触发点**绝不**压缩，也不按消息条数/字符数提前丢历史。
//!
//! 压缩策略刻意保持简单：**不额外请求模型做摘要**（那会明显加延迟），
//! 只按"协议完整的交互单元"从最老的开始整体移除，并用一行摘要记住被折叠的用户目标。
//!
//! 两条容易写错、这里专门处理的规则：
//!  - **内部提示不是用户回合**：程序生成的"（参数不完整，请重新生成）"也占 user 角色，
//!    但它属于当前任务。若把它当成较新的用户回合，就会先删掉真实用户目标。
//!  - **装不下要说明白**：压完仍超硬上限时返回 `over_limit`，调用方必须明确告知用户，
//!    绝不把注定失败的请求照常发出去、更不能静默截断用户任务后假装正常完成。

use serde_json::Value;

use crate::llm::{ChatMessage, Content, Role};

/// 默认上下文窗口：1M tokens。
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
/// 默认压缩触发点：窗口的 90%。
pub const DEFAULT_COMPRESS_PERCENT: u32 = 90;

const MAX_OUTPUT_RESERVE: u64 = 32_768;
const MIN_OUTPUT_RESERVE: u64 = 1_024;
const MESSAGE_OVERHEAD: u64 = 4;
const DIGEST_MAX_CHARS: usize = 300;
const TOOL_SHRINK_MIN_CHARS: usize = 4_000;

/// 预算策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    pub context_window: u64,
    pub compress_percent: u32,
    pub compress_at_tokens: u64,
    pub hard_limit_tokens: u64,
    pub target_tokens: u64,
    pub output_reserve: u64,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        resolve_context_policy(
            Some(DEFAULT_CONTEXT_WINDOW),
            Some(DEFAULT_COMPRESS_PERCENT),
            None,
        )
    }
}

/// 把「用户设置 + 模型元数据」解析成一份可执行的预算策略。
///
/// `model_context_window` 只用来**收紧**，不用来放宽：用户把 1M 调到 200K 是他的选择，
/// 但模型只支持 128K 时必须按 128K 算，绝不能按 1M 硬发。
pub fn resolve_context_policy(
    context_window: Option<u64>,
    compress_percent: Option<u32>,
    model_context_window: Option<u64>,
) -> ContextPolicy {
    let configured = context_window
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let context_window = match model_context_window.filter(|value| *value > 0) {
        Some(meta) => configured.min(meta),
        None => configured,
    };
    let compress_percent = compress_percent
        .unwrap_or(DEFAULT_COMPRESS_PERCENT)
        .clamp(10, 99);
    let output_reserve = (context_window * 5 / 100).clamp(MIN_OUTPUT_RESERVE, MAX_OUTPUT_RESERVE);
    let compress_at_tokens = context_window * compress_percent as u64 / 100;
    let hard_limit_tokens = context_window
        .saturating_sub(output_reserve)
        .max(MIN_OUTPUT_RESERVE);
    // 压缩目标：压到触发点的 75%，留出继续跑几步的空间，避免每一步都重新压一遍。
    let target_tokens = (compress_at_tokens.min(hard_limit_tokens) * 3 / 4).max(MIN_OUTPUT_RESERVE);
    ContextPolicy {
        context_window,
        compress_percent,
        compress_at_tokens,
        hard_limit_tokens,
        target_tokens,
        output_reserve,
    }
}

/// 估算一段文本的 token 数。
///
/// ASCII 约 4 字符 / token；其它（CJK 等）按 1 token / 字符。
/// 这是**保守偏高**的估算：宁可早一点压缩，也不要撞上服务端硬上限。
pub fn estimate_text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in text.chars() {
        if (ch as u32) < 0x80 {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    wide + ascii.div_ceil(4)
}

/// 图片占多少 token：优先按说明文字里的 `宽x高` 算瓦片，拿不到尺寸就按 base64 长度折算。
pub fn estimate_image_tokens(parts: &[Value]) -> u64 {
    let mut dims: Option<(u64, u64)> = None;
    let mut bytes: u64 = 0;
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("text") && dims.is_none() {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if let Some(found) = parse_dimensions(text) {
                    dims = Some(found);
                }
            }
        }
        if part.get("type").and_then(Value::as_str) == Some("image_url") {
            if let Some(url) = part
                .get("image_url")
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
            {
                let base64 = url.split_once(',').map(|(_, tail)| tail).unwrap_or(url);
                bytes += (base64.len() as u64) * 3 / 4;
            }
        }
    }
    if let Some((width, height)) = dims {
        let tiles = width.div_ceil(512).max(1) * height.div_ceil(512).max(1);
        return 85 + 170 * tiles;
    }
    if bytes == 0 {
        0
    } else {
        bytes.div_ceil(200)
    }
}

/// 从说明文字里找 `宽x高`。
pub fn parse_dimensions(text: &str) -> Option<(u64, u64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if index - start < 2 || index - start > 5 {
            continue;
        }
        let width: u64 = chars[start..index]
            .iter()
            .collect::<String>()
            .parse()
            .ok()?;
        // 跳过空白与乘号
        while index < chars.len() && (chars[index].is_whitespace()) {
            index += 1;
        }
        if index < chars.len()
            && (chars[index] == 'x' || chars[index] == '×' || chars[index] == 'X')
        {
            index += 1;
        } else {
            continue;
        }
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        let height_start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if index - height_start < 2 || index - height_start > 5 {
            continue;
        }
        let height: u64 = chars[height_start..index]
            .iter()
            .collect::<String>()
            .parse()
            .ok()?;
        return Some((width, height));
    }
    None
}

/// 一条消息占多少 token（含推理内容、工具调用参数、图片）。
pub fn estimate_message_tokens(message: &ChatMessage) -> u64 {
    let mut total = MESSAGE_OVERHEAD;
    match &message.content {
        Content::Text(text) => total += estimate_text_tokens(text),
        Content::Parts(parts) => {
            let mut text_only = 0;
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    text_only += estimate_text_tokens(
                        part.get("text").and_then(Value::as_str).unwrap_or(""),
                    );
                }
            }
            total += text_only + estimate_image_tokens(parts);
        }
    }
    if let Some(reasoning) = &message.reasoning_content {
        total += estimate_text_tokens(reasoning);
    }
    for call in &message.tool_calls {
        total += estimate_text_tokens(&call.name);
        total += estimate_text_tokens(&call.arguments);
        total += 8; // id / type 等结构开销
    }
    if message.tool_call_id.is_some() {
        total += 4;
    }
    total
}

/// 工具定义占多少 token（JSON Schema 很长，必须计入预算）。
pub fn estimate_tools_tokens(tools: &[Value]) -> u64 {
    let mut total = 0;
    for tool in tools {
        total += estimate_text_tokens(&tool.to_string()) + 8;
    }
    total
}

/// 一次请求的总输入 tokens（消息 + 工具定义）。
pub fn estimate_request_tokens(messages: &[ChatMessage], tools: &[Value]) -> u64 {
    estimate_tools_tokens(tools) + messages.iter().map(estimate_message_tokens).sum::<u64>()
}

/// 压缩结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompressionReport {
    pub compressed: bool,
    pub before: u64,
    pub after: u64,
    pub dropped_units: usize,
    pub dropped_segments: usize,
    pub dropped_images: usize,
    /// 压完仍装不下时带上实际估算值。
    pub over_limit: bool,
    pub over_limit_tokens: u64,
    pub hard_limit_tokens: u64,
}

/// 程序生成的内部提示（不是用户说的话）。
///
/// 兼容两层：显式标记（`internal: true`）优先；再兼容旧版落进历史里的固定措辞。
const INTERNAL_PROMPT_PREFIXES: &[&str] = &[
    "（上一步的工具调用参数不完整",
    "（上一步的模型响应不完整",
    "（已经用满 ",
    "（为控制上下文长度",
    "（这一轮已经附了 ",
];

fn is_internal_user_message(message: &ChatMessage) -> bool {
    if message.role != Role::User {
        return false;
    }
    if message.internal {
        return true;
    }
    let Content::Text(text) = &message.content else {
        return false;
    };
    let trimmed = text.trim();
    if !(trimmed.starts_with('（') && trimmed.ends_with('）')) {
        return false;
    }
    INTERNAL_PROMPT_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn is_plain_user_text(message: &ChatMessage) -> bool {
    message.role == Role::User
        && matches!(message.content, Content::Text(_))
        && !is_internal_user_message(message)
}

fn is_attachment_user(message: &ChatMessage) -> bool {
    message.role == Role::User && matches!(message.content, Content::Parts(_))
}

/// 一个协议完整片段：assistant（可带 tool_calls）+ 它声明的全部 tool 结果 + 紧随的截图附件 + 内部说明。
#[derive(Debug, Clone, Default)]
struct Segment {
    calls: Vec<ChatMessage>,
    results: Vec<ChatMessage>,
    attachments: Vec<ChatMessage>,
    notes: Vec<ChatMessage>,
}

impl Segment {
    fn messages(&self) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        out.extend(self.calls.iter().cloned());
        out.extend(self.results.iter().cloned());
        out.extend(self.attachments.iter().cloned());
        out.extend(self.notes.iter().cloned());
        out
    }
}

#[derive(Debug, Clone)]
struct Unit {
    head: Option<ChatMessage>,
    segments: Vec<Segment>,
    leading_notes: Vec<ChatMessage>,
}

fn declared_call_ids(message: &ChatMessage) -> Vec<String> {
    message
        .tool_calls
        .iter()
        .map(|call| call.id.clone())
        .collect()
}

fn split_segments(calls: &[ChatMessage]) -> (Vec<Segment>, Vec<ChatMessage>) {
    let mut segments: Vec<Segment> = Vec::new();
    let mut leading_notes: Vec<ChatMessage> = Vec::new();
    let mut current: Option<usize> = None;

    for message in calls {
        match message.role {
            Role::Assistant => {
                segments.push(Segment {
                    calls: vec![message.clone()],
                    ..Segment::default()
                });
                current = Some(segments.len() - 1);
            }
            Role::Tool => {
                let Some(index) = current else { continue };
                let ids: Vec<String> = segments[index]
                    .calls
                    .iter()
                    .flat_map(declared_call_ids)
                    .collect();
                let matches = message
                    .tool_call_id
                    .as_ref()
                    .map(|id| ids.contains(id))
                    .unwrap_or(false);
                // 孤立的 tool 结果（没有对应的 assistant tool_calls）直接丢弃：它本身就不合法。
                if matches {
                    segments[index].results.push(message.clone());
                }
            }
            Role::User if is_attachment_user(message) => {
                if let Some(index) = current {
                    segments[index].attachments.push(message.clone());
                }
            }
            Role::User if is_internal_user_message(message) => match current {
                Some(index) => segments[index].notes.push(message.clone()),
                None => leading_notes.push(message.clone()),
            },
            _ => {}
        }
    }

    // 片段完整性：声明的 id 必须有结果。缺结果的整段剔除（不补假结果、不执行半截调用）。
    let complete: Vec<Segment> = segments
        .into_iter()
        .filter(|segment| {
            let declared: Vec<String> = segment.calls.iter().flat_map(declared_call_ids).collect();
            let results: Vec<&String> = segment
                .results
                .iter()
                .filter_map(|result| result.tool_call_id.as_ref())
                .collect();
            declared.iter().all(|id| results.contains(&id))
        })
        .collect();

    (complete, leading_notes)
}

/// 把消息拆成「交互单元」：普通文本 user 开头，后面跟若干片段。
///
/// 截图（`Content::Parts` 的 user 消息）归入它所属片段，绝不开启新单元。
/// 程序生成的内部提示同样**不开启新单元**。
fn split_interaction_units(messages: &[ChatMessage]) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    let mut current: Option<usize> = None;
    let mut leading_notes: Vec<ChatMessage> = Vec::new();

    for message in messages {
        if is_plain_user_text(message) {
            units.push(Unit {
                head: Some(message.clone()),
                segments: Vec::new(),
                leading_notes: Vec::new(),
            });
            current = Some(units.len() - 1);
            continue;
        }
        match current {
            None => {
                if is_internal_user_message(message) {
                    leading_notes.push(message.clone());
                }
            }
            Some(index) => {
                // 先按调用顺序暂存，拆片段在单元收尾时做
                units[index].segments.push(Segment {
                    calls: vec![message.clone()],
                    ..Segment::default()
                });
            }
        }
    }

    // 重新按单元拆片段（保留原始顺序）
    let mut rebuilt: Vec<Unit> = Vec::new();
    for unit in &units {
        let flat: Vec<ChatMessage> = unit.segments.iter().flat_map(|s| s.calls.clone()).collect();
        let (segments, unit_notes) = split_segments(&flat);
        rebuilt.push(Unit {
            head: unit.head.clone(),
            segments,
            leading_notes: unit_notes,
        });
    }
    if let Some(first) = rebuilt.first_mut() {
        let mut notes = leading_notes;
        notes.extend(first.leading_notes.clone());
        first.leading_notes = notes;
    } else if !leading_notes.is_empty() {
        rebuilt.push(Unit {
            head: None,
            segments: Vec::new(),
            leading_notes,
        });
    }
    rebuilt
}

fn flatten_units(units: &[Unit]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for unit in units {
        if let Some(head) = &unit.head {
            out.push(head.clone());
        }
        out.extend(unit.leading_notes.iter().cloned());
        for segment in &unit.segments {
            out.extend(segment.messages());
        }
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    let squeezed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if squeezed.chars().count() <= max {
        return squeezed;
    }
    let head: String = squeezed.chars().take(max).collect();
    format!("{head}…")
}

fn units_tokens(
    system: &[ChatMessage],
    digest: Option<&ChatMessage>,
    units: &[Unit],
    tools: &[Value],
) -> u64 {
    let mut messages: Vec<ChatMessage> = system.to_vec();
    if let Some(digest) = digest {
        messages.push(digest.clone());
    }
    messages.extend(flatten_units(units));
    estimate_request_tokens(&messages, tools)
}

fn is_digest_message(message: &ChatMessage) -> bool {
    is_internal_user_message(message) && message.content.as_text().starts_with("（为控制上下文长度")
}

/// 按 token 预算压缩一次请求的消息（**原地修改**）。
///
/// 调用方只需要在每次发请求前调一次：没到触发点会立刻返回（只做一次估算），
/// 到了才真正动手，所以"低于 90% 不压缩"是结构上保证的。
pub fn compress_messages(
    messages: &mut Vec<ChatMessage>,
    policy: ContextPolicy,
    tools: &[Value],
) -> CompressionReport {
    let mut report = CompressionReport {
        hard_limit_tokens: policy.hard_limit_tokens,
        ..CompressionReport::default()
    };
    if messages.is_empty() {
        return report;
    }

    report.before = estimate_request_tokens(messages, tools);
    if report.before <= policy.compress_at_tokens && report.before <= policy.hard_limit_tokens {
        report.after = report.before;
        return report;
    }

    let system: Vec<ChatMessage> = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .cloned()
        .collect();
    let rest: Vec<ChatMessage> = messages
        .iter()
        .filter(|message| message.role != Role::System)
        .cloned()
        .collect();
    let mut units = split_interaction_units(&rest);
    if units.is_empty() {
        report.after = report.before;
        return report;
    }

    // 之前几轮压缩留下的摘要：装着**更早**被折叠的目标，必须合并进新摘要而不是丢掉。
    let mut inherited: Vec<String> = Vec::new();
    for unit in &units {
        for note in &unit.leading_notes {
            if is_digest_message(note) {
                inherited.push(note.content.as_text());
            }
        }
        for segment in &unit.segments {
            for note in &segment.notes {
                if is_digest_message(note) {
                    inherited.push(note.content.as_text());
                }
            }
        }
    }
    let mut unique_digests: Vec<String> = Vec::new();
    for text in inherited {
        if !unique_digests.contains(&text) {
            unique_digests.push(text);
        }
    }
    if !unique_digests.is_empty() {
        for unit in &mut units {
            unit.leading_notes.retain(|note| !is_digest_message(note));
            for segment in &mut unit.segments {
                segment.notes.retain(|note| !is_digest_message(note));
            }
        }
    }

    let mut dropped_goals: Vec<String> = Vec::new();
    fn note_dropped(dropped_goals: &mut Vec<String>, unit: &Unit) {
        let goal = unit
            .head
            .as_ref()
            .map(|head| clip(&head.content.as_text(), 60))
            .unwrap_or_default();
        if !goal.is_empty() {
            dropped_goals.push(goal);
        }
    }

    // 1) 从最老的整轮开始丢（至少保留当前这一轮）
    while units.len() > 1 && units_tokens(&system, None, &units, tools) > policy.target_tokens {
        note_dropped(&mut dropped_goals, &units[0]);
        units.remove(0);
        report.dropped_units += 1;
    }

    // 2) 仍然超预算：丢当前轮里较早且**已完成**的工具片段（保留最后一个片段与用户目标）
    loop {
        if units_tokens(&system, None, &units, tools) <= policy.target_tokens {
            break;
        }
        let mut dropped = false;
        for unit in &mut units {
            if unit.segments.len() > 1 {
                let removed = unit.segments.remove(0);
                let text = removed.calls.iter().find_map(|call| match &call.content {
                    Content::Text(text) if !text.is_empty() => Some(text.clone()),
                    _ => None,
                });
                if let Some(text) = text {
                    dropped_goals.push(clip(&text, 40));
                }
                report.dropped_segments += 1;
                dropped = true;
                break;
            }
        }
        if !dropped {
            break;
        }
    }

    // 3) 仍然超预算：先摘掉最老的截图附件（图片最贵，且删掉不影响协议配对）
    loop {
        if units_tokens(&system, None, &units, tools) <= policy.target_tokens {
            break;
        }
        let mut dropped = false;
        'outer: for unit in &mut units {
            for segment in &mut unit.segments {
                if !segment.attachments.is_empty() {
                    segment.attachments.remove(0);
                    report.dropped_images += 1;
                    dropped = true;
                    break 'outer;
                }
            }
        }
        if !dropped {
            break;
        }
    }

    // 3b) 仍然超预算：压缩**单条超大工具结果**（只在段落内部截文本，不动 tool_calls 配对）。
    loop {
        let over = units_tokens(&system, None, &units, tools);
        if over <= policy.target_tokens {
            break;
        }
        let need = (over - policy.target_tokens).max(2_000);
        let mut shrunk = false;
        'outer: for unit in &mut units {
            for segment in &mut unit.segments {
                let mut targets: Vec<&mut ChatMessage> = segment
                    .results
                    .iter_mut()
                    .chain(segment.calls.iter_mut())
                    .collect();
                for message in targets.iter_mut() {
                    let Content::Text(text) = &message.content else {
                        continue;
                    };
                    let length = text.chars().count();
                    // 只碰"明显过大"的单条：小结果留着（它们往往是关键状态）。
                    if length < TOOL_SHRINK_MIN_CHARS {
                        continue;
                    }
                    let keep = (length as u64).saturating_sub(need * 4).max(1_000) as usize;
                    if keep >= length {
                        continue;
                    }
                    let head: String = text.chars().take(keep).collect();
                    message.content = Content::Text(format!(
                        "{head}…（结果过长，已按上下文预算截断，只保留前 {keep} 字）"
                    ));
                    shrunk = true;
                    break 'outer;
                }
            }
        }
        if !shrunk {
            break;
        }
    }

    // 4) 逼近/超过模型硬上限：继续丢整轮，直到只剩当前这一轮
    while units.len() > 1 && units_tokens(&system, None, &units, tools) > policy.hard_limit_tokens {
        note_dropped(&mut dropped_goals, &units[0]);
        units.remove(0);
        report.dropped_units += 1;
    }

    // 5) 单轮自身就超过硬上限：**绝不截断真实用户指令**，只如实标记 over_limit。
    if units_tokens(&system, None, &units, tools) > policy.hard_limit_tokens {
        report.over_limit = true;
    }

    // 折叠摘要：让模型知道"之前还要求过什么"，而不是一片空白。
    let digest = if dropped_goals.is_empty() && unique_digests.is_empty() {
        None
    } else {
        let inherited_text = unique_digests
            .iter()
            .map(|text| {
                let stripped = text
                    .trim_start_matches("（为控制上下文长度")
                    .trim_start_matches(|ch: char| ch != '：')
                    .trim_start_matches('：')
                    .trim_end_matches('）')
                    .trim();
                stripped.to_string()
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        let mut parts: Vec<String> = Vec::new();
        if !inherited_text.is_empty() {
            parts.push(format!("更早还提过：{inherited_text}"));
        }
        if !dropped_goals.is_empty() {
            parts.push(format!("此前用户先后提过：{}", dropped_goals.join(" / ")));
        }
        let text = format!(
            "（为控制上下文长度，已折叠较早的对话。{}）",
            parts.join("；")
        );
        let mut message = ChatMessage::internal_note(clip(&text, DIGEST_MAX_CHARS));
        message.internal = true;
        Some(message)
    };

    let mut rebuilt: Vec<ChatMessage> = system;
    if let Some(digest) = digest {
        rebuilt.push(digest);
    }
    rebuilt.extend(flatten_units(&units));
    *messages = rebuilt;

    report.after = estimate_request_tokens(messages, tools);
    report.compressed = true;
    report.over_limit = report.after > policy.hard_limit_tokens;
    report.over_limit_tokens = if report.over_limit { report.after } else { 0 };
    report
}

/// 历史裁剪（跨回合）也走同一套预算：低于触发点一律原样保留。
pub fn compress_history(
    history: &[ChatMessage],
    policy: ContextPolicy,
    tools: &[Value],
) -> Vec<ChatMessage> {
    let mut copy = history.to_vec();
    compress_messages(&mut copy, policy, tools);
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_1m_at_90_percent() {
        let policy = resolve_context_policy(None, None, None);
        assert_eq!(policy.context_window, 1_000_000);
        assert_eq!(policy.compress_percent, 90);
        assert_eq!(policy.compress_at_tokens, 900_000);
        assert!(policy.hard_limit_tokens < policy.context_window);
        assert!(policy.target_tokens < policy.compress_at_tokens);
    }

    #[test]
    fn model_metadata_only_tightens() {
        // 用户设 1M，但模型只支持 128K → 按 128K 算
        let policy = resolve_context_policy(Some(1_000_000), Some(90), Some(131_072));
        assert_eq!(policy.context_window, 131_072);
        // 用户设 200K，模型 1M → 尊重用户选择
        let policy = resolve_context_policy(Some(200_000), Some(90), Some(1_000_000));
        assert_eq!(policy.context_window, 200_000);
    }

    #[test]
    fn compress_percent_is_clamped() {
        assert_eq!(
            resolve_context_policy(None, Some(5), None).compress_percent,
            10
        );
        assert_eq!(
            resolve_context_policy(None, Some(150), None).compress_percent,
            99
        );
    }

    #[test]
    fn token_estimate_is_conservative_for_cjk() {
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("你好"), 2);
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn small_conversation_is_never_compressed() {
        let policy = resolve_context_policy(None, None, None);
        let mut messages = vec![
            ChatMessage::system("prompt"),
            ChatMessage::user("你好"),
            ChatMessage::text(Role::Assistant, "在"),
        ];
        let report = compress_messages(&mut messages, policy, &[]);
        assert!(!report.compressed, "低于触发点绝不压缩");
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn old_turns_are_dropped_when_over_budget() {
        // 用很小的窗口制造"超预算"场景
        let policy = resolve_context_policy(Some(2_000), Some(50), None);
        let mut messages = vec![ChatMessage::system("sys")];
        for index in 0..40 {
            messages.push(ChatMessage::user(format!(
                "历史目标 {index} {}",
                "文".repeat(200)
            )));
            messages.push(ChatMessage::text(Role::Assistant, "好"));
        }
        messages.push(ChatMessage::user("当前目标"));
        let report = compress_messages(&mut messages, policy, &[]);
        assert!(report.compressed);
        assert!(report.dropped_units > 0);
        // 当前目标必须留下
        assert!(messages.iter().any(|m| m.content.as_text() == "当前目标"));
        // system 必须留在最前
        assert_eq!(messages[0].role, Role::System);
    }

    #[test]
    fn tool_pairs_stay_paired_after_compression() {
        let policy = resolve_context_policy(Some(2_000), Some(50), None);
        let mut messages = vec![ChatMessage::system("sys")];
        for index in 0..30 {
            messages.push(ChatMessage::user(format!(
                "目标 {index} {}",
                "文".repeat(150)
            )));
            let mut assistant = ChatMessage::text(Role::Assistant, "");
            assistant.tool_calls.push(crate::llm::ToolCall {
                id: format!("call_{index}"),
                name: "screenshot".to_string(),
                arguments: "{}".to_string(),
                valid: true,
                invalid_reason: None,
            });
            messages.push(assistant);
            messages.push(ChatMessage::tool_result(
                format!("call_{index}"),
                "x".repeat(500),
            ));
        }
        messages.push(ChatMessage::user("当前目标"));
        compress_messages(&mut messages, policy, &[]);

        // 每条带 tool_calls 的 assistant 都要有对应 tool 结果，且不能有孤立 tool。
        let mut expected: Vec<String> = Vec::new();
        let mut seen_results: Vec<String> = Vec::new();
        for message in &messages {
            if message.role == Role::Assistant {
                expected.extend(message.tool_calls.iter().map(|c| c.id.clone()));
            }
            if message.role == Role::Tool {
                let id = message.tool_call_id.clone().unwrap();
                seen_results.push(id);
            }
        }
        for id in &seen_results {
            assert!(expected.contains(id), "不能留下孤立的 tool 结果：{id}");
        }
        assert_eq!(expected.len(), seen_results.len());
    }

    #[test]
    fn digest_remembers_dropped_goals() {
        let policy = resolve_context_policy(Some(2_000), Some(50), None);
        let mut messages = vec![ChatMessage::system("sys")];
        for index in 0..30 {
            messages.push(ChatMessage::user(format!(
                "早期目标{index} {}",
                "文".repeat(200)
            )));
            messages.push(ChatMessage::text(Role::Assistant, "好"));
        }
        messages.push(ChatMessage::user("当前目标"));
        compress_messages(&mut messages, policy, &[]);
        let digest = messages
            .iter()
            .find(|m| m.content.as_text().starts_with("（为控制上下文长度"));
        assert!(digest.is_some(), "折叠后必须留下摘要");
        assert!(digest.unwrap().internal);
    }

    #[test]
    fn internal_notes_do_not_start_new_turns() {
        let units = split_interaction_units(&[
            ChatMessage::user("真实目标"),
            ChatMessage::internal_note("（上一步的工具调用参数不完整，请重试）"),
            ChatMessage::text(Role::Assistant, "好"),
        ]);
        assert_eq!(units.len(), 1, "内部提示不得被当成新的用户回合");
        assert_eq!(
            units[0].head.as_ref().unwrap().content.as_text(),
            "真实目标"
        );
    }

    #[test]
    fn over_limit_is_reported_not_silently_truncated() {
        let policy = resolve_context_policy(Some(1_000), Some(50), None);
        let huge = "文".repeat(20_000);
        let mut messages = vec![ChatMessage::system("sys"), ChatMessage::user(huge.clone())];
        let report = compress_messages(&mut messages, policy, &[]);
        assert!(report.over_limit, "装不下必须如实报告");
        assert!(
            messages.iter().any(|m| m.content.as_text() == huge),
            "绝不能悄悄截断用户指令"
        );
    }

    #[test]
    fn image_tokens_use_dimensions_when_available() {
        let parts = vec![
            serde_json::json!({"type":"text","text":"（这是 screenshot 刚返回的图，1024x768）"}),
            serde_json::json!({"type":"image_url","image_url":{"url":"data:image/jpeg;base64,AAAA"}}),
        ];
        // 1024/512=2 瓦片 × 768/512=2 瓦片 → 85 + 170*4 = 765
        assert_eq!(estimate_image_tokens(&parts), 765);
    }

    #[test]
    fn parse_dimensions_handles_cjk_and_spaces() {
        assert_eq!(parse_dimensions("图 1280 x 720 像素"), Some((1280, 720)));
        assert_eq!(parse_dimensions("1280×720"), Some((1280, 720)));
        assert_eq!(parse_dimensions("no dims here"), None);
    }

    #[test]
    fn tools_tokens_are_counted() {
        let tools = vec![serde_json::json!({"type":"function","function":{"name":"screenshot"}})];
        assert!(estimate_tools_tokens(&tools) > 0);
    }
}
