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

/// 密钥仓库：**每个提供商一个** DPAPI 密文文件。
///
/// 为什么按提供商分开（与旧版 `providerKeys` 同义）：同一份 `secret.bin` 存一个 Key，
/// 切换 MiMo → DeepSeek 时会把上一家的 Key 发到新端点。那既是凭据泄漏，
/// 也会让新端点报 401——用户看到的却是"模型服务坏了"。
///
/// 布局：
/// - `<root>/secrets/<provider-id>.bin`：按提供商的密文
/// - `<root>/secret.bin`：PR5 早期版本的**单 Key** 文件（只读迁移来源）
///
/// 迁移规则（只在 `migrate_legacy` 里做，且只做一次）：
/// 把单 Key 分配给 `settings.json` 里**明确记录的那个 provider_id**，
/// 不做任何猜测，也不删除原文件（用户随时可以回退）。
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

    /// 把 PR5 早期的单 Key 文件迁移到 `provider_id` 名下。
    ///
    /// - 只迁移到调用方**明确**给出的提供商（即 `settings.json` 里的 provider_id）；
    /// - 目标已有 Key 时不覆盖（用户的新输入优先）；
    /// - 迁移后**保留**原文件，不删除用户数据；
    /// - 解密失败（比如换了 Windows 用户）时返回错误，由调用方提示用户重填，
    ///   而不是把坏数据搬过去。
    ///
    /// 返回 `true` 表示这次真的迁移了一个 Key。
    pub fn migrate_legacy(&self, provider_id: &str) -> Result<bool, SecretError> {
        if provider_id.trim().is_empty() || !self.legacy_path.is_file() {
            return Ok(false);
        }
        if self.has(provider_id) {
            return Ok(false);
        }
        let Some(key) = read_secret_file(&self.legacy_path)? else {
            return Ok(false);
        };
        self.save(provider_id, &key)?;
        Ok(true)
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
    // 先写临时文件再改名：写到一半崩溃不会留下半个密文。
    // Windows 的 rename 在目标已存在时会失败，所以先删旧文件
    // （密文本身可重建，且删除与改名之间的窗口只会导致"没有 Key"，不会读到半个值）。
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, encoded)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
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
}
