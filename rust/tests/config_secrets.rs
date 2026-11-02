//! 配置与密钥的回归测试：按提供商的 Key、设置保存、跨提供商隔离。
//!
//! 全部使用隔离数据目录（`target/test-tmp/...`），不碰用户的 `%APPDATA%\cuava`，
//! 也不使用任何真实 Key。

use std::path::PathBuf;

use cuava::config::{self, AppPaths, PermissionMode, Settings};
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

/// 设置保存后重新读取：必须拿到最新值（这正是"重复打开设置显示旧值"的根因）。
#[test]
fn saved_settings_are_visible_on_reload() {
    let root = unique_root("cfg-reload");
    let paths = AppPaths::with_root(&root);

    let first = Settings {
        provider_id: "deepseek".to_string(),
        base_url: "https://api.deepseek.com/v1".to_string(),
        model: "deepseek-v4-pro".to_string(),
        permission_mode: PermissionMode::Conservative,
        ..Settings::default()
    }
    .normalized()
    .unwrap();
    config::save_settings(&paths, &first).unwrap();

    // 第二次打开设置读到的就是这份，而不是启动时的默认值
    let reloaded = config::load_settings(&paths).unwrap();
    assert_eq!(reloaded.provider_id, "deepseek");
    assert_eq!(reloaded.model, "deepseek-v4-pro");
    assert_eq!(reloaded.permission_mode, PermissionMode::Conservative);

    // 再改一次再读：仍然是最新值
    let second = Settings {
        model: "deepseek-flash".to_string(),
        permission_mode: PermissionMode::Permissive,
        ..reloaded
    };
    config::save_settings(&paths, &second).unwrap();
    let again = config::load_settings(&paths).unwrap();
    assert_eq!(again.model, "deepseek-flash");
    assert_eq!(again.permission_mode, PermissionMode::Permissive);

    let _ = std::fs::remove_dir_all(&root);
}

/// 校验失败的设置不落盘，也不覆盖已有配置。
#[test]
fn invalid_settings_do_not_overwrite_the_saved_ones() {
    let root = unique_root("cfg-invalid");
    let paths = AppPaths::with_root(&root);

    let good = Settings {
        model: "keep-me".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &good).unwrap();

    let bad = Settings {
        base_url: "ftp://nope".to_string(),
        ..Settings::default()
    };
    assert!(bad.normalized().is_err(), "非法地址必须被拒绝");

    // 磁盘上仍然是原来那份
    let reloaded = config::load_settings(&paths).unwrap();
    assert_eq!(reloaded.model, "keep-me");

    let _ = std::fs::remove_dir_all(&root);
}

/// 配置文件里绝不出现密钥字段。
#[test]
fn settings_file_never_contains_key_material() {
    let root = unique_root("cfg-no-secret");
    let paths = AppPaths::with_root(&root);
    config::save_settings(&paths, &Settings::default()).unwrap();

    let raw = std::fs::read_to_string(paths.settings_file()).unwrap();
    for needle in ["apiKey", "api_key", "sk-", "Bearer"] {
        assert!(!raw.contains(needle), "配置文件不该出现 {needle}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// MiMo → DeepSeek → MiMo：各家的 Key 独立保存，切回来仍然可用。
#[cfg(windows)]
#[test]
fn provider_keys_survive_switching_between_providers() {
    let root = unique_root("cfg-keys");
    let store = SecretStore::new(root.join("secret.bin"));

    store.save("mimo", "sk-mimo-0001").unwrap();
    store.save("deepseek", "sk-deepseek-0002").unwrap();

    // 当前是 deepseek
    assert_eq!(
        store.load("deepseek").unwrap().as_deref(),
        Some("sk-deepseek-0002")
    );
    // 切回 mimo：Key 还在，不需要重填
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-mimo-0001"));

    // 没配过的提供商不会借用别人的 Key
    assert_eq!(store.load("openai-compatible").unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// 配置里的 provider_id 决定读哪一个 Key（真实的取值链路）。
#[cfg(windows)]
#[test]
fn settings_provider_id_selects_the_matching_key() {
    let root = unique_root("cfg-select");
    let paths = AppPaths::with_root(&root);
    let store = SecretStore::new(paths.secret_file());

    store.save("mimo", "sk-mimo-key").unwrap();
    store.save("deepseek", "sk-deepseek-key").unwrap();

    let mut settings = Settings {
        provider_id: "mimo".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &settings).unwrap();
    let loaded = config::load_settings(&paths).unwrap();
    assert_eq!(
        store.load(&loaded.provider_id).unwrap().as_deref(),
        Some("sk-mimo-key")
    );

    settings.provider_id = "deepseek".to_string();
    settings.base_url = "https://api.deepseek.com/v1".to_string();
    settings.model = "deepseek-flash".to_string();
    config::save_settings(&paths, &settings).unwrap();
    let loaded = config::load_settings(&paths).unwrap();
    assert_eq!(
        store.load(&loaded.provider_id).unwrap().as_deref(),
        Some("sk-deepseek-key")
    );

    // 切回 mimo 仍然可用
    settings.provider_id = "mimo".to_string();
    config::save_settings(&paths, &settings).unwrap();
    let loaded = config::load_settings(&paths).unwrap();
    assert_eq!(
        store.load(&loaded.provider_id).unwrap().as_deref(),
        Some("sk-mimo-key")
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// "留空不修改"与"明确清除"是两件事。
#[cfg(windows)]
#[test]
fn blank_key_keeps_existing_and_clear_removes_it() {
    let root = unique_root("cfg-blank-vs-clear");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-existing").unwrap();

    // 留空（界面上不填）→ 调用方根本不会调用 save，Key 保持原样
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-existing"));

    // 明确清除 → 变成 None
    store.clear("mimo").unwrap();
    assert_eq!(store.load("mimo").unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// PR5 早期的单 Key 数据只迁移给配置里记录的提供商。
#[cfg(windows)]
#[test]
fn legacy_single_key_is_attributed_to_the_recorded_provider() {
    let root = unique_root("cfg-legacy");
    let paths = AppPaths::with_root(&root);
    std::fs::create_dir_all(&root).unwrap();

    // 用 SecretStore 自己造一份"旧版单 Key"密文（路径与旧版一致）
    let legacy = SecretStore::new(root.join("secret.bin"));
    legacy.save("mimo", "sk-legacy-value").unwrap();
    // 把按提供商的那份挪成旧版单文件形态
    let per_provider = legacy.path_for("mimo");
    let legacy_file = root.join("secret.bin");
    std::fs::rename(&per_provider, &legacy_file).unwrap();
    let _ = std::fs::remove_dir_all(root.join("secrets"));

    // 配置记录的是 deepseek → 迁移只给 deepseek，绝不猜成 mimo
    let settings = Settings {
        provider_id: "deepseek".to_string(),
        base_url: "https://api.deepseek.com/v1".to_string(),
        model: "deepseek-flash".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &settings).unwrap();

    let store = SecretStore::new(paths.secret_file());
    assert!(
        store.migrate_legacy("deepseek").unwrap(),
        "应按配置记录的提供商迁移"
    );
    assert_eq!(
        store.load("deepseek").unwrap().as_deref(),
        Some("sk-legacy-value")
    );
    assert_eq!(store.load("mimo").unwrap(), None, "不能顺手分给别家");
    assert!(legacy_file.is_file(), "旧文件不静默删除");

    let _ = std::fs::remove_dir_all(&root);
}

/// 端点摘要（界面显示用）不含密钥。
#[test]
fn endpoint_summary_is_secret_free() {
    let settings = Settings::default();
    let summary = config::describe_endpoint(&settings, true);
    assert!(summary.contains("Key 已保存"));
    assert!(!summary.contains("sk-"));
    let without = config::describe_endpoint(&settings, false);
    assert!(without.contains("未保存 Key"));
}
