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
//!
//! PR8 拆出的三个有明确职责的模块（`worker` 仍是唯一的回合执行路径）：
//!  - [`sessions`]：会话库、切换/新建/改名/删除、时间线与模型选择的持久化；
//!  - [`timeline`]：时间线 → 对话气泡 / 工具卡片（按真实调用实例）；
//!  - [`settings`]：设置页一次保存的组装、校验、落盘与回填。

pub mod sessions;
pub mod settings;
pub mod timeline;
pub mod worker;

pub use worker::{run_app, UiCommand};
