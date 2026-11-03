//! MiMo-V2.5-TTS 流式协议的端到端回归（对着字节级 mock 端点）。
//!
//! 这些缺陷的共同点：**单测里拼好的 JSON 看不出问题**，只有真的按字节分片
//! 才会暴露（事件被切开、Base64 里切出半个采样、`[DONE]` 与空 choices 混在
//! 同一个响应里）。所以这里起一个原始字节级的 mock：按测试给定的切点写字节，
//! 并记录真实请求体与 Authorization 头。
//!
//! 一个 mock 服务供全部用例共享（按请求路径区分用例），避免多个测试争抢
//! `CUAVA_TTS_BASE_URL` 这个进程级环境变量。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::Value;

use cuava::cancel::CancelToken;
use cuava::voice::tts::mimo::MimoTts;
use cuava::voice::tts::{SynthRequest, TtsBackend, TtsEngine, TtsEvent, TtsSettings};

/// 一次请求要回什么。
#[derive(Clone)]
enum Reply {
    /// 按给定的字节块依次写出（块之间留一点时间，确保客户端真的分多次读）。
    Script(Vec<Vec<u8>>),
    /// 返回非 2xx。
    Status(u16, String),
    /// 写一部分就断开（模拟服务端断流）。
    Truncate(Vec<u8>),
}

#[derive(Clone, Default)]
struct Recorded {
    body: Value,
    authorization: Option<String>,
}

struct MockServer {
    port: u16,
    scripts: Arc<Mutex<HashMap<String, Reply>>>,
    recorded: Arc<Mutex<HashMap<String, Vec<Recorded>>>>,
}

impl MockServer {
    fn global() -> &'static MockServer {
        static SERVER: OnceLock<MockServer> = OnceLock::new();
        SERVER.get_or_init(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("测试用 runtime");
            let (tx, rx) = std::sync::mpsc::channel::<u16>();
            let scripts: Arc<Mutex<HashMap<String, Reply>>> = Arc::new(Mutex::new(HashMap::new()));
            let recorded: Arc<Mutex<HashMap<String, Vec<Recorded>>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let scripts_task = scripts.clone();
            let recorded_task = recorded.clone();
            std::thread::spawn(move || {
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .expect("bind");
                    let port = listener.local_addr().unwrap().port();
                    // 让所有用例共用同一个端点（环境变量只设一次）。
                    // SAFETY: 只在测试进程启动早期设置一次，之后的读取发生在
                    // 请求构造时；这里不涉及多线程同时写同一变量。
                    unsafe {
                        std::env::set_var("CUAVA_TTS_BASE_URL", format!("http://127.0.0.1:{port}/v1"));
                    }
                    let _ = tx.send(port);
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            continue;
                        };
                        let scripts = scripts_task.clone();
                        let recorded = recorded_task.clone();
                        tokio::spawn(async move {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let mut buffer = vec![0u8; 32 * 1024];
                            let mut total = Vec::new();
                            let (case, body, authorization) = loop {
                                let Ok(read) = socket.read(&mut buffer).await else {
                                    return;
                                };
                                if read == 0 {
                                    return;
                                }
                                total.extend_from_slice(&buffer[..read]);
                                if let Some(header_end) =
                                    total.windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    let head =
                                        String::from_utf8_lossy(&total[..header_end]).to_string();
                                    let length = head
                                        .lines()
                                        .find_map(|line| {
                                            let lower = line.to_ascii_lowercase();
                                            lower
                                                .strip_prefix("content-length:")
                                                .map(|rest| rest.trim().parse::<usize>().unwrap_or(0))
                                        })
                                        .unwrap_or(0);
                                    let body_start = header_end + 4;
                                    if total.len() >= body_start + length {
                                        let case = head
                                            .lines()
                                            .next()
                                            .and_then(|line| line.split_whitespace().nth(1))
                                            .unwrap_or("/")
                                            .trim_start_matches("/v1/")
                                            .split('/')
                                            .next()
                                            .unwrap_or("default")
                                            .to_string();
                                        let body: Value = serde_json::from_slice(
                                            &total[body_start..body_start + length],
                                        )
                                        .unwrap_or(Value::Null);
                                        let authorization = head.lines().find_map(|line| {
                                            let lower = line.to_ascii_lowercase();
                                            if lower.starts_with("authorization:") {
                                                Some(line["authorization:".len()..].trim().to_string())
                                            } else {
                                                None
                                            }
                                        });
                                        break (case, body, authorization);
                                    }
                                }
                            };

                            recorded
                                .lock()
                                .unwrap()
                                .entry(case.clone())
                                .or_default()
                                .push(Recorded {
                                    body,
                                    authorization,
                                });
                            let reply = scripts.lock().unwrap().get(&case).cloned();
                            let Some(reply) = reply else {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n")
                                    .await;
                                return;
                            };
                            match reply {
                                Reply::Status(code, body) => {
                                    let head = format!(
                                        "HTTP/1.1 {code} ERROR\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                        body.len()
                                    );
                                    let _ = socket.write_all(head.as_bytes()).await;
                                    let _ = socket.write_all(body.as_bytes()).await;
                                }
                                Reply::Script(chunks) => {
                                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                                    let _ = socket.write_all(head.as_bytes()).await;
                                    for chunk in chunks {
                                        let prefix = format!("{:x}\r\n", chunk.len());
                                        let _ = socket.write_all(prefix.as_bytes()).await;
                                        let _ = socket.write_all(&chunk).await;
                                        let _ = socket.write_all(b"\r\n").await;
                                        let _ = socket.flush().await;
                                        tokio::time::sleep(Duration::from_millis(2)).await;
                                    }
                                    let _ = socket.write_all(b"0\r\n\r\n").await;
                                }
                                Reply::Truncate(chunk) => {
                                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                                    let _ = socket.write_all(head.as_bytes()).await;
                                    let prefix = format!("{:x}\r\n", chunk.len());
                                    let _ = socket.write_all(prefix.as_bytes()).await;
                                    let _ = socket.write_all(&chunk).await;
                                    let _ = socket.write_all(b"\r\n").await;
                                    let _ = socket.flush().await;
                                    // 直接 close（不发终止块）：模拟服务端断流。
                                }
                            }
                        });
                    }
                });
            });
            let port = rx.recv().expect("mock server 启动");
            MockServer {
                port,
                scripts,
                recorded,
            }
        })
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn install(&self, case: &str, reply: Reply) {
        self.scripts.lock().unwrap().insert(case.to_string(), reply);
        self.recorded.lock().unwrap().remove(case);
    }

    fn recorded(&self, case: &str) -> Vec<Recorded> {
        let guard = self.recorded.lock().unwrap();
        guard.get(case).cloned().unwrap_or_default()
    }
}

/// 构造一个指向 mock 的引擎；`case` 决定 mock 回什么。
///
/// `CUAVA_TTS_BASE_URL` 是进程级变量，多个用例并行时互相覆盖。这里只把
/// **构造**这一小段放进锁里：端点在 `MimoTts::new` 时就被算好并固定下来，
/// 之后的请求不受别的用例改环境变量影响。
fn engine_for(case: &str, reply: Reply) -> MimoTts {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let server = MockServer::global();
    server.install(case, reply);
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // 用 case 作为路径前缀：同一个 mock 服务上并行跑多个用例互不干扰。
    // SAFETY: 见 MockServer::global 里对 set_var 的说明。
    unsafe {
        std::env::set_var(
            "CUAVA_TTS_BASE_URL",
            format!("{}/{case}", server.base_url()),
        );
    }
    MimoTts::new(
        &TtsSettings {
            enabled: true,
            backend: TtsBackend::Mimo,
            mimo_voice: "alloy".to_string(),
            mimo_style: "语速稍快".to_string(),
            ..TtsSettings::default()
        },
        "sk-mimo-test-key".to_string(),
    )
    .expect("构造 MiMo 引擎")
}

fn sse(json: &str) -> Vec<u8> {
    format!("data: {json}\n\n").into_bytes()
}

fn audio_event(pcm: &[u8]) -> Vec<u8> {
    let data = base64::engine::general_purpose::STANDARD.encode(pcm);
    sse(&format!(
        r#"{{"choices":[{{"delta":{{"audio":{{"data":"{data}"}}}}}}]}}"#
    ))
}

fn done_event() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

fn request(speech_id: u64) -> SynthRequest {
    SynthRequest {
        speech_id,
        turn: speech_id,
        sentence: 0,
        text: "你好，世界。".to_string(),
        style: "语速稍快".to_string(),
        cancel: CancelToken::new(),
    }
}

/// 收集全部音频块，返回（样本总数、失败信息、是否收到 Finished）。
async fn drain(mut stream: cuava::voice::tts::SpeechStream) -> (usize, Option<String>, bool) {
    let mut samples = 0usize;
    let mut failure = None;
    let mut finished = false;
    while let Some(event) = stream.recv().await {
        match event {
            TtsEvent::Chunk(chunk) => {
                assert_eq!(chunk.sample_rate, 24_000, "MiMo 官方示例是 24kHz");
                assert_eq!(chunk.channels, 1, "官方示例是单声道");
                samples += chunk.samples.len();
            }
            TtsEvent::Finished => finished = true,
            TtsEvent::Failed(message) => failure = Some(message),
        }
    }
    (samples, failure, finished)
}

/// **首批 PCM 一到就能开始播放**：不能等整段请求结束。
#[tokio::test]
async fn first_chunk_arrives_before_the_stream_finishes() {
    let case = "early-start";
    let first = audio_event(&[0x00, 0x01, 0x00, 0x02]);
    let mut script = vec![first];
    // 后面还有一大段 + [DONE]，故意中间插入延迟。
    for _ in 0..40 {
        script.push(audio_event(&[0x00, 0x03]));
    }
    script.push(done_event());
    let engine = engine_for(case, Reply::Script(script.clone()));
    let started = Instant::now();
    let mut stream = engine.start(request(1)).expect("启动合成");

    let first_event = tokio::time::timeout(Duration::from_millis(500), stream.recv())
        .await
        .expect("第一批音频必须在 500ms 内到达")
        .expect("应当收到事件");
    let TtsEvent::Chunk(chunk) = first_event else {
        panic!("第一批事件应当是音频块");
    };
    assert_eq!(chunk.samples.len(), 2);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "第一批 PCM 到达得比整段结束还晚：{:?}",
        started.elapsed()
    );
    // 剩下的照常收完（首块已经在这里消费掉了，要一起算总数）。
    let (rest, failure, finished) = drain(stream).await;
    assert!(failure.is_none(), "{failure:?}");
    assert!(finished);
    assert_eq!(rest + 2, 2 + 40, "样本数必须与发送的一致");
}

/// 极端分片 + 奇数字节：不能错位半帧、不能丢样本。
#[tokio::test]
async fn odd_byte_boundaries_do_not_corrupt_the_audio() {
    let case = "odd-bytes";
    // 每个样本 2 字节，故意让每个事件只带 1 或 3 字节。
    let pcm: Vec<u8> = (0..41u8).collect();
    let mut script = Vec::new();
    for chunk in pcm.chunks(1).take(21) {
        script.push(audio_event(chunk));
    }
    for chunk in pcm[21..].chunks(3) {
        script.push(audio_event(chunk));
    }
    script.push(done_event());
    // 再把每个字节块合并成"一个字节一个块"的网络写，验证 SSE 边界也扛得住。
    let flattened: Vec<Vec<u8>> = script
        .into_iter()
        .flat_map(|chunk| chunk.into_iter().map(|byte| vec![byte]).collect::<Vec<_>>())
        .collect();
    let engine = engine_for(case, Reply::Script(flattened));
    let (samples, failure, finished) = drain(engine.start(request(2)).unwrap()).await;
    assert!(failure.is_none(), "{failure:?}");
    assert!(finished);
    assert_eq!(samples, pcm.len() / 2, "41 字节 = 20 个完整样本 + 1 个残留");
}

/// 空 choices（只带 usage 的收尾块）、无关字段与 `[DONE]` 都要正确处理。
#[tokio::test]
async fn empty_choices_and_usage_are_ignored() {
    let case = "empty-choices";
    let mut script = vec![audio_event(&[0x10, 0x20, 0x30, 0x40])];
    script.push(sse(r#"{"choices":[],"usage":{"total_tokens":12}}"#));
    script.push(sse(r#"{"choices":[{"delta":{"content":"hi"}}]}"#));
    script.push(audio_event(&[0x50, 0x60]));
    script.push(done_event());
    let engine = engine_for(case, Reply::Script(script));
    let (samples, failure, finished) = drain(engine.start(request(3)).unwrap()).await;
    assert!(failure.is_none(), "{failure:?}");
    assert!(finished, "[DONE] 之后必须正常结束");
    assert_eq!(samples, 3);
}

/// 401 要给出可读原因，且**不能把 Key 回显到界面上**。
#[tokio::test]
async fn unauthorized_reports_readable_error_without_leaking_the_key() {
    let case = "unauthorized";
    let engine = engine_for(
        case,
        Reply::Status(
            401,
            r#"{"error":{"message":"invalid api key sk-mimo-test-key"}}"#.to_string(),
        ),
    );
    let (_samples, failure, finished) = drain(engine.start(request(4)).unwrap()).await;
    let message = failure.expect("401 必须报失败");
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("MiMo"), "{message}");
    assert!(
        !message.contains("sk-mimo-test-key"),
        "错误信息不能回显 Key：{message}"
    );
    assert!(!finished);
}

/// 服务端断流（没有 `[DONE]`、连接直接断）：不能当成"播完了"。
#[tokio::test]
async fn truncated_stream_is_reported_not_silently_finished() {
    let case = "truncated";
    let engine = engine_for(case, Reply::Truncate(audio_event(&[0x01, 0x02])));
    let (samples, failure, finished) = drain(engine.start(request(5)).unwrap()).await;
    assert!(samples > 0, "已经到达的音频应当照常交付");
    let message = failure.expect("断流必须如实报告");
    assert!(
        message.contains("中断") || message.contains("超时"),
        "{message}"
    );
    assert!(!finished, "断流不能算正常播完");
}

/// 完全没有音频：必须报"没有返回音频"，而不是静默成功。
#[tokio::test]
async fn no_audio_is_an_error() {
    let case = "no-audio";
    let engine = engine_for(
        case,
        Reply::Script(vec![sse(r#"{"choices":[]}"#), done_event()]),
    );
    let (_samples, failure, finished) = drain(engine.start(request(6)).unwrap()).await;
    let message = failure.expect("没有音频必须报错");
    assert!(message.contains("没有返回音频"), "{message}");
    assert!(!finished);
}

/// 取消：立刻停止，不再产出音频。
#[tokio::test]
async fn cancel_stops_the_stream() {
    let case = "cancel";
    let mut script = vec![audio_event(&[0x00, 0x01])];
    for _ in 0..200 {
        script.push(audio_event(&[0x00, 0x02]));
    }
    script.push(done_event());
    let engine = engine_for(case, Reply::Script(script));
    let synth = request(7);
    let cancel = synth.cancel.clone();
    let mut stream = engine.start(synth).unwrap();
    let first = stream.recv().await.expect("先收到一块");
    assert!(matches!(first, TtsEvent::Chunk(_)));
    cancel.cancel();
    // 取消后最多再收到已就绪的一块，然后必须结束；不能一直吐音频。
    let mut extra = 0usize;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(2), stream.recv())
        .await
        .expect("取消后必须在 2 秒内结束")
    {
        if matches!(event, TtsEvent::Chunk(_)) {
            extra += 1;
        }
        if matches!(event, TtsEvent::Failed(_)) {
            panic!("取消不是失败");
        }
        assert!(extra < 50, "取消后还在源源不断地产生音频");
    }
}

/// 请求协议与 Key 归属：只发 MiMo 的 Key，结构与官方示例一致。
#[tokio::test]
async fn request_uses_official_protocol_and_the_mimo_key() {
    let case = "protocol";
    let engine = engine_for(
        case,
        Reply::Script(vec![audio_event(&[0, 0]), done_event()]),
    );
    let (samples, failure, _finished) = drain(engine.start(request(8)).unwrap()).await;
    assert!(failure.is_none(), "{failure:?}");
    assert_eq!(samples, 1);

    let recorded = MockServer::global().recorded(case);
    let record = recorded.first().expect("mock 必须收到一次请求");
    assert_eq!(
        record.authorization.as_deref(),
        Some("Bearer sk-mimo-test-key"),
        "只能用 MiMo 账户自己的 Key"
    );
    let body = &record.body;
    assert_eq!(body["model"], "mimo-v2.5-tts");
    assert_eq!(body["stream"], true);
    assert_eq!(body["audio"]["format"], "pcm16");
    assert_eq!(body["audio"]["voice"], "alloy");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user", "风格指令放 user");
    assert_eq!(messages[1]["role"], "assistant", "目标文本放 assistant");
    assert_eq!(messages[1]["content"], "你好，世界。");
    // 请求体里不能出现 Key 本身（Key 只在 Authorization 头里）。
    assert!(!body.to_string().contains("sk-mimo-test-key"));
}
