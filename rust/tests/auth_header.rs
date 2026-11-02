//! 端到端验证：**按提供商选择 Key** 并真实进入 Authorization Header。
//!
//! 断言的是 mock 端点收到的**真实请求头**——只检查 `SecretStore` 的返回值
//! 证明不了"发出去的是哪一把 Key"。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use cuava::cancel::CancelToken;
use cuava::config::{self, AppPaths, Settings};
use cuava::llm::{ChatMessage, LlmClient};
use cuava::secret::SecretStore;

fn unique_root(prefix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(format!("{prefix}-{seq}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 记录 Authorization 头的 mock 端点。
struct AuthServer {
    base_url: String,
    headers: Arc<Mutex<Vec<Option<String>>>>,
}

impl AuthServer {
    async fn start() -> AuthServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let headers = Arc::new(Mutex::new(Vec::new()));
        let recorded = headers.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = vec![0u8; 32 * 1024];
                    let mut total = Vec::new();
                    loop {
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
                                break;
                            }
                        }
                    }
                    let head = String::from_utf8_lossy(&total).to_string();
                    let auth = head.lines().find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        if lower.starts_with("authorization:") {
                            Some(
                                line.split_once(':')
                                    .map(|(_, value)| value.trim().to_string())
                                    .unwrap_or_default(),
                            )
                        } else {
                            None
                        }
                    });
                    recorded.lock().unwrap().push(auth);

                    let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    let mut response = String::from(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                    );
                    response.push_str(&format!("{:x}\r\n{}\r\n", payload.len(), payload));
                    response.push_str("0\r\n\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        AuthServer {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            headers,
        }
    }

    fn headers(&self) -> Vec<Option<String>> {
        self.headers.lock().unwrap().clone()
    }
}

async fn send(settings: &Settings, key: String) -> Result<(), String> {
    let client = LlmClient::new(settings.profile(), key, 0.3, 5_000).map_err(|e| e.to_string())?;
    client
        .chat(
            &[ChatMessage::user("hi")],
            &[],
            &CancelToken::new(),
            |_| {},
            |_| {},
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 切换提供商时，请求头里带的是**当前提供商**的 Key。
#[cfg(windows)]
#[tokio::test]
async fn authorization_header_uses_the_current_providers_key() {
    let server = AuthServer::start().await;
    let root = unique_root("auth-header");
    let paths = AppPaths::with_root(&root);
    let store = SecretStore::new(paths.secret_file());

    store.save("mimo", "sk-mimo-abc").unwrap();
    store.save("deepseek", "sk-deepseek-xyz").unwrap();

    // 1) 当前是 mimo
    let mut settings = Settings {
        provider_id: "mimo".to_string(),
        base_url: server.base_url.clone(),
        model: "mimo-v2.5-flash".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &settings).unwrap();
    let loaded = config::load_settings(&paths).unwrap();
    let key = store.load(&loaded.provider_id).unwrap().unwrap_or_default();
    send(&settings, key).await.unwrap();

    // 2) 切到 deepseek
    settings.provider_id = "deepseek".to_string();
    settings.model = "deepseek-flash".to_string();
    config::save_settings(&paths, &settings).unwrap();
    let loaded = config::load_settings(&paths).unwrap();
    let key = store.load(&loaded.provider_id).unwrap().unwrap_or_default();
    send(&settings, key).await.unwrap();

    // 3) 切回 mimo
    settings.provider_id = "mimo".to_string();
    settings.model = "mimo-v2.5-flash".to_string();
    config::save_settings(&paths, &settings).unwrap();
    let loaded = config::load_settings(&paths).unwrap();
    let key = store.load(&loaded.provider_id).unwrap().unwrap_or_default();
    send(&settings, key).await.unwrap();

    let headers = server.headers();
    assert_eq!(headers.len(), 3, "应当正好三次请求");
    assert_eq!(headers[0].as_deref(), Some("Bearer sk-mimo-abc"));
    assert_eq!(
        headers[1].as_deref(),
        Some("Bearer sk-deepseek-xyz"),
        "切到 deepseek 后必须用它自己的 Key"
    );
    assert_eq!(
        headers[2].as_deref(),
        Some("Bearer sk-mimo-abc"),
        "切回 mimo 后原来那把 Key 仍然可用"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 当前提供商没有 Key 时，**不发** Authorization，也绝不借用别家的。
#[cfg(windows)]
#[tokio::test]
async fn missing_key_sends_no_authorization_instead_of_borrowing() {
    let server = AuthServer::start().await;
    let root = unique_root("auth-missing");
    let store = SecretStore::new(root.join("secret.bin"));

    // 只给 mimo 配了 Key
    store.save("mimo", "sk-mimo-only").unwrap();

    // 当前是 openai-compatible：没有 Key
    let settings = Settings {
        provider_id: "openai-compatible".to_string(),
        base_url: server.base_url.clone(),
        model: "mock".to_string(),
        ..Settings::default()
    };
    let key = store
        .load(&settings.provider_id)
        .unwrap()
        .unwrap_or_default();
    assert!(key.is_empty(), "当前提供商没有 Key");
    send(&settings, key).await.unwrap();

    let headers = server.headers();
    assert_eq!(headers.len(), 1);
    assert_eq!(
        headers[0], None,
        "没有 Key 时不得发送 Authorization，更不能借用 mimo 的 Key"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 清除当前提供商的 Key 之后，下一轮请求不再带 Authorization。
#[cfg(windows)]
#[tokio::test]
async fn clearing_key_removes_authorization_from_next_request() {
    let server = AuthServer::start().await;
    let root = unique_root("auth-clear");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-to-be-cleared").unwrap();

    let settings = Settings {
        provider_id: "mimo".to_string(),
        base_url: server.base_url.clone(),
        model: "mimo-v2.5-flash".to_string(),
        ..Settings::default()
    };

    let key = store.load("mimo").unwrap().unwrap_or_default();
    send(&settings, key).await.unwrap();

    store.clear("mimo").unwrap();
    let key = store.load("mimo").unwrap().unwrap_or_default();
    assert!(key.is_empty());
    send(&settings, key).await.unwrap();

    let headers = server.headers();
    assert_eq!(headers[0].as_deref(), Some("Bearer sk-to-be-cleared"));
    assert_eq!(headers[1], None, "清除后不得再带 Key");

    let _ = std::fs::remove_dir_all(&root);
}

/// 密钥不会被写进任何日志/摘要里。
#[cfg(windows)]
#[test]
fn key_never_appears_in_summaries() {
    let root = unique_root("auth-summary");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-super-secret-value").unwrap();

    let masked = store.masked_keys(&["mimo", "deepseek"]);
    let text = format!("{masked:?}");
    assert!(!text.contains("sk-super-secret-value"), "摘要里不能有明文");

    let settings = Settings::default();
    let summary = config::describe_endpoint(&settings, true);
    assert!(!summary.contains("sk-"));

    let _ = std::fs::remove_dir_all(&root);
}

/// 请求体里不会出现 Key 字段（只有 Authorization 头）。
#[cfg(windows)]
#[tokio::test]
async fn request_body_never_contains_the_key() {
    let server = AuthServer::start().await;
    let settings = Settings {
        provider_id: "openai-compatible".to_string(),
        base_url: server.base_url.clone(),
        model: "mock".to_string(),
        ..Settings::default()
    };
    let client = LlmClient::new(settings.profile(), "sk-in-header-only", 0.3, 5_000).unwrap();
    let body = client.build_body(&[ChatMessage::user("hi")], &[], true);
    let text = body.to_string();
    assert!(
        !text.contains("sk-in-header-only"),
        "Key 只能出现在 Authorization 头里"
    );
    let _: Value = json!({});
}
