//! Computer Use 语音助手 —— Rust + Slint 原生桌面程序（PR5）。
//!
//! 本 crate 只实现**第一个纵向切片**：文本指令 → LLM Agent → 权限判断 →
//! Windows Computer Use（PowerShell + UIA）→ 界面展示结果。
//! 语音（唤醒/ASR/TTS）、完整设置、HUD、音乐模块都不在本阶段范围。
//!
//! 模块边界（刻意保持扁平，避免为架构而架构）：
//!  - [`config`]：独立用户数据目录、provider 档案、默认值
//!  - [`secret`]：DPAPI 密钥存储（绝不明文落盘）
//!  - [`llm`]：OpenAI 兼容 Chat Completions（流式、工具调用、reasoning 分离）
//!  - [`context`]：token 预算与压缩
//!  - [`permissions`]：工具执行前的授权判断
//!  - [`cua`]：PowerShell 后端桥接 + 12 个 CUA 工具
//!  - [`agent`]：Agent 循环（想 → 授权 → 执行 → 回灌结果）
//!  - [`cancel`]：回合取消语义
//!  - [`prompt`]：系统提示词
//!  - [`ui`]：Slint 界面与工作线程
//!  - [`voice`]：语音输入（采集 / 唤醒词 / 端点 / whisper.cpp 转录）

pub mod agent;
pub mod cancel;
pub mod config;
pub mod context;
pub mod cua;
pub mod llm;
pub mod permissions;
pub mod prompt;
pub mod secret;
pub mod ui;
pub mod voice;

// Slint 生成的组件类型（`AppWindow` / `ChatEntry` / `ToolCard`）注册在 crate 根，
// 这样界面层与测试都能直接引用。
slint::include_modules!();

/// 应用版本（显示在界面与日志里）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
