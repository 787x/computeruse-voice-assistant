//! MiMo-V2.5-TTS：官方 Chat Completions 语音协议（流式 PCM16 / 24kHz 单声道）。
//!
//! 协议要点（任务书与官方示例）：
//!  - 目标文本放在 `assistant` 消息里；可选的简短风格指令放在 `user` 消息里；
//!  - `stream: true` + `audio: { format: "pcm16", voice: ... }`；
//!  - 音频在 `choices[].delta.audio.data`（Base64）里分片到达；
//!  - 解码后的 PCM 是 **24kHz 单声道 PCM16LE**。
//!
//! 这一层必须扛住的脏数据（都按"不损坏音频"处理）：
//!  - 事件被拆在两个网络块之间、一个块里有多条 `data:`；
//!  - 空 `choices`（只带 usage 的收尾块）、`[DONE]`、`event: error`；
//!  - Base64 里恰好切出**半个采样**（奇数字节）——残留字节留到下一块；
//!  - 401 / 429 / 非 2xx / 断流 / 完全没有音频。
//!
//! 安全：Key 只从**MiMo 账户**读取（见 [`KEY_PROVIDER`]），只发往 MiMo 端点，
//! 并且绝不进入错误信息或日志。

use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::voice::pcm::int_to_float;

use super::{
    AudioFormat, SpeechChunk, SpeechStream, SynthRequest, TtsEngine, TtsEvent, TtsSettings,
};

/// 默认模型：标准预置音色模型（不是音色克隆/设计模型）。
pub const DEFAULT_MODEL: &str = "mimo-v2.5-tts";
/// MiMo 官方 OpenAI 兼容端点。
pub const OFFICIAL_BASE_URL: &str = "https://api.xiaomimimo.com/v1";
/// TTS 只使用这个账户的 Key。
pub const KEY_PROVIDER: &str = "mimo";
/// 官方示例的音频格式：24kHz 单声道 PCM16LE。
pub const SAMPLE_RATE: u32 = 24_000;
pub const CHANNELS: u16 = 1;
/// 覆盖 TTS 端点（指向本地 mock / 自建代理）；非法值会被忽略。
pub const BASE_URL_ENV: &str = "CUAVA_TTS_BASE_URL";

/// MiMo TTS 后端。
#[derive(Clone)]
pub struct MimoTts {
    endpoint: String,
    api_key: String,
    model: String,
    voice: String,
    style: String,
    timeout_ms: u64,
    http: reqwest::Client,
}

impl MimoTts {
    pub fn new(settings: &TtsSettings, api_key: String) -> Result<Self, String> {
        let base = std::env::var(BASE_URL_ENV)
            .ok()
            .filter(|value| {
                let lower = value.trim().to_ascii_lowercase();
                lower.starts_with("http://") || lower.starts_with("https://")
            })
            .unwrap_or_else(|| OFFICIAL_BASE_URL.to_string());
        let endpoint = format!("{}/chat/completions", base.trim().trim_end_matches('/'));
        let http = reqwest::Client::builder()
            .user_agent("cuava/0.1 (computer-use desktop)")
            .build()
            .map_err(|error| format!("无法创建 TTS HTTP 客户端：{error}"))?;
        Ok(Self {
            endpoint,
            api_key,
            model: if settings.mimo_model.trim().is_empty() {
                DEFAULT_MODEL.to_string()
            } else {
                settings.mimo_model.trim().to_string()
            },
            voice: settings.mimo_voice.trim().to_string(),
            style: settings.mimo_style.trim().to_string(),
            timeout_ms: settings.timeout_ms.clamp(5_000, 120_000),
            http,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 组装请求体（导出给测试：界面里配置的就是实际发的）。
    pub fn build_body(&self, text: &str) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        let style = self.style.trim();
        if !style.is_empty() {
            messages.push(json!({ "role": "user", "content": style }));
        }
        messages.push(json!({ "role": "assistant", "content": text }));
        let mut audio = serde_json::Map::new();
        audio.insert("format".to_string(), json!("pcm16"));
        if !self.voice.is_empty() {
            audio.insert("voice".to_string(), json!(self.voice));
        }
        json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "audio": Value::Object(audio),
        })
    }

    async fn run(
        &self,
        request: SynthRequest,
        tx: tokio::sync::mpsc::UnboundedSender<TtsEvent>,
    ) -> Result<Outcome, String> {
        let body = self.build_body(&request.text);
        let mut builder = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .json(&body);
        if !self.api_key.trim().is_empty() {
            builder = builder.header("authorization", format!("Bearer {}", self.api_key));
        }

        let response = tokio::select! {
            result = builder.send() => result,
            _ = request.cancel.cancelled() => return Ok(Outcome::Cancelled),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if request.cancel.is_cancelled() {
                    return Ok(Outcome::Cancelled);
                }
                // 只报错误类型，不带 URL/Header（避免任何凭据出现在界面上）。
                return Err(format!("连接 MiMo TTS 失败：{error}"));
            }
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(describe_status(status, &body));
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut events: Vec<String> = Vec::new();
        let mut decoder = Pcm16Decoder::default();
        let mut saw_audio = false;
        /*
         * 只有**协议认可的结束信号**（`[DONE]`）才算正常合成结束。
         *
         * 连接被正常关闭（TCP FIN）并不等于模型说完了：把"读到 EOF"当成完成，
         * 就会把被截断的句子当成"正常播完"，用户听到的是半句话却没有报错。
         */
        let mut saw_done = false;
        let mut cancelled = false;
        // 最后一个事件的解析错误：不能像以前那样 `let _ =` 丢掉。
        let mut tail_error: Option<String> = None;

        'outer: loop {
            let idle = Duration::from_millis(self.timeout_ms);
            let chunk = tokio::select! {
                chunk = tokio::time::timeout(idle, stream.next()) => chunk,
                _ = request.cancel.cancelled() => {
                    cancelled = true;
                    break 'outer;
                }
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    if request.cancel.is_cancelled() {
                        cancelled = true;
                        break 'outer;
                    }
                    return Err(format!(
                        "MiMo TTS 响应超时（{} ms 没有数据）",
                        self.timeout_ms
                    ));
                }
            };
            let Some(chunk) = chunk else { break };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    if request.cancel.is_cancelled() {
                        cancelled = true;
                        break 'outer;
                    }
                    return Err(format!("MiMo TTS 传输中断：{error}"));
                }
            };
            buffer.extend_from_slice(&bytes);

            // 按字节切出完整事件：分片可能正好落在 `\n\n` 或 UTF-8 字符中间。
            while let Some((end, separator)) = crate::llm::find_event_boundary(&buffer) {
                let raw: Vec<u8> = buffer.drain(..end).collect();
                buffer.drain(..separator);
                let raw = String::from_utf8(raw).map_err(|_| {
                    "MiMo TTS 返回了无法解码的数据（不是有效的 SSE 文本）".to_string()
                })?;
                if pump_events(
                    &raw,
                    &mut events,
                    &mut decoder,
                    &request,
                    &tx,
                    &mut saw_audio,
                )? {
                    saw_done = true;
                    break 'outer;
                }
            }
            // 一条事件里的多个 `data:` 行要合并后再解释（SSE 允许多行 data）。
            if request.cancel.is_cancelled() {
                cancelled = true;
                break 'outer;
            }
        }

        if cancelled || request.cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        /*
         * 缓冲区里最后一个没有空行结尾的事件也要处理。
         *
         * 已经收到 `[DONE]` 时后面的字节属于协议尾巴，忽略；否则这个尾事件
         * （可能是 `[DONE]`，也可能是坏 JSON）必须真的参与判定：
         * 前者决定"是否正常结束"，后者必须报错而不是被丢掉。
         */
        if !saw_done && !buffer.is_empty() {
            let raw = std::mem::take(&mut buffer);
            match String::from_utf8(raw) {
                Ok(raw) if raw.trim().is_empty() => {}
                Ok(raw) => {
                    match pump_events(
                        &raw,
                        &mut events,
                        &mut decoder,
                        &request,
                        &tx,
                        &mut saw_audio,
                    ) {
                        Ok(done) => saw_done |= done,
                        Err(error) => tail_error = Some(error),
                    }
                }
                Err(_) => {
                    tail_error =
                        Some("MiMo TTS 返回了无法解码的数据（不是有效的 SSE 文本）".to_string())
                }
            }
        }

        if let Some(error) = tail_error {
            return Err(error);
        }
        // PCM16 是两个字节一个样本：末尾剩半个样本说明音频被截断，
        // 不能把它当成"完整播完"。
        if decoder.has_leftover() {
            return Err("MiMo TTS 音频数据不完整（PCM16 末尾缺半个采样）".to_string());
        }
        if !saw_audio {
            return Err("MiMo TTS 没有返回音频（请检查模型名、音色名与账户权限）".to_string());
        }
        if !saw_done {
            return Err(
                "MiMo TTS 连接在完成标记之前结束，音频可能不完整（请重试或检查网络）".to_string(),
            );
        }
        Ok(Outcome::Finished)
    }
}

/// 一次合成的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Finished,
    Cancelled,
}

impl TtsEngine for MimoTts {
    fn label(&self) -> String {
        format!("MiMo {}（24k 单声道）", self.model)
    }

    fn start(&self, request: SynthRequest) -> Result<SpeechStream, String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TtsEvent>();
        let engine = self.clone();
        // 合成在后台任务里推进：**第一批 PCM 一到就能入队播放**，
        // 不需要等整句（更不需要等整段回复）合成完。
        tokio::spawn(async move {
            match engine.run(request, tx.clone()).await {
                Ok(Outcome::Finished) => {
                    let _ = tx.send(TtsEvent::Finished);
                }
                Ok(Outcome::Cancelled) => {}
                Err(error) => {
                    let _ = tx.send(TtsEvent::Failed(error));
                }
            }
        });
        Ok(SpeechStream::new(rx))
    }
}

/// 解析一个 SSE 事件并把音频块交给 `tx`。返回 `true` 表示收到 `[DONE]`。
fn pump_events(
    raw_event: &str,
    events: &mut Vec<String>,
    decoder: &mut Pcm16Decoder,
    request: &SynthRequest,
    tx: &tokio::sync::mpsc::UnboundedSender<TtsEvent>,
    saw_audio: &mut bool,
) -> Result<bool, String> {
    events.clear();
    let mut event_name = String::new();
    for line in raw_event.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(index) => (
                &line[..index],
                line[index + 1..]
                    .strip_prefix(' ')
                    .unwrap_or(&line[index + 1..]),
            ),
            None => (line, ""),
        };
        match field.trim().to_ascii_lowercase().as_str() {
            "event" => event_name = value.trim().to_ascii_lowercase(),
            "data" => events.push(value.to_string()),
            _ => {}
        }
    }
    let payload = events.join("\n");
    let payload = payload.trim();
    if payload.is_empty() {
        if event_name == "error" {
            return Err("MiMo TTS 返回了错误事件（无内容）".to_string());
        }
        return Ok(false);
    }
    match parse_payload(payload) {
        ParsedEvent::Done => Ok(true),
        ParsedEvent::Ignored => Ok(false),
        ParsedEvent::Error(message) => Err(format!("MiMo TTS 返回错误：{message}")),
        ParsedEvent::Audio(base64) => {
            let added = decoder.push_base64(&base64)?;
            if added > 0 {
                let samples = std::mem::take(&mut decoder.samples);
                *saw_audio = true;
                let _ = tx.send(TtsEvent::Chunk(SpeechChunk {
                    speech_id: request.speech_id,
                    turn: request.turn,
                    sentence: request.sentence,
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS,
                    format: AudioFormat::Pcm16Le,
                    samples,
                }));
            }
            Ok(false)
        }
    }
}

/// 一条 `data:` 负载的解释结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    /// 收到 `[DONE]`：协议结束。
    Done,
    /// 有效音频（Base64）。
    Audio(String),
    /// 空 choices / 无音频字段的收尾块：跳过。
    Ignored,
    /// 服务端错误。
    Error(String),
}

/// 解析单条 `data:` 负载。
pub fn parse_payload(payload: &str) -> ParsedEvent {
    let payload = payload.trim();
    if payload.is_empty() {
        return ParsedEvent::Ignored;
    }
    if payload == "[DONE]" {
        return ParsedEvent::Done;
    }
    let Ok(json) = serde_json::from_str::<Value>(payload) else {
        // 不是 JSON：协议层脏数据，跳过而不是把半截字符串当音频。
        return ParsedEvent::Ignored;
    };
    if let Some(error) = json.get("error") {
        if !error.is_null() {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return ParsedEvent::Error(message.to_string());
        }
    }
    let Some(choices) = json.get("choices").and_then(Value::as_array) else {
        return ParsedEvent::Ignored;
    };
    // `choices: []` 是合法的收尾块（只带 usage），不是错误。
    let Some(choice) = choices.first() else {
        return ParsedEvent::Ignored;
    };
    let data = choice
        .get("delta")
        .and_then(|delta| delta.get("audio"))
        .and_then(|audio| audio.get("data"))
        .and_then(Value::as_str)
        .or_else(|| {
            choice
                .get("message")
                .and_then(|message| message.get("audio"))
                .and_then(|audio| audio.get("data"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            choice
                .get("audio")
                .and_then(|audio| audio.get("data"))
                .and_then(Value::as_str)
        });
    match data {
        Some(data) if !data.is_empty() => ParsedEvent::Audio(data.to_string()),
        _ => ParsedEvent::Ignored,
    }
}

/// PCM16LE 字节流 → 单声道 f32。
///
/// 关键：**不把 Base64/网络块边界当成采样边界**。每个事件独立 Base64 解码
/// （这是协议允许的），但解码出的字节流里如果是奇数字节，残留的那 1 个字节
/// 会留到下一块再和下一个字节拼成一个 int16——否则每个分片都会把音频错位半帧，
/// 听上去是持续的爆音。
#[derive(Debug, Default)]
pub struct Pcm16Decoder {
    leftover: Option<u8>,
    /// 已解出的样本（由调用方取走）。
    pub samples: Vec<f32>,
}

impl Pcm16Decoder {
    /// 追加一段 Base64 音频，返回这次解出的样本数。
    pub fn push_base64(&mut self, data: &str) -> Result<usize, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|error| format!("MiMo TTS 音频数据损坏（Base64 解码失败）：{error}"))?;
        Ok(self.push_bytes(&bytes))
    }

    /// 追加一段原始字节，返回这次解出的样本数。
    pub fn push_bytes(&mut self, bytes: &[u8]) -> usize {
        let mut produced = 0usize;
        let mut index = 0usize;
        if let Some(first) = self.leftover.take() {
            if let Some(second) = bytes.first() {
                let value = i16::from_le_bytes([first, *second]);
                self.samples.push(int_to_float(value as i64, 16));
                produced += 1;
                index = 1;
            } else {
                self.leftover = Some(first);
                return 0;
            }
        }
        while index + 1 < bytes.len() {
            let value = i16::from_le_bytes([bytes[index], bytes[index + 1]]);
            self.samples.push(int_to_float(value as i64, 16));
            produced += 1;
            index += 2;
        }
        if index < bytes.len() {
            self.leftover = Some(bytes[index]);
        }
        produced
    }

    /// 是否还残留半个采样（流结束时应当为 `false`，否则说明数据被截断）。
    pub fn has_leftover(&self) -> bool {
        self.leftover.is_some()
    }
}

/// 把 HTTP 状态码翻译成用户能理解、且**不含凭据**的错误。
pub fn describe_status(status: u16, body: &str) -> String {
    let hint = match status {
        401 => "认证失败：请检查 MiMo 账户的 Key",
        403 => "账户没有该模型的权限",
        404 => "端点或模型不存在",
        429 => "请求过于频繁或额度不足，稍后再试",
        500..=599 => "服务端错误，稍后再试",
        _ => "请求失败",
    };
    let detail = scrub_body(body);
    if detail.is_empty() {
        format!("MiMo TTS {hint}（HTTP {status}）")
    } else {
        format!("MiMo TTS {hint}（HTTP {status}）：{detail}")
    }
}

/// 服务端返回体只保留一小段可读信息，并抹掉任何看起来像凭据的内容。
fn scrub_body(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if lower.contains("bearer") || lower.contains("sk-") || lower.contains("api_key") {
        return String::new();
    }
    let compact: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;

    fn engine(voice: &str, style: &str) -> MimoTts {
        MimoTts::new(
            &TtsSettings {
                enabled: true,
                backend: super::super::TtsBackend::Mimo,
                mimo_voice: voice.to_string(),
                mimo_style: style.to_string(),
                ..TtsSettings::default()
            },
            "sk-test-key".to_string(),
        )
        .unwrap()
    }

    /// 请求结构必须与官方协议一致：模型、`stream`、`audio.format`，
    /// 目标文本在 `assistant` 消息，风格指令在 `user` 消息。
    #[test]
    fn request_body_matches_official_protocol() {
        let body = engine("alloy", "语速稍快，语气自然").build_body("你好，世界。");
        assert_eq!(body["model"], json!("mimo-v2.5-tts"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["audio"]["format"], json!("pcm16"));
        assert_eq!(body["audio"]["voice"], json!("alloy"));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], json!("user"));
        assert_eq!(messages[0]["content"], json!("语速稍快，语气自然"));
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[1]["content"], json!("你好，世界。"));
    }

    /// 没有音色 / 风格时**不发**这两个字段（避免猜一个服务端不认识的音色名）。
    #[test]
    fn empty_voice_and_style_are_omitted() {
        let body = engine("", "").build_body("只有正文");
        assert!(body["audio"].get("voice").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], json!("assistant"));
    }

    /// 奇数字节：分片不能把音频错位半帧。
    #[test]
    fn odd_byte_chunks_do_not_shift_samples() {
        // 3 个 int16 样本：0x0100, 0x0302, 0x0504（小端）
        let bytes = [0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05];
        let mut decoder = Pcm16Decoder::default();
        // 每次只给 1 个字节：最极端的分片。
        let mut produced = 0;
        for byte in bytes {
            produced += decoder.push_bytes(&[byte]);
        }
        assert_eq!(produced, 3);
        assert!(!decoder.has_leftover());
        let expected: Vec<f32> = [256i64, 770, 1284]
            .iter()
            .map(|value| int_to_float(*value, 16))
            .collect();
        assert_eq!(decoder.samples, expected);
    }

    #[test]
    fn empty_push_keeps_leftover_intact() {
        let mut decoder = Pcm16Decoder::default();
        assert_eq!(decoder.push_bytes(&[0x11]), 0);
        assert!(decoder.has_leftover());
        assert_eq!(decoder.push_bytes(&[]), 0);
        assert!(decoder.has_leftover());
        assert_eq!(decoder.push_bytes(&[0x22]), 1);
        assert!(!decoder.has_leftover());
    }

    #[test]
    fn invalid_base64_is_reported_not_silently_dropped() {
        let mut decoder = Pcm16Decoder::default();
        let error = decoder.push_base64("!!!not-base64!!!").unwrap_err();
        assert!(error.contains("损坏"), "{error}");
    }

    /// 末尾剩半个采样 = 数据不完整（不能当成完整播完）。
    #[test]
    fn leftover_half_sample_is_detectable() {
        let mut decoder = Pcm16Decoder::default();
        // 1 个完整样本 + 1 个游离字节。
        assert_eq!(decoder.push_bytes(&[0x01, 0x02, 0x03]), 1);
        assert!(decoder.has_leftover(), "奇数字节必须留下可检测的残留");
        // 再补一个字节就补齐了。
        assert_eq!(decoder.push_bytes(&[0x04]), 1);
        assert!(!decoder.has_leftover());
    }

    /// SSE 负载的各种合法/异常形态。
    #[test]
    fn payload_parsing_covers_protocol_edges() {
        assert_eq!(parse_payload("[DONE]"), ParsedEvent::Done);
        assert_eq!(parse_payload(""), ParsedEvent::Ignored);
        assert_eq!(parse_payload("not json"), ParsedEvent::Ignored);
        assert_eq!(parse_payload(r#"{"choices":[]}"#), ParsedEvent::Ignored);
        assert_eq!(
            parse_payload(r#"{"usage":{"total_tokens":1}}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(
            parse_payload(r#"{"choices":[{"delta":{"content":"hi"}}]}"#),
            ParsedEvent::Ignored
        );
        assert_eq!(
            parse_payload(r#"{"choices":[{"delta":{"audio":{"data":"AAA="}}}]}"#),
            ParsedEvent::Audio("AAA=".to_string())
        );
        // 非流式返回形态也认（message.audio）。
        assert_eq!(
            parse_payload(r#"{"choices":[{"message":{"audio":{"data":"BBB="}}}]}"#),
            ParsedEvent::Audio("BBB=".to_string())
        );
        assert_eq!(
            parse_payload(r#"{"error":{"message":"quota exceeded"}}"#),
            ParsedEvent::Error("quota exceeded".to_string())
        );
    }

    /// 状态码翻译要给出可执行建议，且**不回显凭据**。
    #[test]
    fn status_errors_are_readable_and_secret_free() {
        let unauthorized = describe_status(401, r#"{"error":"invalid api key: sk-secret-value"}"#);
        assert!(unauthorized.contains("401"));
        assert!(unauthorized.contains("MiMo"));
        assert!(!unauthorized.contains("sk-secret-value"), "{unauthorized}");

        let rate = describe_status(429, "");
        assert!(rate.contains("429"));
        let server = describe_status(503, "upstream unavailable");
        assert!(server.contains("503"));
        assert!(server.contains("服务端"));

        // 普通错误体保留一小段可读信息（便于排查），但截断。
        let long = describe_status(400, &"x".repeat(1000));
        assert!(long.chars().count() < 400);
    }

    /// 取消令牌已置位时，`start` 出去的合成不会把音频当成功。
    #[tokio::test]
    async fn cancelled_request_stops_before_network() {
        let engine = engine("", "");
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut stream = engine
            .start(SynthRequest {
                speech_id: 1,
                turn: 2,
                sentence: 0,
                text: "你好".to_string(),
                style: String::new(),
                cancel,
            })
            .unwrap();
        // 已取消：要么没有任何事件，要么最多一个失败/完成事件；绝不能有音频块。
        while let Some(event) = stream.recv().await {
            assert!(
                !matches!(event, TtsEvent::Chunk(_)),
                "已取消的请求不能产出音频"
            );
        }
    }
}
