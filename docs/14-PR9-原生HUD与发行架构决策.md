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
更正：Slint hide 不保证释放 winit 窗口，不能依赖重建实现无焦点。新窗口由 HUD 组件创建调用栈内的线程局部标记选择 inactive 属性（不能等到 show 才设置）；存活 HWND 在 show 期间临时设置 NOACTIVATE，之后恢复原样式。
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

## 当前实现与最终验收记录（2026-11-05）

审查起点为 f8901dd3ded5719784b0265e359374d58f5ac83b，沿原 PR #9 分支继续；本轮开始核对本地/远端为 44dcdd64d06cfe92d162835e4940fa8165efcad2。以下覆盖此前中间候选的验收状态，不把未知发布者提示或启动返回码单独认定为安装成功。

### 本轮必要修复

- 新 HWND 由 HUD 专用调用作用域选择 inactive 创建属性；保留 HWND 的 show 临时增加 NOACTIVATE，随后恢复可点击样式。不靠标题判断，不缓存 HWND。必要 unsafe 仅留在 HUD 原生适配及观察测试。
- 手动、自动和待确认可见性分离；手动 HUD 关闭麦克风和空闲后仍可见，ask 不自动隐藏。坐标缓存不重复 show/置顶；显示器工作区或缩放改变才调整，已显示使用 NOZORDER。
- 实测 150%→100% 曾导致 253×193 窗口裁切，已修复：稳定的 380×290 逻辑首选尺寸交由安全 Slint set_size 调整；原生 SetWindowPos 只定位并加 NOSIZE，避免与 Slint DPI 事件争夺尺寸。
- HUD 复用主窗口已有 AppButton，修复软件渲染下标准主题按钮不可见。允许、拒绝和其他操作现在有明确文字/边框。
- 用户主动打开主窗口时，用安全 winit set_minimized(false)/focus_window 恢复并激活；HUD 自动显示不走此路径。
- 自启核对 Unicode 完整 EXE/参数，关闭及卸载保留其他安装实例的 Run 值；实际注册表回环测试保存并恢复原值。卸载快捷方式/注册信息核对 InstallLocation。
- 最早 main 阶段记录限长的本地启动阶段/错误类别，不保存原始错误、凭据或语音。main 前 DLL 加载失败无法写日志，仍需结合系统错误与退出码。
- 旧配置只填缺失字段，提供商/端点/模型成组保留；压缩比例仅整数10–99，上下文仅整数1–10000000。不猜测默认值是否用户自定义，不迁移 Key，不新增静默覆盖。

### 实际执行的自动与原生窗口检查

CPU release、cargo fmt --check、clippy --all-targets --no-default-features 通过；保留历史警告，不宣称零警告。UI 单测48通过、3个有外部副作用的原生测试默认忽略。Agent29、桥接9、取消4、工具配对5，共47通过；桥接4个真实桌面测试默认忽略。

分别显式执行：HUD 首显和两次保留 HWND 隐藏重显保持前台句柄不变（最终复验前台0x40854）；主窗口最小化恢复并获得焦点；HKCU Run 开启/关闭/其他路径保护并恢复原值。焦点观察测试不经过真实 KWS，也不证明完整语音体验。

导入脚本实测预览不写盘、备份、自定义值保留、密钥排除、源文件哈希不变、重复导入、压缩9/10/99/100/小数边界通过。

### 用户配合的真实图形交互

用户确认托盘菜单齐全、左键恢复主窗口、关闭麦克风后手动 HUD 保持15秒、HUD 操作可点击。用户发现主窗口已显示时无法前置；修复后，遮挡和最小化两种情况下均确认恢复到前台。

隔离配置使用本机127.0.0.1 Mock LLM及假Key，经真实主窗口发送并执行只读 CUA：完成工具结果和正文。用户主动设置 window.read=ask；修复可见按钮后，主窗口隐藏、麦克风关闭，等待15秒确认仍可见，用户点击拒绝；回合3工具3000001为denied、耗时0。随后新请求由用户主动点击允许；回合4工具4000001成功，审计为ask/click/allow。未将早期不可见按钮超时当作拒绝通过。

之后用户自行改用真实提供商/启用语音，不再自动发送测试请求或读取Key。用户手动发送取消验收并点击HUD取消；当前回合7状态cancelled、两个工具均cancelled且耗时0。此项与先前Mock闭环分别记录。

### 实际 DPI 检查

同一运行进程37064，单个2560×1600屏幕，真实系统缩放切换并经打包CUA脚本查询实际窗口边界；每档尺寸与380×290逻辑尺寸相符，按钮完整，位于工作区内：

| 缩放 | DPI | HUD x,y | 宽×高 | 工作区 |
|---|---|---|---|---|
| 100% | 96 | 2164,1246 | 380×290 | 0,0–2560,1552 |
| 150% | 144 | 1966,1069 | 570×435 | 0,0–2560,1528 |
| 200% | 192 | 1768,892 | 760×580 | 0,0–2560,1504 |

已恢复原150%。没有第二块物理显示器，负坐标/热插拔未执行；截图捕获偶尔混入后方设置窗口，用户确认主窗口内容正常，不据此更换渲染器。

### 发行与本机安装证据

CPU包不启用CUDA，GGML_NATIVE/AVX/AVX2/FMA/F16C均OFF，直接导入无CUDA/Node/Python；随包包含CRT、带UTF-8 BOM的cua.ps1、模型说明和许可证。原生归档13个实际链接文件有哈希清单，按上游构建依赖版本收集许可证/NOTICE（含ONNX ThirdPartyNotices），11个原文哈希核对通过；Cargo嵌套LICENSES一起打包。Slint选定条款、MSVC授权及最终发布合规仍须发布者确认，保持发布阻断。

开发主机已实际成功安装、启动、卸载：安装PID38608退出0、payload哈希一致；文件占用时PID37476退出2、不强杀；卸载退出0且Session/模型标记保留。早一轮安装版从C:\Windows、Windows-only PATH独立运行PID8024；真实PowerShell5.1只读list_windows通过并dispose。安装版--background PID14580不显示主窗口，便携--settings转发后同PID打开设置；正常退出安装版后便携独立运行PID30272。卸载保留便携所属Run项和存活进程，测试后恢复原注册表值。机器记录见verification/pr9-local-install.json。这些是开发机验证，不是干净VM。

2026-11-05普通双击向导可打开，但手工目录和默认目录均写入失败，不能认定安装成功。只读核对得到根因：仓库及dist-rust目录/EXE具有继承的Low Mandatory Level，Explorer完整性0x2000、安装器PID38632完整性0x1000，插件临时目录Temp\Low；安装器无法写入正常用户目录。依据微软Mandatory Integrity Control文档，进程取用户与EXE完整性的较低值。未修改ACL/标签、未提权或关闭安全保护。正常构建目录.tmp不带该Low标签，其候选可安装；用户是否手工运行由用户自行决定。最终安装器另修复目录页更改后仍检查旧路径的问题，并保留占用退出码2；最新自动安装PID33040退出0、payload匹配，占用PID6956退出2、卸载0、原有Run保留，见verification/pr9-final-package.json。

当前候选SHA-256：

- setup：11077A098E8D34414CA07624520E4A641D18C3023BB722CEF85F56CDBDD27FFB
- ZIP：D55ECB1ADAC3B4F1073706D746815D9039DC492589314051B03C07C23791B866

### 未执行与发布边界

真实KWS→ASR→Agent→实际TTS字幕/打断完整一轮，用户选择暂缓；干净VM检查用户明确“这项先不做”，已有VM正常关闭、未回滚/清数据。重新登录后的自启、真实音频故障/退出音频资源、多显示器负坐标/热插拔、GPU包缺库场景均未执行。不把Mock或开发机测试当作这些验收。

旧Electron/LX源码、发行入口、模型、Key和用户数据保留；音乐账户/播放器、联网搜索等尚未移植。PR保持草稿，不宣称全部原始Done Criteria已达成，不合并或发布Release。

## 最后一轮时序修复（基线9617fac，2026-11-05）

核对锁定的上游源码：i-slint-backend-winit 1.18.1 的 lib.rs::create_window_adapter 在组件 new 时执行 window_attributes_hook；WinitWindowAdapter::new 将结果缓存为 WinitWindowOrNone::None，show 后才使用该属性创建 HWND。因此仅包围 show 的标记太晚，即使先前无焦点测试通过也不能证明创建属性正确。

新增小型 Safe Rust create()，用可嵌套且 panic 时恢复原值的作用域包围 VoiceHud::new；生产和原生测试共用该入口。显示作用域与已有 HWND 临时 NOACTIVATE 处理保留，不新增 unsafe，不改后端、运行时或权限路径。原生测试断言 new 阶段恰有一次 HUD hook，持续可见重复调用也保持前台不变。

本轮 CPU release、clippy all-targets/no-default-features、fmt及48项UI单测通过（3项交互测试默认ignored，历史警告保留）。显式执行真实桌面焦点测试，首次/持续/隐藏重显三轮前台HWND均为0x40824；用户在记事本输入并确认输入正常，安装版HUD按钮可点击。本项不经过真实KWS。

本轮重新从最新源码打包，安装PID24848退出0，目标卸载注册项匹配、安装EXE与release哈希一致、脚本BOM正确，从C:\Windows和Windows-only PATH启动新隔离配置PID12996。完整退出/卸载结果见 verification/pr9-last-round.json；旧构建证据仅作为历史记录，不作为本次候选结果。

- 安装器 SHA-256：A5D8883CD56484E448D65FE4731F5EBBB8FAE4175C355EFB9B78ADDDB9136297
- ZIP SHA-256：8B89695A7F44598A96C7A7020BBADDC9B3371865025A011677CF434E27EEB06A

真实语音全链路、干净VM、多物理显示器和正式发行许可验收仍未执行/未完成，保持草稿，不合并或发布。本轮不再扩大修复范围。

本轮卸载PID11804退出0，测试EXE和卸载注册项移除，原有用户数据5个文件哈希不变，隔离Session/模型标记保留，自启Run值不变，退出后子进程0。测试前备份的原有安装登记和快捷方式已恢复。打包脚本真实只读CUA调用通过。数据、进程和SHA记录均对应本轮构建。

## 最小收尾（基线74af9e0，2026-11-05）

仅修复两个遗漏：自动HUD在capturing/transcribing期间也持续刷新空闲计时；现有Agent/ask/TTS、手动可见性和关麦语义保留。确定性测试模拟10秒采集、10秒识别，确认全程不隐藏，结束或失败回到空闲后仍按4秒规则收起；不更改窗口/焦点/DPI实现。

安装器在WriteUninstaller之前ClearErrors，之后检查错误标记、文件存在、可读MZ头及长度；任一失败即中止，不继续写卸载登记。用Uninstall.exe同名目录实际阻止生成，PID8904退出1且诊断uninstaller-failed，原有登记未覆盖。正常安装PID33612退出0，卸载程序54104字节、登记路径匹配，实际卸载PID28972退出0，测试EXE和登记移除。测试前原安装登记/快捷方式已备份并恢复。未改安装路径/占用检查/数据删除规则。

本轮fmt、clippy all-targets/no-default-features、CPU release、50项UI单测通过（3项交互测试默认ignored，历史警告保留），重新打包成功。真实长语音未执行：沿用用户此前暂缓，不自动开启麦克风或调用真实账户。未机械重复焦点/DPI/完整语音权限验收；干净VM、多屏、正式许可边界不变。证据见verification/pr9-minimal-closeout.json。

本轮安装器SHA-256：6A9FDDAE78DF73C8D9B878EB8DA9A2DBC15191411126971CFB5640F292997C42；ZIP：96F747C30D595C6D02F822441DCF54A175C7A93B2728EF28409029C7689850F0。此前候选哈希为历史证据。
