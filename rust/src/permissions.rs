//! 权限引擎（对应 `packages/agent/permissions.mjs`，PR8 补齐四档与逐能力覆盖）。
//!
//! 唯一能说"能不能做"的地方。所有工具调用都必须先过这里，包括 PR5 的 12 个 CUA 工具。
//!
//! 三个概念分开：
//!  - 能力（capability）：按"能造成什么后果"划分（看屏幕 / 点鼠标 / 敲键盘…）
//!  - 级别（level）：allow / ask / deny，回答"这个能力现在允许到什么程度"
//!  - 作用域：本版只做时间维度（暂停 + 限时授权），应用/目录作用域留待后续
//!
//! 判定顺序（与旧版 `resolveLevel` 一致，PR8 显式写出来）：
//!  1. **全局暂停**最高优先：暂停期间任何工具都不执行（含已知、可授权工具）；
//!  2. **未知工具**一律拒绝——不在白名单里的工具不因任何档位被放行；
//!  3. 档位预设（保守 / 标准 / 宽松 / 免确认）调整**已知能力**的默认级别；
//!  4. 逐能力覆盖（allow / ask / deny / 跟随档位）覆盖档位结果；
//!  5. 会话内限时授权只把 `ask` 变成 `allow`，不会把 `deny` 或未知工具解开。
//!
//! 免确认（`pass-through`）的语义（旧版 `unattended`）：
//!  - 已知且可授权的工具**不再询问**，包括删除/执行命令这类默认 ask 与默认 deny 的破坏性操作；
//!  - 未知工具仍然拒绝；
//!  - 全局暂停仍然优先；
//!  - 单项覆盖仍可把它掰回 ask / deny（留一个刹车）；
//!  - 风险升级（Win+R、rm -rf 之类）在免确认下不再强制询问。
//!
//! 取消语义：等待确认期间回合被取消 → 迟到的"允许"不生效，工具绝不执行。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::{PermissionMode, PermissionOverride};

/// 决策级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    Allow,
    Ask,
    Deny,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "允许",
            Self::Ask => "询问",
            Self::Deny => "拒绝",
        }
    }
}

/// 一次分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub capability: &'static str,
    /// 该能力的**默认**级别（档位与覆盖会在此基础上调整）。
    pub level: Level,
    /// 定性说明（升级原因 / 拒绝原因），会显示给用户。
    pub reason: String,
    /// 是否因具体参数（Win+R、rm -rf 之类）被风险升级。
    pub escalated: bool,
    /// 是否是不在白名单里的未知工具（未知工具无条件拒绝）。
    pub unknown: bool,
}

/// 能力定义（界面上的"逐能力覆盖"表格直接来自这里）。
#[derive(Debug, Clone, Copy)]
pub struct CapabilityInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub level: Level,
    /// 是否需要留痕（审计）。
    pub audit: bool,
}

const CAPABILITIES: &[CapabilityInfo] = &[
    CapabilityInfo {
        id: "screen.capture",
        label: "看屏幕",
        level: Level::Allow,
        audit: false,
    },
    CapabilityInfo {
        id: "window.read",
        label: "读窗口列表",
        level: Level::Allow,
        audit: false,
    },
    CapabilityInfo {
        id: "input.pointer",
        label: "移动/点击鼠标",
        level: Level::Allow,
        audit: true,
    },
    CapabilityInfo {
        id: "input.keyboard",
        label: "敲键盘",
        level: Level::Allow,
        audit: true,
    },
    CapabilityInfo {
        id: "input.text",
        label: "填写输入框",
        level: Level::Allow,
        audit: true,
    },
    CapabilityInfo {
        id: "app.launch",
        label: "启动应用",
        level: Level::Allow,
        audit: true,
    },
    CapabilityInfo {
        id: "app.close",
        label: "关闭窗口",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "file.read",
        label: "读取文件",
        level: Level::Allow,
        audit: true,
    },
    CapabilityInfo {
        id: "file.write",
        label: "修改文件",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "file.delete",
        label: "删除文件",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "system.shell",
        label: "执行命令",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "system.info",
        label: "查看系统状态",
        level: Level::Allow,
        audit: false,
    },
    CapabilityInfo {
        id: "web.search",
        label: "联网搜索",
        // 默认放行：问"今天天气"不该每次弹卡片。留痕是因为查询词会发给第三方。
        level: Level::Allow,
        audit: true,
    },
    CapabilityInfo {
        id: "system.settings",
        label: "修改系统设置",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "system.power",
        label: "关机/重启/安装",
        level: Level::Deny,
        audit: true,
    },
    CapabilityInfo {
        id: "clipboard.read",
        label: "读剪贴板",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "clipboard.write",
        label: "写剪贴板",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "comm.send",
        label: "发送消息",
        level: Level::Ask,
        audit: true,
    },
    CapabilityInfo {
        id: "pay.transfer",
        label: "支付转账",
        level: Level::Deny,
        audit: true,
    },
    CapabilityInfo {
        id: "none",
        label: "无副作用",
        level: Level::Allow,
        audit: false,
    },
];

/// 全部能力定义（界面/审计用；顺序稳定）。
pub fn capabilities() -> &'static [CapabilityInfo] {
    CAPABILITIES
}

fn capability_info(id: &str) -> Option<&'static CapabilityInfo> {
    CAPABILITIES.iter().find(|item| item.id == id)
}

/// 工具名 → 能力。没有映射的工具一律 deny。
///
/// PR12 恢复了旧版全部非音乐工具，能力映射与旧版 `TOOL_CAPABILITY` 一致：
///  - `run_shell` → `system.shell`（默认 ask）；
///  - `web_search` → `web.search`（默认 allow，但留痕：查询词会发给第三方）；
///  - `get_current_time` / `system_info` / `app_control` → 只读能力（默认 allow）；
///  - `launch_app` → `app.launch`（默认 allow）；`kill_app` → `app.close`（默认 ask）；
///  - `files` 的细粒度分级在 [`classify`] 里按 `action` 决定，**绝不能**把删除
///    归到读取能力下。
pub fn tool_capability(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "screenshot" => "screen.capture",
        "list_windows" | "get_app_state" | "request_access" => "window.read",
        "left_click" => "input.pointer",
        "scroll" => "input.pointer",
        "type" => "input.keyboard",
        "key" => "input.keyboard",
        "set_value" => "input.text",
        "open_application" => "app.launch",
        "read_clipboard" => "clipboard.read",
        "write_clipboard" => "clipboard.write",
        "wait" => "none",
        // ---- PR12：本地非音乐工具 ----
        "run_shell" => "system.shell",
        "web_search" => "web.search",
        "launch_app" => "app.launch",
        "kill_app" => "app.close",
        "system_info" => "system.info",
        "app_control" => "window.read",
        "get_current_time" => "none",
        // `files` 的默认能力是"读"；真正的分级在 `classify` 里按 action 覆盖。
        // 这条兜底只用于"模型漏了 action"的情况——那种调用会被执行前的参数校验拦下。
        "files" => "file.read",
        _ => return None,
    })
}

/// 按键组合的风险升级。机械层面看不出语义风险，只能按具体组合挡。
const RISKY_KEYS: &[(&str, &str, &str)] = &[
    (
        "win+r",
        "system.settings",
        "打开「运行」对话框，等于可以执行任意命令",
    ),
    ("win+x", "system.settings", "打开系统快捷菜单"),
    ("win+i", "system.settings", "打开系统设置"),
    ("win+l", "system.settings", "锁屏"),
    ("alt+f4", "app.close", "关闭当前窗口，可能丢失未保存的改动"),
    ("ctrl+shift+esc", "system.settings", "打开任务管理器"),
    ("ctrl+shift+delete", "system.settings", "打开任务管理器"),
];

/// 文本里的危险命令迹象：模型把这句打进终端/运行框就真会执行。
const RISKY_TEXT: &[(&str, &str, &str)] = &[
    ("rm -rf", "file.delete", "文本里包含递归删除命令"),
    ("del /s", "file.delete", "文本里包含递归删除命令"),
    ("del /q", "file.delete", "文本里包含递归删除命令"),
    ("rd /s", "file.delete", "文本里包含递归删除命令"),
    ("format c:", "file.delete", "文本里包含格式化命令"),
    ("shutdown", "system.power", "文本里包含关机/重启命令"),
    (
        "restart-computer",
        "system.power",
        "文本里包含关机/重启命令",
    ),
    ("stop-computer", "system.power", "文本里包含关机/重启命令"),
    ("git push -f", "system.settings", "强制推送会覆盖远端历史"),
    (
        "git push --force",
        "system.settings",
        "强制推送会覆盖远端历史",
    ),
];

fn normalize_key_chord(raw: &str) -> String {
    raw.chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// 判断一次调用属于哪个能力、默认什么级别。
///
/// 只做**机械风险**判断；语义风险（"这个按钮点了会付款吗"）归模型的白描和用户确认。
pub fn classify(tool: &str, args: &serde_json::Value) -> Classification {
    let Some(capability) = tool_capability(tool) else {
        return Classification {
            capability: "unknown",
            level: Level::Deny,
            reason: format!("未知工具：{tool}"),
            escalated: false,
            unknown: true,
        };
    };

    if tool == "key" {
        let keys = args
            .get("keys")
            .or_else(|| args.get("key"))
            .or_else(|| args.get("combo"))
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let normalized = normalize_key_chord(keys);
        for (pattern, escalated_capability, why) in RISKY_KEYS {
            if normalized.contains(pattern) {
                return Classification {
                    capability: escalated_capability,
                    level: Level::Ask,
                    reason: format!("{keys}：{why}"),
                    escalated: true,
                    unknown: false,
                };
            }
        }
    }

    if tool == "type" || tool == "set_value" {
        let text = args
            .get("text")
            .or_else(|| args.get("value"))
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        for (pattern, escalated_capability, why) in RISKY_TEXT {
            if text.contains(pattern) {
                return Classification {
                    capability: escalated_capability,
                    level: Level::Ask,
                    reason: (*why).to_string(),
                    escalated: true,
                    unknown: false,
                };
            }
        }
    }

    /*
     * `files` 的细粒度分级：同一个工具名，列目录和删除的风险差着量级。
     *
     * 规则与旧版 `classify` 一致，且**删除永远不会落到读取能力下**：
     *  - delete                         → file.delete（默认 ask）
     *  - write / copy / move            → file.write（默认 ask）
     *  - list / read / info / 未知 action → file.read（默认 allow）
     *
     * 未知 action 归到读取是安全的**因为**它会在动作之前被参数校验拒绝
     * （`tools::files::Request::validate`）；也就是说这里最坏的结果是
     * "一个本来就不会执行的调用被按读取放行"，不会造成写入或删除。
     */
    if tool == "files" {
        let action = args
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let capability = match action.as_str() {
            "delete" => "file.delete",
            "write" | "copy" | "move" => "file.write",
            _ => "file.read",
        };
        let base = capability_info(capability)
            .map(|info| info.level)
            .unwrap_or(Level::Ask);
        return Classification {
            capability,
            level: base,
            // 让用户在确认卡上看到"这次到底想做什么"，而不是只看到 files。
            reason: if action.is_empty() {
                "没有给 action（会在执行前被拒绝）".to_string()
            } else {
                format!("action={action}")
            },
            escalated: false,
            unknown: false,
        };
    }

    let base = capability_info(capability)
        .map(|info| info.level)
        .unwrap_or(Level::Ask);
    Classification {
        capability,
        level: base,
        reason: String::new(),
        escalated: false,
        unknown: false,
    }
}

/// 权限状态（运行期可改，`paused_until` / `allowed_once` 属于会话状态，**不落盘**）。
#[derive(Debug, Clone)]
pub struct PermissionState {
    pub mode: PermissionMode,
    /// 逐能力覆盖（来自设置；`Follow` = 跟随档位）。
    pub overrides: std::collections::BTreeMap<String, PermissionOverride>,
    /// 全局暂停：任何工具都不执行。
    pub paused_until: Option<SystemTime>,
    /// 本次会话内"限时允许"的能力 → 到期时间（不持久化、不跨 Session 泄漏）。
    pub allowed_once: Vec<(String, SystemTime)>,
}

impl Default for PermissionState {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Standard,
            overrides: Default::default(),
            paused_until: None,
            allowed_once: Vec::new(),
        }
    }
}

impl PermissionState {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// 从设置构造（档位 + 逐能力覆盖）。
    pub fn from_settings(
        mode: PermissionMode,
        overrides: &std::collections::BTreeMap<String, PermissionOverride>,
    ) -> Self {
        Self {
            mode,
            overrides: overrides.clone(),
            ..Self::default()
        }
    }

    /// 合并"档位预设 + 逐能力覆盖 + 过期临时授权"得出最终级别。
    ///
    /// 注意顺序：**覆盖在档位之后**（用户显式覆盖必须能压过免确认档位，
    /// 这是"免确认也有刹车"的实现），临时授权最后只提升 `ask`。
    pub fn resolve_level(&self, capability: &str, now: SystemTime) -> Level {
        let base = capability_info(capability)
            .map(|info| info.level)
            .unwrap_or(Level::Ask);
        let mut level = match self.mode {
            // 保守档：把输入类动作也提到确认（代价是更啰嗦，但不可逆动作本来就要问）
            PermissionMode::Conservative => match capability {
                "input.pointer" | "input.keyboard" | "input.text" | "app.launch"
                    if base == Level::Allow =>
                {
                    Level::Ask
                }
                _ => base,
            },
            // 宽松档：把"会发内容出去/改文件"的 ask 降为 allow，deny 不动
            PermissionMode::Permissive => match capability {
                "clipboard.read" | "clipboard.write" | "file.write" | "file.delete"
                | "comm.send" | "system.settings"
                    if base == Level::Ask =>
                {
                    Level::Allow
                }
                _ => base,
            },
            // 免确认：已知能力（含默认 deny 的破坏性操作）全部放行。
            // 未知工具在 `PermissionGate::check` 里就被拒绝，走不到这里。
            PermissionMode::PassThrough => Level::Allow,
            PermissionMode::Standard => base,
        };

        // 逐能力覆盖：显式 allow / ask / deny 压过档位；`Follow` 不改变任何东西。
        if let Some(choice) = self.overrides.get(capability) {
            match choice {
                PermissionOverride::Allow => level = Level::Allow,
                PermissionOverride::Ask => level = Level::Ask,
                PermissionOverride::Deny => level = Level::Deny,
                PermissionOverride::Follow => {}
            }
        }

        // 限时授权只把 ask 提升为 allow：deny 与未知工具不可能被一次授权解开。
        if level == Level::Ask {
            let granted = self
                .allowed_once
                .iter()
                .any(|(id, until)| id == capability && *until > now);
            if granted {
                level = Level::Allow;
            }
        }
        level
    }

    /// 会话内限时允许某能力（`minutes` 分钟后自动失效，不落盘）。
    pub fn grant_once(&mut self, capability: &str, minutes: u64) {
        let until = SystemTime::now() + Duration::from_secs(minutes.saturating_mul(60));
        self.allowed_once.retain(|(id, _)| id != capability);
        self.allowed_once.push((capability.to_string(), until));
    }

    /// 当前仍然有效的临时授权（界面显示用）。
    pub fn active_grants(&self, now: SystemTime) -> Vec<(String, u64)> {
        self.allowed_once
            .iter()
            .filter(|(_, until)| *until > now)
            .filter_map(|(id, until)| {
                until
                    .duration_since(now)
                    .ok()
                    .map(|left| (id.clone(), left.as_secs().div_ceil(60)))
            })
            .collect()
    }

    pub fn pause(&mut self, minutes: u64) {
        self.paused_until =
            Some(SystemTime::now() + Duration::from_secs(minutes.saturating_mul(60)));
    }

    pub fn resume(&mut self) {
        self.paused_until = None;
    }

    pub fn is_paused(&self, now: SystemTime) -> bool {
        matches!(self.paused_until, Some(until) if until > now)
    }

    /// 暂停还剩多少分钟（`None` = 没有暂停）。
    pub fn paused_minutes(&self, now: SystemTime) -> Option<u64> {
        self.paused_until.and_then(|until| {
            until
                .duration_since(now)
                .ok()
                .map(|left| left.as_secs().div_ceil(60))
        })
    }
}

/// 权限决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub level: Level,
    pub capability: &'static str,
    /// 决策通道：`policy` / `unattended` / `pause` / `no-channel` / `click` / `voice` / `deny` / `cancelled`。
    pub channel: &'static str,
    pub reason: String,
}

impl Decision {
    pub fn denied(&self) -> bool {
        !self.allow
    }
}

/// "问用户"的异步回调。
///
/// 点击确认与语音确认都通过这一个接口；权限引擎自己不能阻塞，也不碰 UI 细节。
pub type ConfirmFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ConfirmAnswer> + Send + 'a>>;

pub trait Confirmer: Send + Sync {
    /// 向用户求一次确认。
    ///
    /// `cancel` 让等待中的确认能被回合取消/程序退出立刻结束——
    /// 否则会留下一个"还开着、之后可能被点允许"的确认框。
    fn confirm<'a>(
        &'a self,
        request: ConfirmRequest,
        cancel: &'a crate::cancel::CancelToken,
    ) -> ConfirmFuture<'a>;
}

/// `Arc<T>` 也能当确认器用：界面把确认器交给后台任务时不需要额外包装。
impl<T: Confirmer + ?Sized> Confirmer for std::sync::Arc<T> {
    fn confirm<'a>(
        &'a self,
        request: ConfirmRequest,
        cancel: &'a crate::cancel::CancelToken,
    ) -> ConfirmFuture<'a> {
        (**self).confirm(request, cancel)
    }
}

/// 请用户确认时的上下文。
#[derive(Debug, Clone)]
pub struct ConfirmRequest {
    pub tool: String,
    pub capability: &'static str,
    pub label: &'static str,
    pub reason: String,
    /// 参数摘要（不含截图 base64）。
    pub args_summary: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmAnswer {
    pub allow: bool,
    pub channel: &'static str,
    pub reason: String,
}

impl ConfirmAnswer {
    pub fn allow_click() -> Self {
        Self {
            allow: true,
            channel: "click",
            reason: String::new(),
        }
    }

    pub fn deny_click() -> Self {
        Self {
            allow: false,
            channel: "click",
            reason: "用户没有确认".to_string(),
        }
    }

    /// 语音说出的明确肯定（PR7）。只有**明确**的肯定才会构造它：
    /// 含糊、混合、疑问一律走 [`Self::deny_voice`]，不会被当成同意。
    pub fn allow_voice() -> Self {
        Self {
            allow: true,
            channel: "voice",
            reason: String::new(),
        }
    }

    /// 语音说出的否定或无法确认。`reason` 会作为拒绝原因写进历史。
    pub fn deny_voice(reason: &str) -> Self {
        Self {
            allow: false,
            channel: "voice",
            reason: reason.to_string(),
        }
    }
}

/// 一次权限决策的审计记录（只含必要信息，不含密钥/截图）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuditRecord {
    /// Unix 毫秒时间戳（界面直接显示本地时间）。
    pub ts_ms: u64,
    pub tool: String,
    pub capability: String,
    pub level: String,
    /// `allow` / `deny`
    pub decision: String,
    pub channel: String,
    /// 脱敏后的参数摘要（JSON 文本，截断到 120 字）。
    pub args: String,
    pub reason: String,
    /// 该记录归属的会话 id（`None` = 设置页/非回合操作）。
    pub session_id: Option<String>,
    pub turn_id: Option<u64>,
}

impl AuditRecord {
    pub fn label(&self) -> String {
        let when = format_local_short(self.ts_ms);
        let verdict = if self.decision == "allow" {
            "允许"
        } else {
            "拒绝"
        };
        let reason = if self.reason.is_empty() {
            String::new()
        } else {
            format!("（{}）", self.reason)
        };
        format!(
            "{when} {verdict} {} · {} 通过 {}{reason}",
            self.capability_label(),
            self.tool,
            self.channel
        )
    }
    pub fn capability_label(&self) -> String {
        capability_label(&self.capability).to_string()
    }
}

/// `ts_ms` → 本地 `HH:mm:ss`。
///
/// 审计记录里的时间戳是毫秒；显示时用 `clock` 的本地时间换算
/// （Windows 上走系统时区，其它平台退化 UTC）。只用于展示，不影响判定。
fn format_local_short(ts_ms: u64) -> String {
    let local = crate::clock::from_unix_ms(ts_ms);
    format!("{:02}:{:02}:{:02}", local.hour, local.minute, local.second)
}

/// 审计日志：按 JSONL 追加写，最多保留 `MAX_AUDIT_RECORDS` 条（读取时从尾部取）。
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

/// 审计文件最多读取的条数（界面只显示最近若干条）。
pub const MAX_AUDIT_RECORDS: usize = 50;

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条记录。失败只记 stderr——审计写入不能反过来阻断工具执行。
    pub fn append(&self, record: &AuditRecord) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(line) = serde_json::to_string(record) else {
            return;
        };
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{line}");
            }
            Err(error) => eprintln!("[audit] 写入失败：{error}"),
        }
    }

    /// 读取最近 `limit` 条（从旧到新）。
    pub fn recent(&self, limit: usize) -> Vec<AuditRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut records: Vec<AuditRecord> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if records.len() > limit {
            records.drain(0..records.len() - limit);
        }
        records
    }

    /// 导出全部记录到 `exports/` 目录，返回写出的条数与路径。
    pub fn export(&self, exports_dir: &Path) -> Result<(usize, PathBuf), String> {
        let records = self.all();
        std::fs::create_dir_all(exports_dir)
            .map_err(|error| format!("无法创建导出目录：{error}"))?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let path = exports_dir.join(format!("audit-export-{stamp}.jsonl"));
        let mut text = String::new();
        for record in &records {
            if let Ok(line) = serde_json::to_string(record) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        std::fs::write(&path, text).map_err(|error| format!("写出审计导出失败：{error}"))?;
        Ok((records.len(), path))
    }

    fn all(&self) -> Vec<AuditRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

/// 权限门：所有工具调用的唯一入口。
pub struct PermissionGate<'a> {
    state: &'a PermissionState,
    now: Box<dyn Fn() -> SystemTime + Send + Sync>,
    /// 审计记录（可选）：有就按 `should_audit` 留痕。
    audit: Option<AuditLog>,
    session_id: Option<String>,
    turn_id: Option<u64>,
}

impl<'a> PermissionGate<'a> {
    pub fn new(state: &'a PermissionState) -> Self {
        Self {
            state,
            now: Box::new(SystemTime::now),
            audit: None,
            session_id: None,
            turn_id: None,
        }
    }

    /// 带审计的门：回合内的工具判断会写一条脱敏记录。
    pub fn with_audit(mut self, audit: AuditLog, session_id: Option<String>, turn_id: u64) -> Self {
        self.audit = Some(audit);
        self.session_id = session_id;
        self.turn_id = Some(turn_id);
        self
    }

    /// 判定一次调用。`confirm` 为 `None` 时，`ask` 一律拒绝（没有确认通道就不该放行）。
    ///
    /// `cancel` 在等待用户确认期间被置位时，本次判定直接按拒绝返回，
    /// 并且**不**把这条迟到的答复留给下一次确认消费。
    pub async fn check(
        &self,
        tool: &str,
        args: &serde_json::Value,
        confirm: Option<&dyn Confirmer>,
        cancel: &crate::cancel::CancelToken,
    ) -> Decision {
        let now = (self.now)();

        // 全局暂停优先于一切（含免确认档与临时授权）。
        if self.state.is_paused(now) {
            let minutes = self.state.paused_minutes(now).unwrap_or(0);
            return self.finish(
                Decision {
                    allow: false,
                    level: Level::Deny,
                    capability: "all",
                    channel: "pause",
                    reason: format!("已暂停 {minutes} 分钟，期间不执行任何操作"),
                },
                tool,
                args,
            );
        }

        let verdict = classify(tool, args);
        // 未知工具：不因任何档位放行。
        if verdict.unknown {
            return self.finish(
                Decision {
                    allow: false,
                    level: Level::Deny,
                    capability: verdict.capability,
                    channel: "policy",
                    reason: verdict.reason,
                },
                tool,
                args,
            );
        }

        let mut level = self.state.resolve_level(verdict.capability, now);
        // 风险升级（Win+R、rm -rf 之类）默认强制 ask；免确认档下不强制。
        if verdict.escalated && self.state.mode != PermissionMode::PassThrough {
            level = Level::Ask;
        }

        match level {
            Level::Allow => self.finish(
                Decision {
                    allow: true,
                    level,
                    capability: verdict.capability,
                    channel: if self.state.mode == PermissionMode::PassThrough {
                        "unattended"
                    } else {
                        "policy"
                    },
                    reason: verdict.reason,
                },
                tool,
                args,
            ),
            Level::Deny => {
                let label = capability_info(verdict.capability)
                    .map(|info| info.label)
                    .unwrap_or(verdict.capability);
                self.finish(
                    Decision {
                        allow: false,
                        level,
                        capability: verdict.capability,
                        channel: "policy",
                        reason: format!("「{label}」已被设为拒绝"),
                    },
                    tool,
                    args,
                )
            }
            Level::Ask => {
                let Some(confirm) = confirm else {
                    return self.finish(
                        Decision {
                            allow: false,
                            level: Level::Deny,
                            capability: verdict.capability,
                            channel: "no-channel",
                            reason: "需要确认，但当前没有可用的确认通道".to_string(),
                        },
                        tool,
                        args,
                    );
                };
                let request = ConfirmRequest {
                    tool: tool.to_string(),
                    capability: verdict.capability,
                    label: capability_info(verdict.capability)
                        .map(|info| info.label)
                        .unwrap_or(verdict.capability),
                    reason: verdict.reason.clone(),
                    args_summary: summarize_args(args),
                };
                let answer = confirm.confirm(request, cancel).await;
                // 用户点了允许、但回合在此期间已被取消/退出：这条答复**不生效**。
                if cancel.is_cancelled() {
                    return self.finish(
                        Decision {
                            allow: false,
                            level: Level::Ask,
                            capability: verdict.capability,
                            channel: "cancelled",
                            reason: "已中断，未执行".to_string(),
                        },
                        tool,
                        args,
                    );
                }
                let allowed = answer.allow && matches!(answer.channel, "click" | "voice");
                self.finish(
                    Decision {
                        allow: allowed,
                        level: Level::Ask,
                        capability: verdict.capability,
                        channel: if allowed { answer.channel } else { "deny" },
                        reason: if allowed {
                            String::new()
                        } else {
                            answer.reason
                        },
                    },
                    tool,
                    args,
                )
            }
        }
    }

    /// 该能力是否需要留痕（审计日志用）。
    pub fn should_audit(&self, capability: &str) -> bool {
        capability_info(capability)
            .map(|info| info.audit)
            .unwrap_or(true)
    }

    /// 收口：按规则写审计并返回决策。
    fn finish(&self, decision: Decision, tool: &str, args: &serde_json::Value) -> Decision {
        let worth = decision.level != Level::Allow
            || self.should_audit(decision.capability)
            || decision.capability == "unknown"
            || decision.capability == "all";
        if worth {
            if let Some(audit) = &self.audit {
                audit.append(&AuditRecord {
                    ts_ms: (self.now)()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis() as u64)
                        .unwrap_or(0),
                    tool: tool.to_string(),
                    capability: decision.capability.to_string(),
                    level: decision.level.as_str().to_string(),
                    decision: if decision.allow { "allow" } else { "deny" }.to_string(),
                    channel: decision.channel.to_string(),
                    args: audit_args_text(args),
                    reason: truncate(&decision.reason, 120),
                    session_id: self.session_id.clone(),
                    turn_id: self.turn_id,
                });
            }
        }
        decision
    }
}

/// 参数摘要：审计/界面里不能塞整段截图 base64，但也不能只剩个工具名。
pub fn summarize_args(args: &serde_json::Value) -> serde_json::Value {
    let Some(object) = args.as_object() else {
        return serde_json::json!({});
    };
    let mut out = serde_json::Map::new();
    for (key, value) in object {
        let summarized = match value {
            // data URL / base64 图片：整段丢掉，只留类型标记。
            serde_json::Value::String(text) if text.starts_with("data:") => {
                serde_json::Value::String("[图片数据已省略]".to_string())
            }
            serde_json::Value::String(text) if text.chars().count() > 80 => {
                let head: String = text.chars().take(80).collect();
                serde_json::Value::String(format!("{head}…"))
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::String(format!("[{}]", items.len()))
            }
            serde_json::Value::Object(_) => serde_json::Value::String("[object]".to_string()),
            other => other.clone(),
        };
        out.insert(key.clone(), summarized);
    }
    serde_json::Value::Object(out)
}

/// 审计里的参数文本：摘要 + 截断，**绝不**包含 base64。
pub fn audit_args_text(args: &serde_json::Value) -> String {
    let summary = summarize_args(args);
    // 再兜一层：任何长串 base64（哪怕藏在普通字段里）都在这层被削掉。
    let text = redact_base64(&summary.to_string());
    truncate(&text, 120)
}

/// 把长得像 base64 的连续片段替换成占位符。
///
/// 只对**足够长**的串生效（≥ 80 个 base64 字符），避免误伤普通英文/数字字段。
fn redact_base64(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' || ch == '=' {
            run.push(ch);
            continue;
        }
        flush_base64_run(&mut out, &mut run);
        out.push(ch);
    }
    flush_base64_run(&mut out, &mut run);
    out
}

fn flush_base64_run(out: &mut String, run: &mut String) {
    const MIN_BASE64_RUN: usize = 80;
    if run.len() >= MIN_BASE64_RUN {
        out.push_str("[省略]");
    } else {
        out.push_str(run);
    }
    run.clear();
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let head: String = value.chars().take(max).collect();
    format!("{head}…")
}

/// 能力的中文标签（界面/拒绝原因用）。
pub fn capability_label(capability: &str) -> &str {
    capability_info(capability)
        .map(|info| info.label)
        .unwrap_or(capability)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;

    fn gate(mode: PermissionMode) -> PermissionState {
        PermissionState::new(mode)
    }

    /// 固定答复的确认器。
    struct Fixed(bool);
    impl Confirmer for Fixed {
        fn confirm<'a>(
            &'a self,
            _request: ConfirmRequest,
            _cancel: &'a crate::cancel::CancelToken,
        ) -> ConfirmFuture<'a> {
            let answer = if self.0 {
                ConfirmAnswer::allow_click()
            } else {
                ConfirmAnswer::deny_click()
            };
            Box::pin(async move { answer })
        }
    }

    const YES: Fixed = Fixed(true);

    fn args(value: serde_json::Value) -> serde_json::Value {
        value
    }

    #[tokio::test]
    async fn screenshot_is_allowed_without_a_prompt() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "screenshot",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.level, Level::Allow);
        assert_eq!(decision.channel, "policy");
    }

    #[tokio::test]
    async fn click_is_allowed_but_audited() {
        let state = gate(PermissionMode::Standard);
        let gate_ref = PermissionGate::new(&state);
        let decision = gate_ref
            .check(
                "left_click",
                &args(serde_json::json!({"x":1,"y":2})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert!(gate_ref.should_audit("input.pointer"));
    }

    #[tokio::test]
    async fn clipboard_read_requires_confirmation() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.level, Level::Ask);
        assert_eq!(decision.channel, "click");
    }

    #[tokio::test]
    async fn denial_blocks_execution() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                Some(&Fixed(false)),
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "deny");
    }

    #[tokio::test]
    async fn ask_without_confirm_channel_is_denied() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "no-channel");
    }

    #[tokio::test]
    async fn unknown_tool_is_always_denied() {
        for mode in [
            PermissionMode::Standard,
            PermissionMode::Permissive,
            PermissionMode::PassThrough,
        ] {
            let state = gate(mode);
            let decision = PermissionGate::new(&state)
                .check(
                    "os_shutdown",
                    &args(serde_json::json!({})),
                    Some(&YES),
                    &CancelToken::new(),
                )
                .await;
            assert!(!decision.allow, "{mode:?} 下未知工具也必须拒绝");
            assert_eq!(decision.capability, "unknown");
        }
    }

    #[tokio::test]
    async fn risky_key_chord_escalates_to_ask() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "key",
                &args(serde_json::json!({"key":"WIN+R"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert_eq!(
            decision.level,
            Level::Deny,
            "没有确认通道时 ask 必须落到拒绝"
        );
        assert_eq!(decision.capability, "system.settings");
    }

    #[tokio::test]
    async fn risky_key_escalation_allows_with_confirmation() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "key",
                &args(serde_json::json!({"key":"win + r"})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.capability, "system.settings");
    }

    #[tokio::test]
    async fn risky_text_escalates_to_ask() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "type",
                &args(serde_json::json!({"text":"rm -rf /tmp/x"})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.capability, "file.delete");
    }

    #[tokio::test]
    async fn ordinary_typing_is_not_escalated() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check(
                "type",
                &args(serde_json::json!({"text":"你好，请帮我写个备忘"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.level, Level::Allow);
    }

    #[tokio::test]
    async fn conservative_mode_promotes_input_to_ask() {
        let state = gate(PermissionMode::Conservative);
        let decision = PermissionGate::new(&state)
            .check(
                "left_click",
                &args(serde_json::json!({"x":1,"y":2})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow, "保守档下点击必须确认（此处无通道→拒绝）");
    }

    #[tokio::test]
    async fn permissive_mode_demotes_clipboard_read() {
        let state = gate(PermissionMode::Permissive);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
    }

    /// 免确认档：已知且可授权的 ask 工具不再询问；破坏性默认 deny 能力也放行。
    #[tokio::test]
    async fn pass_through_allows_known_tools_without_prompting() {
        let state = gate(PermissionMode::PassThrough);
        for (tool, capability) in [
            ("read_clipboard", "clipboard.read"),
            ("key", "input.keyboard"),
        ] {
            let decision = PermissionGate::new(&state)
                .check(
                    tool,
                    &args(serde_json::json!({"key":"win+r"})),
                    None,
                    &CancelToken::new(),
                )
                .await;
            assert!(decision.allow, "免确认下 {tool} 必须放行：{decision:?}");
            assert_eq!(decision.channel, "unattended");
            let _ = capability;
        }
    }

    /// 免确认档仍保留两条硬边界：未知工具与全局暂停。
    #[tokio::test]
    async fn pass_through_keeps_unknown_and_pause_boundaries() {
        let state = gate(PermissionMode::PassThrough);
        let unknown = PermissionGate::new(&state)
            .check(
                "not_a_tool",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!unknown.allow);

        let mut paused = gate(PermissionMode::PassThrough);
        paused.pause(10);
        let decision = PermissionGate::new(&paused)
            .check(
                "left_click",
                &args(serde_json::json!({"x":1,"y":2})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "pause");
    }

    /// 单项覆盖能压过免确认档（刹车），并且对普通档同样生效。
    #[tokio::test]
    async fn explicit_override_beats_mode_and_survives_pass_through() {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("file.delete".to_string(), PermissionOverride::Deny);
        let state = PermissionState::from_settings(PermissionMode::PassThrough, &overrides);
        let decision = PermissionGate::new(&state)
            .check(
                "type",
                &args(serde_json::json!({"text":"rm -rf /tmp/x"})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(
            !decision.allow,
            "覆盖为拒绝时免确认也不能执行：{decision:?}"
        );

        // 覆盖为允许时，标准档的 ask 不再询问。
        let mut allow_overrides = std::collections::BTreeMap::new();
        allow_overrides.insert("clipboard.read".to_string(), PermissionOverride::Allow);
        let state = PermissionState::from_settings(PermissionMode::Standard, &allow_overrides);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.channel, "policy");

        // 「跟随档位」不改变任何判定。
        let mut follow = std::collections::BTreeMap::new();
        follow.insert("clipboard.read".to_string(), PermissionOverride::Follow);
        let state = PermissionState::from_settings(PermissionMode::Standard, &follow);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow, "跟随档位应保持 ask（无通道→拒绝）");
    }

    #[tokio::test]
    async fn pause_blocks_everything() {
        let mut state = gate(PermissionMode::Standard);
        state.pause(10);
        let decision = PermissionGate::new(&state)
            .check(
                "screenshot",
                &args(serde_json::json!({})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "pause");

        // 恢复后立即生效（下一个工具判断按有效策略处理）。
        state.resume();
        let decision = PermissionGate::new(&state)
            .check(
                "screenshot",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
    }

    #[tokio::test]
    async fn grant_once_turns_ask_into_allow() {
        let mut state = gate(PermissionMode::Standard);
        state.grant_once("clipboard.read", 5);
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(decision.allow);
        assert!(!state.active_grants(SystemTime::now()).is_empty());
    }

    /// 限时授权不会解开 deny 或未知工具。
    #[tokio::test]
    async fn grant_once_cannot_unlock_deny_or_unknown() {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("file.delete".to_string(), PermissionOverride::Deny);
        let mut state = PermissionState::from_settings(PermissionMode::Standard, &overrides);
        state.grant_once("file.delete", 5);
        let decision = PermissionGate::new(&state)
            .check(
                "type",
                &args(serde_json::json!({"text":"rm -rf /tmp/x"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow, "deny 不能被限时授权解开");

        state.grant_once("unknown", 5);
        let decision = PermissionGate::new(&state)
            .check(
                "mystery",
                &args(serde_json::json!({})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!decision.allow, "未知工具不能被限时授权解开");
    }

    /// 语音答复走**同一个**权限门：明确肯定才放行，否定与"不明确"都拒绝。
    #[tokio::test]
    async fn voice_answers_settle_the_gate() {
        struct Voice(bool);
        impl Confirmer for Voice {
            fn confirm<'a>(
                &'a self,
                _request: ConfirmRequest,
                _cancel: &'a crate::cancel::CancelToken,
            ) -> ConfirmFuture<'a> {
                let answer = if self.0 {
                    ConfirmAnswer::allow_voice()
                } else {
                    ConfirmAnswer::deny_voice("用户用语音拒绝了这一次操作")
                };
                Box::pin(async move { answer })
            }
        }

        let state = gate(PermissionMode::Standard);
        let allowed = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                Some(&Voice(true)),
                &CancelToken::new(),
            )
            .await;
        assert!(allowed.allow);
        assert_eq!(allowed.channel, "voice", "语音允许要如实记下通道");

        let denied = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                Some(&Voice(false)),
                &CancelToken::new(),
            )
            .await;
        assert!(!denied.allow);
        assert!(denied.reason.contains("语音拒绝"));
    }

    /// 语音通道也不能绕过"取消即终态"：取消后迟到的语音允许不生效。
    #[tokio::test]
    async fn cancelled_turn_ignores_a_late_voice_allow() {
        let state = gate(PermissionMode::Standard);
        let cancel = CancelToken::new();
        cancel.cancel();
        let decision = PermissionGate::new(&state)
            .check(
                "read_clipboard",
                &args(serde_json::json!({})),
                Some(&YES),
                &cancel,
            )
            .await;
        assert!(!decision.allow);
    }

    #[test]
    fn arg_summary_drops_large_payloads() {
        let summary = summarize_args(&serde_json::json!({
            "text": "x".repeat(200),
            "x": 1,
            "list": [1,2,3],
            "obj": {"a": 1}
        }));
        assert!(summary["text"].as_str().unwrap().chars().count() <= 81);
        assert_eq!(summary["x"], serde_json::json!(1));
        assert_eq!(summary["list"], serde_json::json!("[3]"));
        assert_eq!(
            summary["obj"],
            serde_json::Value::String("[object]".to_string())
        );
    }

    /// 审计只记必要信息：不含密钥、不含大块 base64。
    #[test]
    fn audit_records_are_redacted_and_ordered() {
        let dir = unique_root("perm-audit");
        let log = AuditLog::new(dir.join("audit.jsonl"));
        log.append(&AuditRecord {
            ts_ms: 1,
            tool: "read_clipboard".to_string(),
            capability: "clipboard.read".to_string(),
            level: "ask".to_string(),
            decision: "allow".to_string(),
            channel: "click".to_string(),
            args: audit_args_text(&serde_json::json!({
                "text": "x".repeat(300),
                "image": "data:image/png;base64,".to_string() + &"A".repeat(500)
            })),
            reason: String::new(),
            session_id: Some("s1".to_string()),
            turn_id: Some(3),
        });
        let records = log.recent(10);
        assert_eq!(records.len(), 1);
        assert!(records[0].args.chars().count() <= 121);
        assert!(!records[0].args.contains("sk-"));
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(!text.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unique_root(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("{prefix}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---------------------------------------------------------------
    // PR12：新工具的能力映射与 files 的按 action 分级
    // ---------------------------------------------------------------

    #[test]
    fn restored_tools_map_to_the_legacy_capabilities() {
        for (tool, capability) in [
            ("run_shell", "system.shell"),
            ("web_search", "web.search"),
            ("launch_app", "app.launch"),
            ("kill_app", "app.close"),
            ("system_info", "system.info"),
            ("app_control", "window.read"),
            ("get_current_time", "none"),
            ("files", "file.read"),
        ] {
            assert_eq!(tool_capability(tool), Some(capability), "{tool}");
        }
        // 旧版工具映射不能被这次改动挪走。
        assert_eq!(tool_capability("screenshot"), Some("screen.capture"));
        assert_eq!(tool_capability("read_clipboard"), Some("clipboard.read"));
        assert_eq!(tool_capability("open_application"), Some("app.launch"));
        assert_eq!(tool_capability("not_a_tool"), None);
    }

    /// **删除绝不能被伪装成读取**：这是本 PR 最容易犯的错。
    #[test]
    fn files_actions_map_to_their_own_capabilities() {
        for action in ["list", "read", "info"] {
            let verdict = classify("files", &serde_json::json!({"action": action}));
            assert_eq!(verdict.capability, "file.read", "{action}");
            assert_eq!(verdict.level, Level::Allow, "{action} 只读应当直接放行");
        }
        for action in ["write", "copy", "move"] {
            let verdict = classify("files", &serde_json::json!({"action": action}));
            assert_eq!(verdict.capability, "file.write", "{action}");
            assert_eq!(verdict.level, Level::Ask, "{action} 必须询问");
        }
        let verdict = classify("files", &serde_json::json!({"action": "delete"}));
        assert_eq!(verdict.capability, "file.delete");
        assert_eq!(verdict.level, Level::Ask);
        assert_ne!(verdict.capability, "file.read");
        assert!(verdict.reason.contains("action=delete"), "确认卡要说明动作");
    }

    /// 大小写/空白不能绕过分级；漏 action 时归到读取，但会在执行前被参数校验拒绝。
    #[test]
    fn files_action_parsing_is_case_insensitive_and_fails_closed() {
        let verdict = classify("files", &serde_json::json!({"action": " DELETE "}));
        assert_eq!(verdict.capability, "file.delete");
        let verdict = classify("files", &serde_json::json!({"action": "Write"}));
        assert_eq!(verdict.capability, "file.write");
        let verdict = classify("files", &serde_json::json!({}));
        assert_eq!(verdict.capability, "file.read");
        assert!(verdict.reason.contains("没有给 action"));
    }

    /// 只读工具在标准档**不需要**逐次确认：这是"日常便利不被安全改造牺牲"的落点。
    #[tokio::test]
    async fn read_only_new_tools_do_not_prompt_in_standard_mode() {
        let state = gate(PermissionMode::Standard);
        for (tool, args) in [
            ("system_info", serde_json::json!({})),
            ("app_control", serde_json::json!({"action":"list"})),
            ("get_current_time", serde_json::json!({})),
            ("web_search", serde_json::json!({"query":"天气"})),
            ("files", serde_json::json!({"action":"list","path":"C:\\"})),
        ] {
            let decision = PermissionGate::new(&state)
                .check(tool, &args, None, &CancelToken::new())
                .await;
            assert!(decision.allow, "{tool} 不该要求确认：{decision:?}");
            assert_eq!(decision.level, Level::Allow, "{tool}");
        }
    }

    #[tokio::test]
    async fn shell_and_delete_still_require_confirmation() {
        let state = gate(PermissionMode::Standard);
        let shell = PermissionGate::new(&state)
            .check(
                "run_shell",
                &args(serde_json::json!({"command":"Get-Date"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!shell.allow, "没有确认通道时 run_shell 必须落到拒绝");
        assert_eq!(shell.capability, "system.shell");

        let delete = PermissionGate::new(&state)
            .check(
                "files",
                &args(serde_json::json!({"action":"delete","path":"C:\\tmp\\a.txt"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!delete.allow);
        assert_eq!(delete.capability, "file.delete");

        // 有确认通道时点击允许即可执行。
        let allowed = PermissionGate::new(&state)
            .check(
                "run_shell",
                &args(serde_json::json!({"command":"Get-Date"})),
                Some(&YES),
                &CancelToken::new(),
            )
            .await;
        assert!(allowed.allow);
        assert_eq!(allowed.channel, "click");
    }

    /// 免确认档下新工具同样不再询问；但未知工具、暂停与显式 deny 仍然生效。
    #[tokio::test]
    async fn pass_through_covers_new_tools_but_keeps_hard_limits() {
        let state = gate(PermissionMode::PassThrough);
        for tool in ["run_shell", "kill_app", "launch_app"] {
            let decision = PermissionGate::new(&state)
                .check(
                    tool,
                    &args(serde_json::json!({"command":"x","name":"notepad","query":"微信"})),
                    None,
                    &CancelToken::new(),
                )
                .await;
            assert!(decision.allow, "{tool} 在免确认下应放行：{decision:?}");
            assert_eq!(decision.channel, "unattended");
        }
    }

    /// 宽松档把 `file.write` / `file.delete` 降为放行，但 `system.shell` 仍是 ask。
    #[tokio::test]
    async fn permissive_mode_demotes_file_actions_but_not_shell() {
        let state = gate(PermissionMode::Permissive);
        let delete = PermissionGate::new(&state)
            .check(
                "files",
                &args(serde_json::json!({"action":"delete","path":"C:\\tmp\\a.txt"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(delete.allow, "宽松档下删除放行：{delete:?}");
        let shell = PermissionGate::new(&state)
            .check(
                "run_shell",
                &args(serde_json::json!({"command":"Get-Date"})),
                None,
                &CancelToken::new(),
            )
            .await;
        assert!(!shell.allow, "宽松档不放开执行命令");
    }

    #[test]
    fn web_search_capability_is_registered_and_audited() {
        assert!(capabilities().iter().any(|item| item.id == "web.search"));
        let info = capabilities()
            .iter()
            .find(|item| item.id == "web.search")
            .unwrap();
        assert_eq!(info.level, Level::Allow);
        assert!(info.audit, "查询词会发给第三方，必须留痕");
    }
}
