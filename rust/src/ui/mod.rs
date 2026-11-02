//! 界面层：Slint 窗口 + 后台工作线程。
//!
//! 线程模型（关键：长任务不阻塞 UI）：
//!  - Slint 主线程只跑事件循环；所有模型请求与 PowerShell 子进程 I/O 都在
//!    一个独立的 tokio runtime 线程上执行。
//!  - 两个方向都走通道：界面 → 工作线程用 `mpsc`，工作线程 → 界面用
//!    `slint::invoke_from_event_loop`（Slint 的唯一跨线程入口）。
//!
//! 取消语义：界面点"取消"只置 `CancelToken`，工作线程在下一次检查点干净退出；
//! 迟到的 LLM / PowerShell 响应因此不可能触发新工具或覆盖新回合。

pub mod worker;

pub use worker::{run_app, UiCommand};
