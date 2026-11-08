//! SSE 分片、`[DONE]` 与流停滞超时的回归测试。
//!
//! 这些缺陷的共同点：**单测里拼好的字符串看不出问题，只有真的按字节分片才会暴露**。
//! 所以这里起一个原始字节级的 mock 端点：按测试给定的切点把响应拆成多个 TCP 块，
//! 精确复现"中文字符被切开"、"CRLF 被切开"、"工具参数被切开"。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use cuava::cancel::CancelToken;
use cuava::config::{ReasoningEffort, Settings, ThinkingLevel};
use cuava::llm::{ChatMessage, LlmClient};

/// 原始字节级 mock：`plan` 决定这次返回哪些**字节块**。
type RawPlan = Box<dyn Fn(usize) -> Vec<Vec<u8>> + Send + Sync>;

struct RawServer {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl RawServer {
    async fn start(plan: RawPlan) -> RawServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
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
                        if let Some(header_end) = find_header_end(&total) {
                            let head = String::from_utf8_lossy(&total[..header_end]).to_string();
                            let length = content_length(&head);
                            let body_start = header_end + 4;
                            if total.len() >= body_start + length {
                                break total[body_start..body_start + length].to_vec();
                            }
                        } else if total.len() > 8 * 1024 * 1024 {
                            return;
                        }
                    };
                    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
                    recorded.lock().unwrap().push(body);

                    let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let chunks = plan(attempt);

                    // 头部照常发，然后按给定切点逐块写字节。
                    let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                    let _ = socket.write_all(header.as_bytes()).await;
                    for chunk in chunks {
                        // 每个字节块作为一个 chunk；这正好模拟"一次 read 只拿到半个字符"。
                        let prefix = format!("{:x}\r\n", chunk.len());
                        let _ = socket.write_all(prefix.as_bytes()).await;
                        let _ = socket.write_all(&chunk).await;
                        let _ = socket.write_all(b"\r\n").await;
                        let _ = socket.flush().await;
                        // 让客户端有机会真的分开读（否则内核可能合并成一个包）。
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                    let _ = socket.flush().await;
                });
            }
        });

        RawServer {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
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

/// 把一段字节按**每个字节**切开——最极端的分片方式。
fn split_every_byte(data: &[u8]) -> Vec<Vec<u8>> {
    data.iter().map(|byte| vec![*byte]).collect()
}

/// 按给定切点切。
fn split_at(data: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0;
    for cut in cuts {
        if *cut > start && *cut <= data.len() {
            out.push(data[start..*cut].to_vec());
            start = *cut;
        }
    }
    if start < data.len() {
        out.push(data[start..].to_vec());
    }
    out
}

fn client(base_url: &str, timeout_ms: u64) -> LlmClient {
    let settings = Settings {
        provider_id: "openai-compatible".to_string(),
        base_url: base_url.to_string(),
        model: "mock".to_string(),
        ..Settings::default()
    };
    LlmClient::new(settings.profile(), "", 0.3, timeout_ms).unwrap()
}

async fn chat(client: &LlmClient) -> Result<cuava::llm::ChatResult, cuava::llm::LlmError> {
    client
        .chat(
            &[ChatMessage::user("hi")],
            &[],
            &CancelToken::new(),
            |_| {},
            |_| {},
        )
        .await
}

/// 中文正文 + 工具参数：**逐字节**分片后必须完整还原，不能出现 U+FFFD。
#[tokio::test]
async fn utf8_split_inside_multibyte_characters_is_preserved() {
    let payload = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"中文内容\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"type\",\"arguments\":\"{\\\"text\\\":\\\"你好世界\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec();
    let chunks = split_every_byte(&payload);

    let server = RawServer::start(Box::new(move |_| chunks.clone())).await;
    let result = chat(&client(&server.base_url, 10_000)).await.unwrap();

    assert_eq!(result.text, "中文内容", "跨分片的中文必须完整还原");
    assert!(!result.text.contains('\u{FFFD}'), "不能出现替换字符");
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(
        result.tool_calls[0].arguments, "{\"text\":\"你好世界\"}",
        "工具参数里的中文也必须完整"
    );
    assert!(!result.tool_calls_incomplete);
}

/// CRLF 分隔符被切开：`\r` 和 `\n` 落在不同网络块里仍要正确切事件。
#[tokio::test]
async fn crlf_split_across_chunks_is_handled() {
    let payload = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"第一段\"}}]}\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"第二段\"}}]}\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        "data: [DONE]\r\n\r\n"
    )
    .as_bytes()
    .to_vec();
    // 在第 1、3、7 个字节处切：一定把 \r\n 和紧跟的 \r\n 拆开
    let chunks = split_at(&payload, &[1, 3, 7, 13, 40, 41, 42]);
    let server = RawServer::start(Box::new(move |_| chunks.clone())).await;
    let result = chat(&client(&server.base_url, 10_000)).await.unwrap();

    assert_eq!(result.text, "第一段第二段");
    assert_eq!(result.finish_reason, "stop");
    assert!(!result.incomplete);
}

/// `[DONE]` 收到后应立刻结束，而不是等连接关闭。
#[tokio::test]
async fn done_marker_ends_the_stream_without_waiting_for_close() {
    // 发完 [DONE] 之后**不关闭连接、也不再发送任何东西**。
    // 如果客户端还在等 EOF，就得等到超时——本测试用超时把它暴露出来。
    let payload = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec();
    let chunks = split_at(&payload, &[50]);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total = Vec::new();
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                return;
            }
            total.extend_from_slice(&buffer[..read]);
            if let Some(end) = find_header_end(&total) {
                let head = String::from_utf8_lossy(&total[..end]).to_string();
                if total.len() >= end + 4 + content_length(&head) {
                    break;
                }
            }
        }
        let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
        let _ = socket.write_all(header.as_bytes()).await;
        for chunk in chunks {
            let _ = socket
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await;
            let _ = socket.write_all(&chunk).await;
            let _ = socket.write_all(b"\r\n").await;
        }
        let _ = socket.flush().await;
        // **保持连接不关**，睡够一个客户端超时周期
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });

    let started = std::time::Instant::now();
    let result = chat(&client(&format!("http://127.0.0.1:{port}/v1"), 3_000))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(result.text, "ok");
    assert!(!result.incomplete, "收到 [DONE] 就是正常结束");
    assert!(
        elapsed < Duration::from_secs(3),
        "收到 [DONE] 后不该等到超时（实际 {elapsed:?}）"
    );
}

/// 响应头已返回、之后长时间没有字节：必须按超时结束，不能无限挂起。
#[tokio::test]
async fn stalled_stream_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 64 * 1024];
        let _ = socket.read(&mut buffer).await;
        // 只回头部，然后什么都不发
        let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
        let _ = socket.write_all(header.as_bytes()).await;
        let _ = socket.flush().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let started = std::time::Instant::now();
    let error = chat(&client(&format!("http://127.0.0.1:{port}/v1"), 800))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        matches!(error, cuava::llm::LlmError::Timeout(_)),
        "停滞流必须报超时：{error}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "超时不能被拉长（实际 {elapsed:?}）"
    );
}

/// 持续输出的长推理**不能**被过早终止：每收到一块就重置空闲计时。
#[tokio::test]
async fn continuous_output_is_not_killed_by_idle_timeout() {
    let events: Vec<String> = (0..12)
        .map(|i| format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{i}\"}}}}]}}\n\n"))
        .chain(std::iter::once(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
        ))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
    let payload = events.join("").as_bytes().to_vec();
    // 每 120ms 一块，总时长 > 空闲超时（200ms）但每块之间都 < 超时
    let chunks: Vec<Vec<u8>> = payload.chunks(40).map(|chunk| chunk.to_vec()).collect();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total = Vec::new();
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                return;
            }
            total.extend_from_slice(&buffer[..read]);
            if let Some(end) = find_header_end(&total) {
                let head = String::from_utf8_lossy(&total[..end]).to_string();
                if total.len() >= end + 4 + content_length(&head) {
                    break;
                }
            }
        }
        let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
        let _ = socket.write_all(header.as_bytes()).await;
        for chunk in chunks {
            let _ = socket
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await;
            let _ = socket.write_all(&chunk).await;
            let _ = socket.write_all(b"\r\n").await;
            let _ = socket.flush().await;
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
    });

    let result = chat(&client(&format!("http://127.0.0.1:{port}/v1"), 400))
        .await
        .expect("持续输出不该被空闲超时打断");
    assert_eq!(result.text, "01234567891011", "12 段内容都要完整收到");
}

/// MiMo / DeepSeek 的推理参数规则不因本次改动而改变。
#[tokio::test]
async fn provider_field_rules_are_unchanged_after_stream_rewrite() {
    let server = RawServer::start(Box::new(|_| {
        vec![b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec()]
    }))
    .await;

    let mut settings = Settings {
        provider_id: "mimo".to_string(),
        base_url: server.base_url.clone(),
        model: "mimo-v2.6-flash".to_string(),
        ..Settings::default()
    };
    settings.thinking = ThinkingLevel::Disabled;
    let mimo = LlmClient::new(settings.profile(), "", 0.3, 5_000).unwrap();
    let _ = chat(&mimo).await.unwrap();
    let body = server.bodies().last().unwrap().clone();
    assert_eq!(body["thinking"], json!({"type": "disabled"}));
    assert!(body.get("reasoning_effort").is_none());

    let mut settings = Settings {
        provider_id: "deepseek".to_string(),
        base_url: server.base_url.clone(),
        model: "deepseek-flash".to_string(),
        ..Settings::default()
    };
    settings.reasoning_effort = ReasoningEffort::Low;
    let deepseek = LlmClient::new(settings.profile(), "", 0.3, 5_000).unwrap();
    let _ = chat(&deepseek).await.unwrap();
    let body = server.bodies().last().unwrap().clone();
    assert_eq!(body["reasoning_effort"], json!("low"));
    assert!(body.get("thinking").is_none());
}

#[tokio::test]
async fn pr17_null_error_and_error_text_are_normal_data() {
    let visible = r#"error {"error":"example"} and code: obj["error"]"#;
    let payload = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"error":null, "choices":[{"delta":{"content":visible}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
    );
    let chunks = split_every_byte(payload.as_bytes());
    let server = RawServer::start(Box::new(move |_| chunks.clone())).await;
    let result = chat(&client(&server.base_url, 2000)).await.unwrap();
    assert_eq!(result.text, visible);
    assert!(!result.incomplete && !result.tool_calls_incomplete);
}

#[tokio::test]
async fn pr17_real_sse_error_formats_are_shared_and_prompt() {
    for payload in [
        "event:error\ndata: {\"error\":{\"code\":\"rate_limit_exceeded\",\"type\":\"quota\",\"message\":\"try later\"}}\n\n",
        "event: error\ndata: {\"code\":\"rate_limit_exceeded\",\"message\":\"try later\"}\n\n",
        "data: {\"error\":\"quota_exceeded\"}\n\n",
        "event:error\ndata: quota_exceeded\n\n",
    ] {
        let chunks = split_every_byte(payload.as_bytes());
        let server = RawServer::start(Box::new(move |_| chunks.clone())).await;
        let client = client(&server.base_url, 2000);
        let error = chat(&client).await.unwrap_err();
        let message = client.user_error(&error);
        assert!(matches!(error, cuava::llm::LlmError::Stream(_)));
        assert!(message.contains("exceeded"), "{message}");
        assert!(!message.contains("HTTP"));
    }
}

#[tokio::test]
async fn pr17_invalid_json_with_error_text_does_not_abort_valid_following_events() {
    let payload = b"data: {\"error\":\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let chunks = split_every_byte(payload);
    let server = RawServer::start(Box::new(move |_| chunks.clone())).await;
    assert_eq!(
        chat(&client(&server.base_url, 2000)).await.unwrap().text,
        "ok"
    );
}
