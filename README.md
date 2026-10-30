# 银狼 · 语音助手（Computer-Use Voice Assistant）

一个跑在 Windows 上的**桌面语音助手**：喊一声「你好狼宝」，它就能听懂、开口、动手操作这台电脑。

不是插件，不是某个客户端的附加功能——就是一个独立的小软件，托盘常驻。

## 两个版本

本仓库维护**两个可独立安装的版本**，共用同一套语音/电脑操作底座，区别只在音乐部分：

| | **v0.1.0 网易云版** | **v0.2.0 LX 版** |
|---|---|---|
| 音乐实现 | ncm-cli 官方接口 + 直连取播放地址 + mpv | **LX Music 开放 API + Scheme URL**（推荐） |
| 前置要求 | 安装 ncm-cli、mpv；在设置里填网易云 AppID/PrivateKey、扫码登录 | 本机装好 [LX Music](https://lxmusic.toside.cn/)，并在其设置里打开「开放 API」（默认端口 23330） |
| 歌曲控制浮窗 | 有（播放/暂停/上下曲独立小窗） | 无（音乐完全交给 LX，助手只做遥控） |
| 登录/过期 | accessToken 24 小时过期，需定期扫码 | 无需登录，LX 自己管音源和账号 |
| 推荐度 | — | ⭐ 依赖少、更稳 |

语音唤醒、连续对话、ASR/TTS、电脑操作、联网搜索、权限、定时切换模型等**两个版本完全一致**，见下表。

## 它能做什么

| 能力 | 说明 |
|---|---|
| 语音唤醒 | 本地 KWS，说「你好狼宝」即进入对话，无需联网 |
| 连续对话 | 说完一轮继续待命，要不要等你由模型判断（不是写死的 9 秒） |
| 语音识别 | sherpa-onnx 流式中文/英文 ASR，本地跑 |
| 语音合成 | GPT-SoVITS（银狼音色，6 种情绪），本地跑，支持打断词「等一下」 |
| 操作电脑 | 截图/读界面/点击/输入/滚轮/快捷键/剪贴板，12 个 CUA 工具 |
| 联网搜索 | Tavily，天气/新闻/价格这类实时信息 |
| 音乐 | v0.1.0：网易云全套；v0.2.0：LX Music（searchPlay/暂停/上下曲/进度/音量/歌词） |
| 其它工具 | 按名字启动程序、kill_app（taskkill /F /T）、文件操作、系统状态、执行命令（每次询问） |
| 权限 | 四档：保守 / 标准 / 宽松 / 免确认；逐能力覆盖；语音+点击双重确认 |
| 定时切换模型 | 按星期 + 时间段自动换模型，可切换整套供应商（地址+Key） |
| 资源策略 | 全屏游戏时释放 TTS 显存、内存紧张时卸 ASR（保留唤醒） |

## 快速开始

1. 从 [Releases](../../releases) 下载对应版本的安装包：
   - `v0.2.0` → `语音助手-0.2.0-setup.exe`（LX 版，先装好 LX Music 并打开开放 API）
   - `v0.1.0` → `语音助手-0.1.0-setup.exe`（网易云版，先装 ncm-cli + mpv）
2. 首次启动后打开托盘 → **设置**，填：
   - 模型 API Key（必需）
   - 联网搜索 Key（可选，Tavily）
   - v0.1.0 另需：网易云 AppID / PrivateKey，并扫码登录
3. 托盘勾上「语音唤醒」，说「你好狼宝」。

> 两个安装版都不含语音运行时（venv + 音色包约 5GB）。需要发声就把 `.runtime/` 放到安装目录的
> `resources/` 下，或直接用完整版压缩包（见下）。

## 完整版（含语音）

完整版体积 3.5GB，超过 GitHub 单文件 2GB 上限，因此拆成两个 7z 分卷放在各自 Release 里：

```text
# LX 版（v0.2.0）
语音助手-完整版-0.2.0.7z.001   1.76 GB
语音助手-完整版-0.2.0.7z.002   1.20 GB

# 网易云版（v0.1.0）
语音助手-完整版-0.1.0.7z.001   1.76 GB
语音助手-完整版-0.1.0.7z.002   1.20 GB
```

合并解压（装好 7-Zip 后，只需指定第一个分卷）：

```powershell
7z x 语音助手-完整版-0.2.0.7z.001    # 会自动读取 .002
```

解压出来是免安装目录，双击 `语音助手.exe` 即可。

## 目录结构

```
apps/shell/        Electron 主进程、HUD 渲染层
packages/agent/    Agent loop、LLM 客户端、权限门、离线兜底、判定逻辑
packages/cua/      desktop 工具集（vendored，MIT）
packages/voice/    ASR / KWS / VAD / TTS（GPT-SoVITS worker）
packages/tools/    本地工具：文件、进程、系统、搜索、启动器
                     v0.1.0：netease.mjs / netease-api.mjs / mpv-player.mjs
                     v0.2.0：lxmusic.mjs
packages/config/   路径解析、配置存储、定时表、资源策略
lx/                LX 版完整源码（v0.2.0），与仓库根目录的 v0.1.0 并存
docs/              设计与实现记录（00–09）
tools/             验收脚本（每个特性一个 verify-*.mjs）
```

## 依赖说明（本机需要）

**通用（两个版本都用）**

| 组件 | 用途 | 备注 |
|---|---|---|
| Node.js ≥ 18 | 开发/构建 | 安装包已内置 Electron，终端用户不需要 |

**v0.1.0 网易云版额外**

| 组件 | 用途 | 备注 |
|---|---|---|
| mpv | 音乐播放 | `winget install shinchiro.mpv`，路径会自动探测 |
| ncm-cli | 网易云接口 | `npm i -g @music163/ncm-cli`，账号凭据在设置里填 |

**v0.2.0 LX 版额外**

| 组件 | 用途 | 备注 |
|---|---|---|
| LX Music（桌面版 ≥ v2.7.0） | 音乐播放 | 设置 → 开放 API → 打开服务，默认端口 23330 |

## 开发

仓库根目录是 v0.1.0（网易云版），`lx/` 是 v0.2.0（LX 版）。

```bash
# 网易云版
npm install && node tools/run-shell.mjs --listen

# LX 版
cd lx && npm install && node tools/run-shell.mjs --listen
```

验收脚本（每个都只依赖本机，不需要网络）：

```bash
node tools/verify-wake-runtime.mjs   # 唤醒链路（运行时同路径）
node tools/verify-keywords.mjs       # 关键词命中/误触
node tools/verify-schedule.mjs       # 定时切换模型
node tools/verify-permissions.mjs    # 权限门
node tools/verify-offline.mjs        # 离线兜底
node tools/verify-resources.mjs      # 资源策略
node tools/verify-tools.mjs          # 本地工具
node tools/verify-followup.mjs       # 续聊判定
node tools/verify-speechtext.mjs     # 播报清洗
node tools/verify-hardening.mjs      # 加固项
node tools/verify-bargein.mjs        # 打断
node tools/verify-session.mjs        # 会话
node tools/verify-websearch.mjs      # 联网搜索
# LX 版独有
cd lx && node tools/verify-lxmusic.mjs
# v0.1.0 独有
node tools/verify-netease.mjs
```

打包：

```bash
npm run build:win                                        # 安装包（不含语音运行时）
CUA_VA_BUNDLE_RUNTIME=1 npx electron-builder --win --dir # 完整版目录
node tools/audit-artifacts.mjs dist/win-unpacked         # 密钥审计
```

## 隐私与凭据

- **仓库里没有任何密钥**。`.config/`、`models/`、`.runtime/`、`dist/` 全部在 `.gitignore` 里；
- 你的模型 Key / 搜索 Key / 网易云私钥只存在本机配置，应用内用 Windows DPAPI（`safeStorage`）加密落盘；
- 打包产物有专门的审计脚本 `node tools/audit-artifacts.mjs`，会按"值 + 形态"双向扫描（`BEGIN PRIVATE KEY`、`tvly-` 前缀等），确保密钥不进包；
- 网易云音色（GPT-SoVITS 银狼）版权归 miHoYo，模型为 CC-BY-NC-SA-4.0，**仅供个人本地使用**，未随仓库分发。

## License

代码 MIT。第三方组件遵循各自许可（Electron、sherpa-onnx、GPT-SoVITS、dsh-computer-use MIT）。
音色与角色形象版权归 miHoYo，本项目仅作个人学习用途，不提供任何商业分发。

### 敏感操作确认

确认卡片出现后，可以说「可以」「确认」「同意」「好的」等完整肯定表达同意，或说「不可以」「不同意」「取消」等明确拒绝表达；也可以点击允许、允许 15 分钟或取消。语音确认默认开启，沿用 `voiceConfirmEnabled` 配置；关闭时语音同意和拒绝均禁用。确认超时仍为默认 180 秒，切换开关和含糊回答不会延长超时。疑问、引用、条件句和双重否定保持等待，不通过肯定词子串推断同意。

语音批准只接受本机 ASR 定稿或有效 flush，并校验请求 ID 与监听代次；partial 不批准。页面无公开批准 API，点击/键盘批准由隔离 preload 发送，主进程校验来源与当前请求。可信 DOM 事件不认证人类身份。Enter 可允许，聚焦按钮时沿用按钮原生键盘操作；Esc 和关闭按钮按原有行为关闭助手任务。

拒绝单项工具与关闭整个回合分别处理。取消后的旧识别、迟到回调和重复事件不能重新授权。原有权限档位、免确认模式、正常无需确认操作及临时授权含义保持不变。
