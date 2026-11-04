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
        use std::os::windows::process::CommandExt;
        // Registry strings are Unicode. reg.exe redirected output uses a legacy code page.
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); Get-ItemPropertyValue -LiteralPath 'Registry::HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run' -Name 'CUAVA-Rust' -ErrorAction Stop"])
            .creation_flags(0x08000000).output().is_ok_and(|o| {
                o.status.success() && owns_command(&String::from_utf8_lossy(&o.stdout), &exe.to_string_lossy())
            })
    }
    #[cfg(not(windows))]
    {
        false
    }
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
