#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn diagnostic(stage: &str) {
    use std::io::Write;
    let root = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("cuava");
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    let file = root.join("startup.log");
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > 65536) {
        let _ = std::fs::write(&file, b"");
    }
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(log, "{time} pid={} {stage}", std::process::id());
    }
}
fn main() {
    diagnostic("main-entered");
    if let Err(error) = cuava::ui::run_app() {
        diagnostic(if error.is::<slint::PlatformError>() {
            "failed: window/backend initialization"
        } else if error.is::<std::io::Error>() {
            "failed: local file/instance/runtime initialization"
        } else {
            "failed: application initialization"
        });
        std::process::exit(1);
    }
    diagnostic("normal-exit-or-forwarded-instance");
}
