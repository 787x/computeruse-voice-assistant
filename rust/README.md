# cuava（Rust + Slint 原生桌面程序）

PR6 的纵向功能切片：**语音/文本指令 → LLM Agent → 权限判断 → Windows Computer Use → 界面展示结果**。

语音链路完全本地：麦克风（cpal/WASAPI）→ 唤醒词（sherpa-onnx KWS）→ 端点（自适应 VAD）
→ whisper.cpp 转录 → 交给现有 Agent。**不运行 Electron / WebView / Node / Python。**

这不是 Electron 版本的替代品：语音合成、HUD、安装器与音乐模块还没进来（见文末"已支持 / 未支持"）。
架构取舍与实测数据见 [`docs/10-PR6-语音输入架构决策.md`](../docs/10-PR6-语音输入架构决策.md)。

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


**构建前置（语音相关）**

- CMake + Ninja：whisper.cpp 由 `whisper-rs-sys` 现场编译。
- `libclang.dll`：`whisper-rs-sys` 用 bindgen 生成绑定。装过 LLVM 就设
  `LIBCLANG_PATH` 指向它的 `bin`；没装也可以从 PyPI 的 `libclang` wheel 里取
  （解压后指向 `libclang-*.data/platlib/clang/native`）。
- sherpa-onnx 用**静态库**（crate 默认 feature），产物不需要随包分发 DLL。
  预编译包由构建脚本自动下载；直连 GitHub 慢时，用 `SHERPA_ONNX_LIB_DIR`
  指向已解压的 `...static-MD-Release-lib/lib`，或用 `SHERPA_ONNX_ARCHIVE_DIR`
  指向存放离线压缩包的目录。
- **必须用 `-MD-` 那份静态包**。`-MT-` 是静态 CRT，链接时会报
  `LNK2038 RuntimeLibrary 不匹配（MT vs MD）`——Rust 的 MSVC 默认是 `/MD`。
- 仓库路径过长（尤其含中文）时，MSVC 偶发 `cl.exe error D8050 / c1.dll`；
  把仓库放在较短的纯 ASCII 路径下最省事。

### 语音：模型与开关

语音模型不随程序分发，也不自动下载：

1. **唤醒词模型**（必需，约 11 MB）：把
   `sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01` 整个目录放到
   `%APPDATA%\cuava\models\`（开发机也可直接用仓库 `models/`）。
   需要 `encoder/decoder/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx` 与 `tokens.txt`。
2. **唤醒词文件**：首次启用时自动在 `%APPDATA%\cuava\kws-keywords.txt` 生成「小爱同学」
   （`x iǎo ài t óng x ué @小爱同学`）。换词就改这个文件（格式：音素 + `@显示名`）。
3. **Whisper 模型**（必需）：把 ggml 权重放到 `%APPDATA%\cuava\models\ggml-small.bin`。
   推荐 `small`：`tiny` 中文错字明显，`medium/large` 在 CPU 上太慢。
4. 启动程序 → 顶栏点 **「打开麦克风」**。状态条显示设备与采样率、唤醒词、
   Whisper 用 CPU 还是 GPU。
5. 说 **「小爱同学」**，看到「我在听，请说」后说出指令；停顿约 0.7 秒后识别并交给 Agent。

**默认不开麦**：新安装或重启后都要手点一次。语音偏好写进 `settings.json` 的 `voice` 节，
但**启动时不会自动采集**——避免主人不在时一直被收音。

任一模型缺失/不可用时，程序**照常启动**，界面显示「语音不可用」和具体原因，
文本模式完全不受影响。

### 语音：CUDA（可选）

默认构建是 **CPU 基线**，任何 Windows x64 + MSVC 环境都能编译运行。
有 NVIDIA GPU 且装了 CUDA Toolkit 时可显著降低延迟：

```powershell
# 需要 CUDA Toolkit（nvcc + cudart + cublas），并让 CUDA 的 bin 在 PATH 上
cargo build --release --features cuda
```

实测（Ryzen 9 9955HX + RTX 5070 Laptop，`ggml-small`，3.77 秒中文指令）：

| 后端 | 解码耗时 | RTF | 识别 |
| --- | --- | --- | --- |
| CPU 8 线程 | 2335 ms | 0.62 | 正确 |
| CUDA | **155 ms** | **0.041** | 正确 |

- CUDA 版运行时需要 CUDA 运行库（`cudart64_*.dll`、`cublas64_*.dll` 等），
  它们**不在**本仓库里，需要随发布包分发或由用户安装。
- 不装 CUDA Toolkit 也能用：设置里要求 GPU 但构建没带 CUDA 时，
  程序显示「本构建未启用 CUDA」并使用 CPU，不会崩溃。

开发期直接 `cargo run` 也可以。**不需要**安装或启动 Electron、WebView、Node 或 Python。

### 配置

首次启动时在界面右上角「设置」里填模型端点、模型名与 API Key：

| 提供商 | 地址 | 说明 |
| --- | --- | --- |
| `mimo` | `https://api.xiaomimimo.com/v1` | 只有思考开关（`thinking`），没有强度档位 |
| `deepseek` | `https://api.deepseek.com/v1` | 只发 `reasoning_effort`，不发 `thinking` |
| `openai-compatible` | 任意 | 默认不发专属字段；在设置里显式选协议提示后按该协议发 |

- 设置保存在 `%APPDATA%\cuava\settings.json`（**不含任何密钥**），可用 `CUAVA_DATA_DIR` 覆盖目录。
- API Key 用 **Windows DPAPI** 以当前用户身份加密，**按提供商各存一份**：
  `%APPDATA%\cuava\secrets\<provider-id>.bin`。
  - 切换 MiMo ↔ DeepSeek ↔ 自定义端点时，只会用当前提供商自己的 Key；
  - 当前提供商没配 Key 就**不发** Authorization，绝不借用别家的；
  - 设置里「API Key」留空 = 不修改该提供商的 Key；点「清除」才是真的删掉。
- PR5 早期版本只存过一把 Key（`secret.bin`）。首次启动时会把它迁移到 `settings.json`
  里**明确记录的那个提供商**名下；不猜测、不删除原文件。
- 迁移**只做一次**：迁移成功后写 `secrets/.legacy-migrated`（内容是归属的 provider id）。
  之后重启、切换提供商、清除 Key 都不会再从旧文件导入——
  否则用户点了"清除"，重启又被填回来，看起来像没生效。
- 迁移失败**可以重试**：只有 Key 和标记都写成功才算完成；中途失败会撤销已写的 Key
  并保留旧文件，下次启动会重来（归属仍是同一个提供商，不会被挪走）。
  失败时界面上会明确提示。
- 旧的 `secret.bin` 保留作备份，不会自动删除；想彻底清掉可以手动移除它。
- 旧版 Electron 的 `.config/` 既不会被读取也不会被改写。它的 `enc:` 值由 Chromium
  `safeStorage` 加密，**不是** DPAPI 密文，本程序会明确报错而不是猜测——正式迁移留给后续发布任务。
- 设置保存失败（地址非法、写盘失败、Key 写入失败）会如实报错，**不会**显示"已保存"，
  也不会覆盖当前有效的配置。
- 保存顺序是**先写配置、再写 Key**：配置写失败时 Key 根本没被动过，
  用户原来那把 Key 仍然可用（旧实现"先覆盖 Key 再回滚"会把原 Key 一起删掉）。
- 配置文件与密钥文件都用"临时文件 → 备份 → 上位"替换，失败时还原，
  不会因为改名失败把唯一有效副本弄丢。
- `settings.json` **存在但读不出来**（损坏、字段类型不对）时，程序会停在这一步：
  界面显示具体错误、不使用默认配置、不迁移旧 Key，也不会覆盖原文件。
  修复或删除该文件后重启即可。

### 可选环境变量

| 变量 | 用途 |
| --- | --- |
| `CUAVA_DATA_DIR` | 覆盖数据目录（隔离测试用） |
| `CUAVA_CUA_SCRIPT` | 指定 `cua.ps1` 路径（默认自动从仓库里找） |
| `CUAVA_BASE_URL` / `CUAVA_MODEL` / `CUAVA_PROVIDER_HINT` | 启动时覆盖端点（指向本地 mock 用）；非法值会被忽略 |

## 验证

```powershell
cd rust
cargo test                 # 单元 + 集成（Mock LLM/SSE、配置与密钥、取消真实性、桥接故障注入）
cargo fmt --check
cargo clippy --all-targets
```

语音链路的真实模型测试（需要模型文件；缺模型时会**明确跳过**而不是假装通过）：

```powershell
cargo test --test voice_e2e -- --nocapture --test-threads=1
```

它验证三件事：真实 KWS 能命中「小爱同学」且不误触发其它语音；
VAD 能在真实录音的静音处端点；whisper.cpp 能把中文语音转录成可提交文本。
逐句给结果与耗时。

与 faster-whisper 的对照（**开发实验，不是产品依赖**）：

```powershell
cargo build --release --example whisper_bench
node tools/bench-whisper.mjs --wavs assets/test/asr-zh.wav `
  --cpp-bin target/release/examples/whisper_bench.exe `
  --model .tmp/whisper-models/ggml-small.bin --fw-model small --language zh --threads 8
```

faster-whisper 需要本机 Python + `faster_whisper`；没有就如实报告「未执行」。

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
    ├── voice/             语音输入（PR6）
    │   ├── pcm.rs         下混、整型归一化、加窗 sinc 重采样、电平
    │   ├── capture.rs     cpal/WASAPI 采集 + 有界队列（回调里不做推理）
    │   ├── kws.rs         sherpa-onnx 唤醒词（喂原始 PCM、开口前重置）
    │   ├── vad.rs         噪声底跟踪 + 迟滞端点状态机
    │   ├── asr.rs         whisper.cpp 常驻模型 + 独立工作线程
    │   ├── settings.rs    语音配置与模型路径解析
    │   └── coordinator.rs 会话状态机（Off/Listening/Capturing/Transcribing/Error）
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
  历史里会区分三种情况：已完成的保留真实结果；已发出但没拿到结果的标为
  **状态未知**（并提醒模型不要盲目重试，因为底层操作可能已经生效）；
  还没轮到的明确记为未执行。下一轮请求的工具消息配对始终完整。
- **密钥安全**：DPAPI 加密、配置文件不含密钥、日志只出现打码值。
- **语音输入（PR6）**：本地唤醒词 + 端点 + whisper.cpp 转录，全部在进程内完成。
  - 唤醒：sherpa-onnx Zipformer，默认「小爱同学」，喂**未经增益**的原始 16k PCM；
    开口前与长期静音时重置关键词流（旧版实测的漏检根因）。
  - 端点：自适应噪声底 + 迟滞；最短语音 250 ms、静音收尾 700 ms、单句上限 15 s、
    无声超时 8 s；带 250 ms 预滚与 200 ms 尾音，避免截掉首字/尾音。
  - 采集：任意设备采样率/声道/位深 → 16 kHz 单声道 f32，加窗 sinc 抗混叠重采样；
    回调里只做格式转换与下混，超时/溢出计数并**丢弃整句**而不是阻塞麦克风。
  - 转录：模型进程内常驻、会话间复用，不启动 `whisper-cli`；推理在独立线程。
  - 只有 **Final** 文本会进入 Agent；空转录、噪声幻觉（如「谢谢观看」）、
    过短语音、超时与取消的旧会话**都不会触发工具**。
  - 语音文本与文本输入走**同一个** `Send` 入口：Agent 忙 / 等待点击确认时
    **不启动第二回合**，也不会排队延后执行；语音**不能**代替点击授权。
  - 音频不落盘、不上传；`settings.json` 里没有任何音频字段。

## 未支持（后续 PR）

| 能力 | 计划 |
| --- | --- |
| MiMo-V2.5-TTS、ONNX 本地 TTS、音频播放、字幕、语音打断 | PR7 |
| 完整 HUD / 设置页、音乐模块、定时模型切换、旧数据迁移、安装器 | PR8 |
| 语音确认（`ask` 目前只有点击确认） | PR7 与语音一起 |
| 联网搜索、文件工具、命令执行、音乐控制 | 未排期；默认提示词已如实说明这些能力不存在 |
| 托盘常驻、开机自启、全局快捷键 | 未排期 |

### PR6 的已知限制

- **Whisper 是分段模型**：没有边说边出字，只有「说完 → 端点 → 识别」。
  UI 不展示预览字幕，也不会把预览送进 Agent。
- **中文简繁**：偶尔输出繁体（提示词能压低概率）。这里**不做**繁转简后处理，
  因为那会误改专有名词与产品名。
- **真人唤醒录音**：仓库里只有 TTS 合成的「小爱同学」样本；
  `assets/test/wake-word.wav` 实测并不含该唤醒词，不能用作正向用例。
  真机唤醒需要用户自己录音验证。
- **性能**：CPU 基线下 `small` 单句约 2.3 秒（8 线程），日常短指令会感到停顿；
  要快就用 CUDA 构建。

本程序**不能**替代旧版语音助手：没有语音合成/HUD/安装器，也不读写旧版配置。
