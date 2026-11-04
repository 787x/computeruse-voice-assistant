//! Per-user login-session lock and small local file mailbox. No network listener.
use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

pub struct Instance {
    _lock: File,
    directory: PathBuf,
}
impl Instance {
    pub fn acquire() -> std::io::Result<Option<Self>> {
        let session: String = std::env::var("SESSIONNAME")
            .unwrap_or_else(|_| "console".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let directory = root.join("cuava").join(format!("instance-{session}"));
        fs::create_dir_all(&directory)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {
                // Requests from a dead instance must never activate a fresh launch.
                for entry in fs::read_dir(&directory)?.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "request") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
                Ok(Some(Self {
                    _lock: lock,
                    directory,
                }))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if !std::env::args().any(|a| a == "--background") {
                    let action = if std::env::args().any(|a| a == "--settings") {
                        "settings"
                    } else {
                        "open"
                    };
                    fs::write(
                        directory.join(format!("{}.request", std::process::id())),
                        action,
                    )?;
                }
                Ok(None)
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
    pub fn requests(&self) -> Vec<String> {
        let mut requests = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "request") {
                    if let Ok(action) = fs::read_to_string(&path) {
                        requests.push(action);
                    }
                    let _ = fs::remove_file(path);
                }
            }
        }
        requests
    }
}
