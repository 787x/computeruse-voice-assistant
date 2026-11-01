# cuava（Rust + Slint 原生桌面程序）

PR5 的纵向功能切片：**文本指令 → LLM Agent → 权限判断 → Windows Computer Use → 界面展示结果**。
这不是 Electron 版本的替代品，语音、完整设置、HUD 与音乐模块都还没进来（见文末"已支持 / 未支持"）。

## 构建与运行

前置条件（只需一次）：

- Windows 10/11 x64
- [Rust 工具链](https://rustup.rs)：`rustup-init.exe -y --profile minimal --default-toolchain stable-x86_64-pc-windows-msvc`
  （`rustfmt` / `clippy` 建议一并安装：`rustup component add rustfmt clippy`）
- MSVC 生成工具（Visual Studio Build Tools 的「使用 C++ 的桌面开发」），Rust 的 MSVC 目标需要它链接
- PowerShell 5.1（Windows 自带，`cua.ps1` 就运行在它上面）

构建并启动：

```powershell
cd rust
cargo build --release
.\target\release\cuava.exe
```

开发期直接 `cargo run` 也可以。**不需要**安装或启动 Electron、WebView、Node 或 Python。

### 配置

首次启动时在界面右上角「设置」里填模型端点、模型名与 API Key：

| 提供商 | 地址 | 说明 |
| --- | --- | --- |
| `mimo` | `https://api.xiaomimimo.com/v1` | 只有思考开关（`thinking`），没有强度档位 |
| `deepseek` | `https://api.deepseek.com/v1` | 只发 `reasoning_effort`，不发 `thinking` |
| `openai-compatible` | 任意 | 一个提供商专属字段都不发 |

- 设置保存在 `%APPDATA%\cuava\settings.json`（**不含任何密钥**），可用 `CUAVA_DATA_DIR` 覆盖目录。
- API Key 用 **Windows DPAPI** 以当前用户身份加密后存在同目录的 `secret.bin`，日志里只出现打码值。
- 旧版 Electron 的 `.config/` 既不会被读取也不会被改写。它的 `enc:` 值由 Chromium
  `safeStorage` 加密，**不是** DPAPI 密文，本程序会明确报错而不是猜测——正式迁移留给后续发布任务。

### 可选环境变量

| 变量 | 用途 |
| --- | --- |
| `CUAVA_DATA_DIR` | 覆盖数据目录（隔离测试用） |
| `CUAVA_CUA_SCRIPT` | 指定 `cua.ps1` 路径（默认自动从仓库里找） |
| `CUAVA_BASE_URL` / `CUAVA_MODEL` / `CUAVA_PROVIDER_HINT` | 临时覆盖端点（指向本地 mock 用） |

## 验证

```powershell
cd rust
cargo test                 # 87 单元 + 22 集成（Mock LLM/SSE）+ 9 桥接故障注入
cargo fmt --check
cargo clippy --all-targets
```

真机只读链路（需要真实 Windows 桌面会话）：

```powershell
cargo test --test bridge -- --ignored --nocapture --test-threads=1
```

它会连真实 `cua.ps1` 列窗口、读界面状态、截图，并打印结果。

> `cua.ps1` 启动时用 `Add-Type` 现场编译 UIA 互操作层，需要可写的临时目录。
> 若 `%TEMP%` 在受限环境里不可写，设置 `TEMP`/`TMP` 指向一个可写目录再跑。

## 代码结构

```
rust/
├── ui/app.slint           界面（纯 Slint，无 WebView）
└── src/
    ├── config.rs          数据目录、provider 档案、默认值
    ├── secret.rs          DPAPI 密钥存储
    ├── llm.rs             OpenAI 兼容 Chat Completions（流式、工具调用、reasoning 分离）
    ├── context.rs         token 预算与压缩（默认 1M / 90% 才压）
    ├── permissions.rs     工具执行前的授权判断
    ├── agent.rs           Agent 循环（想 → 授权 → 执行 → 回灌结果）
    ├── cancel.rs          回合取消语义
    ├── prompt.rs          系统提示词
    ├── cua/
    │   ├── bridge.rs      PowerShell 5.1 NDJSON 桥接（串行、超时换进程）
    │   ├── tools.rs       12 个 CUA 工具 + 参数校验 + 模型面信封
    │   └── invoker.rs     把工具接到 Agent
    └── ui/worker.rs       后台工作线程（tokio）+ 界面状态同步
```

## 已支持

- **文本 Agent 闭环**：一次性与多轮工具调用、工具结果回灌、最终回复。
- **12 个 Computer Use 工具**（与旧版 `CuaToolkit` 默认白名单一致）：
  `request_access` / `list_windows` / `get_app_state` / `screenshot` / `open_application` /
  `left_click` / `type` / `key` / `scroll` / `set_value` / `wait` / `read_clipboard`。
- **保留的 CUA 语义**：`state_id` + `element_index` 必须成对；`frame_id` 与截图栅格坐标；
  截图按真实 media type（`image/png` / `image/jpeg`）交给多模态模型；工具参数在动作前校验。
- **权限**：执行前统一判断；`allow` 不弹确认，`ask` 走界面确认，`deny` 不执行，未知工具拒绝。
  三档预设（standard / conservative / permissive）。
- **提供商协议**：MiMo `thinking` 与 DeepSeek `reasoning_effort` 的映射、`reasoning_content`
  与可见回复分离、SSE 分片工具调用归并、多工具调用配对。
- **上下文**：默认 1M 窗口 / 90% 触发压缩；模型有更小可信窗口时取 min；装不下如实报错。
- **取消**：取消后不再发起新工具，迟到的 LLM / PowerShell 响应被丢弃。
- **密钥安全**：DPAPI 加密、配置文件不含密钥、日志只出现打码值。

## 未支持（后续 PR）

| 能力 | 计划 |
| --- | --- |
| 麦克风、KWS、VAD、`whisper.cpp` ASR | PR6 |
| MiMo-V2.5-TTS、ONNX 本地 TTS、音频播放、字幕、语音打断 | PR7 |
| 完整 HUD / 设置页、音乐模块、定时模型切换、旧数据迁移、安装器 | PR8 |
| 语音确认（`ask` 目前只有点击确认） | PR7 与语音一起 |
| 联网搜索、文件工具、命令执行、音乐控制 | 未排期；默认提示词已如实说明这些能力不存在 |
| 托盘常驻、开机自启、全局快捷键 | 未排期 |

本程序**不能**替代旧版语音助手：它没有语音能力，也不读写旧版配置。
