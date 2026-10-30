# 银狼 · 语音助手（Computer-Use Voice Assistant）

一个跑在 Windows 上的**桌面语音助手**：喊一声「你好狼宝」，它就能听懂、开口、动手操作这台电脑，
外加网易云音乐控制。

不是插件，不是某个客户端的附加功能——就是一个独立的小软件，托盘常驻。

## 它能做什么

| 能力 | 说明 |
|---|---|
| 语音唤醒 | 本地 KWS，说「你好狼宝」即进入对话，无需联网 |
| 连续对话 | 说完一轮继续待命，要不要等你由模型判断（不是写死的 9 秒） |
| 语音识别 | sherpa-onnx 流式中文/英文 ASR，本地跑 |
| 语音合成 | GPT-SoVITS（银狼音色，6 种情绪），本地跑，支持打断词「等一下」 |
| 操作电脑 | 截图/读界面/点击/输入/滚轮/快捷键/剪贴板，12 个 CUA 工具 |
| 联网搜索 | Tavily，天气/新闻/价格这类实时信息 |
| 网易云音乐 | 搜索、按歌名播放、暂停/上下曲/音量/进度、歌单、每日推荐、歌词、红心 |
| 其它工具 | 按名字启动程序、kill_app（taskkill /F /T）、文件操作、系统状态、执行命令（每次询问） |
| 权限 | 四档：保守 / 标准 / 宽松 / 免确认；逐能力覆盖；语音+点击双重确认 |
| 定时切换模型 | 按星期 + 时间段自动换模型（比如工作日白天用便宜的，夜里用强的） |
| 资源策略 | 全屏游戏时释放 TTS 显存、内存紧张时卸 ASR（保留唤醒） |

## 快速开始

1. 从 [Releases](../../releases) 下载 `语音助手-0.1.0-setup.exe` 安装；
2. 首次启动后打开托盘 → **设置**，填：
   - 模型 API Key（必需）
   - 联网搜索 Key（可选，Tavily）
   - 网易云音乐 AppID / PrivateKey（可选，见下）
3. 托盘勾上「语音唤醒」，说「你好狼宝」。

> 安装版**不含语音运行时**（venv + 音色包约 5GB）。需要发声就把 `.runtime/` 放到安装目录的
> `resources/` 下，或直接用完整版压缩包（见下）。

## 完整版（含语音）

完整版体积 3.5GB，超过 GitHub 单文件 2GB 上限，因此拆成两个 7z 分卷放在同一个 Release：

```text
语音助手-完整版-0.1.0.zip.001
语音助手-完整版-0.1.0.zip.002
```

合并解压（装好 7-Zip 后）：

```powershell
7z x 语音助手-完整版-0.1.0.zip.001    # 会自动读取 .002
```

解压出来是免安装目录，双击 `语音助手.exe` 即可。

## 生产依赖（本机需要）

| 组件 | 用途 | 备注 |
|---|---|---|
| Node.js ≥ 18 | 运行主程序 | 安装包已内置 Electron，不需要全局 Node |
| mpv | 音乐播放 | `winget install shinchiro.mpv`，路径会自动探测 |
| ncm-cli | 网易云接口（可选） | `npm i -g @music163/ncm-cli`，账号凭据在设置里填 |

## 隐私与凭据

- **仓库里没有任何密钥**。`.config/`、`models/`、`.runtime/`、`dist/` 全部在 `.gitignore` 里；
- 你的模型 Key / 搜索 Key / 网易云私钥只存在本机配置，应用内用 Windows DPAPI（`safeStorage`）加密落盘；
- 打包产物有专门的审计脚本 `node tools/audit-artifacts.mjs`，会按"值 + 形态"双向扫描（`BEGIN PRIVATE KEY`、`tvly-` 前缀等），确保密钥不进包；
- 网易云音色（GPT-SoVITS 银狼）版权归 miHoYo，模型为 CC-BY-NC-SA-4.0，**仅供个人本地使用**，未随仓库分发。

## 目录结构

```
apps/shell/        Electron 主进程、HUD 渲染层、独立播放面板
packages/agent/    Agent loop、LLM 客户端、权限门、离线兜底、判定逻辑
packages/cua/      desktop 工具集（vendored，MIT）
packages/voice/    ASR / KWS / VAD / TTS（GPT-SoVITS worker）
packages/tools/    本地工具：音乐、文件、进程、系统、搜索、启动器
packages/config/   路径解析、配置存储、定时表、资源策略
docs/              设计与实现记录（00–09）
tools/             验收脚本（每个特性一个 verify-*.mjs）
```

## 开发

```bash
npm install            # 需要代理时自行设置 HTTP_PROXY
npm run dev            # 或 node tools/run-shell.mjs --listen

# 验收（每个都只依赖本机，不需要网络）
node tools/verify-schedule.mjs
node tools/verify-permissions.mjs
node tools/verify-netease.mjs
node tools/verify-offline.mjs
node tools/verify-resources.mjs
node tools/verify-tools.mjs
node tools/verify-followup.mjs
node tools/verify-speechtext.mjs
node tools/verify-hardening.mjs
node tools/verify-bargein.mjs
node tools/verify-session.mjs
node tools/verify-websearch.mjs
```

打包：

```bash
npm run build:win                       # 安装包（不含语音运行时）
CUA_VA_BUNDLE_RUNTIME=1 npx electron-builder --win --dir   # 完整版目录
node tools/audit-artifacts.mjs dist/win-unpacked           # 密钥审计
```

## License

代码 MIT。第三方组件遵循各自许可（Electron、sherpa-onnx、GPT-SoVITS、dsh-computer-use MIT）。
音色与角色形象版权归 miHoYo，本项目仅作个人学习用途，不提供任何商业分发。

### 敏感操作确认

需要确认的操作只能通过确认卡片上的用户点击批准，包括「允许 15 分钟」。语音永远不能批准；开启「语音取消」后仅明确取消语句可拒绝，含糊、疑问、条件性或肯定语句保持卡片等待点击。关闭该选项后语音不参与决议，待确认期间重复唤醒不会重开会话。取消、收起或关闭窗口、回合结束及超时按拒绝处理。迟到或重复的卡片答复按请求编号隔离。原有无需确认的权限档位行为保持不变。自动确认环境变量不再提供批准能力。

确认批准 IPC 的发送者保留在隔离 preload 中，普通 HUD 页面脚本没有公开批准 API；按钮监听在隔离环境中检查可信 DOM 事件。可信事件并不认证人类身份，系统级输入自动化仍可能生成可信点击。关闭 HUD 或退出助手会取消整个 Agent 回合；拒绝单项工具仅拒绝该项。取消会中止模型请求，并在权限检查返回及下一项工具启动前再次检查；已经启动且底层不支持中断的工具可能继续完成，不能撤销已完成操作。

待确认期间切换「语音取消」会清理识别残留、重置监听代次；开启后在麦克风及 ASR 可用时恢复取消监听。切换不改变请求编号或超时期限。常见明确取消（如「别执行了」「取消吧」「停止操作」）可拒绝；疑问、引用及「不要取消」保持等待。
