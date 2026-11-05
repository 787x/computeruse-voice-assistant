# CUAVA Rust CPU 发行候选

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
- Whisper：选择本地 ggml 格式模型文件，例如 ggml-base.bin；CPU 包只用 CPU。模型越大延迟/内存越高。
- ONNX TTS：选择 Kokoro 多语言模型目录，包含 model.onnx、voices.bin、tokens.txt、espeak-ng-data 和对应语言词典；具体文件以设置错误提示及 rust/README.md 的 Kokoro 部署说明为准。
- MiMo：在 MiMo 提供商下重新输入 Key，选择云端播报。只有显式开启后才发送助手可见正文合成。

模型从原作者发行处取得，下载前自行确认许可、语言、体积与官方 SHA-256。
程序不会猜测模型文件或许可。此包没有 GPU 加速库；CUDA 版缺 DLL 时可能在入口前失败，不能视为自动 CPU 回退。当前不提供经过发布验收的 GPU 包。

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

## 干净 Windows 10/11 x64 验收清单

在没有 Rust、Node、Python、Electron/WebView、CUDA Toolkit、源码和 Cargo 缓存的 VM：
安装 CPU 包，从任意工作目录启动，配置测试账户后使用文本 Agent 和只读 CUA；
确认 exe 旁 cua.ps1 正常、TEMP 可写、缺模型/无麦克风仍能打开设置。
自愿部署模型后在真机验证 KWS、隐藏主窗口后 HUD 不抢前台输入焦点、确认按钮、字幕和停止播报。
在 100/150/200% DPI、多屏负坐标、拔插显示器后重复唤醒，确认不覆盖任务栏。
退出后检查无音频输出和 PowerShell 子进程；重启/二次启动/登录启动关闭后再登录均须验证。
最后卸载，确认 Rust 用户数据及旧 Electron 数据仍保留。构建通过不能替代上述验收。

## 可复现打包（维护者）

在 x64 MSVC Developer PowerShell，准备 CMake/Ninja、LIBCLANG_PATH 和 sherpa-onnx 静态 MD 包。
执行 `./rust/packaging/build-cpu.ps1`；该脚本关闭 Whisper 主机专用指令集，并清除该依赖的 release 缓存，避免旧 CUDA/native 构建混入。
然后执行 `./rust/packaging/package.ps1 -CpuExe C:/cuava-cpu-target/release/cuava.exe -MsvcRedist <VS许可Redist目录/x64/Microsoft.VC145.CRT> -MakeNsis <makensis.exe>`。
输出到 dist-rust，打印 ZIP/安装器 SHA-256。工具链仅维护者构建需要，用户不需要。

安装诊断记录在 `%TEMP%\cuava-install-diagnostic.ini`，含本次 PID、目标目录和阶段；应同时核对安装进程退出码、目标文件及 EXE 哈希，不能仅根据退出码或旧日志判定成功。目标 EXE 被占用时安装器返回非零并提示先正常退出，不强杀进程。Slint 托盘使用编译内嵌图标，确保后台首次创建时图标已经可用。
