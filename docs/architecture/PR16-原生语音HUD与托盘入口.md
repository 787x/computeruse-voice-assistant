# PR16 原生语音 HUD 与托盘入口

## 决策与交互

基线 main ea79085（已合并 PR15）。托盘左键先用原有 Placement 显示无焦点 HUD，再发送 StartManualFromTray。右键打开主窗口、设置、麦克风偏好、停止播报和退出保持原路由。第二实例与快捷方式不改。

StartManualFromTray 是只启动的命令，不复用 DictateNow 的取消分支：Manual 已武装、已有语音采集/转录、Followup 或审批存在时只维持显示；Agent 忙时由原 wait_turn 串行循环拒绝启动。HUD 另有只取消的 CancelRecognition，避免按钮排队后反向开启采集。播报结束后手动输入沿用原 speech.stop；忙碌 Agent 仍需明确取消任务。识别结果只走 VoiceRoute 一次性消费和原 Agent 入口。

临时启麦沿用 manual_temporary，不保存配置。Final、Rejected、错误和取消结束临时采集；原常驻监听不因此改偏好。模型缺失的具体原因进入 HUD 状态，设置按钮明确前置主窗口。

## 内容归属与性能

复用 Session 已有时间线发布，投影最后一个回合的 reasoning、正文与 call_id 工具实例；没有第二个 Agent、音频引擎或确认器。100ms UI 定时器只同步 SharedString 与 ModelRc，不复制完整历史。现有时间线发布自身仍按原设计重建主窗口历史，本 PR 未重构该机制。

思考来自真实 ReasoningDelta/Done 持久化结果，不进入正文或 TTS。正文按 PR15 资格语义区分生成过程和最终回复，字幕仍只来自 SpeechView 接受的 SpeechId；新 SpeechId 清空旧字幕。切会话清理 ASR/字幕并投影选中会话的最后回合。工具默认展示实例、真实状态、耗时及适合直接阅读的短摘要；敏感输出只展示结果信息，完整输出留在主窗口，避免浮窗直接暴露参数、截图或凭据。

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


## 审查修复（2026-11-07）

### 修复与边界

- 最终回复不再拼接整回合 Assistant 时间线。HUD 直接复用 PR15 SpeechEligible 的 (turn, step) 准入；没有资格的实时文字标为“生成过程（尚未确认为最终答复）”。终态以同一 TurnOutcome.speech_text 收口，失败/取消清空已完成答复。没有修改增量 TTS 的资格规则，也不额外调用模型。
- 新手动输入、有效 KWS 唤醒和 Followup 开始时立即清空 HUD 临时内容。SessionRuntime 的轻量发布版本用于拒绝已经排队的旧快照，版本保存在快照自身，避免延迟回调读取新版本后误恢复旧内容。历史仍然保留。
- 旧音频通过 SpeechRuntime 已发放 SpeechId 的上界作废，包含尚未被 UI 接受的事件。只增加只读 ID 接口，不改合成/播放逻辑；作废时同时清字幕、播放状态和活跃标志，避免幽灵字幕与永久播报状态。
- 手动识别绑定启动 Session ID。新建/切换/删除会话取消当前识别，临时采集关闭，常驻采集恢复 Wake；最终提交前再次校验 Session，重复/迟到 Final 沿用原路由一次性消费。取消识别按钮仅在有效手动输入且没有 Agent/审批时启用，工作线程同样校验，不会取消权限请求。
- 权限说明和按钮在正文滚动区之外的同一固定确认区域；长说明可在内部滚动，新 request_id 将内部位置复位。所有按钮仍结算原 UiConfirmer。
- 工具按实例保留状态和耗时。适合直接阅读的动作/应用/时间结果展示最多 120 字摘要；文件、剪贴板、脚本、结构化数据和疑似密钥内容仍只显示结果信息及主窗口详情入口。不添加 LLM 摘要或通用内容解析器。
- 麦克风文案分别显示 settings.voice.enabled 保存偏好、临时/普通手动识别和实际语音状态；状态码不再被称作设备名。顶部状态只显示一次，长状态可滚动阅读；空思考、正文、过程文本和字幕不创建对应内容块。

### 实际验证

- 默认 CUDA Windows cargo build、cargo fmt --check、cargo clippy --tests 通过；保留既有警告，没有构建 CPU-only。
- 库测试：493 通过、4 忽略、1 环境筛除；筛除仍为 missing_model_message_is_actionable，本机已有对应模型，不符合该测试前提。agent_loop：29 通过，继续覆盖真实 HTTP/SSE、权限、取消、工具配对和供应商协议。
- PR16 针对性测试：8 通过、1 GUI 夹具忽略。覆盖过程/最终回答分层、多工具步骤、终态失败/取消、旧快照拒绝、敏感结果不直出、旧 SpeechId 和托盘准入。
- 新增 Session/ASR 回归：隔离会话 A 武装手动输入，切换/新建/删除/取消后投递两次旧 Final，均不能产生待提交输入。随后新 Session 的 Final 经正式 start_turn 启动真实 Agent，Mock HTTP/SSE 只收到一次新指令；没有旧指令请求或工具启动，A 历史和配置哨兵保持不变。该测试直接使用正式会话识别准入、取消、Session 存储和 Agent 启动函数；不代表真实硬件 ASR 或完整桌面点击验收。
- Windows GUI 夹具实际展开长思考、滚动 12 条工具和长回复、点击第一个请求后展示第二个请求、再拒绝第二请求。说明和按钮始终在同一个可见区域，取消识别按钮禁用。原生首显/重复 show/hide 恢复测试三轮前台 HWND 不变。
- 开发过程中实际失败：Slint 条件子组件 ID 作用域编译失败、状态标签借用临时字符串的 Rust 编译失败，均已修复并复验；新增 HTTP 测试最初未同时覆盖全局端点字段而失败，已将端点明确改为本机 Mock 后通过。未使用真实用户 Key。

仍未执行：真实通知区单双击、麦克风/KWS 到实体扬声器和语音审批的完整声学回合、多屏负坐标与 150%/200% DPI。保持 PR16 Draft，不合并、不发布；本次不增加其他功能。


## 第二轮针对性修复（2026-11-07）

- worker 的 SelectSession/DeleteSession 共用 apply_session_navigation 实际业务入口。空选择和重选当前会话直接返回；其他切换先执行 SessionRuntime.select 的读取及索引保存，成功后才取消旧识别、作废 SpeechId、停止声音、清 HUD 和临时权限。失败只反馈原因，当前内存会话保持原样。
- 删除非当前会话只删除目标并更新列表，不动当前识别、声音、Followup、HUD 或权限；等待 Agent/审批期间同样允许删除历史会话，仍拒绝删除当前正在执行的会话。仅成功删除当前会话才清理交互；选择后继时跳过无法读取的历史文件。
- HUD 使用原 ScrollView 的实际 content-y 变化区分阅读上文与回到底部；内容/尺寸变化只在自动跟随启用时贴近末尾。复用现有 HUD 发布代次，在新交互开始时恢复默认跟随。审批区域保持独立固定布局，没有新滚动引擎、动画或 Session 数据格式。

实际验证：

- 新增共享业务入口回归覆盖删除历史会话、删除失败、空选择、重选当前、目标缺失、目标损坏、成功切换和删除当前。直接断言 VoiceInstance/在途 VoiceTurn、manual_armed、Followup 截止时间、SpeechRuntime.stop_revision/is_speaking、SpeechView 退休上界、HUD 发布版本、当前未保存内存历史及 allowed_once；无关操作后原审批 request_id 仍可正常拒绝结算。成功导航后的旧 Final 在正式 Session 识别准入处被拒绝。
- cargo test --lib ui::：94 通过，4 GUI 项忽略；cargo test --lib voice:: -- --skip missing_model_message_is_actionable：155 通过（沿用本机已存在模型的环境前提筛除）；cargo test --test agent_loop：29 通过。PR16 针对性测试 9 通过、1 GUI 夹具忽略。
- 默认 CUDA Windows cargo build --bin cuava、cargo clippy --tests、cargo fmt --check 通过，保留既有警告。没有构建 CPU-only。
- Windows GUI 动态夹具使用长思考、12 条工具、每秒新增正文和字幕：默认展示最新段落和字幕；实际上滚后 following=false 且更新不抢位置；实际滚回末尾后 following=true，持续展示新字幕；再次上滚后通过新交互代次恢复 following=true。说明与按钮始终可见，夹具正常退出。
- 原生无焦点测试再次执行：首显、重复 show、hide 后再显示三轮前台 HWND 保持不变。
- 本轮真实失败记录：第一版 scrolled 回调没有可靠保留用户上滚位置，GUI 实测失败后改用实际 content-y 变化复验通过；一次测试重新链接因旧 GUI 夹具仍占用 EXE 而 LNK1104，关闭夹具后重新构建通过。

未执行与合并边界：真实托盘单双击、麦克风到实体扬声器的完整链路、多显示器负坐标与高 DPI 仍未验收；动态 GUI 夹具和假音频设备测试不算这些项目通过。本轮未发现其他已知阻断代码问题，原 PR16 的实机验收缺口仍需在合并前完成。保持 Draft，不合并、不发布，到此停止。


## 最终折叠修复与实机验收（2026-11-08）

代码只在 VoiceHud 的 changed interaction-generation 中恢复 expanded=false；原 following-latest 复位保持不变。当前回合的 reasoning、工具及字幕更新不修改 expanded。现有 expanded 改为只读输出，供原生 GUI 测试读取；无持久化字段、历史数据或状态机变更。

- PR16 针对性测试：9 通过、2 GUI 项默认忽略。cargo fmt --check、cargo clippy --tests 和默认 CUDA Windows build 通过；仍有既有警告，未构建 CPU-only。
- 新增 reasoning_generation_fixture，Windows 实际点击展开，普通 reasoning/字幕持续更新后保持展开；通过交互代次切换恢复两行，窗口从展开高度收缩，再展开后继续保持，按钮断言通过，夹具正常退出。首次夹具因真人操作/观察超过 60 秒而超时失败；延长仅用于人工操作的超时时间后重试通过，没有屏蔽功能断言。
- 真实托盘验收由用户协助操作（自动化工具只暴露 Codex/浏览器，没有 Explorer/通知区域）：用户回报单击、连续点击/双击、报时声音及焦点测试通过。隔离 Session 确实记录了“现在几点?”和“现在是上午8点10分。”回复，测试设置常驻监听偏好仍为 false。此报时走本地快捷回答，不能作为普通 Agent/工具审批验收。
- 同一隔离 Session 另记录一次“现在提点。”识别后进入未配置的模型端点并发生 llm_error，按实际失败记录；未声称完整链路通过。
- 隔离实例使用已有 Whisper small、KWS 与本地 Kokoro 模型，独立 CUAVA_DATA_DIR/LOCALAPPDATA，无现有用户配置、Session 或真实密钥读写。系统可枚举到 Realtek 等音频驱动；驱动枚举本身不算声学验收。发现同时存在已安装版，已提示用户区分并退出旧实例，避免混算版本。

完整 Agent、长回复流式播放、语音肯定/否定审批、真实 BargeIn、KWS/Followup 以及多屏/DPI 的最终结果尚待真人协作记录；不将 GUI 夹具或本地报时算作这些项目通过。PR16 保持 Draft，不合并、不发布。


## 实机发现：模型保存入口歧义修复（2026-11-08）

用户多次点击设置面板右下角“保存”后模型仍为测试占位值。检查确认没有 CUAVA_MODEL/BASE_URL 环境覆盖；PR13 模型卡片独立保存，而该按钮只提交其他设置，且刻意丢弃旧全局表单中的模型草稿。界面提示“设置已保存”会使人误以为端点已保存。

模型页底部新增“保存模型端点”，复用各卡片既有 endpoint-action(save) 校验、凭据边界及串行 SaveEndpoint 路径。多卡仍独立结算，错误保留在卡片/面板，不承诺跨卡原子提交。原按钮明确为“保存方案/其他设置”，卡片内按钮改名“保存此端点”。当前生效实例成功保存后同步顶部端点摘要。没有更改工具、权限或模型协议。

Windows GUI 已实际编辑隔离实例地址（127.0.0.1:9 改为 :19，没有发起网络请求），点击新入口后核对 settings.json 的全局和卡片地址均为新值；再保存其他设置未回退。端点专项测试 8 通过，clippy --tests、fmt --check、默认 CUDA Windows 构建通过，保留既有警告。未读取、记录或代填真实密钥。独立供应商保存后，当前 Session 仍需通过输入区模型选择器显式选择模型，这是原 PR13 行为。
