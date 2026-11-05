//! PR8 本轮：草稿探测、Key 隔离和账户匹配的目录恢复。只使用本地 HTTP 与隔离 SecretStore。
use cuava::{
    config::{self, AppPaths, ModelOption, Settings},
    model_catalog::{fetch_models, probe_chat, ModelCache},
    secret::SecretStore,
    ui::{model_probe::ProbeDraft, settings::restore_model_catalogs},
};
use std::io::{Read, Write};
fn paths(label: &str) -> AppPaths {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-tmp")
        .join(format!(
            "{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&root).unwrap();
    AppPaths::with_root(root)
}
fn server(
    responses: Vec<(&str, &str)>,
) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let responses: Vec<_> = responses
        .into_iter()
        .map(|(kind, body)| (kind.to_string(), body.to_string()))
        .collect();
    let task = std::thread::spawn(move || {
        let mut requests = vec![];
        for (kind, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let len = header
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut content = vec![0; len];
            stream.read_exact(&mut content).unwrap();
            requests.push((header, String::from_utf8(content).unwrap()));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    (url, task)
}
fn draft(url: &str, key: &str) -> ProbeDraft {
    ProbeDraft {
        revision: 0,
        provider_id: "deepseek".into(),
        base_url: url.into(),
        model: "draft-model".into(),
        api_key: key.into(),
        hint: 0,
        thinking: 0,
        effort: 3,
    }
}
#[tokio::test]
async fn unsaved_probe_and_discovery_use_the_same_draft_without_saving_key() {
    let paths = paths("draft-probe");
    let secrets = SecretStore::new(paths.secret_file());
    secrets.save("mimo", "fake-mimo").unwrap();
    secrets.save("deepseek", "fake-old-deepseek").unwrap();
    let settings = Settings::default();
    let (url, task) = server(vec![("text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"), ("application/json", r#"{"data":[{"id":"draft-found"}]}"#)]);
    let draft = draft(&url, "fake-new-key");
    let (provider, key) = draft.resolve(&settings, &secrets).unwrap();
    let outcome = probe_chat(&ProbeDraft::profile(&provider), &key, 3000).await;
    assert!(outcome.ok(), "{}", outcome.message());
    let fetch = fetch_models(
        &provider,
        &key,
        &mut ModelCache::with_path(paths.model_cache_file()),
        true,
    )
    .await;
    assert!(fetch.ok);
    let requests = task.join().unwrap();
    assert!(requests[0].0.starts_with("POST /v1/chat/completions"));
    assert!(requests[1].0.starts_with("GET /v1/models"));
    for (headers, _) in &requests {
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fake-new-key"));
        assert!(!headers.contains("fake-mimo"));
        assert!(!headers.contains("fake-old-deepseek"));
    }
    let body: serde_json::Value = serde_json::from_str(&requests[0].1).unwrap();
    assert_eq!(body["model"], "draft-model");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(
        secrets.load("deepseek").unwrap().as_deref(),
        Some("fake-old-deepseek")
    );
    let mut no_key = draft.clone();
    no_key.api_key.clear();
    assert!(no_key
        .resolve(&settings, &secrets)
        .unwrap_err()
        .contains("不会向新地址发送旧 Key"));
    let mut configured = settings.clone();
    configured.provider_id = "deepseek".into();
    configured.base_url = url;
    assert_eq!(
        no_key.resolve(&configured, &secrets).unwrap().1,
        "fake-old-deepseek"
    );
    assert!(!format!("{draft:?}").contains("fake-new-key"));
}
#[tokio::test]
async fn restart_restores_only_current_endpoint_account_and_keeps_manual_models() {
    let paths = paths("account-cache");
    let secrets = SecretStore::new(paths.secret_file());
    secrets.save("deepseek", "account-a").unwrap();
    let (url, task) = server(vec![
        (
            "application/json",
            r#"{"data":[{"id":"a-one"},{"id":"a-two"}]}"#,
        ),
        ("application/json", r#"{"data":[{"id":"b-only"}]}"#),
    ]);
    let mut settings = Settings {
        provider_id: "deepseek".into(),
        base_url: url.clone(),
        model: "manual".into(),
        ..Settings::default()
    };
    settings.remember_discovered_models("deepseek", &url, &[ModelOption::manual("manual")]);
    config::save_settings(&paths, &settings).unwrap();
    // 模拟旧版文件缺少启用字段，但发现目录已在独立缓存里。
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths.settings_file()).unwrap()).unwrap();
    for provider in legacy["providers"].as_array_mut().unwrap() {
        provider.as_object_mut().unwrap().remove("enabledModels");
    }
    std::fs::write(paths.settings_file(), serde_json::to_vec(&legacy).unwrap()).unwrap();
    let provider = settings.provider("deepseek");
    let mut cache = ModelCache::with_path(paths.model_cache_file());
    assert!(
        fetch_models(&provider, "account-a", &mut cache, true)
            .await
            .ok
    );
    let mut restarted = config::load_settings(&paths).unwrap();
    restore_model_catalogs(&mut restarted, &paths, &secrets, None);
    for id in ["manual", "a-one", "a-two"] {
        assert!(restarted
            .provider("deepseek")
            .models
            .iter()
            .any(|m| m.id == id));
    }
    assert!(restarted
        .provider("deepseek")
        .selectable_models()
        .contains(&"a-two".into()));
    secrets.save("deepseek", "account-b").unwrap();
    restore_model_catalogs(&mut restarted, &paths, &secrets, None);
    assert_eq!(
        restarted
            .provider("deepseek")
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["manual"]
    );
    assert!(
        fetch_models(&provider, "account-b", &mut cache, true)
            .await
            .ok
    );
    task.join().unwrap();
    assert_eq!(
        cache
            .models_for("deepseek", &url, "account-a")
            .unwrap()
            .len(),
        2
    );
    restore_model_catalogs(&mut restarted, &paths, &secrets, None);
    assert!(restarted
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id == "b-only"));
    assert!(!restarted
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id.starts_with("a-")));
    restarted
        .providers
        .iter_mut()
        .find(|p| p.id == "deepseek")
        .unwrap()
        .base_url = "http://127.0.0.1:1/v1".into();
    restore_model_catalogs(&mut restarted, &paths, &secrets, None);
    assert_eq!(restarted.provider("deepseek").models.len(), 1);
}

#[tokio::test]
async fn selected_discovered_model_uses_saved_provider_endpoint_and_own_key() {
    let paths = paths("selected-discovery");
    let secret = SecretStore::new(paths.secret_file());
    secret.save("mimo", "wrong-provider-key").unwrap();
    secret.save("deepseek", "right-provider-key").unwrap();
    let (url, server) = server(vec![("application/json", r#"{"data":[{"id":"selected-found"}]}"#), ("text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")]);
    let mut saved = Settings {
        provider_id: "deepseek".into(),
        base_url: url.clone(),
        ..Settings::default()
    };
    config::save_settings(&paths, &saved).unwrap();
    let fetched = fetch_models(
        &saved.provider("deepseek"),
        "right-provider-key",
        &mut ModelCache::with_path(paths.model_cache_file()),
        true,
    )
    .await;
    assert!(fetched.ok);
    saved = config::load_settings(&paths).unwrap();
    restore_model_catalogs(&mut saved, &paths, &secret, None);
    assert!(saved
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id == "selected-found"));
    let choice = config::SessionModelChoice {
        provider_id: "deepseek".into(),
        model: "selected-found".into(),
        ..Default::default()
    };
    let selection = saved.resolve_selection(Some(&choice), &cuava::clock::now());
    let profile = saved.profile_for_selection(&selection);
    assert_eq!(profile.base_url, url);
    let client = cuava::llm::LlmClient::new(
        profile,
        &secret.load("deepseek").unwrap().unwrap(),
        0.3,
        3000,
    )
    .unwrap();
    client
        .chat(
            &[cuava::llm::ChatMessage::user("test")],
            &[],
            &cuava::cancel::CancelToken::new(),
            |_| {},
            |_| {},
        )
        .await
        .unwrap();
    let requests = server.join().unwrap();
    let body: serde_json::Value = serde_json::from_str(&requests[1].1).unwrap();
    assert_eq!(body["model"], "selected-found");
    assert!(requests[1]
        .0
        .to_ascii_lowercase()
        .contains("authorization: bearer right-provider-key"));
    assert!(!requests[1].0.contains("wrong-provider-key"));
}
