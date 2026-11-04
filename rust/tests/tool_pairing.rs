//! 工具调用历史配对的回归测试。
//!
//! 核心不变量（每一轮**实际发出去**的请求都必须满足）：
//!  集合(assistant.tool_calls[].id) 严格等于 集合(tool.tool_call_id)
//!
//! 历史里出现过"声明了但没执行"或"执行了但没声明"的任何一侧，都是协议错误：
//! 严格端点会直接 400，宽松端点会把模型带偏。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use cuava::agent::{Agent, AgentConfig, AgentEvent, ToolExecution, ToolInvoker};
use cuava::cancel::CancelToken;
use cuava::config::{PermissionMode, Settings};
use cuava::context::resolve_context_policy;
use cuava::cua::tools::ToolError;
use cuava::cua::ImageAttachment;
use cuava::llm::{ChatMessage, LlmClient};
use cuava::permissions::{
    ConfirmAnswer, ConfirmFuture, ConfirmRequest, Confirmer, PermissionState,
};

// ---------------------------------------------------------------------------
// Mock 端点：记录每次请求体
// ---------------------------------------------------------------------------

type Plan = Box<dyn Fn(usize, &Value) -> Vec<String> + Send + Sync>;

struct Mock {
    base_url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Mock {
    async fn start(plan: Plan) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorded = bodies.clone();
        let plan = Arc::new(plan);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

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
                    let body_bytes = loop {
                        let Ok(read) = socket.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        total.extend_from_slice(&buffer[..read]);
                        if let Some(end) = total.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&total[..end]).to_string();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let lower = line.to_ascii_lowercase();
                                    lower
                                        .strip_prefix("content-length:")
                                        .and_then(|rest| rest.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if total.len() >= end + 4 + length {
                                break total[end + 4..end + 4 + length].to_vec();
                            }
                        }
                    };
                    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
                    recorded.lock().unwrap().push(body);
                    let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let events = plan(attempt, &Value::Null);

                    let mut response = String::from(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                    );
                    for event in events {
                        let chunk = format!("{event}\n\n");
                        response.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
                    }
                    response.push_str("0\r\n\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        Mock {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            bodies,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

fn sse(data: Value) -> String {
    format!("data: {data}")
}

fn delta(text: &str) -> String {
    sse(json!({"choices":[{"delta":{"content":text}}]}))
}

fn tool_delta(index: usize, id: &str, name: Option<&str>, args: &str) -> String {
    let mut function = serde_json::Map::new();
    if let Some(name) = name {
        function.insert("name".to_string(), json!(name));
    }
    function.insert("arguments".to_string(), json!(args));
    sse(json!({"choices":[{"delta":{"tool_calls":[{
        "index": index, "id": id, "function": Value::Object(function)
    }]}}]}))
}

fn finish(reason: &str) -> String {
    sse(json!({"choices":[{"delta":{},"finish_reason":reason}]}))
}

fn done() -> String {
    "data: [DONE]".to_string()
}

// ---------------------------------------------------------------------------
// 假工具
// ---------------------------------------------------------------------------

struct FakeTools {
    calls: Mutex<Vec<String>>,
    /// 每次执行前等待多久（用于"执行到一半被取消"）。
    delay_ms: u64,
}

impl FakeTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            delay_ms: 0,
        })
    }

    fn slow(ms: u64) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            delay_ms: ms,
        })
    }

    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn names(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl ToolInvoker for FakeTools {
    fn tool_definitions(&self) -> Vec<Value> {
        [
            "list_windows",
            "get_app_state",
            "read_clipboard",
            "screenshot",
        ]
        .iter()
        .map(|name| {
            json!({"type":"function","function":{
                "name": name,
                "description": name,
                "parameters": {"type":"object","properties":{}}
            }})
        })
        .collect()
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
        cancel: &'a CancelToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolExecution, ToolError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if self.delay_ms > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(self.delay_ms)) => {}
                    _ = cancel.cancelled() => {
                        // 没执行完就取消：**不记**成一次完成的调用。
                        return Err(ToolError::Bridge(cuava::cua::BridgeError::Cancelled));
                    }
                }
            }
            // 只在真正完成后才记账：这样 `names()` 就是"实际完成的工具"。
            self.calls.lock().unwrap().push(name.to_string());
            Ok(ToolExecution {
                text: format!("<result tool=\"{name}\">ok</result>"),
                image: None::<ImageAttachment>,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// 确认器
// ---------------------------------------------------------------------------

struct Allow;

impl Confirmer for Allow {
    fn confirm<'a>(
        &'a self,
        _request: ConfirmRequest,
        _cancel: &'a CancelToken,
    ) -> ConfirmFuture<'a> {
        Box::pin(async { ConfirmAnswer::allow_click() })
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn settings_for(base_url: &str) -> Settings {
    Settings {
        provider_id: "openai-compatible".to_string(),
        base_url: base_url.to_string(),
        model: "mock".to_string(),
        ..Settings::default()
    }
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        max_steps: 8,
        context_policy: resolve_context_policy(Some(1_000_000), Some(90), None),
        system_prompt: "test".to_string(),
        echo_reasoning: false,
        turn_id: 1,
        use_tools: true,
    }
}

/// 断言一次请求里的工具调用与结果严格配对。
fn assert_paired(body: &Value, label: &str) {
    let messages = body["messages"].as_array().expect("messages 必须是数组");
    let mut declared: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for message in messages {
        if message["role"] == json!("assistant") {
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    declared.push(
                        call["id"]
                            .as_str()
                            .unwrap_or_else(|| panic!("{label}: tool_call 缺 id"))
                            .to_string(),
                    );
                }
            }
        }
        if message["role"] == json!("tool") {
            results.push(
                message["tool_call_id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{label}: tool 结果缺 tool_call_id"))
                    .to_string(),
            );
        }
    }
    declared.sort();
    results.sort();
    assert_eq!(
        declared, results,
        "{label}: 声明的 tool_calls 与 tool 结果必须一一对应\n\
         declared={declared:?}\nresults={results:?}"
    );
}

/// 跑一轮，返回结果与发出的事件。
async fn run(
    agent: &mut Agent,
    input: &str,
    history: &mut Vec<ChatMessage>,
    cancel: &CancelToken,
    confirmer: &dyn Confirmer,
) -> (cuava::agent::TurnOutcome, Vec<String>) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let emit = move |event: AgentEvent| {
        let label = match &event {
            AgentEvent::ToolStart { name, .. } => format!("start:{name}"),
            AgentEvent::ToolEnd { name, status, .. } => {
                format!("end:{name}:{}", status.code())
            }
            AgentEvent::TextDelta { text, .. } => format!("text:{text}"),
            AgentEvent::Failed { reason, .. } => format!("failed:{reason}"),
            _ => "other".to_string(),
        };
        sink.lock().unwrap().push(label);
    };
    let outcome = agent.run(input, history, cancel, confirmer, &emit).await;
    let collected = events.lock().unwrap().clone();
    (outcome, collected)
}

fn client(settings: &Settings) -> LlmClient {
    LlmClient::new(settings.profile(), "", 0.3, 10_000).unwrap()
}

// ---------------------------------------------------------------------------
// A. 同批工具调用部分损坏
// ---------------------------------------------------------------------------

/// 一批里有一个合法调用 + 一个损坏调用：不执行；**下一次请求的配对必须完整**。
#[tokio::test]
async fn mixed_broken_batch_keeps_requests_paired() {
    let server = Mock::start(Box::new(|attempt, _| {
        if attempt == 0 {
            vec![
                tool_delta(0, "call_ok", Some("list_windows"), "{}"),
                tool_delta(1, "call_bad", Some("read_clipboard"), "{oops"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("那我不读了。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let (outcome, _) = run(
        &mut agent,
        "看一下再读",
        &mut history,
        &CancelToken::new(),
        &Allow,
    )
    .await;

    assert_eq!(tools.count(), 0, "整批不执行");
    assert!(!outcome.failed, "模型能继续响应");

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2, "应当有两次请求");
    // 关键：第二次**真实发出**的请求必须配对完整
    assert_paired(&bodies[1], "混合损坏批次之后");

    // 损坏的调用绝不能出现在请求的 tool_calls 里
    let second = bodies[1]["messages"].as_array().unwrap();
    for message in second {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                assert_ne!(
                    call["id"],
                    json!("call_bad"),
                    "残缺调用不得写进 assistant.tool_calls"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// B. 执行中取消
// ---------------------------------------------------------------------------

/// 三个工具，执行完第一个后取消：剩余不执行，且下一轮请求仍然配对完整。
#[tokio::test]
async fn cancel_mid_batch_keeps_next_request_paired() {
    let server = Mock::start(Box::new(|attempt, _| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", Some("list_windows"), "{}"),
                tool_delta(1, "c2", Some("get_app_state"), "{}"),
                tool_delta(2, "c3", Some("read_clipboard"), "{}"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("继续。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::slow(150);
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let cancel = CancelToken::new();

    // 第一个工具执行完、第二个开始后取消
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        token.cancel();
    });

    let (outcome, _) = run(&mut agent, "读三个", &mut history, &cancel, &Allow).await;

    assert!(outcome.cancelled);
    assert_eq!(
        tools.names(),
        vec!["list_windows"],
        "取消后不得执行剩余工具"
    );

    // 取消之后开始新一轮：必须先修好历史，再发请求
    let next_cancel = CancelToken::new();
    let (next, _) = run(&mut agent, "接着来", &mut history, &next_cancel, &Allow).await;
    assert!(!next.cancelled, "取消后新一轮应当正常完成");

    let bodies = server.bodies();
    assert!(bodies.len() >= 2);
    for (index, body) in bodies.iter().enumerate() {
        assert_paired(body, &format!("第 {} 次请求", index + 1));
    }

    // 已执行的那一步必须仍然有据可查：历史里保留它的结果
    let last = bodies.last().unwrap()["messages"].as_array().unwrap();
    assert!(
        last.iter()
            .any(|m| m["role"] == json!("tool") && m["tool_call_id"] == json!("c1")),
        "已实际执行的操作不能被抹掉"
    );
}

/// 取消发生在**第一个工具执行期间**：同样不能在历史里留下没有结果的调用。
#[tokio::test]
async fn cancel_during_first_tool_keeps_history_valid() {
    let server = Mock::start(Box::new(|attempt, _| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", Some("list_windows"), "{}"),
                tool_delta(1, "c2", Some("get_app_state"), "{}"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("好。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::slow(300);
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        token.cancel();
    });

    let (outcome, _) = run(&mut agent, "读两个", &mut history, &cancel, &Allow).await;
    assert!(outcome.cancelled);
    assert_eq!(tools.count(), 0, "第一个还在跑就被取消，没有完成的执行");

    let _ = run(
        &mut agent,
        "算了，看看窗口",
        &mut history,
        &CancelToken::new(),
        &Allow,
    )
    .await;

    let bodies = server.bodies();
    for (index, body) in bodies.iter().enumerate() {
        assert_paired(body, &format!("取消期间第 {} 次请求", index + 1));
    }
}

// ---------------------------------------------------------------------------
// 正常路径不能被误伤
// ---------------------------------------------------------------------------

/// 完整的多工具批次照常执行、结果正确配对。
#[tokio::test]
async fn complete_batch_still_executes_and_pairs() {
    let server = Mock::start(Box::new(|attempt, _| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", Some("list_windows"), "{}"),
                tool_delta(1, "c2", Some("get_app_state"), "{}"),
                tool_delta(2, "c3", Some("read_clipboard"), "{}"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("齐了。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new();
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let (outcome, _) = run(
        &mut agent,
        "都读",
        &mut history,
        &CancelToken::new(),
        &Allow,
    )
    .await;

    assert!(!outcome.failed && !outcome.cancelled);
    assert_eq!(tools.count(), 3);
    let bodies = server.bodies();
    for (index, body) in bodies.iter().enumerate() {
        assert_paired(body, &format!("正常批次第 {} 次请求", index + 1));
    }
}

/// 普通许可操作不需要确认（修复不能把摩擦加回来）。
#[tokio::test]
async fn ordinary_tools_need_no_confirmation() {
    let server = Mock::start(Box::new(|attempt, _| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", Some("list_windows"), "{}"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("好。"), finish("stop"), done()]
        }
    }))
    .await;

    struct Counting(Arc<Mutex<usize>>);
    impl Confirmer for Counting {
        fn confirm<'a>(
            &'a self,
            _request: ConfirmRequest,
            _cancel: &'a CancelToken,
        ) -> ConfirmFuture<'a> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async { ConfirmAnswer::deny_click() })
        }
    }

    let asked = Arc::new(Mutex::new(0usize));
    let confirmer = Counting(asked.clone());
    let tools = FakeTools::new();
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::Standard),
    );
    let mut history = Vec::new();
    let _ = run(
        &mut agent,
        "看看窗口",
        &mut history,
        &CancelToken::new(),
        &confirmer,
    )
    .await;

    assert_eq!(*asked.lock().unwrap(), 0, "普通许可操作不该弹确认");
    assert_eq!(tools.count(), 1);
}
