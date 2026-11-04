//! Computer Use 语音助手 —— Rust + Slint 原生桌面程序。
//!
//! 纵向切片：文本/语音指令 → LLM Agent → 权限判断 →
//! Windows Computer Use（PowerShell + UIA）→ 界面展示结果 → 语音播报。
//! **不运行 Electron / WebView / Node / Python。**
//!
//! 模块边界（刻意保持扁平，避免为架构而架构）：
//!  - [`config`]：数据目录、**多提供商**档案、模型方案与时段规则、默认值
//!  - [`clock`]：本地时间（时段规则与会话更新时间都依赖它）
//!  - [`secret`]：DPAPI 密钥存储（按提供商隔离，绝不明文落盘）
//!  - [`llm`]：OpenAI 兼容 Chat Completions（流式、工具调用、reasoning 分离）
//!  - [`model_catalog`]：`/models` 分类/缓存 + 连通性测试的结论分类
//!  - [`context`]：token 预算与压缩
//!  - [`permissions`]：四档权限、逐能力覆盖、暂停/限时授权、审计
//!  - [`cua`]：PowerShell 后端桥接 + CUA 工具
//!  - [`agent`]：Agent 循环（想 → 授权 → 执行 → 回灌结果）
//!  - [`cancel`]：回合取消语义
//!  - [`session`]：多会话持久化（原子写、损坏隔离、时间线）
//!  - [`prompt`]：系统提示词
//!  - [`ui`]：Slint 界面与工作线程
//!  - [`voice`]：语音输入（采集 / 唤醒词 / 端点 / whisper.cpp 转录）与播报
//!
//! 架构取舍见 `docs/13-PR8-多会话与设置架构决策.md`。

pub mod agent;
pub mod cancel;
pub mod clock;
pub mod config;
pub mod context;
pub mod cua;
pub mod llm;
pub mod model_catalog;
pub mod permissions;
pub mod prompt;
pub mod secret;
pub mod session;
pub mod ui;
pub mod voice;

// Slint 生成的组件类型（`AppWindow` / `ChatEntry` / `ToolCard`）注册在 crate 根，
// 这样界面层与测试都能直接引用。
slint::include_modules!();

/// 应用版本（显示在界面与日志里）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
