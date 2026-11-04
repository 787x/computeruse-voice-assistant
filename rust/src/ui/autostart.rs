//! Current-user startup only; never enables audio.
#[cfg(windows)]
fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg.exe")
        .args(args)
        .creation_flags(0x08000000)
        .output()
}
const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub fn enabled() -> bool {
    #[cfg(windows)]
    {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        registered_command().is_some_and(|value| owns_command(&value, &exe.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        false
    }
}
#[cfg(windows)]
fn registered_command() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); Get-ItemPropertyValue -LiteralPath 'Registry::HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name 'CUAVA-Rust' -ErrorAction Stop"])
        .creation_flags(0x08000000).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn set(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        if !enabled && !self::enabled() {
            return Ok(());
        }
        let output = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let value = format!("\"{}\" --background", exe.display());
            reg(&[
                "add",
                KEY,
                "/v",
                "CUAVA-Rust",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
        } else {
            reg(&["delete", KEY, "/v", "CUAVA-Rust", "/f"])
        }
        .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err("无法修改当前用户登录启动项".into())
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("只支持 Windows 登录启动".into())
    }
}

fn owns_command(value: &str, exe: &str) -> bool {
    value
        .trim()
        .eq_ignore_ascii_case(&format!("\"{exe}\" --background"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_only_current_exe_and_exact_background_arguments() {
        let q = "\"C:\\Portable\\cuava.exe\" --background";
        assert!(owns_command(q, r"C:\Portable\cuava.exe"));
        assert!(!owns_command(q, r"C:\Installed\cuava.exe"));
        assert!(!owns_command(
            &q.replace("--background", "--settings"),
            r"C:\Portable\cuava.exe"
        ));
    }
}

#[cfg(all(test, windows))]
mod registry_tests {
    use super::*;
    #[test]
    #[ignore = "temporarily edits current-user CUAVA-Rust Run value and restores it"]
    fn registry_roundtrip_and_foreign_owner_preservation() {
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(value) = &self.0 {
                    let _ = reg(&[
                        "add",
                        KEY,
                        "/v",
                        "CUAVA-Rust",
                        "/t",
                        "REG_SZ",
                        "/d",
                        value,
                        "/f",
                    ]);
                } else {
                    let _ = reg(&["delete", KEY, "/v", "CUAVA-Rust", "/f"]);
                }
            }
        }
        let _restore = Restore(registered_command());
        set(true).unwrap();
        assert!(
            enabled(),
            "enable must persist a value owned by the current executable"
        );
        set(false).unwrap();
        assert!(
            registered_command().is_none(),
            "disable must remove our value"
        );
        let foreign = r#""C:\portable-other\cuava.exe" --background"#;
        assert!(reg(&[
            "add",
            KEY,
            "/v",
            "CUAVA-Rust",
            "/t",
            "REG_SZ",
            "/d",
            foreign,
            "/f"
        ])
        .unwrap()
        .status
        .success());
        set(false).unwrap();
        assert_eq!(registered_command().as_deref(), Some(foreign));
    }
}
