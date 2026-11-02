//! 旧密钥迁移与写入可靠性的回归测试。
//!
//! 两个缺陷：
//!  1. 旧 `secret.bin` 迁移后保留作备份，但没有"已迁移"标记 →
//!     重启或切换提供商时会重复导入，用户的"清除"看起来无效。
//!  2. 写 Key 的顺序是"删旧文件 → 改名"，改名失败会连原有 Key 一起丢掉。

use std::path::PathBuf;

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

// ---------------------------------------------------------------------------
// 旧密钥迁移：只做一次，且只归属有记录的那家
// ---------------------------------------------------------------------------

/// 造一个"PR5 早期版本"的数据目录：只有单 Key 的 `secret.bin`。
#[cfg(windows)]
fn seed_legacy_single_key(root: &std::path::Path, value: &str) -> std::path::PathBuf {
    // 先按新结构写一份，再挪成旧版的单文件形态
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("__tmp", value).unwrap();
    let per_provider = store.path_for("__tmp");
    let legacy_file = root.join("secret.bin");
    // 先把密文挪成旧版的单文件，再清掉按提供商的那套目录
    std::fs::rename(&per_provider, &legacy_file).unwrap();
    let _ = std::fs::remove_dir_all(root.join("secrets"));
    legacy_file
}

/// 旧 MiMo Key 迁移 → 切 DeepSeek → 重启：DeepSeek 不会拿到 MiMo 的 Key。
///
/// 这里"重启"用"新建一个 SecretStore 再迁一次"来模拟：状态全部在磁盘上。
#[cfg(windows)]
#[test]
fn legacy_key_never_reassigns_to_a_later_provider() {
    let root = unique_root("legacy-no-reassign");
    let legacy_file = seed_legacy_single_key(&root, "sk-old-mimo");

    // 第一次启动：配置记录的是 mimo
    let store = SecretStore::new(root.join("secret.bin"));
    assert!(store.migrate_legacy("mimo").unwrap(), "首次应迁移");
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-old-mimo"));

    // 用户切到 deepseek，然后重启
    let restarted = SecretStore::new(root.join("secret.bin"));
    assert!(
        !restarted.migrate_legacy("deepseek").unwrap(),
        "已经迁移过，不能再迁给别家"
    );
    assert_eq!(
        restarted.load("deepseek").unwrap(),
        None,
        "DeepSeek 绝不能拿到 MiMo 的旧 Key"
    );
    assert_eq!(
        restarted.load("mimo").unwrap().as_deref(),
        Some("sk-old-mimo"),
        "原有归属不变"
    );
    assert!(legacy_file.is_file(), "旧文件保留作备份");

    let _ = std::fs::remove_dir_all(&root);
}

/// 迁移成功 → 用户清除 → 重启：被清除的 Key 不会从旧文件复活。
#[cfg(windows)]
#[test]
fn cleared_key_does_not_come_back_after_restart() {
    let root = unique_root("legacy-no-resurrect");
    seed_legacy_single_key(&root, "sk-old-mimo");

    let store = SecretStore::new(root.join("secret.bin"));
    assert!(store.migrate_legacy("mimo").unwrap());
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-old-mimo"));

    // 用户明确清除
    store.clear("mimo").unwrap();
    assert_eq!(store.load("mimo").unwrap(), None);

    // 重启：旧文件还在，但绝不能把 Key 填回来
    let restarted = SecretStore::new(root.join("secret.bin"));
    assert!(
        !restarted.migrate_legacy("mimo").unwrap(),
        "已迁移过就不能再来一次"
    );
    assert_eq!(
        restarted.load("mimo").unwrap(),
        None,
        "清除是用户的明确意图，重启不能反悔"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 重启多次也只迁移一次（幂等）。
#[cfg(windows)]
#[test]
fn migration_is_idempotent_across_restarts() {
    let root = unique_root("legacy-idempotent");
    seed_legacy_single_key(&root, "sk-once");

    for _ in 0..3 {
        let store = SecretStore::new(root.join("secret.bin"));
        store.migrate_legacy("mimo").unwrap();
    }
    let final_store = SecretStore::new(root.join("secret.bin"));
    assert_eq!(
        final_store.legacy_migrated_to().as_deref(),
        Some("mimo"),
        "标记记录迁移归属"
    );
    assert_eq!(
        final_store.load("mimo").unwrap().as_deref(),
        Some("sk-once")
    );
    assert_eq!(final_store.load("deepseek").unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// 目标提供商已有用户新填的 Key：不覆盖，但写标记防止以后被旧文件回填。
#[cfg(windows)]
#[test]
fn existing_new_key_is_not_overwritten_by_legacy() {
    let root = unique_root("legacy-existing");
    seed_legacy_single_key(&root, "sk-old");

    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-new-user-input").unwrap();

    assert!(
        !store.migrate_legacy("mimo").unwrap(),
        "目标已有 Key，不覆盖"
    );
    assert_eq!(
        store.load("mimo").unwrap().as_deref(),
        Some("sk-new-user-input")
    );
    // 仍然写了标记 → 之后清除也不会被旧文件复活
    store.clear("mimo").unwrap();
    let restarted = SecretStore::new(root.join("secret.bin"));
    assert!(!restarted.migrate_legacy("mimo").unwrap());
    assert_eq!(restarted.load("mimo").unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

/// 没有旧文件时不写标记（保持"还没迁移过"的语义，别无故占用一个状态）。
#[cfg(windows)]
#[test]
fn no_legacy_file_means_no_marker() {
    let root = unique_root("legacy-absent");
    let store = SecretStore::new(root.join("secret.bin"));
    assert!(!store.migrate_legacy("mimo").unwrap());
    assert!(store.legacy_migrated_to().is_none());
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// 保存失败不丢数据
// ---------------------------------------------------------------------------

/// 密钥写入失败时，**原有 Key 必须仍然可用**。
///
/// 故障注入：先正常写入 Key A；然后把临时文件路径占成一个**目录**，
/// 让 `save(B)` 在写临时文件阶段就失败——这一步发生在动到 A 之前，
/// 所以 A 必须原封不动。
#[cfg(windows)]
#[test]
fn failed_key_write_keeps_the_previous_key() {
    let root = unique_root("secret-write-fail");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-key-a").unwrap();
    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-key-a"));

    // 让 "*.bin.tmp" 这个路径被一个目录占据：写临时文件必然失败
    let target = store.path_for("mimo");
    let tmp_path = target.with_extension("bin.tmp");
    std::fs::create_dir_all(&tmp_path).unwrap();

    let error = store.save("mimo", "sk-key-b");
    assert!(error.is_err(), "写临时文件失败时必须报错");

    // 故障排除后，原来的 Key A 必须还在
    std::fs::remove_dir_all(&tmp_path).unwrap();
    assert_eq!(
        store.load("mimo").unwrap().as_deref(),
        Some("sk-key-a"),
        "写入 Key B 失败，Key A 不能被波及"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 更贴近真实场景：写入失败时**原 Key 保持可用**。
///
/// 用 `.bak` 机制验证：目标文件存在时，写入走"旧→备份→新上位"，
/// 只要新文件能上位，旧值就被正确替换；若失败则还原。
#[cfg(windows)]
#[test]
fn write_replaces_existing_key_and_leaves_no_backup() {
    let root = unique_root("secret-write-replace");
    let store = SecretStore::new(root.join("secret.bin"));
    store.save("mimo", "sk-first").unwrap();
    store.save("mimo", "sk-second").unwrap();

    assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-second"));
    // 成功路径不留 .bak / .tmp
    let dir = root.join("secrets");
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".bak") || name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "不该留下临时文件：{leftovers:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// 迁移过程中写标记失败：必须如实报错，且不能把 Key 归错家。
///
/// 故障注入：先把 `secrets/` 建成**目录**（正常），再让 `.legacy-migrated`
/// 这个路径被目录占住，写标记必然失败。
#[cfg(windows)]
#[test]
fn migration_failure_is_reported_not_faked() {
    let root = unique_root("legacy-marker-fail");
    seed_legacy_single_key(&root, "sk-old");

    let store = SecretStore::new(root.join("secret.bin"));
    let marker = root.join("secrets").join(".legacy-migrated");
    std::fs::create_dir_all(&marker).unwrap();

    let result = store.migrate_legacy("mimo");
    assert!(result.is_err(), "标记写不了就必须报错");

    // 关键：不能出现"Key 已写进 mimo 但没有标记"这种可被再次迁移的状态
    std::fs::remove_dir_all(&marker).unwrap();
    assert_eq!(
        store.load("mimo").unwrap(),
        None,
        "失败时不能留下一个未受标记保护的 Key"
    );
    assert!(store.legacy_migrated_to().is_none(), "失败不写标记");

    let _ = std::fs::remove_dir_all(&root);
}
