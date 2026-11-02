// Windows 子系统：双击启动不弹控制台窗口（调试时可用 RUST_LOG 观察 stderr）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = cuava::ui::run_app() {
        eprintln!("启动失败：{error}");
        std::process::exit(1);
    }
}
