//! API Key 的本地安全存储（Windows DPAPI）。
//!
//! 目标：真实密钥**绝不明文落盘、绝不进日志**。
//!
//! 做法：用 DPAPI（`CryptProtectData` / `CryptUnprotectData`）以「当前 Windows 用户」
//! 为作用域加密，密文单独存 `secret.bin`。配置文件（`settings.json`）里根本不出现密钥字段，
//! 所以即使配置被打印/上传也不会带出密钥。
//!
//! 明确**不做**的事：不尝试解密 Electron `safeStorage` 写下的 `enc:` 值。
//! 那是 Chromium 的另一套封装（前缀、随机 IV、`v10` 头都不相同），
//! 把它当 DPAPI 密文直接丢进来只会解出乱码或直接失败。
//! 旧配置的正式迁移留给后续发布任务；此处若发现 `enc:` 前缀会明确报错而不是猜测。

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// 密钥存储错误。
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("读不到密钥文件：{0}")]
    Io(#[from] std::io::Error),
    #[error("密钥文件已损坏或不属于当前 Windows 用户（无法解密）")]
    Decrypt,
    #[error("旧版 Electron safeStorage 的 enc: 值不能按 DPAPI 密文处理，请在设置里重新填写 Key")]
    LegacyEncrypted,
}

/// 迁移完成标记的文件名（放在 secrets 目录里）。
///
/// 为什么需要它：旧 `secret.bin` 迁移后**保留**作为备份（不删用户数据），
/// 但只要它还在，"目标提供商没 Key" 就和"还没迁移过"长得一模一样。
/// 没有标记的话，用户清除某个 Key 之后重启，就会被旧文件重新填回来——
/// 用户的清除操作看起来"没生效"。
///
/// 标记里记下**迁移给了哪个提供商**：之后无论配置切到哪家，都不会再迁一次。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";

/// 密钥仓库：**每个提供商一个** DPAPI 密文文件。
///
/// 为什么按提供商分开（与旧版 `providerKeys` 同义）：同一份 `secret.bin` 存一个 Key，
/// 切换 MiMo → DeepSeek 时会把上一家的 Key 发到新端点。那既是凭据泄漏，
/// 也会让新端点报 401——用户看到的却是"模型服务坏了"。
///
/// 布局：
/// - `<root>/secrets/<provider-id>.bin`：按提供商的密文
/// - `<root>/secrets/.legacy-migrated`：旧 Key 已迁移的标记（记下归属提供商）
/// - `<root>/secret.bin`：PR5 早期版本的**单 Key** 文件（只读迁移来源，保留作备份）
///
/// 迁移规则（`migrate_legacy`）：把单 Key 分配给 `settings.json` 里**明确记录**的
/// provider_id，做一次就写标记；不猜测归属、不删除原文件。
pub struct SecretStore {
    dir: PathBuf,
    legacy_path: PathBuf,
}

impl SecretStore {
    /// `root` 是数据目录；单 Key 时代传的是 `<root>/secret.bin`，
    /// 这里同时接受两种形态，保证老调用点仍能工作。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (dir, legacy_path) = if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == "secret.bin")
            .unwrap_or(false)
        {
            let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
            (root.join("secrets"), path)
        } else {
            (path.join("secrets"), path.join("secret.bin"))
        };
        Self { dir, legacy_path }
    }

    /// 某个提供商的密文文件路径（供测试与排查）。
    pub fn path_for(&self, provider_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.bin", sanitize_provider_id(provider_id)))
    }

    pub fn legacy_path(&self) -> &Path {
        &self.legacy_path
    }

    fn marker_path(&self) -> PathBuf {
        self.dir.join(LEGACY_MIGRATION_MARKER)
    }

    /// 旧 Key 已经迁移给哪个提供商（`None` = 还没迁移过）。
    pub fn legacy_migrated_to(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.marker_path()).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// 该提供商是否已有保存的 Key。
    pub fn has(&self, provider_id: &str) -> bool {
        self.path_for(provider_id).is_file()
    }

    /// 读出**指定提供商**的明文 Key。没有文件 → `Ok(None)`。
    ///
    /// 关键语义：**绝不回退到别的提供商**。当前提供商没 Key 就返回 `None`，
    /// 调用方据此决定"不发 Authorization"，而不是借用上一家的 Key。
    pub fn load(&self, provider_id: &str) -> Result<Option<String>, SecretError> {
        read_secret_file(&self.path_for(provider_id))
    }

    /// 写入指定提供商的 Key。空串等同于删除**这一个**提供商的 Key。
    pub fn save(&self, provider_id: &str, plaintext: &str) -> Result<(), SecretError> {
        if plaintext.is_empty() {
            return self.clear(provider_id);
        }
        if plaintext.starts_with("enc:") {
            return Err(SecretError::LegacyEncrypted);
        }
        std::fs::create_dir_all(&self.dir)?;
        write_secret_file(&self.path_for(provider_id), plaintext)
    }

    /// 清除指定提供商的 Key（不影响其它提供商）。
    pub fn clear(&self, provider_id: &str) -> Result<(), SecretError> {
        remove_secret_file(&self.path_for(provider_id))
    }

    /// 把所有已知提供商的 Key 摘要（打码值）——只用于诊断，不含明文。
    pub fn masked_keys(&self, provider_ids: &[&str]) -> Vec<(String, String)> {
        provider_ids
            .iter()
            .filter_map(|id| {
                self.load(id)
                    .ok()
                    .flatten()
                    .map(|key| (id.to_string(), mask_secret(&key)))
            })
            .collect()
    }

    /// 读联网搜索（Tavily）的 Key。
    ///
    /// 为什么单独一对方法而不是复用 `load("tavily")`：搜索 Key **不是**一个模型提供商，
    /// 它不能出现在模型下拉里、也不能被"切换提供商"的清空逻辑碰到。
    /// 用一个固定槽位 + 专门的入口，这两件事都不会误伤。
    pub fn load_search_key(&self) -> Result<Option<String>, SecretError> {
        read_secret_file(&self.search_key_path())
    }

    /// 写联网搜索 Key（空串 = 删除）。
    pub fn save_search_key(&self, plaintext: &str) -> Result<(), SecretError> {
        if plaintext.is_empty() {
            return self.clear_search_key();
        }
        if plaintext.starts_with("enc:") {
            return Err(SecretError::LegacyEncrypted);
        }
        std::fs::create_dir_all(&self.dir)?;
        write_secret_file(&self.search_key_path(), plaintext)
    }

    /// 是否已有联网搜索 Key。
    pub fn has_search_key(&self) -> bool {
        self.search_key_path().is_file()
    }

    /// 清除联网搜索 Key（不影响任何模型提供商的 Key）。
    pub fn clear_search_key(&self) -> Result<(), SecretError> {
        remove_secret_file(&self.search_key_path())
    }

    /// 联网搜索 Key 的密文文件路径（供测试与排查）。
    pub fn search_key_path(&self) -> PathBuf {
        self.path_for(crate::config::SEARCH_SECRET_ID)
    }

    /// 把 PR5 早期的单 Key 文件迁移到 `provider_id` 名下。
    ///
    /// 规则：
    /// - 只迁移到调用方**明确**给出的提供商（即 `settings.json` 里的 provider_id）；
    /// - 迁移成功后写标记，之后重启、切换提供商、清除 Key 都不会重来；
    /// - 目标已有 Key 时不覆盖（用户的新输入优先），写标记防止以后被旧文件回填；
    /// - 迁移后**保留**原文件，不删除用户数据；
    /// - **失败可重试**：只有 Key 与标记**都写成功**才算完成。
    ///
    /// 写入顺序是「先 Key、后标记」，失败时会**撤销 Key**：
    ///  - 先写标记的旧做法有个死结——标记写成功、Key 写失败时，
    ///    下次启动会以为"已迁移"，于是永远不再重试，用户的旧 Key 就此丢失；
    ///  - 现在最坏情况是"Key 写了但标记没写成"：撤销 Key 后状态回到迁移前，
    ///    下次启动可以安全重试，且重试目标仍是同一个 provider_id（不会归错家）。
    ///
    /// 返回 `true` 表示这次真的迁移了一个 Key。
    pub fn migrate_legacy(&self, provider_id: &str) -> Result<bool, SecretError> {
        let provider_id = provider_id.trim();
        if provider_id.is_empty() {
            return Ok(false);
        }
        // 已经迁移过：无论配置现在指向哪家，都不再迁第二次。
        if self.legacy_migrated_to().is_some() {
            return Ok(false);
        }
        if !self.legacy_path.is_file() {
            return Ok(false);
        }
        // 目标已经有 Key：说明用户在新版本里自己填过，旧文件不该覆盖它。
        // 写标记，这样以后清除该 Key 也不会被旧文件复活。
        if self.has(provider_id) {
            self.write_marker(provider_id)?;
            return Ok(false);
        }

        // 读失败直接返回错误（不写任何东西）：下次启动仍可重试。
        let Some(key) = read_secret_file(&self.legacy_path)? else {
            return Ok(false);
        };
        std::fs::create_dir_all(&self.dir)?;

        // 1) 先写 Key。
        write_secret_file(&self.path_for(provider_id), &key)?;
        // 2) 再写标记。标记失败要撤销刚写的 Key，让状态回到"可重试"。
        if let Err(error) = self.write_marker(provider_id) {
            let _ = remove_secret_file(&self.path_for(provider_id));
            return Err(error);
        }
        Ok(true)
    }

    /// 写迁移标记。失败必须让调用方知道——否则下次会重复迁移。
    fn write_marker(&self, provider_id: &str) -> Result<(), SecretError> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.marker_path(), provider_id)?;
        Ok(())
    }
}

/// 提供商 id 用作文件名前先做一次白名单收敛：
/// 只保留字母、数字、`-`、`_`，避免路径穿越或非法字符。
fn sanitize_provider_id(provider_id: &str) -> String {
    let cleaned: String = provider_id
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn read_secret_file(path: &Path) -> Result<Option<String>, SecretError> {
    if !path.is_file() {
        return Ok(None);
    }
    let encoded = std::fs::read_to_string(path)?;
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Ok(None);
    }
    let cipher = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| SecretError::Decrypt)?;
    let plain = protect::unprotect(&cipher)?;
    let text = String::from_utf8(plain).map_err(|_| SecretError::Decrypt)?;
    Ok(Some(text))
}

fn write_secret_file(path: &Path, plaintext: &str) -> Result<(), SecretError> {
    let cipher = protect::protect(plaintext.as_bytes())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(cipher);
    /*
     * 写入策略：**绝不先删除唯一有效副本**。
     *
     * 之前是"写临时文件 → 删旧文件 → 改名"。Windows 上 rename 不能覆盖已有目标，
     * 但这个顺序在改名失败时会把原 Key 连根拔掉——用户只是改个 Key，
     * 结果原来那把也没了。
     *
     * 现在：
     *  - 没有旧文件 → 直接改名上位；
     *  - 有旧文件 → 先把旧文件降级为 `.bak`，再把临时文件改名为正式文件；
     *    第二步失败时用备份还原。密文随时可从内存里的明文重新加密，
     *    所以备份只是"这次操作失败不丢数据"的保险，不是长期存储。
     */
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &encoded)?;

    if !path.exists() {
        std::fs::rename(&tmp, path)?;
        return Ok(());
    }

    let backup = path.with_extension("bin.bak");
    let _ = remove_secret_file(&backup);
    // 旧文件 → 备份。这一步失败说明旧文件动不了，直接放弃写入（原 Key 还在）。
    std::fs::rename(path, &backup)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            let _ = remove_secret_file(&backup);
            Ok(())
        }
        Err(error) => {
            // 新文件没能上位：把旧文件放回原位，保证用户原来的 Key 仍然可用。
            let _ = std::fs::rename(&backup, path);
            let _ = remove_secret_file(&tmp);
            Err(error.into())
        }
    }
}

fn remove_secret_file(path: &Path) -> Result<(), SecretError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 打码显示，供界面/日志使用：只留首尾各 4 个字符。
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}…{tail}")
}

#[cfg(windows)]
mod protect {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::LocalFree;
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    use super::SecretError;

    /// 给密文加一个固定熵：同一台机器上的其它程序随手调用 DPAPI 也解不出我们的值。
    const ENTROPY: &[u8] = b"cuava-agent/v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            // DPAPI 只读入参，这里借用而非持有
            pbData: data.as_ptr() as *mut u8,
        }
    }

    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() || out.cbData == 0 {
            return Vec::new();
        }
        // SAFETY: DPAPI 成功返回时，pbData 指向 LocalAlloc 分配的 cbData 字节。
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                out.pbData as *mut _,
            )));
        }
        bytes
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, SecretError> {
        let data_in = blob(plain);
        let entropy = blob(ENTROPY);
        let mut data_out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: 入参缓冲区在整个调用期间有效；data_out 由系统填充。
        unsafe {
            CryptProtectData(
                &data_in,
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                0,
                &mut data_out,
            )
            .map_err(|_| SecretError::Decrypt)?;
        }
        Ok(take(data_out))
    }

    pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, SecretError> {
        let data_in = blob(cipher);
        let entropy = blob(ENTROPY);
        let mut data_out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: 同 protect。
        unsafe {
            CryptUnprotectData(&data_in, None, Some(&entropy), None, None, 0, &mut data_out)
                .map_err(|_| SecretError::Decrypt)?;
        }
        Ok(take(data_out))
    }
}

#[cfg(not(windows))]
mod protect {
    //! 非 Windows 构建仅用于编译/CI 的纯逻辑测试。
    //! 这些平台上**没有**等价于 DPAPI 的系统安全存储，因此直接拒绝写入，
    //! 而不是退回明文（那会让"密钥不落明文"的承诺在任何平台测试里失效）。

    use super::SecretError;

    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, SecretError> {
        Err(SecretError::Decrypt)
    }

    pub fn unprotect(_cipher: &[u8]) -> Result<Vec<u8>, SecretError> {
        Err(SecretError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_keeps_edges_only() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("short"), "•••••");
        assert_eq!(mask_secret("sk-1234567890abcd"), "sk-1…abcd");
    }

    /// 每个测试用独立目录（放在 `target/test-tmp` 下）。
    ///
    /// 不用系统临时目录：受限环境（沙箱/托管 CI）里 `%TEMP%` 可能不可写，
    /// 而测试只应在本仓库的构建目录内读写。
    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("{prefix}-{seq}"))
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_and_never_plaintext_on_disk() {
        let dir = unique_temp_dir("cuava-secret");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(dir.join("secret.bin"));

        store.save("mimo", "sk-test-key-value-0123456789").unwrap();
        assert_eq!(
            store.load("mimo").unwrap().as_deref(),
            Some("sk-test-key-value-0123456789")
        );

        // 磁盘上不能出现明文
        let raw = std::fs::read_to_string(store.path_for("mimo")).unwrap();
        assert!(!raw.contains("sk-test-key-value"));

        store.clear("mimo").unwrap();
        assert_eq!(store.load("mimo").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn legacy_electron_enc_value_is_rejected() {
        let dir = unique_temp_dir("cuava-secret-enc");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(dir.join("secret.bin"));
        let error = store.save("mimo", "enc:abcdef==").unwrap_err();
        assert!(matches!(error, SecretError::LegacyEncrypted));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每个提供商的 Key 相互独立：切换不会串用，缺失也不回退。
    #[cfg(windows)]
    #[test]
    fn keys_are_isolated_per_provider() {
        let dir = unique_temp_dir("cuava-secret-isolation");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(&dir);

        store.save("mimo", "sk-mimo-key").unwrap();
        store.save("deepseek", "sk-deepseek-key").unwrap();

        assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-mimo-key"));
        assert_eq!(
            store.load("deepseek").unwrap().as_deref(),
            Some("sk-deepseek-key")
        );
        // 没配置过的提供商必须返回 None，绝不能借用别家的 Key。
        assert_eq!(store.load("my-gateway").unwrap(), None);
        assert!(!store.has("my-gateway"));

        // 清除一家不影响另一家（MiMo → DeepSeek → MiMo 仍可用）
        store.clear("deepseek").unwrap();
        assert_eq!(store.load("deepseek").unwrap(), None);
        assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-mimo-key"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PR5 早期的单 Key 文件：只迁移到配置里明确记录的提供商，且不删原文件。
    #[cfg(windows)]
    #[test]
    fn legacy_single_key_migrates_to_recorded_provider_only() {
        let dir = unique_temp_dir("cuava-secret-migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("secret.bin");
        // 用保存路径先造出与旧版一致的密文
        write_secret_file(&legacy, "sk-legacy-single-key").unwrap();

        let store = SecretStore::new(&dir);
        assert!(
            store.migrate_legacy("mimo").unwrap(),
            "应迁移到记录的提供商"
        );
        assert_eq!(
            store.load("mimo").unwrap().as_deref(),
            Some("sk-legacy-single-key")
        );
        // 绝不猜测分配给别的提供商
        assert_eq!(store.load("deepseek").unwrap(), None);
        // 原文件保留（用户数据不静默删除）
        assert!(legacy.is_file());

        // 再迁一次不会再复制一遍，也不会覆盖目标已有的 Key
        store.save("mimo", "sk-new-key").unwrap();
        assert!(!store.migrate_legacy("mimo").unwrap());
        assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-new-key"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 提供商 id 会被收敛成安全的文件名，不能出现路径穿越。
    #[test]
    fn provider_id_is_sanitized_for_file_names() {
        assert_eq!(sanitize_provider_id("mimo"), "mimo");
        assert_eq!(sanitize_provider_id("my-gateway_v2"), "my-gateway_v2");
        assert_eq!(sanitize_provider_id("../../etc"), "______etc");
        assert_eq!(sanitize_provider_id("   "), "unknown");
        assert_eq!(sanitize_provider_id("a/b"), "a_b");
    }

    /// 联网搜索的 Key 是一个**独立槽位**：不撞任何提供商，也能单独清掉。
    #[cfg(windows)]
    #[test]
    fn search_key_uses_its_own_slot_and_never_touches_provider_keys() {
        let dir = unique_temp_dir("cuava-secret-search");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(&dir);

        store.save("mimo", "sk-mimo-key").unwrap();
        assert!(!store.has_search_key());

        store.save_search_key("tvly-search-key").unwrap();
        assert!(store.has_search_key());
        assert_eq!(
            store.load_search_key().unwrap().as_deref(),
            Some("tvly-search-key")
        );
        // 模型 Key 一个都不能被影响。
        assert_eq!(store.load("mimo").unwrap().as_deref(), Some("sk-mimo-key"));
        assert_eq!(store.load("deepseek").unwrap(), None);
        // 搜索 Key 也不能出现在"提供商 Key"列表里。
        assert!(!store
            .masked_keys(&["mimo", "deepseek"])
            .iter()
            .any(|(id, _)| id == "web-search"));

        // 明文不落盘。
        let raw = std::fs::read_to_string(store.search_key_path()).unwrap();
        assert!(!raw.contains("tvly-search-key"));

        store.clear_search_key().unwrap();
        assert!(!store.has_search_key());
        assert_eq!(
            store.load("mimo").unwrap().as_deref(),
            Some("sk-mimo-key"),
            "清搜索 Key 不能顺手清模型 Key"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn search_key_rejects_legacy_electron_values() {
        let dir = unique_temp_dir("cuava-secret-search-enc");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(&dir);
        let error = store.save_search_key("enc:abcdef==").unwrap_err();
        assert!(matches!(error, SecretError::LegacyEncrypted));
        assert!(!store.has_search_key());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
