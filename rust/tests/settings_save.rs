//! 设置保存与清除 Key 的回归测试（worker 层）。
//!
//! 两个缺陷：
//!  3. 清除命令不带提供商 → 可能删掉"已保存配置"那家，而不是界面选中的那家。
//!  4. 保存顺序是"先覆盖 Key、配置写失败后回滚" → 回滚把用户原有的 Key A 也删了。

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

/// 场景 3：界面从 MiMo 切到 DeepSeek（**未保存**），此时点"清除"只该清 DeepSeek。
///
/// 清除命令现在带 provider_id，所以这里直接按"界面选中的那家"调用，
/// 断言 MiMo 的 Key 不受影响。
#[cfg(windows)]
#[test]
fn clearing_selected_provider_does_not_touch_the_saved_one() {
    let root = unique_root("clear-target");
    let paths = AppPaths::with_root(&root);
    let store = SecretStore::new(paths.secret_file());

    // 已保存的配置是 mimo，且它有 Key
    let saved = Settings {
        provider_id: "mimo".to_string(),
        base_url: "https://api.xiaomimimo.com/v1".to_string(),
        model: "mimo-v2.6-flash".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &saved).unwrap();
    store.save("mimo", "sk-mimo-saved").unwrap();
    store.save("deepseek", "sk-deepseek-saved").unwrap();

    // 用户在设置页切到 deepseek（只是下拉框变了，没有保存），然后点清除
    let selected_provider = "deepseek";
    store.clear(selected_provider).unwrap();

    assert_eq!(
        store.load("mimo").unwrap().as_deref(),
        Some("sk-mimo-saved"),
        "清除界面选中的 DeepSeek，绝不能动到已保存的 MiMo Key"
    );
    assert_eq!(store.load("deepseek").unwrap(), None);
    // 配置本身也没被改动
    let reloaded = config::load_settings(&paths).unwrap();
    assert_eq!(reloaded.provider_id, "mimo");

    let _ = std::fs::remove_dir_all(&root);
}

/// 反过来：清除 MiMo 不影响 DeepSeek。
#[cfg(windows)]
#[test]
fn clearing_one_provider_spares_the_other() {
    let root = unique_root("clear-isolation");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-mimo").unwrap();
    store.save("deepseek", "sk-deepseek").unwrap();

    store.clear("mimo").unwrap();
    assert_eq!(store.load("mimo").unwrap(), None);
    assert_eq!(
        store.load("deepseek").unwrap().as_deref(),
        Some("sk-deepseek")
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 场景 4：已有 Key A → 输入 Key B → **配置写盘失败** → Key A 仍然可用。
///
/// 故障注入：把 `settings.json.tmp` 占成目录，让写临时文件必然失败。
/// 因为新的保存顺序是"先写配置、再写 Key"，配置失败时 Key 压根没被碰过。
#[cfg(windows)]
#[test]
fn config_write_failure_leaves_previous_key_and_config_intact() {
    let root = unique_root("save-fail-keeps-key");
    let paths = AppPaths::with_root(&root);
    let store = SecretStore::new(paths.secret_file());

    // 已有配置与 Key A
    let original = Settings {
        provider_id: "mimo".to_string(),
        base_url: "https://api.xiaomimimo.com/v1".to_string(),
        model: "mimo-v2.6-flash".to_string(),
        permission_mode: PermissionMode::Standard,
        ..Settings::default()
    };
    config::save_settings(&paths, &original).unwrap();
    store.save("mimo", "sk-key-a").unwrap();

    // 注入故障：占住配置的临时文件路径
    let tmp = paths.settings_file().with_extension("json.tmp");
    std::fs::create_dir_all(&tmp).unwrap();

    // 模拟 handle_save_settings 的顺序：先写配置（会失败），失败就不写 Key
    let attempted = Settings {
        model: "mimo-v2.6-pro".to_string(),
        ..original.clone()
    };
    let result = config::save_settings(&paths, &attempted);
    assert!(result.is_err(), "写配置必须失败");

    // 清理故障后检查：Key A 与旧配置都还在
    std::fs::remove_dir_all(&tmp).unwrap();
    assert_eq!(
        store.load("mimo").unwrap().as_deref(),
        Some("sk-key-a"),
        "配置保存失败时不能丢原来的 Key"
    );
    let reloaded = config::load_settings(&paths).unwrap();
    assert_eq!(reloaded.model, "mimo-v2.6-flash", "原有有效配置必须保留");

    let _ = std::fs::remove_dir_all(&root);
}

/// 配置写失败时，磁盘上的旧配置文件不能被删成不存在。
#[cfg(windows)]
#[test]
fn failed_config_write_keeps_the_old_file_on_disk() {
    let root = unique_root("save-fail-keeps-file");
    let paths = AppPaths::with_root(&root);

    let original = Settings {
        model: "keep-this".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &original).unwrap();
    let path = paths.settings_file();
    assert!(path.is_file());

    // 让临时文件路径不可写
    let tmp = path.with_extension("json.tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let attempted = Settings {
        model: "should-not-appear".to_string(),
        ..original
    };
    assert!(config::save_settings(&paths, &attempted).is_err());

    std::fs::remove_dir_all(&tmp).unwrap();
    assert!(path.is_file(), "失败不能把配置文件删掉");
    let reloaded = config::load_settings(&paths).unwrap();
    assert_eq!(reloaded.model, "keep-this", "内容仍是原来那份");

    let _ = std::fs::remove_dir_all(&root);
}

/// 保存成功：配置与 Key 都反映用户的新选择。
#[cfg(windows)]
#[test]
fn successful_save_updates_both_config_and_key() {
    let root = unique_root("save-ok");
    let paths = AppPaths::with_root(&root);
    let store = SecretStore::new(paths.secret_file());

    store.save("mimo", "sk-old").unwrap();
    let next = Settings {
        provider_id: "deepseek".to_string(),
        base_url: "https://api.deepseek.com/v1".to_string(),
        model: "deepseek-v4-pro".to_string(),
        ..Settings::default()
    };
    config::save_settings(&paths, &next).unwrap();
    store.save("deepseek", "sk-new").unwrap();

    let reloaded = config::load_settings(&paths).unwrap();
    assert_eq!(reloaded.provider_id, "deepseek");
    assert_eq!(reloaded.model, "deepseek-v4-pro");
    assert_eq!(store.load("deepseek").unwrap().as_deref(), Some("sk-new"));
    // 原提供商的 Key 不受影响
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-old"));

    let _ = std::fs::remove_dir_all(&root);
}

/// 留空表示不修改：已有 Key 原样保留。
#[cfg(windows)]
#[test]
fn blank_key_never_deletes_the_existing_one() {
    let root = unique_root("save-blank");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-keep-me").unwrap();

    // 界面留空 = 调用方不会调用 save()，Key 不受影响
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-keep-me"));
    // 明确清除才会删
    store.clear("mimo").unwrap();
    assert_eq!(store.load("mimo").unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}
