# PR16 原生语音 HUD 与托盘入口

## 决策与交互

基线 main ea79085（已合并 PR15）。托盘左键先用原有 Placement 显示无焦点 HUD，再发送 StartManualFromTray。右键打开主窗口、设置、麦克风偏好、停止播报和退出保持原路由。第二实例与快捷方式不改。

StartManualFromTray 是只启动的命令，不复用 DictateNow 的取消分支：Manual 已武装、已有语音采集/转录、Followup 或审批存在时只维持显示；Agent 忙时由原 wait_turn 串行循环拒绝启动。HUD 另有只取消的 CancelRecognition，避免按钮排队后反向开启采集。播报结束后手动输入沿用原 speech.stop；忙碌 Agent 仍需明确取消任务。识别结果只走 VoiceRoute 一次性消费和原 Agent 入口。

临时启麦沿用 manual_temporary，不保存配置。Final、Rejected、错误和取消结束临时采集；原常驻监听不因此改偏好。模型缺失的具体原因进入 HUD 状态，设置按钮明确前置主窗口。

## 内容归属与性能

复用 Session 已有时间线发布，投影最后一个回合的 reasoning、正文与 call_id 工具实例；没有第二个 Agent、音频引擎或确认器。100ms UI 定时器只同步 SharedString 与 ModelRc，不复制完整历史。现有时间线发布自身仍按原设计重建主窗口历史，本 PR 未重构该机制。

思考来自真实 ReasoningDelta/Done 持久化结果，不进入正文或 TTS。正文保留全部可见增量，字幕仍只来自 SpeechView 接受的 SpeechId；新 SpeechId 清空旧字幕。切会话清理 ASR/字幕并投影选中会话的最后回合。工具默认只展示实例、真实状态、耗时和结果字符数，完整输出留在主窗口，避免浮窗直接暴露参数、截图或凭据。

Followup 显示原 VoiceRuntime 截止时间，每 250ms 更新；不增加另一套计时或准入机制。审批共用 UiConfirmer request_id，即使 HUD 100ms 投影尚未刷新，旧按钮也不能批准下一请求。

## 窗口与替代方案

沿用 PR9 原生无激活显示，不改 HWND 安全边界，不新增 unsafe。尺寸随可见内容收缩，上限 680 逻辑像素，再由原 Placement 限制为当前工作区。Placement 缓存加入内容宽高，防止同一屏幕上布局变化被忽略。长内容滚动，审批和主要操作固定在滚动区之外。思考默认最近两行，展开区最多 220px，可滚动，收起移除全文高度。

不选择固定大窗口或自动激活主窗口：前者浪费空间，后者破坏后台语音使用。隐藏不取消任务或麦克风；审批强制可见，其他主动隐藏可保持隐藏直到下一次唤醒/托盘入口。托盘显示使用空闲自动收起，菜单手动显示则保持显示。

## 验证记录（2026-11-07）

- 使用用户环境中的 CUDA 13.4、MSVC、LIBCLANG 和本地 sherpa static MD 库，默认 CUDA Windows build 通过；未运行 CPU-only 构建。
- cargo fmt --check、cargo clippy --tests 通过，保留既有未使用字段、参数数量、枚举大小等警告。
- cargo test --lib -- --skip missing_model_message_is_actionable：488 通过，4 忽略，1 筛除。筛除沿用 PR15 原因：本机存在 Kokoro 模型，缺模型测试前提不成立。
- cargo test --test agent_loop：29 通过，包括请求协议、工具配对、权限拒绝、取消及重复审批。
- 最后调整后 cargo test --lib pr16：3 通过、1 GUI 夹具忽略；fmt、clippy 和默认 CUDA build 再次通过。
- 新增托盘准入幂等、Manual Final 一次性消费/临时采集结束、当前回合独立投影/同名工具实例/参数不进入 HUD 测试；原 Followup、增量播报、旧实例、SpeechId、否定优先回归继续通过。
- Windows 原生 native_first_and_retained_show_preserve_foreground 实测通过：首显、重复 show、hide 后再显示三轮前台 HWND 不变。
- Windows GUI 长内容夹具实际点击展开思考，滚动长回复：文字可读，审批按钮与底部操作持续可见。夹具不是实际模型/音频验收。
- 隔离配置启动 --background 实例成功；自动化提供的窗口清单未暴露通知区域，未能完成真实托盘单击/双击操作。隔离实例已停止，不触碰用户配置/Key。

尚未完成：真实麦克风/KWS/手动 ASR → Agent → 实体扬声器 → Followup/语音审批完整回合；实际通知区单双击；多屏负坐标及 150%/200% DPI；真实模型/声卡缺失的完整 GUI 操作。现有 Mock 与窗口测试不能替代这些项目。PR 保持草稿，不能宣称全部 Done Criteria 实机通过；不合并、不发 Release。
