//! 端到端集成测试：Mock LLM（真实 HTTP/SSE）+ 假工具后端。
//!
//! 为什么必须打真实 HTTP：推理参数、`reasoning_content` 回传、SSE 分片工具调用
//! 这三件事**只能靠看真实发出去的字节**验证。这里起一个 tokio TCP 监听器当
//! OpenAI 兼容端点，逐行断言请求体与流式行为，不花钱、不联网。
//!
//! 覆盖（对应 Done Criteria）：
//!  - 一次及多次工具调用 → 权限判断 → 工具结果 → 最终回复
//!  - `ask` 的允许 / 拒绝路径（拒绝时工具调用次数为零）
//!  - 取消后迟到的响应不触发新工具
//!  - MiMo / DeepSeek / 标准端点的请求字段
//!  - 残缺参数不执行

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use cuava::agent::{Agent, AgentConfig, AgentEvent, ToolExecution, ToolInvoker};
use cuava::cancel::CancelToken;
use cuava::config::{PermissionMode, ReasoningEffort, Settings, ThinkingLevel};
use cuava::context::resolve_context_policy;
use cuava::cua::tools::ToolError;
use cuava::llm::{ChatMessage, LlmClient};
use cuava::permissions::{
    ConfirmAnswer, ConfirmFuture, ConfirmRequest, Confirmer, PermissionState,
};

// ---------------------------------------------------------------------------
// Mock OpenAI 兼容端点（真实 HTTP + SSE）
// ---------------------------------------------------------------------------

/// 一次收到的请求。
#[derive(Debug, Clone)]
struct Recorded {
    /// 只有 `body` 参与断言；`path` 留在记录里便于排查（见 `bodies()` 的注释）。
    #[allow(dead_code)]
    path: String,
    body: Value,
}

/// 剧本：第 n 次请求返回什么。
type Plan = Box<dyn Fn(usize, &Value) -> Vec<String> + Send + Sync>;

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// 启动一个流式 mock 端点。
    async fn start(plan: Plan) -> MockServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let plan = Arc::new(plan);
        let counter = Arc::new(AtomicUsize::new(0));

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                let plan = plan.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut total = Vec::new();
                    // 读到 header 结束 + content-length 指定的 body
                    let (head, body_bytes) = loop {
                        let Ok(read) = socket.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        total.extend_from_slice(&buffer[..read]);
                        if let Some(header_end) = find_header_end(&total) {
                            let head = String::from_utf8_lossy(&total[..header_end]).to_string();
                            let length = content_length(&head);
                            let body_start = header_end + 4;
                            if total.len() >= body_start + length {
                                break (head, total[body_start..body_start + length].to_vec());
                            }
                        } else if total.len() > 8 * 1024 * 1024 {
                            return;
                        }
                    };
                    let path = head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
                    recorded.lock().unwrap().push(Recorded {
                        path: path.clone(),
                        body: body.clone(),
                    });
                    let attempt = counter.fetch_add(1, Ordering::SeqCst);
                    let events = plan(attempt, &body);
                    // SSE 事件之间必须是**空行**（`\n\n`）：只写一个 `\n` 会把多条
                    // data 拼成同一个事件，解析出来的就不是预期的增量了。
                    //
                    // 用 chunked 编码、一个事件一个 chunk：真实端点也是分块推送，
                    // 客户端不该依赖"一次读完整个响应"。
                    let mut response = String::from(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                    );
                    for event in events {
                        let chunk = format!("{event}\n\n");
                        response.push_str(&format!("{:x}\r\n{}\r\n", chunk.len(), chunk));
                    }
                    response.push_str("0\r\n\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        MockServer {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.body.clone())
            .collect()
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(head: &str) -> usize {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

fn sse(data: Value) -> String {
    format!("data: {data}")
}

fn done() -> String {
    "data: [DONE]".to_string()
}

/// 一条只带正文的增量。
fn text_delta(text: &str) -> String {
    sse(json!({"choices":[{"delta":{"content":text}}]}))
}

/// 一条思考增量。
fn reasoning_delta(text: &str) -> String {
    sse(json!({"choices":[{"delta":{"reasoning_content":text}}]}))
}

/// 一条工具调用增量。
fn tool_delta(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> String {
    let mut function = serde_json::Map::new();
    if let Some(name) = name {
        function.insert("name".to_string(), json!(name));
    }
    if let Some(args) = args {
        function.insert("arguments".to_string(), json!(args));
    }
    let mut delta = serde_json::Map::new();
    delta.insert("index".to_string(), json!(index));
    if let Some(id) = id {
        delta.insert("id".to_string(), json!(id));
    }
    delta.insert("function".to_string(), Value::Object(function));
    sse(json!({"choices":[{"delta":{"tool_calls":[delta]}}]}))
}

fn stop_event(reason: &str) -> String {
    sse(json!({"choices":[{"delta":{},"finish_reason":reason}]}))
}

// ---------------------------------------------------------------------------
// 假工具后端
// ---------------------------------------------------------------------------

/// 记录每次执行，并按剧本返回结果。
struct FakeTools {
    calls: Mutex<Vec<(String, Value)>>,
    delay_ms: u64,
    image: bool,
}

impl FakeTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            delay_ms: 0,
            image: false,
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

impl ToolInvoker for FakeTools {
    fn tool_definitions(&self) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"list_windows","description":"list","parameters":{"type":"object","properties":{}}}}),
            json!({"type":"function","function":{"name":"get_app_state","description":"state","parameters":{"type":"object","properties":{}}}}),
            json!({"type":"function","function":{"name":"read_clipboard","description":"clip","parameters":{"type":"object","properties":{}}}}),
            json!({"type":"function","function":{"name":"screenshot","description":"shot","parameters":{"type":"object","properties":{}}}}),
            json!({"type":"function","function":{"name":"left_click","description":"click","parameters":{"type":"object","properties":{}}}}),
        ]
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a CancelToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolExecution, ToolError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), args.clone()));
            if self.delay_ms > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(self.delay_ms)) => {}
                    _ = cancel.cancelled() => {
                        return Err(ToolError::Bridge(cuava::cua::BridgeError::Cancelled));
                    }
                }
            }
            Ok(ToolExecution {
                text: format!("<result tool=\"{name}\">ok</result>"),
                image: if name == "screenshot" && self.image {
                    Some(cuava::cua::ImageAttachment {
                        media_type: "image/jpeg".to_string(),
                        base64: "AAAA".to_string(),
                        width: Some(1024),
                        height: Some(768),
                    })
                } else {
                    None
                },
            })
        })
    }
}

// ---------------------------------------------------------------------------
// 确认器
// ---------------------------------------------------------------------------

struct FixedConfirmer {
    allow: bool,
    seen: Mutex<Vec<String>>,
}

impl FixedConfirmer {
    fn new(allow: bool) -> Self {
        Self {
            allow,
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Confirmer for FixedConfirmer {
    fn confirm<'a>(
        &'a self,
        request: ConfirmRequest,
        _cancel: &'a CancelToken,
    ) -> ConfirmFuture<'a> {
        self.seen.lock().unwrap().push(request.tool.clone());
        let answer = if self.allow {
            ConfirmAnswer::allow_click()
        } else {
            ConfirmAnswer::deny_click()
        };
        Box::pin(async move { answer })
    }
}

/// 永远"打不开"的确认器（模拟用户没点按钮就关闭了确认框）。
struct NeverConfirmer;

impl Confirmer for NeverConfirmer {
    fn confirm<'a>(
        &'a self,
        _request: ConfirmRequest,
        _cancel: &'a CancelToken,
    ) -> ConfirmFuture<'a> {
        Box::pin(async {
            ConfirmAnswer {
                allow: false,
                channel: "click",
                reason: "用户没有确认".to_string(),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

fn settings_for(provider: &str, base_url: &str) -> Settings {
    let mut settings = Settings {
        provider_id: provider.to_string(),
        base_url: base_url.to_string(),
        model: "mock-model".to_string(),
        ..Settings::default()
    };
    if provider == "openai-compatible" {
        settings.model = "mock-model".to_string();
    }
    settings
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        max_steps: 8,
        context_policy: resolve_context_policy(Some(1_000_000), Some(90), None),
        system_prompt: "test system prompt".to_string(),
        echo_reasoning: false,
        turn_id: 1,
        use_tools: true,
    }
}

fn client(settings: &Settings) -> LlmClient {
    LlmClient::new(settings.profile(), "", settings.temperature, 10_000).unwrap()
}

/// 跑一轮，收集界面事件。
async fn run_turn(
    agent: &mut Agent,
    input: &str,
    history: &mut Vec<ChatMessage>,
    confirmer: &dyn Confirmer,
    cancel: &CancelToken,
) -> (cuava::agent::TurnOutcome, Vec<String>) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let emit = move |event: AgentEvent| {
        let label = match &event {
            AgentEvent::TextDelta { text, .. } => format!("text:{text}"),
            AgentEvent::ReasoningDone { chars, .. } => format!("reasoning:{chars}"),
            AgentEvent::ReasoningDelta { .. } => "reasoning_delta".to_string(),
            AgentEvent::ToolStart { name, .. } => format!("tool_start:{name}"),
            AgentEvent::ToolEnd { name, status, .. } => {
                format!("tool_end:{name}:{}", status.code())
            }
            AgentEvent::ConfirmationRequired(prompt) => format!("confirm:{}", prompt.tool),
            AgentEvent::Done(_) => "done".to_string(),
            AgentEvent::Failed { reason, .. } => format!("failed:{reason}"),
        };
        sink.lock().unwrap().push(label);
    };
    let outcome = agent.run(input, history, cancel, confirmer, &emit).await;
    let collected = events.lock().unwrap().clone();
    (outcome, collected)
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 一次工具调用 → 权限判断 → 工具结果 → 最终回复。
#[tokio::test]
async fn single_tool_call_closes_the_loop() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("list_windows"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("窗口 3 个。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, events) = run_turn(
        &mut agent,
        "看看有哪些窗口",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    if outcome.failed {
        eprintln!("DEBUG events = {:#?}", events);
        eprintln!("DEBUG count = {}", server.count());
    }
    assert!(
        !outcome.failed,
        "本回合应正常完成：{:?}",
        outcome.failure_reason
    );
    assert_eq!(outcome.text, "窗口 3 个。");
    assert_eq!(tools.names(), vec!["list_windows"]);
    assert_eq!(outcome.steps.len(), 1);
    assert!(outcome.steps[0].ok());
    assert!(events
        .iter()
        .any(|event| event == "tool_start:list_windows"));
    assert!(events.iter().any(|event| event == "done"));
    assert_eq!(server.count(), 2, "应该正好两次模型往返");

    // 第二次请求必须带上工具结果，且工具调用与结果配对
    let bodies = server.bodies();
    let second = bodies[1]["messages"].as_array().unwrap();
    let assistant = second
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .expect("必须回传 assistant.tool_calls");
    assert_eq!(assistant["tool_calls"][0]["id"], json!("call_1"));
    let tool_message = second
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("必须带上工具结果");
    assert_eq!(tool_message["tool_call_id"], json!("call_1"));
    assert!(tool_message["content"]
        .as_str()
        .unwrap()
        .contains("list_windows"));
}

/// 多次工具调用：同一批里的两个调用都要执行并各自配对。
#[tokio::test]
async fn multiple_tool_calls_in_one_batch() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_a"), Some("list_windows"), Some("{}")),
                tool_delta(
                    1,
                    Some("call_b"),
                    Some("get_app_state"),
                    Some("{\"app\":\"notepad\"}"),
                ),
                stop_event("tool_calls"),
                done(),
            ]
        } else if attempt == 1 {
            vec![
                tool_delta(0, Some("call_c"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("都读完了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "读一下",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    assert!(!outcome.failed);
    assert_eq!(
        tools.names(),
        vec!["list_windows", "get_app_state", "read_clipboard"]
    );
    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    let tool_ids: Vec<String> = last
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(tool_ids, vec!["call_a", "call_b", "call_c"]);
}

/// 拒绝：工具调用次数必须为零，且模型收到"已拒绝"。
#[tokio::test]
async fn denial_executes_zero_tool_calls() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("那算了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let confirmer = FixedConfirmer::new(false);
    let (outcome, events) = run_turn(
        &mut agent,
        "读剪贴板",
        &mut history,
        &confirmer,
        &CancelToken::new(),
    )
    .await;

    assert_eq!(tools.call_count(), 0, "拒绝时工具调用次数必须为零");
    assert_eq!(
        confirmer.seen.lock().unwrap().len(),
        1,
        "ask 必须真的问过用户"
    );
    assert!(events
        .iter()
        .any(|event| event == "tool_end:read_clipboard:denied"));
    assert!(outcome
        .steps
        .iter()
        .any(|step| step.text.contains("已拒绝")));

    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    let tool_message = last
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("拒绝也要给模型一条 tool 结果");
    assert!(tool_message["content"].as_str().unwrap().contains("已拒绝"));
}

/// 确认框没点就关闭（无答复）：按拒绝处理，不执行。
#[tokio::test]
async fn closed_confirmation_denies_execution() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("好。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "读剪贴板",
        &mut history,
        &NeverConfirmer,
        &CancelToken::new(),
    )
    .await;
    assert_eq!(tools.call_count(), 0);
}

/// 普通许可操作不能平白弹确认。
#[tokio::test]
async fn allowed_tools_never_prompt() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("list_windows"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("好。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let confirmer = FixedConfirmer::new(true);
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "看看窗口",
        &mut history,
        &confirmer,
        &CancelToken::new(),
    )
    .await;
    assert!(
        confirmer.seen.lock().unwrap().is_empty(),
        "普通许可操作不该弹确认"
    );
    assert_eq!(tools.call_count(), 1);
}

/// 残缺参数（非法 JSON）不执行任何工具。
#[tokio::test]
async fn malformed_arguments_are_not_executed() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("left_click"), Some("{oops")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("参数坏了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "点一下",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    assert_eq!(tools.call_count(), 0, "半截参数绝不能执行");
    // 与旧版一致：`finish_reason=tool_calls` 但参数坏掉时，整条响应按"不完整"处理，
    // 一个工具都不执行，并把原因如实写进步骤记录。
    assert!(outcome
        .steps
        .iter()
        .any(|step| step.text.contains("参数不完整") || step.text.contains("响应不完整")));
}

/// 响应被截断（finish_reason=length）：一个工具都不执行。
#[tokio::test]
async fn truncated_response_never_executes_tools() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![
            tool_delta(0, Some("call_1"), Some("list_windows"), Some("{}")),
            stop_event("length"),
            done(),
        ]
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "看看",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    assert_eq!(tools.call_count(), 0, "被截断的响应不得触发工具");
}

/// 取消：迟到的模型响应不能触发新工具。
///
/// 端点先"思考"足够久，取消一定发生在请求进行中；等令牌真正取消后
/// 流才把工具调用吐出来。此时 Agent 必须丢弃这次响应，一个工具都不执行。
#[tokio::test]
async fn cancellation_after_response_blocks_tool_execution() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![
            text_delta("我先看看…"),
            tool_delta(0, Some("call_1"), Some("list_windows"), Some("{}")),
            stop_event("tool_calls"),
            done(),
        ]
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let cancel = CancelToken::new();
    // 在请求发出**之前**就取消，覆盖"取消后迟到的响应"这条路径。
    cancel.cancel();

    let (outcome, _) = run_turn(
        &mut agent,
        "看看窗口",
        &mut history,
        &FixedConfirmer::new(true),
        &cancel,
    )
    .await;

    assert!(outcome.cancelled);
    assert_eq!(tools.call_count(), 0, "取消后不得执行任何工具");
}

/// 取消发生在工具执行期间：不会再发起下一批工具。
#[tokio::test]
async fn cancellation_during_tool_run_stops_the_loop() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("list_windows"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![
                tool_delta(0, Some("call_2"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        }
    }))
    .await;

    let tools = Arc::new(FakeTools {
        calls: Mutex::new(Vec::new()),
        delay_ms: 200,
        image: false,
    });
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        token.cancel();
    });

    let (outcome, _) = run_turn(
        &mut agent,
        "看看窗口",
        &mut history,
        &FixedConfirmer::new(true),
        &cancel,
    )
    .await;

    assert!(outcome.cancelled);
    assert_eq!(
        tools.names(),
        vec!["list_windows"],
        "取消后不得发起第二批工具"
    );
}

// ---------------------------------------------------------------------------
// 请求字段（提供商协议）
// ---------------------------------------------------------------------------

/// MiMo：默认不发 thinking；开启时只发 thinking 开关；V2.6 思考下不发温度。
#[tokio::test]
async fn mimo_request_fields_match_policy() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![text_delta("稳。"), stop_event("stop"), done()]
    }))
    .await;

    // 默认档
    let mut settings = settings_for("mimo", &server.base_url);
    settings.model = "mimo-v2.6-flash".to_string();
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let body = server.bodies().last().unwrap().clone();
    assert!(body.get("thinking").is_none(), "默认档不传 thinking");
    assert!(
        body.get("temperature").is_none(),
        "V2.6 思考模式下温度会被强制覆盖，不该发"
    );
    assert!(body.get("reasoning_effort").is_none(), "MiMo 没有强度档");

    // 显式关闭思考
    settings.thinking = ThinkingLevel::Disabled;
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let _ = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let body = server.bodies().last().unwrap().clone();
    assert_eq!(body["thinking"], json!({"type":"disabled"}));
    assert!(
        body.get("temperature").is_some(),
        "关闭思考后温度应照常发送"
    );
}

/// DeepSeek：只发 reasoning_effort 一个字段，绝不发 thinking。
#[tokio::test]
async fn deepseek_request_fields_match_policy() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![text_delta("稳。"), stop_event("stop"), done()]
    }))
    .await;

    let mut settings = settings_for("deepseek", &server.base_url);
    settings.reasoning_effort = ReasoningEffort::High;
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let body = server.bodies().last().unwrap().clone();
    assert_eq!(body["reasoning_effort"], json!("high"));
    assert!(body.get("thinking").is_none(), "两个字段不能混发");
    assert!(body.get("temperature").is_none(), "思考模式不支持温度");
}

/// 标准兼容端点：一个提供商专属字段都不发。
#[tokio::test]
async fn plain_endpoint_sends_no_vendor_fields() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![text_delta("ok"), stop_event("stop"), done()]
    }))
    .await;

    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let body = server.bodies().last().unwrap().clone();
    for field in ["thinking", "reasoning_effort", "reasoning"] {
        assert!(body.get(field).is_none(), "标准端点不得发送 {field}");
    }
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["tool_choice"], json!("auto"));
}

/// reasoning_content 与可见回复分离；MiMo 要求回传。
#[tokio::test]
async fn reasoning_is_separated_and_echoed_for_mimo() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                reasoning_delta("先想一下"),
                text_delta("在。"),
                stop_event("stop"),
                done(),
            ]
        } else {
            vec![text_delta("嗯。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let mut settings = settings_for("mimo", &server.base_url);
    settings.model = "mimo-v2.5-flash".to_string();
    let mut config = agent_config();
    config.echo_reasoning = true;
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        config,
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, events) = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    // 思考内容不进入用户可见正文
    assert_eq!(outcome.text, "在。");
    assert!(!outcome.text.contains("先想一下"));
    // 只上报长度，不上报内容
    assert!(events.iter().any(|event| event.starts_with("reasoning:")));
    assert!(
        !events.iter().any(|event| event.contains("先想一下")),
        "思考内容绝不能出现在界面事件里"
    );
    assert!(outcome.reasoning_chars > 0);

    // 下一轮请求里 assistant 必须带回 reasoning_content（否则 MiMo 会 400）
    let _ = run_turn(
        &mut agent,
        "再说一次",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    let assistant = last
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("历史里必须保留上一条 assistant");
    assert_eq!(assistant["reasoning_content"], json!("先想一下"));
}

/// 标准端点绝不回传 reasoning_content（私有字段可能被判非法）。
#[tokio::test]
async fn plain_endpoint_never_echoes_reasoning() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                reasoning_delta("想"),
                text_delta("答"),
                stop_event("stop"),
                done(),
            ]
        } else {
            vec![text_delta("嗯"), stop_event("stop"), done()]
        }
    }))
    .await;

    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let _ = run_turn(
        &mut agent,
        "hi2",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    for message in last {
        assert!(
            message.get("reasoning_content").is_none(),
            "标准端点不得回传 reasoning_content"
        );
    }
}

/// 截图结果按真实 media type 交给多模态模型（data URI）。
#[tokio::test]
async fn screenshot_is_attached_with_real_media_type() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("screenshot"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("看到了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = Arc::new(FakeTools {
        calls: Mutex::new(Vec::new()),
        delay_ms: 0,
        image: true,
    });
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools,
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "截图",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    let image_message = last
        .iter()
        .find(|message| message["content"].is_array())
        .expect("截图必须以数组内容的形式进入对话");
    let parts = image_message["content"].as_array().unwrap();
    let image_part = parts
        .iter()
        .find(|part| part["type"] == json!("image_url"))
        .expect("必须带 image_url 段");
    let url = image_part["image_url"]["url"].as_str().unwrap();
    assert!(
        url.starts_with("data:image/jpeg;base64,"),
        "必须带真实 media type：{url}"
    );
}

/// 首次请求必须带 system 提示词与工具定义。
#[tokio::test]
async fn first_request_carries_system_prompt_and_tools() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![text_delta("ok"), stop_event("stop"), done()]
    }))
    .await;

    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let body = server.bodies().last().unwrap().clone();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], json!("system"));
    assert_eq!(messages[0]["content"], json!("test system prompt"));
    assert!(body["tools"].as_array().unwrap().len() >= 5);
}

/// 服务端错误（非 2xx）要如实失败，不能当正常回答。
#[tokio::test]
async fn http_error_surfaces_as_failure() {
    // 起一个直接返回 500 的端点。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = vec![0u8; 16 * 1024];
            let _ = socket.read(&mut buffer).await;
            let body = "{\"error\":{\"message\":\"boom\"}}";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });

    let settings = settings_for("openai-compatible", &format!("http://127.0.0.1:{port}/v1"));
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    assert!(outcome.failed);
    assert_eq!(outcome.failure_reason.as_deref(), Some("llm_error"));
}

/// SSE 里的 `event: error` 必须被识别为失败流。
#[tokio::test]
async fn sse_error_event_is_treated_as_failure() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![
            "event: error".to_string(),
            sse(json!({"message":"quota exceeded"})),
            done(),
        ]
    }))
    .await;

    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "hi",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    assert!(outcome.failed);
}

/// 上下文装不下时如实失败，而不是静默截断用户指令。
#[tokio::test]
async fn context_overflow_fails_loudly() {
    let server = MockServer::start(Box::new(|_attempt, _body| {
        vec![text_delta("不应到这里"), stop_event("stop"), done()]
    }))
    .await;

    let settings = settings_for("openai-compatible", &server.base_url);
    let mut config = agent_config();
    config.context_policy = resolve_context_policy(Some(1_000), Some(50), None);
    let mut agent = Agent::new(
        client(&settings),
        FakeTools::new(),
        config,
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let huge = "文".repeat(20_000);
    let (outcome, events) = run_turn(
        &mut agent,
        &huge,
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    assert!(outcome.failed);
    assert_eq!(outcome.failure_reason.as_deref(), Some("context_overflow"));
    assert!(events.iter().any(|event| event.starts_with("failed:")));
    assert_eq!(server.count(), 0, "装不下就不该发出请求");
}

/// 权限档位：宽松档把剪贴板读取降为直通（不弹确认），但未知工具仍拒绝。
#[tokio::test]
async fn permissive_mode_still_denies_unknown_tools() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("好。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Permissive),
    );
    let confirmer = FixedConfirmer::new(false);
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "读剪贴板",
        &mut history,
        &confirmer,
        &CancelToken::new(),
    )
    .await;
    assert_eq!(tools.call_count(), 1, "宽松档下剪贴板读取不需要确认");
    assert!(confirmer.seen.lock().unwrap().is_empty());
}

/// 工具名不在白名单（模型幻觉出一个工具）→ 不执行。
#[tokio::test]
async fn hallucinated_tool_is_denied() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_1"), Some("format_disk"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("做不到。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "格式化磁盘",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    assert_eq!(tools.call_count(), 0);
    assert!(outcome
        .steps
        .iter()
        .any(|step| step.text.contains("未知工具") || step.text.contains("已拒绝")));
}

/// 历史在多轮之间保持工具调用与结果的配对（协议合法性）。
#[tokio::test]
async fn history_keeps_tool_pairs_across_turns() {
    let server = MockServer::start(Box::new(|attempt, _body| match attempt {
        0 => vec![
            tool_delta(0, Some("call_1"), Some("list_windows"), Some("{}")),
            stop_event("tool_calls"),
            done(),
        ],
        1 => vec![text_delta("第一轮完。"), stop_event("stop"), done()],
        2 => vec![text_delta("第二轮完。"), stop_event("stop"), done()],
        _ => vec![text_delta("ok"), stop_event("stop"), done()],
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "第一轮",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;
    let _ = run_turn(
        &mut agent,
        "第二轮",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    let mut declared: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for message in last {
        if message["role"] == json!("assistant") {
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    declared.push(call["id"].as_str().unwrap().to_string());
                }
            }
        }
        if message["role"] == json!("tool") {
            results.push(message["tool_call_id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(
        declared, results,
        "历史里 tool_calls 与 tool 结果必须一一配对"
    );
    // 第二条 user 消息应该在历史里
    assert!(last
        .iter()
        .any(|message| message["role"] == json!("user") && message["content"] == json!("第二轮")));
}

// ---------------------------------------------------------------------------
// 同批工具调用里任意一项残缺 → 整批不执行
// ---------------------------------------------------------------------------

/// 一批里既有合法调用、又有参数坏掉的调用：**一个都不能执行**。
#[tokio::test]
async fn mixed_batch_with_one_broken_call_executes_nothing() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_ok"), Some("list_windows"), Some("{}")),
                tool_delta(1, Some("call_bad"), Some("left_click"), Some("{oops")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("那我不点了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "点一下",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    assert_eq!(
        tools.call_count(),
        0,
        "同批有残缺调用时，工具执行次数必须为零"
    );
    assert!(
        outcome
            .steps
            .iter()
            .all(|step| step.text.contains("整批未执行") || step.text.contains("响应不完整")),
        "每一步都该说明未执行：{:?}",
        outcome.steps
    );
    let bodies = server.bodies();
    let second = bodies[1]["messages"].as_array().unwrap();
    assert!(
        second.iter().any(|message| {
            message["content"]
                .as_str()
                .map(|text| text.contains("整批都没有执行"))
                .unwrap_or(false)
        }),
        "必须把'整批没执行'如实告诉模型"
    );
}

/// 一批里缺函数名的调用同样让整批作废。
#[tokio::test]
async fn batch_with_missing_function_name_executes_nothing() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("call_ok"), Some("list_windows"), Some("{}")),
                tool_delta(1, Some("call_noname"), None, Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("好。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "看看",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    assert_eq!(tools.call_count(), 0, "缺函数名的调用让整批作废");
}

/// 完整的多工具批次仍然照常执行并配对（修复不能误伤正常路径）。
#[tokio::test]
async fn complete_multi_tool_batch_still_executes() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("c1"), Some("list_windows"), Some("{}")),
                tool_delta(
                    1,
                    Some("c2"),
                    Some("get_app_state"),
                    Some("{\"app\":\"notepad\"}"),
                ),
                tool_delta(2, Some("c3"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("齐了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let (outcome, _) = run_turn(
        &mut agent,
        "都读一下",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    assert!(!outcome.failed);
    assert_eq!(tools.call_count(), 3);
    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    let ids: Vec<String> = last
        .iter()
        .filter(|message| message["role"] == json!("tool"))
        .map(|message| message["tool_call_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["c1", "c2", "c3"], "三条结果都要配对");
}

// ---------------------------------------------------------------------------
// 权限确认的请求关联与生命周期
// ---------------------------------------------------------------------------

/// 连续两次 ask：两次都必须真正各问一次，不能被上一次的答复顶掉。
#[tokio::test]
async fn repeated_confirm_clicks_do_not_grant_the_next_tool() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("c1"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else if attempt == 1 {
            vec![
                tool_delta(0, Some("c2"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("好。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    // 第一次允许、第二次拒绝：两次都必须**真正各问一次**。
    let confirmer = ScriptedConfirmer::new(vec![true, false]);
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "读两次",
        &mut history,
        &confirmer,
        &CancelToken::new(),
    )
    .await;

    assert_eq!(confirmer.asked(), 2, "两次 ask 必须各问一次");
    assert_eq!(tools.call_count(), 1, "只执行被允许的那一次");
    assert_eq!(tools.names(), vec!["read_clipboard"]);
}

/// 取消令牌已置位时，确认环节按拒绝处理，不执行工具。
#[tokio::test]
async fn cancelled_turn_never_executes_pending_tool() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("c1"), Some("read_clipboard"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("算了。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let cancel = CancelToken::new();
    // 确认器会在被调用时立刻取消回合，模拟"确认框还开着时点了退出"。
    let confirmer = CancellingConfirmer::new(cancel.clone());
    let (outcome, _) = run_turn(&mut agent, "读剪贴板", &mut history, &confirmer, &cancel).await;

    assert!(outcome.cancelled);
    assert_eq!(tools.call_count(), 0, "取消后不能执行等待中的工具");
}

/// 普通许可操作仍然不弹确认（修复不能把摩擦加回来）。
#[tokio::test]
async fn normal_tools_still_need_no_confirmation() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                tool_delta(0, Some("c1"), Some("list_windows"), Some("{}")),
                tool_delta(1, Some("c2"), Some("get_app_state"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else {
            vec![text_delta("好。"), stop_event("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for("openai-compatible", &server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let confirmer = ScriptedConfirmer::new(vec![]);
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "看看",
        &mut history,
        &confirmer,
        &CancelToken::new(),
    )
    .await;

    assert_eq!(confirmer.asked(), 0, "普通操作不该弹确认");
    assert_eq!(tools.call_count(), 2);
}

/// 按剧本答复的确认器（记录被问了几次）。
struct ScriptedConfirmer {
    answers: Mutex<Vec<bool>>,
    asked: AtomicUsize,
}

impl ScriptedConfirmer {
    fn new(answers: Vec<bool>) -> Self {
        Self {
            answers: Mutex::new(answers),
            asked: AtomicUsize::new(0),
        }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Confirmer for ScriptedConfirmer {
    fn confirm<'a>(
        &'a self,
        _request: ConfirmRequest,
        _cancel: &'a CancelToken,
    ) -> ConfirmFuture<'a> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let allow = {
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                false
            } else {
                answers.remove(0)
            }
        };
        Box::pin(async move {
            if allow {
                ConfirmAnswer::allow_click()
            } else {
                ConfirmAnswer::deny_click()
            }
        })
    }
}

/// 被问到时立刻取消回合的确认器（模拟"确认框开着时点了退出"）。
struct CancellingConfirmer {
    token: CancelToken,
}

impl CancellingConfirmer {
    fn new(token: CancelToken) -> Self {
        Self { token }
    }
}

impl Confirmer for CancellingConfirmer {
    fn confirm<'a>(
        &'a self,
        _request: ConfirmRequest,
        _cancel: &'a CancelToken,
    ) -> ConfirmFuture<'a> {
        self.token.cancel();
        Box::pin(async { ConfirmAnswer::allow_click() })
    }
}

// ---------------------------------------------------------------------------
// 跨提供商的 reasoning_content
// ---------------------------------------------------------------------------

/// 切换提供商后，**不**把上一家的私有 reasoning_content 发给新端点，
/// 但普通对话与工具调用链照常保留。
#[tokio::test]
async fn switching_provider_drops_foreign_reasoning_only() {
    let server = MockServer::start(Box::new(|attempt, _body| {
        if attempt == 0 {
            vec![
                reasoning_delta("旧提供商的思考"),
                tool_delta(0, Some("c1"), Some("list_windows"), Some("{}")),
                stop_event("tool_calls"),
                done(),
            ]
        } else if attempt == 1 {
            vec![text_delta("第一轮完。"), stop_event("stop"), done()]
        } else {
            vec![
                reasoning_delta("新提供商的思考"),
                text_delta("第二轮完。"),
                stop_event("stop"),
                done(),
            ]
        }
    }))
    .await;

    let tools = FakeTools::new();
    // 第一轮：mimo（会回传 reasoning_content）
    let mimo = Settings {
        provider_id: "mimo".to_string(),
        base_url: server.base_url.clone(),
        model: "mimo-v2.5-flash".to_string(),
        ..Settings::default()
    };
    let mut config = agent_config();
    config.echo_reasoning = true;
    let mut agent = Agent::new(
        client(&mimo),
        tools.clone(),
        config.clone(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run_turn(
        &mut agent,
        "第一轮",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    // 第二轮：切到 openai-compatible（标准端点，不认私有 reasoning 字段）
    let plain = Settings {
        provider_id: "openai-compatible".to_string(),
        base_url: server.base_url.clone(),
        model: "mock".to_string(),
        ..Settings::default()
    };
    let mut agent = Agent::new(
        client(&plain),
        tools.clone(),
        AgentConfig {
            echo_reasoning: false,
            ..config
        },
        PermissionState::new(PermissionMode::Standard),
    );
    let _ = run_turn(
        &mut agent,
        "第二轮",
        &mut history,
        &FixedConfirmer::new(true),
        &CancelToken::new(),
    )
    .await;

    let bodies = server.bodies();
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();

    // 1) 来自 MiMo 的 reasoning_content 不得出现
    for message in last {
        if message["role"] == json!("assistant") {
            assert!(
                message.get("reasoning_content").is_none(),
                "不能把上一家的私有 reasoning 发给新端点：{message}"
            );
        }
    }
    // 2) 但普通对话与工具链要保留
    assert!(
        last.iter()
            .any(|message| message["role"] == json!("assistant")
                && message["content"] == json!("第一轮完。")),
        "普通对话记忆不能丢"
    );
    assert!(
        last.iter()
            .any(|message| message["role"] == json!("tool")
                && message["tool_call_id"] == json!("c1")),
        "工具调用链不能丢"
    );
}
