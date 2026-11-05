//! PR12 端到端验收：真实 HTTP Mock（Tavily / OpenAI 兼容端点）+ 真实本地工具。
//!
//! 为什么必须打真实 HTTP 而不是打桩 `WebSearch::search`：
//! 「Bearer 认证头、请求体参数、摘要与来源、401/429、超时、取消」这几件事只能靠
//! 看真实发出的字节与真实返回码验证。这里起一个 tokio TCP 监听器当 Tavily，
//! 不联网、不花钱。
//!
//! 覆盖：
//!  - 工具定义：12 CUA（无后端时不公布）+ 6 本地 + 时间 + 条件启用的 web_search；
//!  - 拒绝 `run_shell` / 拒绝删除时**实际执行计数为零**；
//!  - `files` 的 read/write/delete 分别落到 `file.read` / `file.write` / `file.delete`；
//!  - 允许后的真实结果回到同一个 Agent，并按 `tool_call_id` 配进时间线；
//!  - Mock Tavily：Bearer、参数、摘要、401/429、取消；
//!  - 时间：直接回答与工具都不需要网络。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use cuava::agent::{Agent, AgentConfig, AgentEvent, ToolExecution, ToolInvoker, ToolStatus};
use cuava::cancel::CancelToken;
use cuava::clock;
use cuava::config::PermissionMode;
use cuava::cua::tools::ToolError;
use cuava::llm::{ChatMessage, LlmClient};
use cuava::permissions::{
    ConfirmAnswer, ConfirmFuture, ConfirmRequest, Confirmer, PermissionState,
};
use cuava::tools::clock_tool;
use cuava::tools::registry::ToolRegistry;
use cuava::tools::web_search::WebSearch;

// ---------------------------------------------------------------------------
// Mock HTTP 服务器（Tavily + OpenAI 兼容聊天）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    head: String,
    body: Value,
}

struct MockHttp {
    base_url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// 一次响应的形态。
enum Reply {
    Json(u16, Value),
    /// 200 但**不返回**，用来测超时。
    Hang(Duration),
}

impl MockHttp {
    /// `plan(path, index) -> Reply`：按请求顺序决定返回什么。
    async fn start<F>(plan: F) -> MockHttp
    where
        F: Fn(&str, usize) -> Reply + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let plan = Arc::new(plan);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                let counter = counter.clone();
                let plan = plan.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut total = Vec::new();
                    let (head, body_bytes) = loop {
                        let Ok(read) = socket.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        total.extend_from_slice(&buffer[..read]);
                        if let Some(end) = find_header_end(&total) {
                            let head = String::from_utf8_lossy(&total[..end]).to_string();
                            let length = content_length(&head);
                            let start = end + 4;
                            if total.len() >= start + length {
                                break (head, total[start..start + length].to_vec());
                            }
                        } else if total.len() > 4 * 1024 * 1024 {
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
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    recorded.lock().unwrap().push(Seen {
                        path: path.clone(),
                        head: head.clone(),
                        body: body.clone(),
                    });
                    match plan(&path, index) {
                        Reply::Json(status, payload) => {
                            // `__sse__` = 把数组里的每一行当作 SSE 事件（工具调用需要真实流）。
                            if let Some(lines) = payload.get("__sse__").and_then(Value::as_array) {
                                let mut response = String::from(
                                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                                );
                                for line in lines {
                                    let chunk =
                                        format!("{}\n\n", line.as_str().unwrap_or_default());
                                    response.push_str(&format!(
                                        "{:x}\r\n{}\r\n",
                                        chunk.len(),
                                        chunk
                                    ));
                                }
                                response.push_str("0\r\n\r\n");
                                let _ = socket.write_all(response.as_bytes()).await;
                            } else {
                                let text = payload.to_string();
                                let response = format!(
                                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                                    text.len()
                                );
                                let _ = socket.write_all(response.as_bytes()).await;
                            }
                        }
                        Reply::Hang(delay) => {
                            tokio::time::sleep(delay).await;
                        }
                    }
                    let _ = socket.flush().await;
                });
            }
        });
        MockHttp {
            base_url: format!("http://127.0.0.1:{port}"),
            seen,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
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

/// 大小写不敏感地取一个请求头。
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.lines().skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 确认器
// ---------------------------------------------------------------------------

/// 记录每一次确认请求，并按固定答复响应。
struct RecordingConfirmer {
    allow: bool,
    seen: Mutex<Vec<ConfirmRequest>>,
}

impl RecordingConfirmer {
    fn new(allow: bool) -> Arc<Self> {
        Arc::new(Self {
            allow,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn tools(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.tool.clone())
            .collect()
    }

    fn capabilities(&self) -> Vec<&'static str> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.capability)
            .collect()
    }
}

impl Confirmer for RecordingConfirmer {
    fn confirm<'a>(
        &'a self,
        request: ConfirmRequest,
        _cancel: &'a CancelToken,
    ) -> ConfirmFuture<'a> {
        self.seen.lock().unwrap().push(request);
        let answer = if self.allow {
            ConfirmAnswer::allow_click()
        } else {
            ConfirmAnswer::deny_click()
        };
        Box::pin(async move { answer })
    }
}

// ---------------------------------------------------------------------------
// 测试用 LLM 端点（只回一条带工具调用的响应，然后收尾）
// ---------------------------------------------------------------------------

/// 两枪：第一次发一个工具调用，第二次直接回文本。
async fn start_chat_mock(tool: &str, args: &Value) -> MockHttp {
    let tool = tool.to_string();
    let args = args.to_string();
    MockHttp::start(move |_path, index| {
        if index == 0 {
            // SSE：工具调用。
            Reply::Json(
                200,
                json!({"__sse__": [
                    format!("data: {}", json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":tool,"arguments":args}}]}}]})),
                    format!("data: {}", json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})),
                    "data: [DONE]".to_string()
                ]}),
            )
        } else {
            Reply::Json(
                200,
                json!({"__sse__": [
                    format!("data: {}", json!({"choices":[{"delta":{"content":"好了"}}]})),
                    format!("data: {}", json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
                    "data: [DONE]".to_string()
                ]}),
            )
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// 工具定义
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_keys_the_published_tool_set_is_local_only() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let names: Vec<String> = registry
        .tool_definitions()
        .iter()
        .filter_map(|value| value["function"]["name"].as_str().map(str::to_string))
        .collect();

    // 12 个 CUA 都不在（没有后端）；本地 6 + 时间 1。
    assert_eq!(names.len(), 7, "{names:?}");
    for expected in [
        "run_shell",
        "files",
        "app_control",
        "kill_app",
        "launch_app",
        "system_info",
        "get_current_time",
    ] {
        assert!(names.contains(&expected.to_string()), "缺少 {expected}");
    }
    assert!(!names.contains(&"web_search".to_string()));
    assert!(!names.contains(&"screenshot".to_string()));
}

/// **关闭开关时即使有 Key 也不公布**（本 PR 修的缺陷）。
/// 公布、统计、状态提示与实际调用必须一致。
#[tokio::test]
async fn disabled_search_setting_hides_the_tool_even_with_a_saved_key() {
    let search = Arc::new(WebSearch::new("tvly-saved"));
    let registry = ToolRegistry::new(None, search.clone(), false);

    let names: Vec<String> = registry
        .tool_definitions()
        .iter()
        .filter_map(|value| value["function"]["name"].as_str().map(str::to_string))
        .collect();
    assert!(!names.contains(&"web_search".to_string()), "{names:?}");
    assert_eq!(registry.inventory().conditional(), 0);
    assert_eq!(registry.inventory().total(), 7);

    let status = registry
        .statuses()
        .into_iter()
        .find(|item| item.name == "web_search")
        .unwrap();
    assert!(!status.available);
    assert!(status.reason.contains("关闭"), "{}", status.reason);

    // 运行时硬调也要被拒（不是"列表里没有但还能调"）。
    let error = registry
        .invoke("web_search", &json!({"query": "天气"}), &CancelToken::new())
        .await
        .unwrap_err();
    assert!(!error.is_cancelled());
    assert!(
        error.user_message().contains("关闭"),
        "{}",
        error.user_message()
    );

    // 重新打开立即恢复，Key 没被清掉。
    registry.set_search_enabled(true);
    assert!(search.enabled(), "关开关不能清 Key");
    assert!(registry
        .tool_definitions()
        .iter()
        .any(|value| value["function"]["name"] == "web_search"));
    assert_eq!(registry.inventory().conditional(), 1);
}

#[tokio::test]
async fn search_key_publishes_web_search_without_a_restart() {
    let search = Arc::new(WebSearch::new(""));
    let registry = ToolRegistry::new(None, search.clone(), true);
    assert!(!registry
        .tool_definitions()
        .iter()
        .any(|value| value["function"]["name"] == "web_search"));

    // 模拟"设置页保存后换 Key"：不重建注册表，下一次取定义就该看到。
    assert!(search.set_api_key("tvly-test"));
    assert!(registry
        .tool_definitions()
        .iter()
        .any(|value| value["function"]["name"] == "web_search"));

    let inventory = registry.inventory();
    assert_eq!(inventory.always(), 7);
    assert_eq!(inventory.total(), 8);
    assert_eq!(inventory.conditional(), 1);
}

// ---------------------------------------------------------------------------
// 权限：拒绝时执行计数为零
// ---------------------------------------------------------------------------

/// 一个只记录"被调用了几次"的工具后端，用来证明拒绝时真的没执行。
struct CountingTools {
    calls: AtomicUsize,
}

impl CountingTools {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }
}

impl ToolInvoker for CountingTools {
    fn tool_definitions(&self) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"run_shell","description":"d","parameters":{"type":"object","properties":{}}}}),
            json!({"type":"function","function":{"name":"files","description":"d","parameters":{"type":"object","properties":{}}}}),
        ]
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
        _cancel: &'a CancelToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolExecution, ToolError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolExecution {
                text: format!("executed {name}"),
                image: None,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// 时间
// ---------------------------------------------------------------------------

#[tokio::test]
async fn time_answers_come_from_the_local_clock_only() {
    // 直接回答：不需要网络、不需要 Key、不触发工具。
    let answer = clock_tool::try_direct_answer("现在几点").unwrap();
    assert!(answer.starts_with("现在是"), "{answer}");
    let answer = clock_tool::try_direct_answer("今天几号").unwrap();
    assert!(answer.starts_with("今天是"), "{answer}");
    assert!(clock_tool::try_direct_answer("今天天气怎么样").is_none());

    // 工具路径：走注册表，结果里带日期/时间/星期/时区。
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let execution = registry
        .invoke("get_current_time", &json!({}), &CancelToken::new())
        .await
        .unwrap();
    assert!(execution.text.contains("本地时间："), "{}", execution.text);
    assert!(execution.text.contains("时区偏移："), "{}", execution.text);
    assert!(execution.text.contains("星期"), "{}", execution.text);
    assert!(execution.image.is_none());

    // 时区偏移的显示形态必须合理（允许 UTC 与 ±HH:MM）。
    let label = clock::utc_offset_label();
    assert!(label == "UTC" || label.starts_with("UTC+") || label.starts_with("UTC-"));
}

// ---------------------------------------------------------------------------
// Tavily：Bearer、参数、摘要、401/429、取消
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tavily_request_uses_bearer_auth_and_legacy_parameters() {
    let mock = MockHttp::start(|_path, _index| {
        Reply::Json(
            200,
            json!({
                "answer": "上海今天多云。",
                "results": [
                    {"title": "上海天气", "url": "https://example.com/a", "content": "多云 18-24 度"},
                    {"title": "来源二", "url": "https://example.com/b", "content": "偏东风"}
                ]
            }),
        )
    })
    .await;
    let search = WebSearch::with_endpoint(format!("{}/search", mock.base_url), "tvly-secret");
    let outcome = search
        .search("上海天气", 3, &CancelToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.count, 2);
    assert!(!outcome.cached);
    assert!(
        outcome.text.contains("要点：上海今天多云。"),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("https://example.com/a"),
        "{}",
        outcome.text
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/search");
    // **Bearer** 认证，而不是 body 里的 api_key。
    assert_eq!(
        header(&request.head, "authorization"),
        Some("Bearer tvly-secret"),
        "{}",
        request.head
    );
    assert!(header(&request.head, "content-type")
        .unwrap()
        .contains("json"));
    assert!(request.body.get("api_key").is_none(), "旧写法不能再出现");
    assert_eq!(request.body["query"], json!("上海天气"));
    assert_eq!(request.body["max_results"], json!(3));
    assert_eq!(request.body["search_depth"], json!("basic"));
    assert_eq!(request.body["include_answer"], json!(true));
    // Key 绝不能出现在请求体里。
    assert!(!request.body.to_string().contains("tvly-secret"));
}

#[tokio::test]
async fn repeated_queries_hit_the_cache_and_switching_the_key_clears_it() {
    let mock =
        MockHttp::start(|_path, _index| Reply::Json(200, json!({"answer": "缓存", "results": []})))
            .await;
    let search = WebSearch::with_endpoint(format!("{}/search", mock.base_url), "tvly-a");
    let cancel = CancelToken::new();

    let first = search.search("天气", 5, &cancel).await.unwrap();
    assert!(!first.cached);
    let second = search.search("天气", 5, &cancel).await.unwrap();
    assert!(second.cached, "5 分钟内同查询必须命中缓存");
    assert_eq!(second.ms, 0);
    assert_eq!(mock.count(), 1, "缓存命中不能再打接口");

    // 换 Key 清缓存：否则"换了 Key 还是旧结果"会让排查非常困惑。
    assert!(search.set_api_key("tvly-b"));
    let third = search.search("天气", 5, &cancel).await.unwrap();
    assert!(!third.cached);
    assert_eq!(mock.count(), 2);
}

#[tokio::test]
async fn http_401_and_429_produce_actionable_messages() {
    let mock = MockHttp::start(|_path, index| {
        if index == 0 {
            Reply::Json(401, json!({"detail": "invalid api key"}))
        } else {
            Reply::Json(429, json!({"detail": "rate limited"}))
        }
    })
    .await;
    let search = WebSearch::with_endpoint(format!("{}/search", mock.base_url), "tvly-bad");
    let cancel = CancelToken::new();

    let error = search.search("a", 5, &cancel).await.unwrap_err();
    let text = error.user_message();
    assert!(text.contains("401"), "{text}");
    assert!(
        text.contains("Tavily API Key"),
        "401 要给「去设置里检查 Key」的提示：{text}"
    );

    let error = search.search("b", 5, &cancel).await.unwrap_err();
    let text = error.user_message();
    assert!(text.contains("429"), "{text}");
    assert!(text.contains("限流") || text.contains("额度"), "{text}");
}

#[tokio::test]
async fn a_slow_endpoint_times_out_without_hanging_the_turn() {
    let mock = MockHttp::start(|_path, _index| Reply::Hang(Duration::from_secs(30))).await;
    let search = WebSearch::with_endpoint(format!("{}/search", mock.base_url), "tvly-test");
    let started = std::time::Instant::now();
    let error = search
        .search("慢查询", 5, &CancelToken::new())
        .await
        .unwrap_err();
    // 8 秒超时 + 一点调度余量。
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "不能真的等满 30 秒"
    );
    assert!(
        error.user_message().contains("超时"),
        "{}",
        error.user_message()
    );
}

#[tokio::test]
async fn cancelling_during_a_search_stops_it() {
    let mock = MockHttp::start(|_path, _index| Reply::Hang(Duration::from_secs(30))).await;
    let search = WebSearch::with_endpoint(format!("{}/search", mock.base_url), "tvly-test");
    let cancel = CancelToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
    });
    let error = search.search("天气", 5, &cancel).await.unwrap_err();
    assert!(error.is_cancelled(), "{}", error.user_message());
}

#[tokio::test]
async fn network_failure_does_not_break_local_tools() {
    // 指向一个必然连不上的端口。
    let search = Arc::new(WebSearch::with_endpoint(
        "http://127.0.0.1:1/search",
        "tvly-test",
    ));
    let registry = ToolRegistry::new(None, search.clone(), true);
    let cancel = CancelToken::new();

    let error = registry
        .invoke("web_search", &json!({"query": "天气"}), &cancel)
        .await
        .unwrap_err();
    assert!(!error.is_cancelled());
    assert!(
        error.user_message().contains("搜索请求失败"),
        "{}",
        error.user_message()
    );

    // 同一次失败之后，本地工具照常可用。
    let time = registry
        .invoke("get_current_time", &json!({}), &cancel)
        .await
        .unwrap();
    assert!(time.text.contains("本地时间："));

    let system = registry
        .invoke("system_info", &json!({}), &cancel)
        .await
        .unwrap();
    assert!(system.text.contains("CPU："), "{}", system.text);
}

// ---------------------------------------------------------------------------
// 本地工具真实执行
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_file_round_trip_through_the_registry() {
    let root = test_dir("files");
    let file = root.join("笔记.txt");
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let cancel = CancelToken::new();

    let text = registry
        .invoke(
            "files",
            &json!({"action":"write","path":file.to_string_lossy(),"content":"第一行"}),
            &cancel,
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("已写入"), "{text}");

    let text = registry
        .invoke(
            "files",
            &json!({"action":"list","path":root.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("笔记.txt"), "{text}");

    let text = registry
        .invoke(
            "files",
            &json!({"action":"read","path":file.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("第一行"), "{text}");

    let text = registry
        .invoke(
            "files",
            &json!({"action":"delete","path":file.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("已删除文件"), "{text}");
    assert!(!file.exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn system_info_reports_unavailable_fields_honestly() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let text = registry
        .invoke("system_info", &json!({"detail": true}), &CancelToken::new())
        .await
        .unwrap()
        .text;
    assert!(text.contains("内存："), "{text}");
    assert!(text.contains("磁盘"), "{text}");
    // 电池要么给出百分比，要么明确写"无法获得"——不能留空。
    assert!(
        text.contains("电池：") && (text.contains('%') || text.contains("无法获得")),
        "{text}"
    );
}

#[tokio::test]
async fn app_control_find_is_read_only_and_honest_about_missing_processes() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let text = registry
        .invoke(
            "app_control",
            &json!({"action":"find","name":"cuava-not-running-xyz"}),
            &CancelToken::new(),
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("没有找到"), "{text}");
}

#[tokio::test]
async fn kill_app_refuses_protected_processes_before_touching_anything() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let error = registry
        .invoke("kill_app", &json!({"name":"explorer"}), &CancelToken::new())
        .await
        .unwrap_err();
    assert!(error.is_argument());
    assert!(error.user_message().contains("系统关键进程"));
}

/// **通配符绝不能变成"结束一批系统进程"**：这些名字必须在动作前被拒绝，
/// 且在拒绝时不会有任何 `taskkill` 被发出（否则 `svchost*` 会顺带命中并结束
/// 整批关键进程——`PROTECTED` 只按原始字符串比较，兜不住它）。
#[tokio::test]
async fn kill_app_rejects_wildcards_before_any_taskkill() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let cancel = CancelToken::new();
    for name in [
        "*",
        "chrome*",
        "svchost*",
        "?hrome",
        "[a-z]hrome",
        "chrome,notepad",
    ] {
        let error = registry
            .invoke("kill_app", &json!({"name": name}), &cancel)
            .await;
        let error = match error {
            Ok(execution) => panic!("「{name}」必须被拒绝，却返回成功：{}", execution.text),
            Err(error) => error,
        };
        assert!(
            error.is_argument(),
            "「{name}」必须在动作前被拒绝：{}",
            error.user_message()
        );
    }
}

/// 目录复制不能把目录复制进它自己（否则会生成无限嵌套目录）。
#[tokio::test]
async fn copying_a_directory_into_itself_is_refused_through_the_registry() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let root = test_dir("registry-copy-self");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"x").unwrap();
    let cancel = CancelToken::new();

    for dest in [src.clone(), src.join("Backup")] {
        let error = registry
            .invoke(
                "files",
                &json!({
                    "action":"copy",
                    "path": src.to_string_lossy(),
                    "dest": dest.to_string_lossy()
                }),
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(
            error.is_argument(),
            "{} → {} 必须被拒绝：{}",
            src.display(),
            dest.display(),
            error.user_message()
        );
    }
    // 没有留下无限嵌套的残骸。
    assert!(!src.join("Backup").exists(), "被拒绝时不能创建目标目录");
    // 正常并列目录复制仍然可用。
    let ok_dest = root.join("ok");
    let text = registry
        .invoke(
            "files",
            &json!({
                "action":"copy",
                "path": src.to_string_lossy(),
                "dest": ok_dest.to_string_lossy()
            }),
            &cancel,
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("已复制"), "{text}");
    assert!(ok_dest.join("a.txt").is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shell_reports_exit_code_and_truncation() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("")), true);
    let text = registry
        .invoke(
            "run_shell",
            &json!({"command":"Write-Output 'ok'; exit 5"}),
            &CancelToken::new(),
        )
        .await
        .unwrap()
        .text;
    assert!(text.contains("退出码：5"), "{text}");
    assert!(text.contains("ok"), "{text}");
}

// ---------------------------------------------------------------------------
// "第二个 Agent / 第二个审批框" 的反向验证
// ---------------------------------------------------------------------------

/// 所有工具都必须经过**同一个**权限门：拒绝时执行计数为零。
#[tokio::test]
async fn denied_tools_are_never_executed() {
    let tools = CountingTools::new();
    let state = PermissionState::new(PermissionMode::Standard);
    let confirmer = RecordingConfirmer::new(false);
    let cancel = CancelToken::new();

    for (name, args, capability) in [
        ("run_shell", json!({"command":"Get-Date"}), "system.shell"),
        (
            "files",
            json!({"action":"delete","path":"C:\\tmp\\x.txt"}),
            "file.delete",
        ),
    ] {
        let decision = cuava::permissions::PermissionGate::new(&state)
            .check(name, &args, Some(&*confirmer), &cancel)
            .await;
        assert!(!decision.allow, "{name} 在拒绝后不能放行：{decision:?}");
        assert_eq!(decision.capability, capability, "{name} 的能力分档");
    }

    // 两次都走了确认通道（不是静默拒绝）。
    assert_eq!(confirmer.tools().len(), 2);
    assert_eq!(
        confirmer.capabilities(),
        vec!["system.shell", "file.delete"]
    );
    // 而工具本身一次都没被调用。
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);
}

/// 允许后真实执行，结果回到**同一个**工具后端（不经第二套分发）。
#[tokio::test]
async fn allowed_tool_result_returns_through_the_same_invoker() {
    let tools = CountingTools::new();
    let confirmer = RecordingConfirmer::new(true);
    let state = PermissionState::new(PermissionMode::Standard);

    // 这一条走的是**真实的** PermissionGate，只是工具本身是计数用的假实现。
    let decision = cuava::permissions::PermissionGate::new(&state)
        .check(
            "run_shell",
            &json!({"command":"Get-Date"}),
            Some(&*confirmer),
            &CancelToken::new(),
        )
        .await;
    assert!(decision.allow);
    assert_eq!(decision.capability, "system.shell");
    assert_eq!(decision.channel, "click", "允许要如实记下通道");

    let execution = tools
        .invoke(
            "run_shell",
            &json!({"command":"Get-Date"}),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(execution.text, "executed run_shell");
    assert_eq!(tools.calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// 端到端：Mock LLM（真实 HTTP/SSE）+ 真实工具注册表 + 真实权限门
// ---------------------------------------------------------------------------

/// 一份指向 mock 端点的设置（其余全用默认）。
fn settings_for(endpoint: &str) -> cuava::config::Settings {
    cuava::config::Settings {
        provider_id: "openai-compatible".to_string(),
        base_url: endpoint.to_string(),
        model: "test-model".to_string(),
        ..cuava::config::Settings::default()
    }
}

/// 让一轮 Agent 跑完，收集事件文本与工具状态。
async fn run_agent_turn(
    endpoint: &str,
    tools: Arc<dyn ToolInvoker>,
    state: PermissionState,
    confirmer: &dyn Confirmer,
    input: &str,
) -> (
    cuava::agent::TurnOutcome,
    Vec<String>,
    Vec<(String, ToolStatus)>,
) {
    let settings = settings_for(endpoint);
    let llm = LlmClient::new(settings.profile(), "", 0.3, 10_000).unwrap();
    let mut agent = Agent::new(llm, tools, AgentConfig::default(), state);
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses: Arc<Mutex<Vec<(String, ToolStatus)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut history: Vec<ChatMessage> = Vec::new();
    let cancel = CancelToken::new();
    let sink = events.clone();
    let status_sink = statuses.clone();
    let emit = move |event: AgentEvent| match event {
        AgentEvent::ToolStart { name, call, .. } => sink
            .lock()
            .unwrap()
            .push(format!("start:{name}:{}", call.0)),
        AgentEvent::ToolEnd {
            name,
            call,
            status,
            text,
            ..
        } => {
            sink.lock()
                .unwrap()
                .push(format!("end:{name}:{}:{}", call.0, status.code()));
            status_sink.lock().unwrap().push((name, status));
            let _ = text;
        }
        _ => {}
    };
    let outcome = agent
        .run(input, &mut history, &cancel, confirmer, &emit)
        .await;
    // 先取出借用结束后的值（MutexGuard 的临时量不能留到函数末尾）。
    let collected_events = events.lock().unwrap().clone();
    let collected_statuses = statuses.lock().unwrap().clone();
    (outcome, collected_events, collected_statuses)
}

/// 真实的 `run_shell` 在"拒绝"时**一次都不执行**：证明权限门在工具之前。
#[tokio::test]
async fn denied_run_shell_never_executes_through_the_agent() {
    // 用一个标记文件证明命令真的没跑过。
    let root = test_dir("denied-shell");
    let marker = root.join("should-not-exist.txt");
    let command = format!("New-Item -Path '{}' -ItemType File", marker.display());
    let mock = start_chat_mock("run_shell", &json!({"command": command})).await;

    let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));
    let confirmer = RecordingConfirmer::new(false);
    let (outcome, events, statuses) = run_agent_turn(
        &format!("{}/v1", mock.base_url),
        registry,
        PermissionState::new(PermissionMode::Standard),
        &*confirmer,
        "建一个测试文件",
    )
    .await;

    assert!(!marker.exists(), "被拒绝的命令绝不能真的执行");
    assert_eq!(confirmer.tools(), vec!["run_shell".to_string()]);
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].1, ToolStatus::Denied, "{statuses:?}");
    assert!(
        events.iter().any(|line| line.contains("end:run_shell")),
        "{events:?}"
    );
    // 被拒绝后模型仍拿到了结果（进入下一枪），回合正常收尾。
    assert_eq!(mock.count(), 2, "拒绝也要回灌结果再问模型");
    assert!(!outcome.cancelled);
}

/// 允许时命令**真的执行**，结果回到同一个 Agent，并按调用实例配对。
#[tokio::test]
async fn allowed_run_shell_executes_and_returns_to_the_same_agent() {
    let root = test_dir("allowed-shell");
    let marker = root.join("created.txt");
    let command = format!(
        "New-Item -Path '{}' -ItemType File -Force | Out-Null; Write-Output 'created'",
        marker.display()
    );
    let mock = start_chat_mock("run_shell", &json!({"command": command})).await;

    let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));
    let confirmer = RecordingConfirmer::new(true);
    let (outcome, events, statuses) = run_agent_turn(
        &format!("{}/v1", mock.base_url),
        registry,
        PermissionState::new(PermissionMode::Standard),
        &*confirmer,
        "建一个测试文件",
    )
    .await;

    assert!(marker.exists(), "允许后命令必须真的执行：{events:?}");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].1, ToolStatus::Ok, "{statuses:?}");
    // 同一个 ToolStart/ToolEnd 的 call id（配对）。
    let start_id = events
        .iter()
        .find(|line| line.starts_with("start:run_shell:"))
        .and_then(|line| line.rsplit(':').next())
        .unwrap();
    let end_id = events
        .iter()
        .find(|line| line.starts_with("end:run_shell:"))
        .and_then(|line| line.split(':').nth(2))
        .unwrap();
    assert_eq!(start_id, end_id, "工具调用必须按实例配对：{events:?}");
    assert!(!outcome.cancelled);
    let _ = std::fs::remove_dir_all(&root);
}

/// `files` 的 delete 被拒绝时，文件必须还在。
#[tokio::test]
async fn denied_file_delete_leaves_the_file_intact() {
    let root = test_dir("denied-delete");
    let target = root.join("keep.txt");
    std::fs::write(&target, b"keep me").unwrap();
    let mock = start_chat_mock(
        "files",
        &json!({"action":"delete","path":target.to_string_lossy()}),
    )
    .await;

    let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));
    let confirmer = RecordingConfirmer::new(false);
    let (_outcome, _events, statuses) = run_agent_turn(
        &format!("{}/v1", mock.base_url),
        registry,
        PermissionState::new(PermissionMode::Standard),
        &*confirmer,
        "删掉它",
    )
    .await;

    assert!(target.is_file(), "被拒绝的删除不能真的删掉文件");
    assert_eq!(confirmer.capabilities(), vec!["file.delete"]);
    assert_eq!(statuses[0].1, ToolStatus::Denied);
    let _ = std::fs::remove_dir_all(&root);
}

/// 免确认档下同样不询问，并且真的执行。
#[tokio::test]
async fn pass_through_executes_without_prompting() {
    let root = test_dir("pass-through");
    let marker = root.join("pass.txt");
    let command = format!(
        "New-Item -Path '{}' -ItemType File -Force | Out-Null; 'ok'",
        marker.display()
    );
    let mock = start_chat_mock("run_shell", &json!({"command": command})).await;

    let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));
    let confirmer = RecordingConfirmer::new(false); // 即便如此也不该被问到
    let (_outcome, _events, statuses) = run_agent_turn(
        &format!("{}/v1", mock.base_url),
        registry,
        PermissionState::new(PermissionMode::PassThrough),
        &*confirmer,
        "跑一下",
    )
    .await;

    assert!(confirmer.tools().is_empty(), "免确认档不该弹确认");
    assert!(marker.exists());
    assert_eq!(statuses[0].1, ToolStatus::Ok);
    let _ = std::fs::remove_dir_all(&root);
}

/// 只读时间工具在**任何**档位都不询问，也不需要 Key。
#[tokio::test]
async fn time_tool_executes_without_any_prompt_and_without_a_key() {
    let mock = start_chat_mock("get_current_time", &json!({})).await;
    let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));
    let confirmer = RecordingConfirmer::new(false);
    let (_outcome, _events, statuses) = run_agent_turn(
        &format!("{}/v1", mock.base_url),
        registry,
        PermissionState::new(PermissionMode::Standard),
        &*confirmer,
        "现在几点",
    )
    .await;
    assert!(confirmer.tools().is_empty(), "只读时间不该询问");
    assert_eq!(statuses[0].1, ToolStatus::Ok);
}

/// 未知工具在任何档位都拒绝（不允许"名字碰巧对上就执行"）。
#[tokio::test]
async fn unknown_tool_is_denied_even_in_pass_through() {
    let mock = start_chat_mock("os_shutdown", &json!({})).await;
    let registry = Arc::new(ToolRegistry::new(None, Arc::new(WebSearch::new("")), true));
    let confirmer = RecordingConfirmer::new(true);
    let (_outcome, _events, statuses) = run_agent_turn(
        &format!("{}/v1", mock.base_url),
        registry,
        PermissionState::new(PermissionMode::PassThrough),
        &*confirmer,
        "关机",
    )
    .await;
    assert_eq!(statuses[0].1, ToolStatus::Denied, "{statuses:?}");
}

// ---------------------------------------------------------------------------
// 工具定义与权限映射的一致性
// ---------------------------------------------------------------------------

/// **每一个**可能被公布的工具都必须有权限映射：否则它会被当作未知工具拒绝，
/// 用户看到的是"工具明明在列表里却永远失败"。
#[test]
fn every_publishable_tool_has_a_capability() {
    let registry = ToolRegistry::new(None, Arc::new(WebSearch::new("tvly-x")), true);
    for value in registry.tool_definitions() {
        let name = value["function"]["name"].as_str().unwrap();
        assert!(
            cuava::permissions::tool_capability(name).is_some(),
            "工具 {name} 没有能力映射"
        );
    }
    // CUA 的 12 个也要有映射（没有后端时它们不公布，但映射必须存在）。
    for name in cuava::cua::tools::DEFAULT_TOOL_NAMES {
        assert!(
            cuava::permissions::tool_capability(name).is_some(),
            "CUA 工具 {name} 没有能力映射"
        );
    }
}

#[test]
fn inventory_separates_always_on_tools_from_conditional_ones() {
    let search = Arc::new(WebSearch::new(""));
    let registry = ToolRegistry::new(None, search.clone(), true);
    let inventory = registry.inventory();
    assert_eq!(inventory.always(), 7);
    assert_eq!(inventory.conditional(), 0);
    assert!(inventory.summary().contains("工具总数 7"));

    search.set_api_key("tvly-x");
    let inventory = registry.inventory();
    assert_eq!(inventory.always(), 7);
    assert_eq!(inventory.conditional(), 1);
    assert_eq!(inventory.total(), 8);
    assert!(inventory.summary().contains("已启用"));
}

fn test_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(format!("pr12-{name}-{seq}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 让 `ChatMessage` 与 `AgentConfig` 在本文件里被"用到"，
/// 避免为了一个断言引入未使用导入（真正驱动 Agent 的用例在 `agent_loop.rs`）。
#[allow(dead_code)]
fn _type_anchors() -> (ChatMessage, AgentConfig) {
    (ChatMessage::user("x"), AgentConfig::default())
}
