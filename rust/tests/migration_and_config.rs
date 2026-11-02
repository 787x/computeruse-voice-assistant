//! 迁移中途失败与配置损坏的回归测试。
//!
//! 两个缺陷：
//!  1. `migrate_legacy` 曾"先写标记、后写 Key"：标记成了、Key 没写成时，
//!     下次启动会以为已迁移，于是永远不再重试 —— 用户的旧 Key 就此丢失。
//!  2. `run_app` 曾用 `unwrap_or_else(|_| Settings::default())` 兜底：
//!     配置损坏时静默改用默认配置，既可能把请求打到没配过的端点，
//!     又可能拿默认的 provider_id 给旧 Key 定错归属。

use std::path::PathBuf;

use cuava::config::{self, AppPaths, Settings};
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

/// 造一份"PR5 早期版本"的单 Key `secret.bin`。
fn seed_legacy(root: &std::path::Path, value: &str) -> PathBuf {
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("__tmp", value).unwrap();
    let per_provider = store.path_for("__tmp");
    let legacy = root.join("secret.bin");
    std::fs::rename(&per_provider, &legacy).unwrap();
    let _ = std::fs::remove_dir_all(root.join("secrets"));
    legacy
}

// ---------------------------------------------------------------------------
// 1. 迁移中途失败可恢复
// ---------------------------------------------------------------------------

/// 标记写失败时，**不能留下"Key 已写但没有标记"**，且下次能重试成功。
///
/// 故障注入：先让 `.legacy-migrated` 这个路径被目录占住，写标记必然失败。
#[cfg(windows)]
#[test]
fn failed_marker_write_leaves_state_retryable() {
    let root = unique_root("migrate-retry");
    seed_legacy(&root, "sk-old-key");

    let store = SecretStore::new(root.join("secret.bin"));
    let marker = root.join("secrets").join(".legacy-migrated");
    std::fs::create_dir_all(&marker).unwrap();

    let result = store.migrate_legacy("mimo");
    assert!(result.is_err(), "标记写不了必须报错，不能假装成功");

    // 关键：Key 必须被撤销，否则就会出现"没标记但有 Key"的坏状态
    std::fs::remove_dir_all(&marker).unwrap();
    assert_eq!(
        store.load("mimo").unwrap(),
        None,
        "标记失败时必须撤销刚写的 Key，让状态回到可重试"
    );
    assert!(store.legacy_migrated_to().is_none(), "标记不能残留");
    assert!(
        root.join("secret.bin").is_file(),
        "旧文件必须还在，否则用户的数据真丢了"
    );

    // 故障排除后重试：应当成功，且归属仍是同一个提供商
    assert!(store.migrate_legacy("mimo").unwrap(), "重试必须成功");
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-old-key"));
    assert_eq!(store.legacy_migrated_to().as_deref(), Some("mimo"));

    let _ = std::fs::remove_dir_all(&root);
}

/// 重试时提供商已经变了：旧的失败不该让 Key 被归到新提供商名下。
#[cfg(windows)]
#[test]
fn retry_after_failure_never_reassigns_provider() {
    let root = unique_root("migrate-retry-owner");
    seed_legacy(&root, "sk-old-key");

    let store = SecretStore::new(root.join("secret.bin"));
    let marker = root.join("secrets").join(".legacy-migrated");
    std::fs::create_dir_all(&marker).unwrap();
    assert!(store.migrate_legacy("mimo").is_err());
    std::fs::remove_dir_all(&marker).unwrap();

    // 模拟"用户这时切到了 deepseek，然后重启"——第一次成功的迁移才是归属
    assert!(store.migrate_legacy("mimo").unwrap());
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-old-key"));

    // 再次调用（比如又切了提供商）绝不能把它挪走
    assert!(!store.migrate_legacy("deepseek").unwrap());
    assert_eq!(
        store.load("deepseek").unwrap(),
        None,
        "重试完成后不得改归属"
    );
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-old-key"));

    let _ = std::fs::remove_dir_all(&root);
}

/// 迁移成功后用户清除 → 重启：不复活（与既有行为一致，这里再钉一次）。
#[cfg(windows)]
#[test]
fn cleared_after_successful_migration_stays_cleared() {
    let root = unique_root("migrate-clear-stays");
    seed_legacy(&root, "sk-old-key");

    let store = SecretStore::new(root.join("secret.bin"));
    assert!(store.migrate_legacy("mimo").unwrap());
    store.clear("mimo").unwrap();

    let restarted = SecretStore::new(root.join("secret.bin"));
    assert!(!restarted.migrate_legacy("mimo").unwrap());
    assert_eq!(restarted.load("mimo").unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// 已有有效 Key 时不被旧 Key 覆盖。
#[cfg(windows)]
#[test]
fn existing_key_is_never_overwritten() {
    let root = unique_root("migrate-no-overwrite");
    seed_legacy(&root, "sk-old-key");

    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-user-entered").unwrap();

    assert!(!store.migrate_legacy("mimo").unwrap());
    assert_eq!(
        store.load("mimo").unwrap().as_deref(),
        Some("sk-user-entered"),
        "用户自己填的 Key 优先"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 2. 配置损坏时不静默回退
// ---------------------------------------------------------------------------

/// `load_settings` 对损坏配置必须报错，而不是给一份默认值。
#[test]
fn corrupt_config_is_reported_not_defaulted() {
    let root = unique_root("config-corrupt");
    let paths = AppPaths::with_root(&root);
    std::fs::write(paths.settings_file(), "{ this is not json").unwrap();

    let result = config::load_settings(&paths);
    match result {
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("无法解析") || text.contains("读取配置失败"),
                "错误说明要指出问题：{text}"
            );
        }
        Ok(settings) => panic!(
            "损坏的配置不能被当成默认值读出来（会拿到 provider={}）",
            settings.provider_id
        ),
    }

    // 原文件不能被改动
    let raw = std::fs::read_to_string(paths.settings_file()).unwrap();
    assert_eq!(raw, "{ this is not json", "读失败不能顺手覆盖原文件");

    let _ = std::fs::remove_dir_all(&root);
}

/// 文件根本不存在时才返回默认值（全新安装的正常路径）。
#[test]
fn missing_config_yields_defaults() {
    let root = unique_root("config-missing");
    let paths = AppPaths::with_root(&root);
    let settings = config::load_settings(&paths).expect("没有配置文件应当用默认值");
    assert_eq!(settings.provider_id, "mimo");

    let _ = std::fs::remove_dir_all(&root);
}

/// 配置读不出来时，**不能**据此给旧 Key 定归属。
///
/// 这条用"调用方拿不到 provider_id"来保证：`load_settings` 返回 Err 时，
/// 启动流程直接停住，迁移根本不会被调用。
#[cfg(windows)]
#[test]
fn corrupt_config_blocks_legacy_migration() {
    let root = unique_root("config-blocks-migration");
    let paths = AppPaths::with_root(&root);
    std::fs::write(paths.settings_file(), "not json at all").unwrap();
    seed_legacy(&root, "sk-old-key");

    // 启动流程的第一步：读配置。失败就停，不会走到迁移。
    let loaded = config::load_settings(&paths);
    assert!(loaded.is_err(), "损坏配置必须让启动停下");

    // 迁移一次都没有发生过，任何提供商名下都不该有 Key
    let store = SecretStore::new(paths.secret_file());
    for provider in ["mimo", "deepseek", "openai-compatible"] {
        assert_eq!(
            store.load(provider).unwrap(),
            None,
            "{provider} 不该有被猜测归属的旧 Key"
        );
    }
    assert!(store.legacy_migrated_to().is_none(), "不该写迁移标记");
    assert!(root.join("secret.bin").is_file(), "旧文件保留");

    let _ = std::fs::remove_dir_all(&root);
}

/// 配置是合法 JSON 但字段类型不对：同样算损坏，不能静默变成默认值。
#[test]
fn wrong_typed_config_is_also_corrupt() {
    let root = unique_root("config-wrong-type");
    let paths = AppPaths::with_root(&root);
    // maxSteps 期望整数，这里给字符串
    std::fs::write(
        paths.settings_file(),
        r#"{"schemaVersion":1,"providerId":"deepseek","baseUrl":"https://x/v1","model":"m","maxSteps":"lots"}"#,
    )
    .unwrap();

    assert!(
        config::load_settings(&paths).is_err(),
        "类型不对也算配置损坏，不能悄悄替换成默认配置"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 完好配置照常读出（确认这次改动没把正常路径也挡住）。
#[test]
fn healthy_config_loads_normally() {
    let root = unique_root("config-healthy");
    let paths = AppPaths::with_root(&root);
    let settings = Settings {
        provider_id: "deepseek".to_string(),
        base_url: "https://api.deepseek.com/v1".to_string(),
        model: "deepseek-flash".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &settings).unwrap();

    let loaded = config::load_settings(&paths).unwrap();
    assert_eq!(loaded.provider_id, "deepseek");
    assert_eq!(loaded.model, "deepseek-flash");

    let _ = std::fs::remove_dir_all(&root);
}
