//! PR13：卡片草稿只在 UI 中；保存通过原工作线程串行提交。
use super::{settings, worker::UiCommand};
use crate::config::{
    self, AppPaths, ModelOption, ModelSource, ProtocolKind, ProviderConfig, Settings,
};
use crate::{AppWindow, EndpointCard, EndpointModel};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn row(m: &ModelOption, enabled: bool) -> EndpointModel {
    EndpointModel {
        id: m.id.clone().into(),
        label: m.display_name.clone().into(),
        enabled,
        context: m
            .context_window
            .map(|v| v.to_string())
            .unwrap_or_default()
            .into(),
        output: m
            .max_output_tokens
            .map(|v| v.to_string())
            .unwrap_or_default()
            .into(),
        image: m.image_input.unwrap_or(false),
        image_known: m.image_input.is_some(),
        source: match m.source {
            ModelSource::Manual => "manual",
            ModelSource::Discovered => "server",
            ModelSource::Unverified => "unverified",
        }
        .into(),
        expanded: false,
    }
}
pub(super) fn card(p: &ProviderConfig) -> EndpointCard {
    let mut p = p.clone();
    p.remember_manual_model(&p.model.clone());
    let enabled = p.selectable_models();
    EndpointCard {
        id: p.id.clone().into(),
        name: p.display_name().into(),
        url: p.base_url_or_default().into(),
        default_model: p.model.clone().into(),
        kind: match p.protocol_kind() {
            ProtocolKind::Mimo => 0,
            ProtocolKind::DeepSeek => 1,
            ProtocolKind::OpenaiCompatible => 2,
        },
        thinking: settings::thinking_index(p.thinking),
        effort: settings::reasoning_effort_index(p.reasoning_effort),
        rows: ModelRc::new(VecModel::from(
            p.models
                .iter()
                .map(|m| row(m, enabled.contains(&m.id)))
                .collect::<Vec<_>>(),
        )),
        ..Default::default()
    }
}
pub fn fill(w: &AppWindow, s: &Settings) {
    let mut ids = vec!["mimo".to_string(), "deepseek".to_string()];
    ids.retain(|id| !s.removed_provider_ids.contains(id));
    for p in &s.providers {
        if !ids.contains(&p.id) {
            ids.push(p.id.clone());
        }
    }
    w.set_endpoint_cards(ModelRc::new(VecModel::from(
        ids.iter()
            .map(|id| card(&s.provider(id)))
            .collect::<Vec<_>>(),
    )));
}
fn number(s: &str) -> Result<Option<u64>, String> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    s.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or_else(|| "元数据必须为正整数或留空（未知）".into())
}
pub fn provider(c: &EndpointCard) -> Result<ProviderConfig, String> {
    let kind = match c.kind {
        0 => ProtocolKind::Mimo,
        1 => ProtocolKind::DeepSeek,
        _ => ProtocolKind::OpenaiCompatible,
    };
    let mut p = Settings::default().provider(&c.id);
    p.id = c.id.to_string();
    p.name = c.name.trim().to_string();
    p.base_url = c.url.to_string();
    p.protocol_kind = Some(kind);
    p.model = c.default_model.trim().to_string();
    p.thinking = settings::thinking_for_index(c.thinking);
    p.reasoning_effort = settings::reasoning_effort_for_index(c.effort);
    p.models = c
        .rows
        .iter()
        .map(|m| {
            Ok(ModelOption {
                id: m.id.trim().to_string(),
                display_name: m.label.to_string(),
                source: match m.source.as_str() {
                    "manual" => ModelSource::Manual,
                    "unverified" => ModelSource::Unverified,
                    _ => ModelSource::Discovered,
                },
                context_window: number(&m.context)?,
                max_output_tokens: number(&m.output)?,
                image_input: m.image_known.then_some(m.image),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if p.models.len() > ProviderConfig::MAX_MODELS {
        return Err(format!(
            "每个实例最多保存 {} 个模型，请先减少目录条目",
            ProviderConfig::MAX_MODELS
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    if p.models
        .iter()
        .any(|m| m.id.is_empty() || !ids.insert(m.id.clone()))
    {
        return Err("模型 ID 不能为空或重复".into());
    }
    p.enabled_models = Some(
        c.rows
            .iter()
            .filter(|m| m.enabled)
            .map(|m| m.id.trim().to_string())
            .collect(),
    );
    Ok(p)
}
fn validate_default(p: &ProviderConfig) -> Result<(), String> {
    if !p
        .enabled_models
        .as_ref()
        .is_some_and(|v| v.contains(&p.model))
    {
        return Err("请先选择目录中已启用的默认模型，再删除或禁用旧默认模型".into());
    }
    Ok(())
}
pub fn install(w: &AppWindow, paths: AppPaths, tx: tokio::sync::mpsc::UnboundedSender<UiCommand>) {
    let weak = w.as_weak();
    w.on_add_endpoint(move |kind| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let mut p = Settings::default().provider(match kind {
            0 => "mimo",
            1 => "deepseek",
            _ => "openai-compatible",
        });
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        static INSTANCE_SEQUENCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence = INSTANCE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        p.id = format!("endpoint-{stamp}-{sequence}");
        p.protocol_kind = Some(match kind {
            0 => ProtocolKind::Mimo,
            1 => ProtocolKind::DeepSeek,
            _ => ProtocolKind::OpenaiCompatible,
        });
        p.name = match kind {
            0 => "Xiaomi MiMo",
            1 => "DeepSeek",
            _ => "OpenAI-compatible",
        }
        .into();
        let mut cards = w.get_endpoint_cards().iter().collect::<Vec<_>>();
        cards.push(card(&p));
        w.set_endpoint_cards(ModelRc::new(VecModel::from(cards)));
    });
    let revision = std::cell::Cell::new(0i32);
    let weak = w.as_weak();
    w.on_endpoint_action(move |op, mut c| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        revision.set(revision.get().wrapping_add(1));
        c.revision = revision.get();
        let cards = w.get_endpoint_cards();
        let Some(i) = cards.iter().position(|v| v.id == c.id) else {
            return;
        };
        let result = (|| -> Result<(), String> {
            match op.as_str() {
                "save" => {
                    let p = provider(&c)?;
                    validate_default(&p)?;
                    let _ = tx.send(UiCommand::SaveEndpoint(
                        p,
                        settings::ProviderDraft {
                            api_key: c.key.to_string(),
                            ..Default::default()
                        },
                        c.revision,
                    ));
                }
                "clear-key" => {
                    c.key = "".into();
                    c.status = "".into();
                    tx.send(UiCommand::ClearApiKey(c.id.to_string()))
                        .map_err(|_| "后台线程已退出".to_string())?;
                }
                "delete" => {
                    if !c.delete_pending {
                        c.delete_pending = true;
                        cards.set_row_data(i, c);
                        return Ok(());
                    }
                    let _ = tx.send(UiCommand::DeleteEndpoint(c.id.to_string()));
                }
                "cancel" => {
                    let s = config::load_settings(&paths).map_err(|e| e.to_string())?;
                    if s.providers.iter().any(|p| c.id == p.id)
                        || c.id == "mimo"
                        || c.id == "deepseek"
                    {
                        c = card(&s.provider(&c.id));
                    } else {
                        let v = cards.iter().filter(|v| v.id != c.id).collect::<Vec<_>>();
                        w.set_endpoint_cards(ModelRc::new(VecModel::from(v)));
                        return Ok(());
                    }
                }
                "restore" => {
                    let p = Settings::default().provider(match c.kind {
                        0 => "mimo",
                        1 => "deepseek",
                        _ => "openai-compatible",
                    });
                    let defaults = card(&p);
                    c.rows = defaults.rows;
                    c.default_model = defaults.default_model;
                    c.status = "".into();
                }
                "add-model" => {
                    let mut rows = c.rows.iter().collect::<Vec<_>>();
                    rows.push(row(&ModelOption::manual(""), true));
                    c.rows = ModelRc::new(VecModel::from(rows));
                    c.status = "".into();
                }
                "fetch" | "test" => {
                    let p = provider(&c)?;
                    if op == "test" {
                        validate_default(&p)?;
                    }
                    let key = if c.key.is_empty() {
                        crate::secret::SecretStore::new(paths.secret_file())
                            .load(&p.id)
                            .map_err(|e| e.to_string())?
                            .unwrap_or_default()
                    } else {
                        c.key.to_string()
                    };
                    let weak = w.as_weak();
                    let signature = serde_json::to_string(&p).unwrap_or_default();
                    let request_revision = c.revision;
                    let before = (
                        c.id.to_string(),
                        c.url.to_string(),
                        c.key.to_string(),
                        c.default_model.to_string(),
                        c.status.to_string(),
                    );
                    let test = op == "test";
                    std::thread::spawn(move || {
                        let runtime =
                            tokio::runtime::Runtime::new().expect("model request runtime");
                        let result = runtime.block_on(async {
                            if test {
                                let mut s = Settings::default();
                                s.providers.push(p.clone());
                                let result = crate::model_catalog::probe_chat(
                                    &s.profile_of(&p.id, &p.model, p.thinking, p.reasoning_effort),
                                    &key,
                                    15000,
                                )
                                .await;
                                (Some(result.ok()), result.message(), Vec::new())
                            } else {
                                let result = crate::model_catalog::fetch_models(
                                    &p,
                                    &key,
                                    &mut crate::model_catalog::ModelCache::new(),
                                    true,
                                )
                                .await;
                                (
                                    None,
                                    result.error.unwrap_or_default(),
                                    if result.ok { result.models } else { Vec::new() },
                                )
                            }
                        });
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            let cards = w.get_endpoint_cards();
                            // 比较完整草稿；任何编辑都使旧请求失效。
                            if let Some(i) = cards.iter().position(|v| {
                                v.revision == request_revision
                                    && v.id == before.0
                                    && v.url == before.1
                                    && v.key == before.2
                                    && v.default_model == before.3
                                    && v.status == before.4
                                    && provider(&v)
                                        .ok()
                                        .and_then(|p| serde_json::to_string(&p).ok())
                                        .as_deref()
                                        == Some(signature.as_str())
                            }) {
                                let mut c = cards.row_data(i).unwrap();
                                if let Some(ok) = result.0 {
                                    c.status = if ok { "成功" } else { "失败" }.into();
                                } else if result.1.is_empty() {
                                    let mut p = provider(&c).unwrap();
                                    p.remember_models(&result.2);
                                    let updated = card(&p);
                                    c.rows = updated.rows;
                                }
                                if !result.1.is_empty() {
                                    w.set_settings_error(result.1.into());
                                }
                                cards.set_row_data(i, c);
                            }
                        });
                    });
                }
                _ => {
                    if let Some(index) = op
                        .strip_prefix("remove:")
                        .and_then(|v| v.parse::<usize>().ok())
                    {
                        let mut rows = c.rows.iter().collect::<Vec<_>>();
                        if rows.get(index).is_some_and(|r| r.id == c.default_model) {
                            return Err("请先指定另一已启用的默认模型".into());
                        }
                        if index < rows.len() {
                            rows.remove(index);
                        }
                        c.rows = ModelRc::new(VecModel::from(rows));
                        c.status = "".into();
                    }
                }
            }
            cards.set_row_data(i, c);
            Ok(())
        })();
        if let Err(e) = result {
            w.set_settings_error(e.into());
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn legacy_protocol_migrates_once_and_manual_metadata_survives_discovery() {
        let mut s = Settings::default();
        let mut p = s.provider("openai-compatible");
        p.id = "gateway".into();
        p.base_url = "http://localhost:1/v1".into();
        p.provider_hint = config::ProviderHint::Mimo;
        let mut manual = ModelOption::manual("private-model");
        manual.context_window = Some(32000);
        manual.display_name = "我的模型".into();
        p.models = vec![manual.clone()];
        p.enabled_models = Some(vec![manual.id.clone()]);
        s.providers.push(p);
        let migrated = s.normalized().unwrap();
        let json = serde_json::to_string(&migrated).unwrap();
        let mut restored: Settings = serde_json::from_str(&json).unwrap();
        let p = restored
            .providers
            .iter_mut()
            .find(|p| p.id == "gateway")
            .unwrap();
        assert_eq!(p.protocol_kind, Some(ProtocolKind::Mimo));
        p.provider_hint = config::ProviderHint::DeepSeek;
        assert_eq!(p.capability(), crate::llm::Capability::Mimo);
        let mut remote = manual.clone();
        remote.source = ModelSource::Discovered;
        remote.context_window = Some(999999);
        p.remember_models(&[remote]);
        assert_eq!(p.models[0], manual);
        assert_eq!(p.enabled_models.as_ref().unwrap(), &["private-model"]);
    }

    #[test]
    fn card_roundtrip_keeps_five_instance_credentials_and_model_metadata() {
        let root = std::env::temp_dir().join(format!(
            "cuava-pr13-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = AppPaths { root };
        let secret = crate::secret::SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        for i in 0..5 {
            let mut p = settings.provider(if i < 2 {
                "mimo"
            } else if i < 4 {
                "deepseek"
            } else {
                "openai-compatible"
            });
            p.protocol_kind = Some(p.protocol_kind());
            p.id = format!("saved-{i}");
            p.base_url = format!("http://127.0.0.1:{}/v1", 21000 + i);
            p.model = "first".into();
            let mut model = ModelOption::manual("first");
            model.display_name = "自定义显示名".into();
            model.context_window = Some(32000);
            model.max_output_tokens = Some(4000);
            model.image_input = Some(false);
            p.models = vec![model, ModelOption::manual("second")];
            p.enabled_models = Some(vec!["first".into(), "second".into()]);
            let mut draft = card(&p);
            draft.name = format!("名称 {i}").into();
            let converted = provider(&draft).unwrap();
            validate_default(&converted).unwrap();
            assert_eq!(converted.models, p.models);
            secret.save(&p.id, &format!("mock-key-{i}")).unwrap();
            settings.providers.push(converted);
        }
        config::save_settings(&paths, &settings).unwrap();
        let reloaded = config::load_settings(&paths).unwrap();
        for i in 0..5 {
            let id = format!("saved-{i}");
            let p = reloaded.provider(&id);
            assert_eq!(secret.load(&id).unwrap().unwrap(), format!("mock-key-{i}"));
            assert_eq!(p.display_name(), format!("名称 {i}"));
            assert_eq!(p.selectable_models(), vec!["first", "second"]);
            let profile = reloaded.profile_of(&id, "first", p.thinking, p.reasoning_effort);
            assert_eq!(profile.max_output_tokens, Some(4000));
            assert_eq!(profile.image_input, Some(false));
            assert_eq!(profile.model_context_window, Some(32000));
            let mut draft = card(&p);
            draft.default_model = "absent".into();
            assert!(validate_default(&provider(&draft).unwrap()).is_err());
        }
        assert!(secret.load("unknown-instance").unwrap().is_none());
        std::fs::remove_dir_all(&paths.root).unwrap();
    }

    #[tokio::test]
    async fn five_instances_use_their_own_http_endpoint_key_model_and_protocol() {
        let mut settings = Settings::default();
        let mut servers = Vec::new();
        let kinds = [
            ProtocolKind::Mimo,
            ProtocolKind::Mimo,
            ProtocolKind::DeepSeek,
            ProtocolKind::DeepSeek,
            ProtocolKind::OpenaiCompatible,
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let mut p = settings.provider("mimo");
            p.id = format!("instance-{i}");
            p.name = "任意名称".into();
            p.protocol_kind = Some(kind);
            p.base_url = format!("http://{address}/v1");
            p.model = format!("model-{i}");
            p.thinking = config::ThinkingLevel::Enabled;
            p.reasoning_effort = config::ReasoningEffort::High;
            settings.providers.push(p.clone());
            servers.push(std::thread::spawn(move || {
                for request_index in 0..2 {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                    let mut bytes = Vec::new(); let mut buf = [0; 4096];
                    loop {
                        let n = stream.read(&mut buf).unwrap(); assert!(n > 0); bytes.extend_from_slice(&buf[..n]);
                        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                            let len: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())).unwrap_or(0);
                            if bytes.len() < end + 4 + len {continue;}
                            assert!(headers.contains(&format!("authorization: bearer test-key-{i}")));
                            if request_index == 0 { assert!(headers.starts_with("get /v1/models ")); }
                            else {
                                assert!(headers.starts_with("post /v1/chat/completions "));
                                let body: serde_json::Value = serde_json::from_slice(&bytes[end+4..]).unwrap();
                                assert_eq!(body["model"], p.model);
                                match kind {
                                    ProtocolKind::Mimo => {assert_eq!(body["thinking"]["type"], "enabled"); assert!(body.get("reasoning_effort").is_none());}
                                    ProtocolKind::DeepSeek => {assert_eq!(body["reasoning_effort"], "high"); assert!(body.get("thinking").is_none());}
                                    ProtocolKind::OpenaiCompatible => {assert!(body.get("thinking").is_none()); assert!(body.get("reasoning_effort").is_none());}
                                }
                            }
                            break;
                        }
                    }
                    let response = if request_index == 0 { format!(r#"{{"data":[{{"id":"model-{i}"}}]}}"#) }
                        else {"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()};
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                }
            }));
        }
        for i in 0..5 {
            let p = settings.provider(&format!("instance-{i}"));
            let key = format!("test-key-{i}");
            let result = crate::model_catalog::fetch_models(
                &p,
                &key,
                &mut crate::model_catalog::ModelCache::new(),
                true,
            )
            .await;
            assert!(result.ok, "{:?}", result.error);
            let profile = settings.profile_of(&p.id, &p.model, p.thinking, p.reasoning_effort);
            let result = crate::model_catalog::probe_chat(&profile, &key, 10000).await;
            assert!(result.ok(), "{}", result.message());
        }
        for server in servers {
            server.join().unwrap();
        }
    }
}
