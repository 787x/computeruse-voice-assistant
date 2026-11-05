# CUAVA Rust CUDA 发行候选

本目录为原生 Slint 版本，无 Electron/WebView、Node、Python、Rust 或 CUDA Toolkit 运行要求。
当前为未签名候选，系统可能提示未知发布者。不要关闭安全功能；正式使用前核对来源和 SHA-256。
安装器默认安装到当前用户 LocalAppData/Programs/CUAVA-Rust；便携包解压后运行 cuava.exe。
升级前通过托盘「退出」关闭 Rust 程序，再安装；不会强杀任何进程。
卸载默认保留 AppData/Roaming/cuava（Session、模型、DPAPI Key），不触碰旧 Electron 数据。

普通启动显示主窗口，--background 仅托盘，--settings 用于唤起已有实例设置。
主窗口 X 隐藏至托盘。托盘可打开主窗口/设置、启停麦克风、停止播报和退出。
登录启动默认关闭，通过托盘显式开启，传入 --background，不会自动开麦。
首次启动无模型、Key、麦克风也可进入主窗口；文本 Agent 需配置提供商/端点/模型/Key。
麦克风需要用户每次主动开启；若旧 Electron 也开麦，两者可能竞争设备，须用户自行关闭其中一个监听。

## 模型（不随包提供，不默认联网下载）

在设置的语音输入/播报页面选择资源路径，不依赖工作目录。

- KWS：sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01 目录；需要 encoder/decoder/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx 与 tokens.txt。建议放 AppData/Roaming/cuava/models。唤醒词文件 kws-keywords.txt 首次开启时生成，保存后按设置提示重新加载。
- Whisper：选择本地 ggml 格式模型文件，例如 ggml-base.bin；默认使用 CUDA；设置关闭 GPU 或初始化失败时在同一构建中回退 CPU。模型越大延迟/内存越高。
- ONNX TTS：选择 Kokoro 多语言模型目录，包含 model.onnx、voices.bin、tokens.txt、espeak-ng-data 和对应语言词典；具体文件以设置错误提示及 rust/README.md 的 Kokoro 部署说明为准。
- MiMo：在 MiMo 提供商下重新输入 Key，选择云端播报。只有显式开启后才发送助手可见正文合成。

模型从原作者发行处取得，下载前自行确认许可、语言、体积与官方 SHA-256。
程序不会猜测模型文件或许可。需要兼容 NVIDIA GPU、驱动及随包 CUDA 运行库；加载期 DLL 缺失可能使进程无法启动，Rust 内部 CPU 回退无法处理这种情况。CPU 回退仅适用于进程成功加载后的 GPU 初始化失败或关闭 GPU。Whisper CUDA 与实验性 ONNX TTS CUDA 是独立能力，本发行未启用后者。

## 旧版导入

旧安装、配置与密钥独立保留。退出 Rust 程序，在 PowerShell 执行：

    powershell -NoProfile -File .\import-legacy.ps1 -Source D:\旧版\.config\agent.json

预览列出可映射的非密钥字段；系统提示词只列长度，正文请在源文件核对。核对源文件后，添加 -Apply 并输入 IMPORT。
工具仅填目标缺失字段；已有字段（包括默认值）也保留。存在目标时先备份。
enc: safeStorage 与 Rust DPAPI 不兼容，所有 Key 均不导入；按提供商手工重新输入。
模型路径、自定义唤醒词、语音、模型方案/时段须手工配置。重复导入不会覆盖已有字段。

| 功能 | Rust 候选状态 |
| --- | --- |
| 文本 Agent、权限/CUA、多 Session、模型/上下文/思考工具时间线 | 已实现（PR5–8） |
| KWS/Whisper、MiMo/ONNX TTS、语音确认/打断 | 已实现，需资源和实机验证 |
| 提供商 Key、模型、唤醒词、方案/时段 | 需要手动重新配置 |
| 音乐登录/播放器、搜索服务、角色动画/音色、GPT-SoVITS | 尚未移植 |

## CUDA 发行构建与验收

在 x64 MSVC Developer PowerShell 准备 CUDA Toolkit 13.4.1（设置 CUDA_PATH）、CMake、Ninja、LIBCLANG_PATH、sherpa-onnx 静态 MD 库、NSIS、微软 CRT Redist，以及与 Toolkit 相同版本的 NVIDIA 官方 Windows x64 redist 解压目录。`package.ps1` 在全新 Cargo target 中执行默认 feature 的 `cargo build --release --locked`，检查 CMake CUDA 配置和 PE 导入，拒绝使用外部 EXE；它不会启用实验性 `tts-cuda`。

    ./rust/packaging/package.ps1 -CudaRedist <NVIDIA-redist目录> -MsvcRedist <微软CRT-redist目录> -MakeNsis <makensis.exe>

脚本根据 PE 导入递归收集 NVIDIA 运行库，要求其与构建 Toolkit 的文件哈希一致，记录导入、文件哈希及来源。`nvcuda.dll` 由 NVIDIA 驱动提供，不随包复制。输出 ZIP 和安装器均为 CUDA 候选；正式发布前须审查 NVIDIA、MSVC、Slint 的许可。

在兼容 NVIDIA 驱动的 Windows x64 上，从非源码工作目录安装和启动，使用真实 Whisper 模型确认 UI 显示 `Whisper CUDA` 且转录正确；然后关闭 `voice.useGpu`，确认同一 EXE 显示 `Whisper CPU` 且转录正确。退出、卸载并确认用户数据保留。缺少加载期 DLL 会导致启动失败，不可宣称会自动回退。构建通过不能替代实机验收。

安装诊断记录在 `%TEMP%\cuava-install-diagnostic.ini`，含本次 PID、目标目录和阶段；应同时核对安装进程退出码、目标文件及 EXE 哈希，不能仅根据退出码或旧日志判定成功。目标 EXE 被占用时安装器返回非零并提示先正常退出，不强杀进程。Slint 托盘使用编译内嵌图标，确保后台首次创建时图标已经可用。
