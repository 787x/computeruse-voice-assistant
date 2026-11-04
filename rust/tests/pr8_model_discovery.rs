//! PR8：真实 /models 请求、提供商隔离及端点目录持久化（全部使用隔离 Mock）。
use cuava::config::{self, AppPaths, Settings};
use cuava::model_catalog::{fetch_models, ModelCache};
use std::io::{Read, Write};

fn mock(model: &str, key: &str, ok: bool) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let model = model.to_string();
    let key = key.to_string();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /v1/models "));
        assert!(request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {key}")));
        let body = if ok {
            format!(r#"{{"data":[{{"id":"{model}"}}]}}"#)
        } else {
            "{}".into()
        };
        let status = if ok {
            "200 OK"
        } else {
            "500 Internal Server Error"
        };
        write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    (endpoint, handle)
}

#[tokio::test]
async fn discovered_models_materialize_missing_providers_and_survive_saved_config() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-tmp")
        .join(format!(
            "pr8-model-discovery-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&root).unwrap();
    let paths = AppPaths::with_root(&root);
    let mut settings = Settings::default();
    assert!(settings.providers.is_empty());
    for (id, key, model) in [
        ("mimo", "fake-mimo", "mimo-discovered"),
        ("deepseek", "fake-deepseek", "deepseek-discovered"),
        ("openai-compatible", "fake-custom", "custom-discovered"),
    ] {
        let (endpoint, server) = mock(model, key, true);
        let mut provider = settings.provider(id);
        provider.base_url = endpoint.clone();
        // 配置一个端点，但不物化 providers 中的条目（内置提供商的合法旧配置）。
        settings.provider_id = id.into();
        settings.base_url = endpoint.clone();
        let mut cache = ModelCache::with_path(paths.model_cache_file());
        let fetch = fetch_models(&provider, key, &mut cache, true).await;
        server.join().unwrap();
        assert!(fetch.ok, "{}", fetch.summary());
        assert!(settings.remember_discovered_models(id, &endpoint, &fetch.models));
        assert!(settings.provider(id).models.iter().any(|m| m.id == model));
    }
    assert!(!settings
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id == "mimo-discovered"));
    config::save_settings(&paths, &settings).unwrap();
    let restored = config::load_settings(&paths).unwrap();
    assert!(restored
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id == "deepseek-discovered"));
    let cache = ModelCache::with_path(paths.model_cache_file());
    assert_eq!(cache.len(), 3);
    let (draft, server) = mock("draft-model", "fake-deepseek", true);
    let mut provider = settings.provider("deepseek");
    provider.base_url = draft.clone();
    let mut cache = ModelCache::with_path(paths.model_cache_file());
    let fetch = fetch_models(&provider, "fake-deepseek", &mut cache, true).await;
    server.join().unwrap();
    assert!(!settings.remember_discovered_models("deepseek", &draft, &fetch.models));
    assert!(!settings
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id == "draft-model"));
    assert_eq!(
        cache
            .models_for("deepseek", &draft, "fake-deepseek")
            .unwrap()[0]
            .id,
        "draft-model"
    );
    let (failed, server) = mock("unused", "fake-deepseek", false);
    provider.base_url = failed;
    let fetch = fetch_models(&provider, "fake-deepseek", &mut cache, true).await;
    server.join().unwrap();
    assert!(!fetch.ok);
    assert!(settings
        .provider("deepseek")
        .models
        .iter()
        .any(|m| m.id == "deepseek-discovered"));
}
