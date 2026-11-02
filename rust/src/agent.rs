//! Agent 循环（对应 `packages/agent/loop.mjs`）。
//!
//! 职责边界：只负责"想和调用工具"，不碰 UI、不碰音频、不碰权限实现细节。
//! 权限通过注入的 `Confirmer` 外部化；工具通过 `ToolInvoker` trait 注入，
//! 这样 Agent 本身可以完全脱离 PowerShell 单测。
//!
//! 关键设计（逐条对应旧版语义）：
//!  - **思考内容与最终回答分开**：`reasoning_content` 只用于协议回传，绝不进界面正文。
//!  - **响应不完整时一个工具都不执行**，即使个别调用参数已是合法 JSON。
//!  - **参数不完整/解析失败的工具调用不执行**，只把失败原因回给模型。
//!  - **每一步都可中断**：所有 await 前后都检查取消令牌；取消后迟到的结果一律丢弃。
//!  - **上下文按 token 预算管理**：默认 1M 窗口、到 90% 才压缩。
//!  - **同一批工具结果必须连续出现**：截图附件攒到这一批工具全部返回后再补进消息。

use std::sync::Arc;

use serde_json::Value;

use crate::cancel::{CancelToken, Cancelled};
use crate::context::{
    compress_messages, estimate_request_tokens, ContextPolicy, DEFAULT_CONTEXT_WINDOW,
};
use crate::cua::{ImageAttachment, ToolError};
use crate::llm::{
    assistant_message_from, ChatMessage, ChatResult, Content, LlmClient, LlmError, Role,
};
use crate::permissions::{Confirmer, Decision, PermissionGate, PermissionState};

/// 一轮里最多把几张图塞给模型（只防"模型自己陷入截图死循环"）。
pub const MAX_IMAGES_PER_RUN: usize = 24;
/// 单条工具结果的长度上限。只在**真的过长**时才截断，并明确标注被砍了多少。
pub const TOOL_RESULT_MAX_CHARS: usize = 24_000;

/// 工具执行能力（由 `CuaToolkit` 实现；测试里用假实现）。
pub trait ToolInvoker: Send + Sync {
    /// 发给模型的工具定义。
    fn tool_definitions(&self) -> Vec<Value>;
    /// 执行一次工具调用。
    fn invoke<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a CancelToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolExecution, ToolError>> + Send + 'a>,
    >;
}

/// 一次工具执行的结果。
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub text: String,
    pub image: Option<ImageAttachment>,
}

/// Agent 的流式事件（界面据此更新状态）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 可见回复的增量文本。
    TextDelta(String),
    /// 思考内容长度（**只带长度，不带内容**：思考过程不展示、不播报）。
    Reasoning { step: usize, chars: usize },
    /// 模型开始调用工具（已解析出参数，尚未过权限）。
    ToolStart {
        name: String,
        args: Value,
        step: usize,
    },
    /// 工具调用结束（含被拒绝的情况）。
    ToolEnd {
        name: String,
        ok: bool,
        text: String,
        step: usize,
    },
    /// 需要用户确认（`ask`）。返回的 future 由界面 resolve。
    ConfirmationRequired(ConfirmationPrompt),
    /// 本回合结束。
    Done(TurnOutcome),
    /// 本回合失败（网络/协议/上下文装不下）。
    Failed { reason: String },
}

/// 一次需要用户确认的请求（界面展示"允许 / 拒绝"）。
#[derive(Debug, Clone)]
pub struct ConfirmationPrompt {
    pub tool: String,
    pub capability: String,
    pub label: String,
    pub reason: String,
    pub args_summary: Value,
}

/// 单个工具步骤的记录（用于界面与最终汇报）。
#[derive(Debug, Clone)]
pub struct StepRecord {
    pub step: usize,
    pub tool: String,
    pub ok: bool,
    pub text: String,
    pub ms: u64,
}

/// 一回合的结果。
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub text: String,
    pub steps: Vec<StepRecord>,
    pub limited: bool,
    pub reasoning_chars: usize,
    /// 本回合因流式异常/错误/取消而未正常完成。
    pub failed: bool,
    pub failure_reason: Option<String>,
    pub context_tokens: u64,
    pub context_window: u64,
    pub cancelled: bool,
}

/// Agent 配置。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_steps: u32,
    pub context_policy: ContextPolicy,
    pub system_prompt: String,
    pub echo_reasoning: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: crate::config::DEFAULT_MAX_STEPS,
            context_policy: crate::context::resolve_context_policy(
                Some(DEFAULT_CONTEXT_WINDOW),
                None,
                None,
            ),
            system_prompt: crate::prompt::DEFAULT_SYSTEM_PROMPT.to_string(),
            echo_reasoning: false,
        }
    }
}

/// Agent。
pub struct Agent {
    llm: LlmClient,
    tools: Arc<dyn ToolInvoker>,
    config: AgentConfig,
    pub permissions: PermissionState,
}

impl Agent {
    pub fn new(
        llm: LlmClient,
        tools: Arc<dyn ToolInvoker>,
        config: AgentConfig,
        permissions: PermissionState,
    ) -> Self {
        Self {
            llm,
            tools,
            config,
            permissions,
        }
    }

    /// 跑一个用户回合。
    ///
    /// `history` 是同一会话里之前几轮的消息（会被就地追加本轮增量）。
    /// 返回的 `TurnOutcome` 里带**本轮增量**（`history` 末尾新增的部分）。
    pub async fn run(
        &mut self,
        input: &str,
        history: &mut Vec<ChatMessage>,
        cancel: &CancelToken,
        confirmer: &dyn Confirmer,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> TurnOutcome {
        let mut outcome = TurnOutcome {
            context_window: self.config.context_policy.context_window,
            ..TurnOutcome::default()
        };
        let tools = self.tools.tool_definitions();
        let policy = self.config.context_policy;

        // 组装本轮工作集：system + 会话历史 + 本轮 user
        let mut messages: Vec<ChatMessage> = Vec::new();
        messages.push(ChatMessage::system(self.config.system_prompt.clone()));
        messages.extend(history.iter().cloned());
        messages.push(ChatMessage::user(input));
        // 本轮 user 在历史里的下标（压缩可能删掉更老的回合，但这个对象会保留）。
        let turn_user_marker = messages.len() - 1;
        let _ = turn_user_marker;

        // 发第一枪之前先看一眼总预算：历史本身就可能已经很大。
        let initial = compress_messages(&mut messages, policy, &tools);
        if initial.over_limit {
            let reason = format!(
                "上下文装不下这一轮（估算 {} tokens，模型输入上限 {}）。",
                initial.over_limit_tokens, policy.hard_limit_tokens
            );
            outcome.text = reason.clone();
            outcome.failed = true;
            outcome.failure_reason = Some("context_overflow".to_string());
            emit(AgentEvent::Failed { reason });
            return outcome;
        }
        // 压缩后的工作集就是新的会话历史基准（被折叠的老回合不再累积）。
        sync_history(history, &messages);

        let mut images_attached = 0usize;
        let mut hit_step_limit = false;

        for step in 1..=self.config.max_steps.max(1) {
            if cancel.is_cancelled() {
                return cancelled(outcome, &mut messages, history, emit, policy, &tools);
            }

            // 每次发请求前压一次。没到 90% 会立刻返回（只花一次估算）。
            let report = compress_messages(&mut messages, policy, &tools);
            if report.over_limit {
                let reason = format!(
                    "上下文装不下这一轮（估算 {} tokens，模型输入上限 {}）。",
                    report.over_limit_tokens, policy.hard_limit_tokens
                );
                outcome.failed = true;
                outcome.failure_reason = Some("context_overflow".to_string());
                outcome.text = if outcome.text.is_empty() {
                    reason.clone()
                } else {
                    outcome.text.clone()
                };
                sync_history(history, &messages);
                emit(AgentEvent::Failed { reason });
                return outcome;
            }

            let request_messages = strip_local_markers(&messages);
            let chat = self
                .llm
                .chat(
                    &request_messages,
                    &tools,
                    cancel,
                    |text| emit(AgentEvent::TextDelta(text.to_string())),
                    |_reasoning| {},
                )
                .await;

            let result = match chat {
                Ok(result) => result,
                Err(LlmError::Aborted) => {
                    return cancelled(outcome, &mut messages, history, emit, policy, &tools);
                }
                Err(error) => {
                    let reason = error.to_string();
                    outcome.failed = true;
                    outcome.failure_reason = Some("llm_error".to_string());
                    sync_history(history, &messages);
                    emit(AgentEvent::Failed { reason });
                    return outcome;
                }
            };

            if cancel.is_cancelled() {
                return cancelled(outcome, &mut messages, history, emit, policy, &tools);
            }

            let step = step as usize;
            if !result.text.is_empty() {
                outcome.text.push_str(&result.text);
            }
            if !result.reasoning.is_empty() {
                outcome.reasoning_chars += result.reasoning.chars().count();
                // 只带长度，不带内容：思考过程不展示。
                emit(AgentEvent::Reasoning {
                    step,
                    chars: result.reasoning.chars().count(),
                });
            }

            // ---- 响应完整性硬闸 ----------------------------------------------
            if result.tool_calls_incomplete
                && (!result.tool_calls.is_empty() || !result.broken_tool_calls.is_empty())
            {
                {
                    messages.push(assistant_message_from(
                        &result,
                        self.config.echo_reasoning,
                        &self.llm.provider_id(),
                    ));
                    let note = if result.finish_reason == "length" {
                        "（上一步的模型响应不完整（输出被长度上限截断），未执行任何工具调用。请重新组织你的回答；如果需要工具，请完整地重新发起调用。）"
                    } else if result.interrupted {
                        "（上一步的模型响应被中断，未执行任何工具调用。）"
                    } else {
                        "（上一步的模型响应不完整，未执行任何工具调用。请重新组织你的回答；如果需要工具，请完整地重新发起调用。）"
                    };
                    messages.push(ChatMessage::internal_note(note));
                    let broken_names: Vec<String> = result
                        .tool_calls
                        .iter()
                        .chain(result.broken_tool_calls.iter())
                        .map(|call| {
                            if call.name.is_empty() {
                                "(未知工具)".to_string()
                            } else {
                                call.name.clone()
                            }
                        })
                        .collect();
                    for name in &broken_names {
                        outcome.steps.push(StepRecord {
                            step,
                            tool: name.clone(),
                            ok: false,
                            text: "响应不完整，未执行".to_string(),
                            ms: 0,
                        });
                        emit(AgentEvent::ToolEnd {
                            name: name.clone(),
                            ok: false,
                            text: "响应不完整，未执行".to_string(),
                            step,
                        });
                    }
                    if step as u32 == self.config.max_steps {
                        hit_step_limit = true;
                    }
                    continue;
                }
            }

            // ---- 纯文本回复（或错误流） --------------------------------------
            if result.tool_calls.is_empty() && result.broken_tool_calls.is_empty() {
                let stream_failed = matches!(
                    result.finish_reason.as_str(),
                    "incomplete" | "length" | "content_filter" | "repetition_truncation"
                ) || result.tool_calls_incomplete;
                if stream_failed {
                    if outcome.text.is_empty() {
                        outcome.text = "（模型响应异常，未能完成回答）".to_string();
                    }
                    if !result.text.is_empty() {
                        messages.push(assistant_message_from(
                            &result,
                            self.config.echo_reasoning,
                            &self.llm.provider_id(),
                        ));
                    }
                    outcome.failed = true;
                    outcome.failure_reason = Some("stream_error".to_string());
                    sync_history(history, &messages);
                    emit(AgentEvent::Failed {
                        reason: format!("模型响应异常（finish_reason={}）", result.finish_reason),
                    });
                    return outcome;
                }
                messages.push(assistant_message_from(
                    &result,
                    self.config.echo_reasoning,
                    &self.llm.provider_id(),
                ));
                outcome.text = if outcome.text.is_empty() {
                    result.text.clone()
                } else {
                    outcome.text.clone()
                };
                break;
            }

            // ---- 只有残缺调用：一条都不执行 -----------------------------------
            if result.tool_calls.is_empty() {
                // 与混合批次同理：没执行过，就不能在历史里留下"声明了却没结果"的调用。
                let mut assistant = assistant_message_from(
                    &result,
                    self.config.echo_reasoning,
                    &self.llm.provider_id(),
                );
                assistant.tool_calls.clear();
                messages.push(assistant);
                let names: Vec<String> = result
                    .broken_tool_calls
                    .iter()
                    .map(|call| {
                        if call.name.is_empty() {
                            String::new()
                        } else {
                            format!("（{}）", call.name)
                        }
                    })
                    .collect();
                messages.push(ChatMessage::internal_note(format!(
                    "（上一步的工具调用参数不完整，没有执行{}。请重新给出完整参数，或直接用一句话告诉用户做不到以及原因。）",
                    names.join("")
                )));
                for call in &result.broken_tool_calls {
                    let name = if call.name.is_empty() {
                        "(未知工具)".to_string()
                    } else {
                        call.name.clone()
                    };
                    outcome.steps.push(StepRecord {
                        step,
                        tool: name.clone(),
                        ok: false,
                        text: "参数不完整，未执行".to_string(),
                        ms: 0,
                    });
                    emit(AgentEvent::ToolEnd {
                        name,
                        ok: false,
                        text: "参数不完整，未执行".to_string(),
                        step,
                    });
                }
                if step as u32 == self.config.max_steps {
                    hit_step_limit = true;
                }
                continue;
            }

            /*
             * ---- 同批里有残缺调用：整批一条都不执行 -------------------------
             *
             * 模型的"一次响应"是一个整体意图：`["先点A", "再输入半截…"]` 里的第一条
             * 看着合法，但它是建立在一条**没写完**的指令上的。只执行"看起来对"的那几条，
             * 等于替模型猜它想干什么——对点击/输入这类动作就是事故。
             *
             * 处理方式与"只有残缺调用"完全一致：assistant 消息只带**全部调用**
             * （不给残缺的那些补 tool 结果，避免孤立的 tool 消息），
             * 再用一条内部提示说明"这批没执行，请完整重新发起"。
             */
            if !result.broken_tool_calls.is_empty() {
                let all: Vec<&crate::llm::ToolCall> = result
                    .tool_calls
                    .iter()
                    .chain(result.broken_tool_calls.iter())
                    .collect();
                let names: Vec<String> = all
                    .iter()
                    .map(|call| {
                        if call.name.is_empty() {
                            "(未知工具)".to_string()
                        } else {
                            call.name.clone()
                        }
                    })
                    .collect();
                /*
                 * 写进历史的 assistant 消息**不能带任何 tool_calls**。
                 *
                 * 这一批一条都没执行；只要在 `tool_calls` 里留下 id，协议就要求
                 * 后面必须跟一条对应的 `tool` 结果。留下"声明了却没结果"的调用，
                 * 下一次请求会被严格端点判成非法（400），宽松端点也会把模型带偏。
                 * 文字部分（模型已经说出口的话）保留，只把调用记录去掉。
                 */
                let mut assistant = assistant_message_from(
                    &result,
                    self.config.echo_reasoning,
                    &self.llm.provider_id(),
                );
                assistant.tool_calls.clear();
                messages.push(assistant);
                messages.push(ChatMessage::internal_note(format!(
                    "（上一步这一批工具调用里有参数不完整或缺少函数名的调用（{}），整批都没有执行。请重新给出这一批调用的完整参数，或直接用一句话告诉用户做不到以及原因。）",
                    names.join("、")
                )));
                for name in &names {
                    outcome.steps.push(StepRecord {
                        step,
                        tool: name.clone(),
                        ok: false,
                        text: "参数不完整，整批未执行".to_string(),
                        ms: 0,
                    });
                    emit(AgentEvent::ToolEnd {
                        name: name.clone(),
                        ok: false,
                        text: "参数不完整，整批未执行".to_string(),
                        step,
                    });
                }
                if step as u32 == self.config.max_steps {
                    hit_step_limit = true;
                }
                continue;
            }

            // ---- 正常的一批工具调用 -------------------------------------------
            messages.push(assistant_message_from(
                &result,
                self.config.echo_reasoning,
                &self.llm.provider_id(),
            ));

            let mut pending_images: Vec<ChatMessage> = Vec::new();
            for call in result.tool_calls.clone() {
                if cancel.is_cancelled() {
                    return cancelled(outcome, &mut messages, history, emit, policy, &tools);
                }
                let args = parse_args(&call.arguments);
                emit(AgentEvent::ToolStart {
                    name: call.name.clone(),
                    args: args.clone(),
                    step,
                });

                // ---- 权限判断（执行前） -----------------------------------
                let decision = PermissionGate::new(&self.permissions)
                    .check(&call.name, &args, Some(confirmer), cancel)
                    .await;
                if cancel.is_cancelled() {
                    return cancelled(outcome, &mut messages, history, emit, policy, &tools);
                }
                if decision.denied() {
                    let denied = format!("已拒绝：{}", decision.reason);
                    messages.push(ChatMessage::tool_result(call.id.clone(), denied.clone()));
                    outcome.steps.push(StepRecord {
                        step,
                        tool: call.name.clone(),
                        ok: false,
                        text: denied.clone(),
                        ms: 0,
                    });
                    emit(AgentEvent::ToolEnd {
                        name: call.name.clone(),
                        ok: false,
                        text: denied,
                        step,
                    });
                    continue;
                }

                // ---- 执行 -------------------------------------------------
                let started = std::time::Instant::now();
                let executed = self.tools.invoke(&call.name, &args, cancel).await;
                if cancel.is_cancelled() {
                    return cancelled(outcome, &mut messages, history, emit, policy, &tools);
                }
                match executed {
                    Ok(execution) => {
                        let text = clamp(&execution.text, TOOL_RESULT_MAX_CHARS);
                        let ms = started.elapsed().as_millis() as u64;
                        messages.push(ChatMessage::tool_result(call.id.clone(), text.clone()));
                        outcome.steps.push(StepRecord {
                            step,
                            tool: call.name.clone(),
                            ok: true,
                            text: text.clone(),
                            ms,
                        });
                        emit(AgentEvent::ToolEnd {
                            name: call.name.clone(),
                            ok: true,
                            text: text.clone(),
                            step,
                        });

                        // 截图：必须把图塞进对话，否则模型对屏幕就是瞎的。
                        if let Some(image) = execution.image {
                            if images_attached < MAX_IMAGES_PER_RUN {
                                images_attached += 1;
                                let size = match (image.width, image.height) {
                                    (Some(width), Some(height)) => format!("{width}x{height}"),
                                    _ => "?".to_string(),
                                };
                                pending_images.push(ChatMessage::image_attachment(
                                    format!("（这是 {} 刚返回的图，{}）", call.name, size),
                                    &image.media_type,
                                    &image.base64,
                                ));
                            } else {
                                // 超上限也**不能静默丢图**：模型会以为屏幕没变化而继续瞎点。
                                pending_images.push(ChatMessage::internal_note(format!(
                                    "（这一轮已经附了 {} 张截图，达到单轮上限，{} 的这张图没有附上。请改用 get_app_state 这类文字描述来确认界面状态，不要假设屏幕没变化。）",
                                    images_attached, call.name
                                )));
                                emit(AgentEvent::ToolEnd {
                                    name: call.name.clone(),
                                    ok: true,
                                    text: "截图超出单轮上限，未附图".to_string(),
                                    step,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        let message = if error.is_cancelled() {
                            return cancelled(
                                outcome,
                                &mut messages,
                                history,
                                emit,
                                policy,
                                &tools,
                            );
                        } else {
                            format!("工具执行失败：{}", error.user_message())
                        };
                        messages.push(ChatMessage::tool_result(call.id.clone(), message.clone()));
                        outcome.steps.push(StepRecord {
                            step,
                            tool: call.name.clone(),
                            ok: false,
                            text: message.clone(),
                            ms: started.elapsed().as_millis() as u64,
                        });
                        emit(AgentEvent::ToolEnd {
                            name: call.name.clone(),
                            ok: false,
                            text: message,
                            step,
                        });
                    }
                }
            }
            // 这一批工具结果已经齐了，截图附件现在才能进来（不会插在结果之间）。
            messages.extend(pending_images);

            if step as u32 == self.config.max_steps {
                hit_step_limit = true;
            }
        }

        // ---- 步数用满时补一次"交代"往返 --------------------------------------
        if hit_step_limit && !outcome.steps.is_empty() && !cancel.is_cancelled() {
            let note_index = messages.len();
            messages.push(ChatMessage::internal_note(format!(
                "（已经用满 {} 步，不能再调工具了。用一句话告诉用户：你做到哪一步、还差什么、需要他怎么配合。）",
                self.config.max_steps
            )));
            let request_messages = strip_local_markers(&messages);
            match self
                .llm
                .chat(&request_messages, &[], cancel, |_| {}, |_| {})
                .await
            {
                Ok(closing) => {
                    // 这条内部提示不属于对话内容，拿到回复后就撤掉。
                    messages.remove(note_index);
                    if !closing.text.is_empty() {
                        outcome.text.push_str(&closing.text);
                        messages.push(assistant_message_from(
                            &closing,
                            self.config.echo_reasoning,
                            &self.llm.provider_id(),
                        ));
                    }
                    if closing.tool_calls_incomplete && closing.text.is_empty() {
                        outcome.failed = true;
                        outcome.failure_reason = Some("stream_error".to_string());
                    }
                }
                Err(LlmError::Aborted) => {
                    messages.remove(note_index);
                    return cancelled(outcome, &mut messages, history, emit, policy, &tools);
                }
                Err(error) => {
                    messages.remove(note_index);
                    let fallback = format!("（步数用完了，没能收尾：{error}）");
                    messages.push(ChatMessage::text(Role::Assistant, fallback.clone()));
                    outcome.text.push_str(&fallback);
                    outcome.failed = true;
                    outcome.failure_reason = Some("step_limit".to_string());
                }
            }
        }

        outcome.limited = hit_step_limit;
        outcome.context_tokens = estimate_request_tokens(&messages, &tools);
        sync_history(history, &messages);
        emit(AgentEvent::Done(outcome.clone()));
        outcome
    }
}

/// 把工作集写回会话历史（`messages[0]` 是 system，不进入历史，每轮由 Agent 自己加）。
///
/// 压缩也作用在这份工作集上，所以历史会随之变短——这正是"到 90% 才压"的落点：
/// 被折叠的老回合不再随会话累积。
fn sync_history(history: &mut Vec<ChatMessage>, messages: &[ChatMessage]) {
    history.clear();
    history.extend(messages.iter().skip(1).cloned());
}

/// 取消时的收尾：丢弃本轮未完成的一切，不触发任何新工具。
///
/// 关键：取消可能发生在"一批工具调用**执行到一半**"。
/// 那时 assistant 消息已经带着全部 `tool_calls` 进了历史，但只有前面的几条有结果。
/// 直接把这样一段历史留到下一轮，就是"声明了却没结果"的协议错误。
///
/// 所以这里给**还没执行**的调用补一条明确的"未执行"结果：
///  - 配对完整，下一轮请求合法；
///  - 已完成的那些结果原样保留（已发生的操作不能被抹掉）；
///  - 模型能看到"这几条因为中断没做"，不会误以为做过。
fn cancelled(
    mut outcome: TurnOutcome,
    messages: &mut Vec<ChatMessage>,
    history: &mut Vec<ChatMessage>,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    policy: ContextPolicy,
    tools: &[Value],
) -> TurnOutcome {
    outcome.cancelled = true;
    outcome.failed = true;
    outcome.failure_reason = Some("cancelled".to_string());
    if outcome.text.is_empty() {
        outcome.text = "已中断".to_string();
    }
    backfill_unanswered_tool_calls(messages);
    outcome.context_tokens = estimate_request_tokens(messages, tools);
    outcome.context_window = policy.context_window;
    sync_history(history, messages);
    emit(AgentEvent::Failed {
        reason: "已中断".to_string(),
    });
    outcome
}

/// 给历史里"声明了但没有结果"的 `tool_calls` 补一条未执行说明。
///
/// 只补缺失的那些，已经有的结果一个都不动——已完成的执行结果必须保留。
fn backfill_unanswered_tool_calls(messages: &mut Vec<ChatMessage>) {
    // 先收集历史里所有已存在的 tool 结果 id。
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in messages.iter() {
        if message.role == Role::Tool {
            if let Some(id) = &message.tool_call_id {
                answered.insert(id.clone());
            }
        }
    }
    /*
     * 逐条补：`tool` 消息必须**紧跟**声明它的 assistant 之后，
     * 不能统一追加到末尾——中间隔了别的消息就又是孤立结果。
     * 这里从后往前插，插入位置不受后续改动影响。
     */
    let mut insertions: Vec<(usize, Vec<String>)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::Assistant || message.tool_calls.is_empty() {
            continue;
        }
        let missing: Vec<String> = message
            .tool_calls
            .iter()
            .map(|call| call.id.clone())
            .filter(|id| !answered.contains(id))
            .collect();
        if !missing.is_empty() {
            insertions.push((index, missing));
        }
    }
    for (index, missing) in insertions.into_iter().rev() {
        let mut inserts: Vec<ChatMessage> = missing
            .into_iter()
            .map(|id| ChatMessage::tool_result(id, "（这一条因为中断没有执行。）"))
            .collect();
        // 插在 assistant 之后、它已有结果之前——顺序不影响协议，紧跟即可。
        inserts.reverse();
        for message in inserts {
            messages.insert(index + 1, message);
        }
    }
}

/// 解析模型给的参数文本。解析失败返回空对象——但那种调用**不会**走到这里
/// （`valid == false` 的调用在 Agent 之前就被拦下了）。
fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => value,
        Ok(other) => serde_json::json!({ "__raw": other }),
        Err(_) => serde_json::json!({ "__raw": raw }),
    }
}

fn clamp(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!(
        "{head}\n…（结果过长已截断：原文 {length} 字，只保留前 {max} 字。如果判断还需要后面的内容，请换更精确的命令/参数重新获取，不要凭空推测。）"
    )
}

/// 去掉只属于本地的标记（internal 等），得到真正可以发出去的 messages。
fn strip_local_markers(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        if message.internal {
            // 内部提示是程序生成的 user 消息，**必须**发出去（模型要看到它），
            // 但标记本身不进请求体——`to_request_value` 已经不再序列化它。
            out.push(message.clone());
        } else {
            out.push(message.clone());
        }
    }
    out
}

/// 供测试与界面使用的便捷方法：本轮增量。
pub fn turn_increment(history: &[ChatMessage], turn_start_index: usize) -> Vec<ChatMessage> {
    history.iter().skip(turn_start_index).cloned().collect()
}

/// 判断一条消息是否是截图附件（界面据此不把它当正文）。
pub fn is_image_message(message: &ChatMessage) -> bool {
    matches!(&message.content, Content::Parts(parts)
        if parts.iter().any(|part| part.get("type").and_then(Value::as_str) == Some("image_url")))
}

/// 一个确认器的直通实现：把请求交给回调并等待答复。
pub struct CallbackConfirmer<F>(pub F);

impl<F> Confirmer for CallbackConfirmer<F>
where
    F: Fn(
            ConfirmationPrompt,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::permissions::ConfirmAnswer> + Send>,
        > + Send
        + Sync,
{
    fn confirm<'a>(
        &'a self,
        request: crate::permissions::ConfirmRequest,
        _cancel: &'a crate::cancel::CancelToken,
    ) -> crate::permissions::ConfirmFuture<'a> {
        let prompt = ConfirmationPrompt {
            tool: request.tool,
            capability: request.capability.to_string(),
            label: request.label.to_string(),
            reason: request.reason,
            args_summary: request.args_summary,
        };
        (self.0)(prompt)
    }
}

/// 不做任何确认、一律拒绝的确认器（无界面环境/测试用）。
pub struct DenyAllConfirmer;

impl Confirmer for DenyAllConfirmer {
    fn confirm<'a>(
        &'a self,
        _request: crate::permissions::ConfirmRequest,
        _cancel: &'a crate::cancel::CancelToken,
    ) -> crate::permissions::ConfirmFuture<'a> {
        Box::pin(async { crate::permissions::ConfirmAnswer::deny_click() })
    }
}

/// 一律允许的确认器（仅用于自动化测试）。
pub struct AllowAllConfirmer;

impl Confirmer for AllowAllConfirmer {
    fn confirm<'a>(
        &'a self,
        _request: crate::permissions::ConfirmRequest,
        _cancel: &'a crate::cancel::CancelToken,
    ) -> crate::permissions::ConfirmFuture<'a> {
        Box::pin(async { crate::permissions::ConfirmAnswer::allow_click() })
    }
}

/// 判断一个决策是否属于 `ask`（界面据此提示用户）。
pub fn is_ask(decision: &Decision) -> bool {
    decision.level == crate::permissions::Level::Ask
}

/// 取消错误转成 Agent 事件的可读文案。
pub fn cancelled_message(error: &Cancelled) -> String {
    error.to_string()
}

/// 未使用的占位：保持 `ChatResult` 在公开签名里可见（便于下游扩展）。
pub fn last_result_text(result: &ChatResult) -> &str {
    &result.text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_marks_truncation() {
        let text = "x".repeat(100);
        let clamped = clamp(&text, 10);
        assert!(clamped.starts_with("xxxxxxxxxx"));
        assert!(clamped.contains("已截断"));
        assert!(clamped.contains("100 字"));
    }

    #[test]
    fn clamp_keeps_short_text_intact() {
        assert_eq!(clamp("短", 10), "短");
    }

    #[test]
    fn parse_args_handles_empty_and_non_object() {
        assert_eq!(parse_args(""), serde_json::json!({}));
        assert_eq!(parse_args("{\"a\":1}"), serde_json::json!({"a":1}));
        assert!(parse_args("[1,2]").get("__raw").is_some());
        assert!(parse_args("{bad").get("__raw").is_some());
    }

    #[test]
    fn image_message_detection() {
        let image = ChatMessage::image_attachment("（图 1x1）", "image/jpeg", "AAAA");
        assert!(is_image_message(&image));
        assert!(!is_image_message(&ChatMessage::user("hi")));
    }
}
