//! 取消时"工具结果未知"的回归测试。
//!
//! 缺陷：`backfill_unanswered_tool_calls()` 曾把所有没收到结果的调用
//! 一律写成"因为中断没有执行"。但 CUA 工具只要把命令写进 PowerShell，
//! 桌面就可能已经动了；这时取消拿到的是 `Cancelled`，我们**无法判断**生效没有。
//! 谎称"没执行"会让模型（和用户）以为操作没发生，进而放心重试——
//! 对点击/输入这类动作就是重复副作用。
//!
//! 这里要验证三种状态被正确区分：
//!  - 已发起、无结果  → "状态未知"
//!  - 未发起          → "没有执行"
//!  - 有真实结果      → 原样保留

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use cuava::agent::{Agent, AgentConfig, AgentEvent, ToolExecution, ToolInvoker};
use cuava::cancel::CancelToken;
use cuava::config::{PermissionMode, Settings};
use cuava::context::resolve_context_policy;
use cuava::cua::tools::ToolError;
use cuava::cua::BridgeError;
use cuava::cua::ImageAttachment;
use cuava::llm::{ChatMessage, LlmClient};
use cuava::permissions::{
    ConfirmAnswer, ConfirmFuture, ConfirmRequest, Confirmer, PermissionState,
};

// ---------------------------------------------------------------------------
// Mock 端点
// ---------------------------------------------------------------------------

type Plan = Box<dyn Fn(usize) -> Vec<String> + Send + Sync>;

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

                    let mut response = String::from(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                    );
                    for event in plan(attempt) {
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
fn tool_delta(index: usize, id: &str, name: &str) -> String {
    sse(json!({"choices":[{"delta":{"tool_calls":[{
        "index": index, "id": id,
        "function": {"name": name, "arguments": "{}"}
    }]}}]}))
}
fn finish(reason: &str) -> String {
    sse(json!({"choices":[{"delta":{},"finish_reason":reason}]}))
}
fn done() -> String {
    "data: [DONE]".to_string()
}

// ---------------------------------------------------------------------------
// 假工具：可配置"执行行为"
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Behaviour {
    /// 立即成功返回。
    Succeed,
    /// 先等一会儿再成功（用于"执行途中被取消"）。
    Slow,
    /// 模拟"命令已发出、但响应没回来就被取消"：
    /// 真实 CUA 在 write_all 成功后取消就是这个形态（`BridgeError::Cancelled`）。
    DispatchedThenCancelled,
}

struct FakeTools {
    behaviour: Behaviour,
    completed: Mutex<Vec<String>>,
}

impl FakeTools {
    fn new(behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            behaviour,
            completed: Mutex::new(Vec::new()),
        })
    }

    fn completed(&self) -> Vec<String> {
        self.completed.lock().unwrap().clone()
    }
}

impl ToolInvoker for FakeTools {
    fn tool_definitions(&self) -> Vec<Value> {
        [
            "list_windows",
            "get_app_state",
            "read_clipboard",
            "left_click",
        ]
        .iter()
        .map(|name| {
            json!({"type":"function","function":{
                "name": name, "description": name,
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
            match self.behaviour {
                Behaviour::Succeed => {
                    self.completed.lock().unwrap().push(name.to_string());
                    Ok(ToolExecution {
                        text: format!("<result tool=\"{name}\">ok</result>"),
                        image: None::<ImageAttachment>,
                    })
                }
                Behaviour::Slow => {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(300)) => {
                            self.completed.lock().unwrap().push(name.to_string());
                            Ok(ToolExecution {
                                text: format!("<result tool=\"{name}\">ok</result>"),
                                image: None::<ImageAttachment>,
                            })
                        }
                        _ = cancel.cancelled() => {
                            // 没跑完就取消：不算完成
                            Err(ToolError::Bridge(BridgeError::Cancelled))
                        }
                    }
                }
                Behaviour::DispatchedThenCancelled => {
                    // 关键场景：动作**已经发出去**（桌面可能已受影响），
                    // 但响应还没回来时被取消——只能报 Cancelled，不能报成功或未执行。
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Err(ToolError::Bridge(BridgeError::Cancelled))
                }
            }
        })
    }
}

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

fn client(settings: &Settings) -> LlmClient {
    LlmClient::new(settings.profile(), "", 0.3, 10_000).unwrap()
}

async fn run(
    agent: &mut Agent,
    input: &str,
    history: &mut Vec<ChatMessage>,
    cancel: &CancelToken,
) -> (cuava::agent::TurnOutcome, Vec<String>) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let emit = move |event: AgentEvent| {
        if let AgentEvent::Failed { reason, .. } = &event {
            sink.lock().unwrap().push(format!("failed:{reason}"));
        }
    };
    let outcome = agent.run(input, history, cancel, &Allow, &emit).await;
    let collected = events.lock().unwrap().clone();
    (outcome, collected)
}

/// 断言配对完整，并把每条的文本取出来。
fn collect_results(body: &Value) -> (Vec<String>, Vec<(String, String)>) {
    let messages = body["messages"].as_array().expect("messages");
    let mut declared = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        if message["role"] == json!("assistant") {
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    declared.push(call["id"].as_str().unwrap_or_default().to_string());
                }
            }
        }
        if message["role"] == json!("tool") {
            results.push((
                message["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                message["content"].as_str().unwrap_or_default().to_string(),
            ));
        }
    }
    let mut a = declared.clone();
    let mut b: Vec<String> = results.iter().map(|(id, _)| id.clone()).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b, "tool_calls 与 tool 结果必须一一对应");
    (declared, results)
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

/// 动作已发出、响应没回来就取消：历史里必须写"状态未知"，不能说没执行。
#[tokio::test]
async fn dispatched_but_unanswered_is_marked_unknown() {
    let server = Mock::start(Box::new(|attempt| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", "left_click"),
                tool_delta(1, "c2", "get_app_state"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("好。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new(Behaviour::DispatchedThenCancelled);
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let cancel = CancelToken::new();
    // 让工具先跑起来（进入"已发出"状态）再取消
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        token.cancel();
    });

    let (outcome, _) = run(&mut agent, "点一下", &mut history, &cancel).await;
    assert!(outcome.cancelled);

    // 下一轮：先把这个回合的历史交给模型，检查它看到的到底是什么
    let (_, _) = run(&mut agent, "接着来", &mut history, &CancelToken::new()).await;

    let bodies = server.bodies();
    let (_, results) = collect_results(bodies.last().unwrap());

    // c1 已经发出去但没结果 → 必须说"未知"，不能声称没执行
    let c1 = results
        .iter()
        .find(|(id, _)| id == "c1")
        .map(|(_, text)| text.clone())
        .expect("c1 必须有结果占位");
    assert!(
        c1.contains("未知"),
        "已发出但没拿到结果的调用必须标为状态未知，实际是：{c1}"
    );
    assert!(
        !c1.contains("没有执行"),
        "不能把'可能已经生效'的操作说成没执行：{c1}"
    );
    assert!(
        c1.contains("不要") && (c1.contains("重试") || c1.contains("确认")),
        "要明确提醒模型不要盲目重试：{c1}"
    );

    // c2 还没轮到，确定没执行
    let c2 = results
        .iter()
        .find(|(id, _)| id == "c2")
        .map(|(_, text)| text.clone())
        .expect("c2 必须有结果占位");
    assert!(
        c2.contains("没有执行"),
        "还没轮到的调用应当明确说没执行，实际是：{c2}"
    );
}

/// 取消发生在第二个工具执行途中：第一个的真实结果必须保留，
/// 第二个标"未知"，第三个标"没执行"。
#[tokio::test]
async fn partial_execution_keeps_real_result_and_marks_the_rest() {
    let server = Mock::start(Box::new(|attempt| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", "list_windows"),
                tool_delta(1, "c2", "read_clipboard"),
                tool_delta(2, "c3", "get_app_state"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("继续。"), finish("stop"), done()]
        }
    }))
    .await;

    /*
     * 用一个"第一个成功、第二个卡住"的假工具。
     * 这样取消时正好落在第二个上，最接近真实场景。
     */
    struct Mixed {
        counter: Mutex<usize>,
    }
    impl ToolInvoker for Mixed {
        fn tool_definitions(&self) -> Vec<Value> {
            ["list_windows", "read_clipboard", "get_app_state"]
                .iter()
                .map(|name| {
                    json!({"type":"function","function":{
                        "name": name, "description": name,
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
                let index = {
                    let mut guard = self.counter.lock().unwrap();
                    *guard += 1;
                    *guard
                };
                if index == 1 {
                    // 第一个：立刻成功
                    return Ok(ToolExecution {
                        text: format!("<result tool=\"{name}\">first-ok</result>"),
                        image: None,
                    });
                }
                if index == 2 {
                    // 第二个：已发起，但响应拖到取消之后
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        _ = cancel.cancelled() => {}
                    }
                    return Err(ToolError::Bridge(BridgeError::Cancelled));
                }
                // 第三个：根本轮不到
                Ok(ToolExecution {
                    text: format!("<result tool=\"{name}\">third</result>"),
                    image: None,
                })
            })
        }
    }

    let tools = Arc::new(Mixed {
        counter: Mutex::new(0),
    });
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools,
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

    let (outcome, _) = run(&mut agent, "读三个", &mut history, &cancel).await;
    assert!(outcome.cancelled);

    let _ = run(&mut agent, "接着来", &mut history, &CancelToken::new()).await;

    let bodies = server.bodies();
    let (declared, results) = collect_results(bodies.last().unwrap());
    assert_eq!(declared.len(), 3, "三个调用都要在历史里");

    let get = |id: &str| {
        results
            .iter()
            .find(|(result_id, _)| result_id == id)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("缺少 {id} 的结果"))
    };

    // 第一个：真实结果必须原样保留
    assert!(
        get("c1").contains("first-ok"),
        "已完成的真实结果不能丢：{}",
        get("c1")
    );
    // 第二个：已发起但没结果 → 未知
    assert!(
        get("c2").contains("未知"),
        "执行途中被取消的要标未知：{}",
        get("c2")
    );
    // 第三个：没轮到 → 没执行
    assert!(
        get("c3").contains("没有执行"),
        "还没轮到的要说没执行：{}",
        get("c3")
    );
}

/// 正常路径不受影响：全部成功，结果都是真实内容，没有任何"未知/未执行"占位。
#[tokio::test]
async fn successful_batch_has_no_placeholders() {
    let server = Mock::start(Box::new(|attempt| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", "list_windows"),
                tool_delta(1, "c2", "get_app_state"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("齐了。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new(Behaviour::Succeed);
    let settings = settings_for(&server.base_url);
    let mut agent = Agent::new(
        client(&settings),
        tools.clone(),
        agent_config(),
        PermissionState::new(PermissionMode::PassThrough),
    );
    let mut history = Vec::new();
    let (outcome, _) = run(&mut agent, "都读", &mut history, &CancelToken::new()).await;

    assert!(!outcome.failed && !outcome.cancelled);
    assert_eq!(tools.completed().len(), 2);

    let bodies = server.bodies();
    let (_, results) = collect_results(bodies.last().unwrap());
    for (id, text) in &results {
        assert!(
            !text.contains("未知") && !text.contains("没有执行"),
            "{id} 是正常执行，不该出现占位说明：{text}"
        );
        assert!(text.contains("ok"), "应当是真实结果：{text}");
    }
}

/// 取消后不发起新的工具操作。
#[tokio::test]
async fn cancel_stops_further_tools() {
    let server = Mock::start(Box::new(|attempt| {
        if attempt == 0 {
            vec![
                tool_delta(0, "c1", "list_windows"),
                tool_delta(1, "c2", "get_app_state"),
                tool_delta(2, "c3", "read_clipboard"),
                finish("tool_calls"),
                done(),
            ]
        } else {
            vec![delta("好。"), finish("stop"), done()]
        }
    }))
    .await;

    let tools = FakeTools::new(Behaviour::Slow);
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
        tokio::time::sleep(Duration::from_millis(120)).await;
        token.cancel();
    });

    let (outcome, _) = run(&mut agent, "读三个", &mut history, &cancel).await;
    assert!(outcome.cancelled);
    assert!(
        tools.completed().len() <= 1,
        "取消后不能再完成新的工具：{:?}",
        tools.completed()
    );
}
