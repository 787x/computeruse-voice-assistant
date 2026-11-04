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
        reg(&["query", KEY, "/v", "CUAVA-Rust"]).is_ok_and(|o| o.status.success())
    }
    #[cfg(not(windows))]
    {
        false
    }
}
pub fn set(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
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
