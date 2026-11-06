//! `web_search`：Tavily 联网搜索（独立的"联网搜索"配置，**不复用** LLM Key）。
//!
//! 为什么需要它：走"打开浏览器 → 地址栏打字 → 截图看结果"查一次实时信息要 10~20 秒，
//! 页面一改就废。Tavily 是给 AI 用的搜索接口：它自己抓正文并给一段 `answer`，
//! 语音助手需要的正是这一句话，而不是十条链接。
//!
//! 协议按当前官方文档：`POST https://api.tavily.com/search`，
//! `Authorization: Bearer <key>`，body 里**不再**带 `api_key`（旧版那样做会被拒）。
//!
//! 语义（沿旧版 `packages/tools/web-search.mjs`，只换认证方式与传输层）：
//!  - `max_results` 夹到 1~8，默认 5；
//!  - 单条摘要截断到 320 字，来源 URL 保留（语音前端只念摘要）；
//!  - 同一查询 + 同样条数在 5 分钟内命中缓存；
//!  - 超时 8 秒、可取消；网络失败**不影响**本地工具（错误只回给这一次调用）；
//!  - 没有 Key 时 `enabled() == false`，调用方**不把工具公布给模型**。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cancel::CancelToken;
use crate::tools::error::{ArgError, ToolError};
use crate::tools::spec::{as_args, coerce_int, object_schema, require_string, ToolSpec};

/// 工具名。
pub const TOOL_NAME: &str = "web_search";
/// 官方端点（测试用 [`WebSearch::with_endpoint`] 换成 mock）。
pub const DEFAULT_ENDPOINT: &str = "https://api.tavily.com/search";
/// 单次搜索的默认结果条数。
pub const DEFAULT_MAX_RESULTS: i64 = 5;
/// 允许模型要求的最大条数（再多也不会让语音回答更好，只会更慢）。
pub const MAX_RESULTS_LIMIT: i64 = 8;
/// 单条摘要的最大字符数。
pub const SNIPPET_CHARS: usize = 320;
/// 请求超时。
pub const TIMEOUT: Duration = Duration::from_secs(8);
/// 缓存有效期：同一句话连续追问时别反复打接口。
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// 缓存最多保留多少条。
pub const CACHE_CAPACITY: usize = 32;
/// 搜索深度（Tavily 参数，旧版用 basic）。
pub const SEARCH_DEPTH: &str = "basic";

/// 工具定义。
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: TOOL_NAME,
        description: "联网搜索实时信息（天气、新闻、价格、今天发生的事、你不确定的事实）。\
比打开浏览器快得多，查这类内容优先用它。一次一个问题，不要拿它查本地文件或屏幕内容。\
它会把查询词发给第三方搜索服务。",
        parameters: object_schema(
            json!({
                "query": { "type": "string", "description": "搜索词，用最关键的几个词，不要写整句话" },
                "max_results": { "type": "number", "description": "要几条结果，默认 5，最多 8" }
            }),
            &["query"],
        ),
        read_only: true,
        long_timeout: false,
    }
}

/// 一次搜索的结论（供工具结果与设置页的"可用性测试"共用）。
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    /// 给模型读的文本。
    pub text: String,
    /// 结果条数。
    pub count: usize,
    /// 耗时（毫秒；命中缓存为 0）。
    pub ms: u64,
    pub cached: bool,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    at: Instant,
    outcome: SearchOutcome,
}

/// Tavily 客户端。**Key 可在运行期更换**（设置页保存后不用重启）。
#[derive(Debug)]
pub struct WebSearch {
    endpoint: String,
    api_key: Mutex<String>,
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// 只用于"设置页改了 Key 之后，旧代次的测试结果不许覆盖新状态"。
    revision: AtomicU64,
    /// 本次请求的整体时限。生产固定为 [`TIMEOUT`]；测试可以缩短，
    /// 免得每个超时用例都真等 8 秒（语义完全相同，只是数值不同）。
    timeout: Duration,
    http: reqwest::Client,
}

impl WebSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_endpoint(DEFAULT_ENDPOINT, api_key)
    }

    /// 测试用：指向本地 mock 端点。
    pub fn with_endpoint(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: Mutex::new(api_key.into().trim().to_string()),
            cache: Mutex::new(HashMap::new()),
            revision: AtomicU64::new(0),
            timeout: TIMEOUT,
            http: reqwest::Client::builder()
                .user_agent("cuava/0.1 (computer-use desktop)")
                .build()
                .expect("reqwest client 构建失败（TLS 后端不可用）"),
        }
    }

    /// 测试用：缩短整体时限，避免超时用例真的等满 8 秒。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 是否有可用的 Key（空 = 这个工具不存在，模型不会看到它）。
    pub fn enabled(&self) -> bool {
        self.api_key
            .lock()
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false)
    }

    /// 当前 Key 的版本号（设置页据此丢弃过期的测试结果）。
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// 运行期更换 Key。换 Key 顺手清缓存——换了 Key 还拿旧结果，排查会很困惑。
    ///
    /// 返回 `true` 表示真的变了（调用方据此决定要不要刷新"可用工具总览"）。
    pub fn set_api_key(&self, key: &str) -> bool {
        let next = key.trim().to_string();
        let Ok(mut current) = self.api_key.lock() else {
            return false;
        };
        if *current == next {
            return false;
        }
        *current = next;
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
        self.revision.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// 打一次真实的搜索接口（设置页的"测试可用性"也走这里）。
    pub async fn search(
        &self,
        query: &str,
        max_results: i64,
        cancel: &CancelToken,
    ) -> Result<SearchOutcome, ToolError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(ToolError::Arg(ArgError::new("web_search 需要 query")));
        }
        let key = self
            .api_key
            .lock()
            .map(|key| key.clone())
            .unwrap_or_default();
        if key.trim().is_empty() {
            return Err(ToolError::failed(
                "没有配置联网搜索的 API Key（在设置里填写 Tavily Key 后即可使用）",
            ));
        }
        let max_results = max_results.clamp(1, MAX_RESULTS_LIMIT);
        let cache_key = format!("{query}|{max_results}");
        if let Ok(cache) = self.cache.lock() {
            if let Some(entry) = cache.get(&cache_key) {
                if entry.at.elapsed() < CACHE_TTL {
                    let mut outcome = entry.outcome.clone();
                    outcome.cached = true;
                    outcome.ms = 0;
                    // 缓存命中也要能被打断（否则"取消"在这条路径上不生效）。
                    if cancel.is_cancelled() {
                        return Err(ToolError::cancelled());
                    }
                    return Ok(outcome);
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(ToolError::cancelled());
        }

        /*
         * 整体超时用**一个 deadline**贯穿三个阶段：发请求 → 收响应头 → 读响应体。
         *
         * 之前每个阶段各自 `sleep(TIMEOUT)`，最坏情况是 8+8+8 秒；更糟的是
         * 错误分支直接 `response.text().await`，既不看超时也不看取消——
         * 服务器回完 401 响应头后把响应体吊住，工具就会一直卡在那里。
         */
        let deadline = tokio::time::Instant::now() + self.timeout;
        let started = Instant::now();
        let request = self
            .http
            .post(&self.endpoint)
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .json(&json!({
                "query": query,
                "search_depth": SEARCH_DEPTH,
                "include_answer": true,
                "max_results": max_results,
            }));
        let response = tokio::select! {
            result = request.send() => result,
            _ = cancel.cancelled() => return Err(ToolError::cancelled()),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(ToolError::failed(format!(
                    "搜索超时（{}ms 内 Tavily 没有响应）",
                    self.timeout.as_millis()
                )));
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if cancel.is_cancelled() {
                    return Err(ToolError::cancelled());
                }
                return Err(ToolError::failed(format!(
                    "搜索请求失败：{}（本地工具不受影响）",
                    describe_transport_error(&error)
                )));
            }
        };

        let status = response.status();
        if !status.is_success() {
            /*
             * 非 2xx：**响应体也要受同一个 deadline 与取消约束**。
             *
             * 读取失败/超时不能吞掉状态码本身——401/429 的提示比"超时"更有用，
             * 所以这里退回空 body 继续走 `describe_status`，只是明确标注
             * "响应体没读完"。取消则按取消处理（回合已经不需要这个结果）。
             */
            let body = match read_error_body(response, cancel, deadline).await {
                BodyRead::Text(text) => text,
                BodyRead::TimedOut => {
                    return Err(ToolError::failed(describe_status(
                        status.as_u16(),
                        "（响应体未在超时内读完）",
                    )));
                }
                BodyRead::Cancelled => return Err(ToolError::cancelled()),
                BodyRead::Failed => String::new(),
            };
            return Err(ToolError::failed(describe_status(status.as_u16(), &body)));
        }

        // 读 body 同样受同一个 deadline 与取消约束。
        let payload = tokio::select! {
            result = response.json::<Value>() => result,
            _ = cancel.cancelled() => return Err(ToolError::cancelled()),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(ToolError::failed("搜索超时（响应体没有在限定时间内读完）"));
            }
        };
        let payload = payload
            .map_err(|error| ToolError::failed(format!("搜索接口返回的不是合法 JSON：{error}")))?;

        let outcome = SearchOutcome {
            text: format_for_model(query, &payload),
            count: payload
                .get("results")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0),
            ms: started.elapsed().as_millis() as u64,
            cached: false,
        };
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= CACHE_CAPACITY {
                // 简单策略：清掉最旧的一半，避免无界增长又不引入 LRU 复杂度。
                let mut ages: Vec<(String, Instant)> = cache
                    .iter()
                    .map(|(key, entry)| (key.clone(), entry.at))
                    .collect();
                ages.sort_by_key(|(_, at)| *at);
                for (key, _) in ages.into_iter().take(CACHE_CAPACITY / 2) {
                    cache.remove(&key);
                }
            }
            cache.insert(
                cache_key,
                CacheEntry {
                    at: Instant::now(),
                    outcome: outcome.clone(),
                },
            );
        }
        Ok(outcome)
    }
}

/// 读取错误响应体的结果。**必须区分"超时""取消""读失败"**：
/// 三者对调用方的含义完全不同（前者重试、中者丢弃、后者仍可看状态码）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyRead {
    /// 读到了（可能被截断）。
    Text(String),
    /// 超过了整体 deadline。
    TimedOut,
    /// 用户取消。
    Cancelled,
    /// 传输层错误（连接断了等）：没有内容，但状态码仍然有效。
    Failed,
}

/// 读错误响应体，受**同一个 deadline** 与取消约束。
async fn read_error_body(
    response: reqwest::Response,
    cancel: &CancelToken,
    deadline: tokio::time::Instant,
) -> BodyRead {
    let read = tokio::select! {
        result = response.text() => result,
        _ = cancel.cancelled() => return BodyRead::Cancelled,
        _ = tokio::time::sleep_until(deadline) => return BodyRead::TimedOut,
    };
    match read {
        Ok(text) => BodyRead::Text(truncate(&text, 160)),
        Err(_) => BodyRead::Failed,
    }
}

/// 401 / 403 / 429 分别给"能照做"的说明，而不是一句"请求失败"。
fn describe_status(status: u16, body: &str) -> String {
    let detail = if body.is_empty() {
        String::new()
    } else {
        format!("（{body}）")
    };
    match status {
        401 | 403 => format!(
            "搜索接口拒绝了这个 Key（HTTP {status}）{detail}。请在设置的「联网搜索」里检查 Tavily API Key"
        ),
        429 => format!("搜索接口限流或额度用完了（HTTP 429）{detail}。过一会儿再试，或检查 Tavily 账户配额"),
        432 | 433 => format!(
            "搜索接口拒绝了这次请求（HTTP {status}，常见原因是账户额度或参数不合法）{detail}"
        ),
        _ => format!("搜索接口返回 HTTP {status}{detail}"),
    }
}

/// 传输层错误：区分"连不上""TLS""被 abort"，都给可照做的提示。
fn describe_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "网络超时".to_string();
    }
    if error.is_connect() {
        return "连不上 api.tavily.com（检查网络或代理）".to_string();
    }
    truncate(&error.to_string(), 200)
}

/// 把接口结果压成模型能直接用的文本。
///
/// 故意不返回 JSON：模型读表格化文本比读 JSON 稳，长链接把它拖进幻觉的概率也更低。
/// 有 `answer` 就放最前面——语音场景要的就是这一句。
pub fn format_for_model(query: &str, payload: &Value) -> String {
    let mut lines = vec![format!("查询：{query}")];
    let answer = payload
        .get("answer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !answer.is_empty() {
        lines.push(format!("要点：{answer}"));
    }
    let results = payload
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        lines.push("（没有搜到结果）".to_string());
        return lines.join("\n");
    }
    lines.push("来源：".to_string());
    for (index, item) in results.iter().take(MAX_RESULTS_LIMIT as usize).enumerate() {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("(无标题)");
        let url = item.get("url").and_then(Value::as_str).unwrap_or("").trim();
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!(
            "{}. {title}{}",
            index + 1,
            if url.is_empty() {
                String::new()
            } else {
                format!(" — {url}")
            }
        ));
        if !content.is_empty() {
            let snippet = if content.chars().count() > SNIPPET_CHARS {
                format!(
                    "{}…",
                    content.chars().take(SNIPPET_CHARS).collect::<String>()
                )
            } else {
                content
            };
            lines.push(format!("   {snippet}"));
        }
    }
    lines.join("\n")
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max).collect::<String>())
}

/// 执行一次 `web_search` 工具调用。
pub async fn run(
    search: Arc<WebSearch>,
    args: &Value,
    cancel: &CancelToken,
) -> Result<String, ToolError> {
    let object = as_args(args);
    let query = require_string(&object, "query", "web_search")?;
    let max_results = coerce_int(&object, "max_results").unwrap_or(DEFAULT_MAX_RESULTS);
    let outcome = search.search(&query, max_results, cancel).await?;
    Ok(outcome.text)
}

/// 供设置页/测试使用的"Key 是否可用"探测。
///
/// 用一句固定的中文查询（与旧版一致），避免用户输入影响结论；
/// 返回 `Ok(描述)` 或 `Err(可读原因)`。
pub async fn probe_key(search: &WebSearch, cancel: &CancelToken) -> Result<String, ToolError> {
    let started = Instant::now();
    let outcome = search.search("上海今天天气", 3, cancel).await?;
    Ok(format!(
        "联网搜索可用：{} 条结果，{}ms{}",
        outcome.count,
        started.elapsed().as_millis(),
        if outcome.text.contains("要点：") {
            "，含摘要"
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Value {
        json!({
            "answer": "上海今天多云，18~24 度。",
            "results": [
                {"title": "上海天气", "url": "https://example.com/a", "content": "多云  转晴 18-24 度"},
                {"title": "第二个来源", "url": "https://example.com/b", "content": "x".repeat(500)},
            ]
        })
    }

    #[test]
    fn formatting_puts_the_answer_first_then_sources_with_urls() {
        let text = format_for_model("上海天气", &payload());
        assert!(text.starts_with("查询：上海天气"), "{text}");
        assert!(text.contains("要点：上海今天多云，18~24 度。"), "{text}");
        assert!(
            text.contains("1. 上海天气 — https://example.com/a"),
            "{text}"
        );
        assert!(
            text.contains("多云 转晴 18-24 度"),
            "空白要压成一个空格：{text}"
        );
    }

    #[test]
    fn snippets_are_capped_and_empty_results_are_explicit() {
        let text = format_for_model("x", &payload());
        for line in text.lines().filter(|line| line.starts_with("   ")) {
            assert!(line.chars().count() <= SNIPPET_CHARS + 4, "{line}");
        }
        let empty = format_for_model("x", &json!({"results": []}));
        assert!(empty.contains("（没有搜到结果）"), "{empty}");
    }

    #[test]
    fn results_beyond_the_limit_are_dropped() {
        let many: Vec<Value> = (0..12)
            .map(|index| json!({"title": format!("t{index}"), "url": "https://e.com", "content": "c"}))
            .collect();
        let text = format_for_model("q", &json!({"results": many}));
        assert!(text.contains("8. t7"), "{text}");
        assert!(!text.contains("9. t8"), "{text}");
    }

    #[test]
    fn key_presence_controls_availability_and_changes_clear_the_cache() {
        let search = WebSearch::with_endpoint("http://127.0.0.1:1/search", "");
        assert!(!search.enabled());
        assert!(search.set_api_key("tvly-test"));
        assert!(search.enabled());
        let before = search.revision();
        assert!(!search.set_api_key("tvly-test"), "同一个 Key 不算变化");
        assert_eq!(search.revision(), before);
        assert!(search.set_api_key("tvly-other"));
        assert!(search.revision() > before, "换 Key 必须让版本号推进");
        assert!(search.set_api_key(""));
        assert!(!search.enabled());
    }

    /// 没有 Key 时**不发出任何请求**：错误必须是"配置问题"，不是"网络问题"。
    #[tokio::test]
    async fn missing_key_fails_before_any_request() {
        let search = WebSearch::with_endpoint("http://127.0.0.1:1/search", "");
        let cancel = CancelToken::new();
        let error = search.search("天气", 5, &cancel).await.unwrap_err();
        assert!(!error.is_cancelled());
        assert!(error.user_message().contains("没有配置联网搜索"));
    }

    #[tokio::test]
    async fn empty_query_is_an_argument_error() {
        let search = WebSearch::with_endpoint("http://127.0.0.1:1/search", "tvly-test");
        let cancel = CancelToken::new();
        let error = search.search("   ", 5, &cancel).await.unwrap_err();
        assert!(error.is_argument());
    }

    #[tokio::test]
    async fn cancellation_stops_the_search_immediately() {
        let search = WebSearch::new("tvly-test");
        let cancel = CancelToken::new();
        cancel.cancel();
        let error = search.search("天气", 5, &cancel).await.unwrap_err();
        assert!(error.is_cancelled());
    }

    #[test]
    fn status_messages_name_the_actual_problem() {
        assert!(describe_status(401, "").contains("检查 Tavily API Key"));
        assert!(describe_status(429, "").contains("限流或额度用完"));
        assert!(describe_status(500, "boom").contains("500"));
        assert!(describe_status(500, "boom").contains("boom"));
    }

    #[test]
    fn spec_declares_query_and_rejects_unknown_fields() {
        let spec = spec();
        assert_eq!(spec.name, TOOL_NAME);
        assert!(spec.read_only);
        assert_eq!(spec.parameters["required"], json!(["query"]));
        assert_eq!(spec.parameters["additionalProperties"], json!(false));
    }
}
