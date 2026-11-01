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

/// 密钥文件：DPAPI 密文的 base64 文本。
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// 读出明文 Key。没有文件 → `Ok(None)`。
    pub fn load(&self) -> Result<Option<String>, SecretError> {
        if !self.path.is_file() {
            return Ok(None);
        }
        let encoded = std::fs::read_to_string(&self.path)?;
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

    /// 写入明文 Key（内部加密）。空串等同于删除。
    pub fn save(&self, plaintext: &str) -> Result<(), SecretError> {
        if plaintext.is_empty() {
            return self.clear();
        }
        if plaintext.starts_with("enc:") {
            return Err(SecretError::LegacyEncrypted);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cipher = protect::protect(plaintext.as_bytes())?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(cipher);
        // 先写临时文件再改名：写到一半崩溃不会留下半个密文。
        // Windows 的 rename 在目标已存在时会失败，所以先删旧文件
        // （密文本身可重建，且删除与改名之间的窗口只会导致"没有 Key"，不会读到半个值）。
        let tmp = self.path.with_extension("bin.tmp");
        std::fs::write(&tmp, encoded)?;
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), SecretError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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

        store.save("sk-test-key-value-0123456789").unwrap();
        assert_eq!(
            store.load().unwrap().as_deref(),
            Some("sk-test-key-value-0123456789")
        );

        // 磁盘上不能出现明文
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("sk-test-key-value"));

        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn legacy_electron_enc_value_is_rejected() {
        let dir = unique_temp_dir("cuava-secret-enc");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(dir.join("secret.bin"));
        let error = store.save("enc:abcdef==").unwrap_err();
        assert!(matches!(error, SecretError::LegacyEncrypted));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
