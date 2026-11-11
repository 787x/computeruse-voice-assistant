# PR10：Whisper CUDA 发行与运行时回退

正式发行固定使用 CUDA Toolkit 13.4.1 构建 Whisper，打包入口在新建的 Cargo target 内执行锁定依赖的默认 feature 构建，不接受外部 EXE。这样可避免历史旧构建的 CMake 缓存或同名程序混入；构建失败即停止。开发测试与正式发行都使用默认 CUDA 构建；CPU 对照应在同一构建中关闭 GPU 推理。

对实际 PE 导入递归扫描，只从 NVIDIA 官方 redist 收集需要的 DLL，并要求它们与构建 Toolkit 的对应文件哈希一致；不复制驱动提供的 nvcuda.dll。随包记录版本、来源、依赖与哈希并保留许可。与打包整个 Toolkit 相比，此方案减少分发内容，并避免按猜测的 DLL 后缀选择文件。CUDA Toolkit 是构建要求，运行时只需对应运行库与兼容驱动；动态加载阶段缺库时进程可能在 Rust 启动前失败，不能依靠应用内 CPU 回退。

正常加载之后，用户关闭 GPU 或 CUDA 初始化失败时，由同一构建内的 CPU 后端处理，并报告实际后端和原因。Whisper CUDA 与 ONNX TTS 的实验性 CUDA feature 独立，发行不启用后者。将来升级 Toolkit 时应同时更新固定版本、核查官方 redist 许可和哈希、重新扫描导入并完成真实 GPU、CPU、安装及卸载验收；不能因文件存在或编译成功推定推理成功。

## 2026-11-05 本机针对性验证

- 独立 CUDA release 构建：CUDA Toolkit 13.4.1，驱动 616.92，NVIDIA GeForce RTX 5070 Laptop GPU。Whisper small 模型 SHA-256 `1BE3A9B2063867B937E64E2EC7483364A79917E157FA98C5D94B5C1FFFEA987B`，输入 `assets/test/asr-zh.wav` SHA-256 `20437A960D242EA28B031B85E43A0FA2C00DF517D45C64B28852E7C786A6D6AE`。
- `pr10_real_whisper` 在同一个 CUDA 构建的测试程序中分别通过 `use_gpu=true` 和 `false` 加载真实模型并转录。前者日志报告 `using CUDA0 backend` 和 `Whisper CUDA`（407 ms）；后者报告 `Whisper CPU`（32316 ms，测试期间与其他构建并发）。两次识别为“帮我把浏览器倒开,然后搜索今天的天气。”，包含识别错误。
- 这证明同一构建的 ASR 路径，不等于已经验证安装包 EXE 的 UI、安装或卸载。最终候选仍须逐项验收。
- 对初步 CUDA EXE（SHA-256 `3AAE7C2DD04A357C1F8EB8A23EB89F15795EAC2B6AA1056019495838F118F3B4`）使用隔离配置目录启动，设置同一模型及 KWS 模型并打开真实麦克风；设置页显示 `Whisper CUDA`。退出后将隔离配置 `voice.useGpu` 设为 false，以同一个 EXE 重启并打开麦克风，设置页显示 `Whisper CPU`。这两次界面检查尚未证明发行包 EXE 的识别结果。
- 正式脚本生成候选 EXE SHA-256 `CDBCDBA76B30CA00D8209BCB780B05C8B12F60B9039FAC9EBD5D6313DF600286`；其 CMake 缓存中 CUDA=ON，native/AVX/AVX2=OFF。该 EXE 从非源码目录、使用随包 DLL 和隔离数据启动。以已安装 VB-Audio 虚拟音频线输入仓库合成唤醒词与 `asr-zh.wav`，默认 GPU 及 `voice.useGpu=false` 两次均识别为“帮我把浏览器倒开,然后搜索今天的天气。”；设置页对应显示 `Whisper CUDA` 和 `Whisper CPU`。样本识别存在错字；未配置服务商密钥，未执行识别出的指令。
- ZIP SHA-256 `95E58002EB7BD7F8888760C492A03D2561318240863BFA734DC77A7B4C569269`；安装器 SHA-256 `B335BD68F8941217DA707E2103FAF55CA0B5F417594411E4EC11B99079771DBD`。对 ZIP 中 EXE、CUA 脚本、两个 CUDA DLL 与清单逐项核对 SHA-256，一致。自动化安装及卸载尝试曾被审批拒绝；随后用户确认验收已通过，具体实测过程未在本会话中记录。
- 将 ZIP 解压至独立目录，在 PATH 只含 Windows 系统目录、工作目录为 `D:\` 且用户数据隔离时启动，主窗口出现并正常退出。另把 EXE 单独复制到没有 CUDA DLL 的目录并限制 PATH，Windows 显示缺少 `cublas64_13.dll`、无法继续执行；程序未进入 main，也没有显示 CPU 回退。测试错误提示已关闭。
