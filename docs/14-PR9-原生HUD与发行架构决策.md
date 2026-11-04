# PR9 原生 HUD 与发行架构决策

## 状态与范围

以 origin/main cafd6a8（合并 PR8）为基线。旧 Electron 的代码、发行入口和数据不修改。
此文记录实现约束，验证结论另列，不能以编译通过代表干净环境发布验收。

## 单一执行路径

Agent、Session、VoiceRuntime、SpeechRuntime 和 UiConfirmer 仍仅在原 worker 中存在。
原生托盘调用主窗口现有回调。HUD 从已经经过归属过滤的主窗口投影读取状态；
唤醒代次只在 worker 接受当前实例 Woke 后递增。确认按钮携带显示时的 request_id，
直接进入原 UiConfirmer，重复或过期答复由原有单次结算规则拒绝。

## 窗口与 Safe Rust 边界

使用 Slint 1.18.1 的 SystemTrayIcon、原生 Window、软件渲染器和显式退出事件循环。
不引入 WebView、角色视觉或第二套音频运行时。
winit 0.30.13 的 WindowFlags::apply_diff 对后续显示使用 SW_SHOW；
with_active(false) 仅保护初次显示，不能独立满足持续无焦点唤醒。
因此允许仅在 HUD 的当前 UI 线程、借用中的 HWND 上使用 windows crate 的必要 unsafe 调用：
用前台窗口所属显示器的 rcWork
定位，失败回退主显示器。不设置永久 NOACTIVATE 或穿透样式，按钮允许用户主动点击。
更正：Slint hide 不保证释放 winit 窗口，不能依赖重建实现无焦点。新窗口由 HUD 显示调用栈内的线程局部标记选择 inactive 属性；存活 HWND 在 show 期间临时设置 NOACTIVATE，之后恢复原样式。
已显示时不重复 show，以免进入 winit 后续 SW_SHOW 路径。
替代方案是直接用 Win32 绘制窗口或永久禁止激活；前者重复窗口系统，后者影响交互，均不采用。
不缓存 HWND；每次通过 Slint/winit 借用取得，窗口重建后重新获取。
指针仅用于同步系统调用，MONITORINFO 使用正确 cbSize 和可写栈对象。
不修改主窗口样式。必须用真实前台输入场景检查重建后的焦点与按钮行为。

## 常驻与退出

窗口 X 仅隐藏。退出仍经 worker 停播、关闭采集、拒绝确认、取消回合和 dispose CUA，最后 quit。
单实例使用当前用户登录会话目录内的操作系统文件锁；异常退出由系统释放锁。
本地小文件邮箱仅传 open/settings，不承载指令、密钥或授权，不监听网络端口。

## 发布边界

CPU 通用发行采用 --no-default-features，GPU 构建不能据此宣称缺库可启动。
不下载模型，不迁移 Electron safeStorage 密文，不触碰旧安装或旧数据。
任何未实测的 GPU、DPI、麦克风和干净环境项目必须标为未执行。

## 本次实际验证（2026-11-04，Windows x64）

- 基线 origin/main cafd6a8；修改在 codex/pr9-native-hud。旧 Electron 文件未修改。
- CPU `cargo check`、`cargo fmt --check`、`cargo clippy --no-default-features --all-targets`、release 构建通过。clippy 有历史 dead_code/too_many_arguments 等警告，不宣称零警告。默认 CUDA 构建未执行。
- `cargo test --no-default-features --lib ui::`：44 通过（包括 3 项 HUD 自动打开条件测试、既有旧实例/确认 id/语音否定/Session 测试）。HUD 自动打开条件测试不能证明 Win32 焦点或真实字幕体验。
- `agent_loop` 29、`bridge` 9、`cancel_truthfulness` 4、`tool_pairing` 5：共 47 通过；bridge 3 项真实桌面测试保持 ignored。
- `packaging/verify-import.ps1`：预览不写盘、目标备份、自定义字段保留、排除源 Key、源文件不变及重复导入通过。目标已有字段即使为默认值也不覆盖；提供商/端点/模型作为一组保留，避免拼接出错配账户。
- 使用 computer-use 技能，在隔离 CUAVA_DATA_DIR、未配置 Key/未开启麦克风的真实图形会话中观察主窗口；点击 X 后窗口消失，PID 31972 保留；二次启动恢复同一 PID/窗口；点击程序退出后无 cuava 进程。此时没有启动 CUA，因此不冒充 CUA 子进程退出实测。
- 实际生成 CPU 安装器和便携 ZIP（dist-rust）。dumpbin 直接导入检查包含 Windows 系统库及 MSVC CRT，无 CUDA/cuBLAS/cuDNN/Node/Python 导入；CRT 随包复制。Whisper CMakeCache 已核对 GGML_NATIVE/AVX/AVX2/FMA/F16C 全部 OFF。exe 已嵌入项目中性图标及 0.1.0 版本资源。
- 本机安装/便携再次启动时出现 Windows「未知发布者」文件安全警告，用户反馈三次均点运行。检查过程被中断，最终没有观察到目标安装目录或存活进程，不能把启动返回码视为安装成功，也没有证据据此认定程序崩溃。没有关闭安全功能或伪造签名。后续不重复弹提示，安装验收仍待重做。

最终候选 SHA-256：

- ZIP：982793A10DA20EDE1A13DAD626433BDF4A7A4686140EB906FB71B511CA809A93
- 安装器：93FC8E194BA070397C77682AB2FBC7011BFF6294A0CFAAFE14FA89767B9650F4

## 未执行及合并前验收项

以下不是通过项，本 PR 应保持草稿，不能发布 Release 或宣称全部 Done Criteria 完成：

1. 真机 KWS、麦克风持续监听、HUD 反复唤醒不夺焦点、HUD 确认可点击、真实 ASR/工具/播放字幕、连续唤醒/打断/退出音频设备检查：未执行。
2. 100/150/200% DPI、多显示器负坐标、热插拔和缩放变化：未执行。代码使用物理 rcWork、缩放边距和区域裁剪，并在可见时重新定位，但必须实测。
3. 完整托盘菜单点击、实际麦克风错误状态、登录启动注册/取消与重新登录：未执行。主窗口关闭/恢复/退出的图形检查不替代这些路径。
4. 无开发工具链的干净 Windows 10/11 安装、任意工作目录运行、文本 Agent/只读 CUA、安装升级与卸载保留数据：未执行；当前没有经过验收的干净 VM，开发机的依赖导入检查不能替代。
5. GPU 可选包、无 GPU/缺库场景：未执行，本 PR 只生成 CPU 候选，不提供经过验证的 CUDA 安装包。
6. 原生 sherpa 预编译归档内全部第三方许可证/NOTICE 最终复核：未完成；已随包列出审核边界和 Cargo 可获得的许可证。未签名。

## 维护约束

Slint/winit 的 HWND 生命周期或显示实现升级后必须重新验证焦点。winit 的安全显示器接口未提供 Windows 可用工作区，故保留仅 HUD 的窗口定位原生调用；不缓存句柄、不设置全窗口穿透，也不将 unsafe 扩散到业务层。
打包脚本不会自动下载模型或修改旧数据。启动项只写 HKCU，卸载保留 AppData/cuava 和旧 Electron 全部数据。


## f8901dd 审查后的修复与验证

开始时本地与远端 PR #9 均为 f8901dd3ded5719784b0265e359374d58f5ac83b，继续原分支，不新建 PR。

移除 attributes.title 判断，以 HUD 专用调用作用域标记识别新建窗口。已有窗口仅在 Slint show 期间临时增加 NOACTIVATE，借用结束重新取得 HWND 恢复样式；不保留句柄或永久禁止点击。新增 unsafe 仅限 HUD 原生适配和该模块的观察测试，其他运行时无扩展。
位置检查缓存坐标/尺寸/缩放，只有几何变化才 SetWindowPos；已显示窗口使用 NOZORDER，不重复置顶。手动可见独立于麦克风/空闲，ask 强制可见，自动唤醒仍可超时收起。

登录启动核对当前 EXE 完整命令，Unicode 注册表读取通过 PowerShell UTF-8 输出；显式开启可转移到当前版本，关闭不删除别的版本启动项。卸载仅删除与本路径匹配的 Run 值；卸载注册信息及快捷方式也核对 InstallLocation。安装升级不自动打开自启。
main 入口及失败类别追加至 LOCALAPPDATA/cuava/startup.log（限长），不保存错误原文/凭据/语音；main 前依赖加载失败不能写日志，需结合进程退出码诊断。
导入压缩比例仅接受整数 10–99，上下文接受整数 1–10000000；默认不覆盖已有值，未新增覆盖模式。已有默认值同样保留，用户应在新设置页手工调整，不能静默推断是否自定义。

实际执行：CPU release、clippy all-targets、fmt、UI 单测（新增手动空闲/ask/自动超时与自启命令绑定）及导入预览/备份/幂等/秘密排除和 9/10/99/100/小数边界检查。另运行显式 ignored 原生窗口测试；在记事本为前台时，首次和两次隐藏重显后 HWND 均保持 0x40f4a，真实 Slint/Win32 窗口测试通过。该测试不经过 KWS/Agent，不验证用户点击确认或真实字幕；不能当作 Done Criteria 0 完成。

本机安装检查：本轮候选使用 /S /D=D:\Projects\computeruse-voice-assistant\.tmp\pr9-repair-install，记录安装器 PID 33788、退出码 0，但目标 cuava.exe/cua.ps1 未观察到，未认定成功，也没有证据认定应用崩溃。尚未可靠定位此异常；未修改安全设置，不重复启动制造提示。

仍未执行：真实 KWS/完整语音与按钮交互、100/150/200% DPI 及多屏变化组合、完整托盘/登录重新登录实测、干净 VM 安装/便携/只读 CUA/卸载数据保留。未完成原生预编译库内部全部许可证核验，继续作为发布阻断。保留草稿，不宣称全部验收达成。

本轮最终复验：fmt/check diff 通过，CPU release 与 clippy all-targets 通过（历史警告保留）；UI 单测 48 通过、1 个原生图形测试默认 ignored，该图形测试另已显式运行通过。最终重新生成候选，未再次启动安装器：

- ZIP SHA-256：CBD0C3677BAB6B1CB054B08D775915960880AB2F361939899829C4ABA6DDD047
- 安装器 SHA-256：3274BC3FE90EDF07B444E10C94704AFE2DCF949F3DD4DF0CB36E83614D74ED9C

此前 PID 33788 对应本轮中间候选，不能归为上述最终 SHA 的安装验收。最终候选安装/便携启动验收仍为未执行。
