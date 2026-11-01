//! 权限引擎（对应 `packages/agent/permissions.mjs`）。
//!
//! 唯一能说"能不能做"的地方。所有工具调用都必须先过这里，包括 PR5 的 12 个 CUA 工具。
//!
//! 三个概念分开：
//!  - 能力（capability）：按"能造成什么后果"划分（看屏幕 / 点鼠标 / 敲键盘…）
//!  - 级别（level）：allow / ask / deny，回答"这个能力现在允许到什么程度"
//!  - 作用域：PR5 只做时间维度（暂停），应用/目录作用域留待后续
//!
//! 默认取向与旧版一致：低风险放行保证流畅，不可逆动作询问，破坏性系统操作拒绝。
//! **未知工具一律 deny**——不在白名单里的工具不该因为任何档位被放行。

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::PermissionMode;

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
}

/// 一次分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub capability: &'static str,
    pub level: Level,
    /// 定性说明（升级原因 / 拒绝原因），会显示给用户。
    pub reason: String,
    /// 是否因具体参数（Win+R、rm -rf 之类）被风险升级。
    pub escalated: bool,
}

/// 能力默认级别。
struct CapabilityInfo {
    id: &'static str,
    label: &'static str,
    level: Level,
    audit: bool,
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

fn capability_info(id: &str) -> Option<&'static CapabilityInfo> {
    CAPABILITIES.iter().find(|item| item.id == id)
}

/// 工具名 → 能力。没有映射的工具一律 deny。
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
                };
            }
        }
    }

    let base = capability_info(capability)
        .map(|info| info.level)
        .unwrap_or(Level::Ask);
    Classification {
        capability,
        level: base,
        reason: String::new(),
        escalated: false,
    }
}

/// 权限状态（运行期可改，`paused_until` / `allowed_once` 属于会话状态）。
#[derive(Debug, Clone)]
pub struct PermissionState {
    pub mode: PermissionMode,
    /// 全局暂停：任何工具都不执行。
    pub paused_until: Option<SystemTime>,
    /// 本次会话内"一次性允许"的能力 → 到期时间。
    pub allowed_once: Vec<(&'static str, SystemTime)>,
}

impl Default for PermissionState {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Standard,
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

    /// 合并"档位默认 + 过期临时授权"得出最终级别。
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
            // 免确认：连 deny 也放开（否则"完全权限"名不副实）。
            // 未知工具在被调用前就已按未知拒绝，不会走到这里。
            PermissionMode::PassThrough => Level::Allow,
            PermissionMode::Standard => base,
        };
        if level == Level::Ask {
            let granted = self
                .allowed_once
                .iter()
                .any(|(id, until)| *id == capability && *until > now);
            if granted {
                level = Level::Allow;
            }
        }
        level
    }

    pub fn grant_once(&mut self, capability: &'static str, minutes: u64) {
        let until = SystemTime::now() + Duration::from_secs(minutes * 60);
        self.allowed_once.retain(|(id, _)| *id != capability);
        self.allowed_once.push((capability, until));
    }

    pub fn pause(&mut self, minutes: u64) {
        self.paused_until = Some(SystemTime::now() + Duration::from_secs(minutes * 60));
    }

    pub fn is_paused(&self, now: SystemTime) -> bool {
        matches!(self.paused_until, Some(until) if until > now)
    }
}

/// 权限决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub level: Level,
    pub capability: &'static str,
    /// 决策通道：`policy` / `pause` / `no-channel` / `click` / `deny`。
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
/// PR5 只有**点击确认**（界面上的允许/拒绝按钮）。语音确认由语音阶段实现；
/// 这里的签名不假设音频存在，也不影响旧版语音确认。
///
/// 为什么是异步：确认要绕到 Slint 主线程再等用户点按钮，
/// 权限引擎自己不能阻塞，也不能碰 UI 细节。
pub type ConfirmFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ConfirmAnswer> + Send + 'a>>;

pub trait Confirmer: Send + Sync {
    fn confirm<'a>(&'a self, request: ConfirmRequest) -> ConfirmFuture<'a>;
}

/// `Arc<T>` 也能当确认器用：界面把确认器交给后台任务时不需要额外包装。
impl<T: Confirmer + ?Sized> Confirmer for std::sync::Arc<T> {
    fn confirm<'a>(&'a self, request: ConfirmRequest) -> ConfirmFuture<'a> {
        (**self).confirm(request)
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
}

/// 权限门：所有工具调用的唯一入口。
pub struct PermissionGate<'a> {
    state: &'a PermissionState,
    now: Box<dyn Fn() -> SystemTime + Send + Sync>,
}

impl<'a> PermissionGate<'a> {
    pub fn new(state: &'a PermissionState) -> Self {
        Self {
            state,
            now: Box::new(SystemTime::now),
        }
    }

    /// 判定一次调用。`confirm` 为 `None` 时，`ask` 一律拒绝（没有确认通道就不该放行）。
    pub async fn check(
        &self,
        tool: &str,
        args: &serde_json::Value,
        confirm: Option<&dyn Confirmer>,
    ) -> Decision {
        let now = (self.now)();

        // 全局暂停优先于一切。
        if self.state.is_paused(now) {
            let minutes = self
                .state
                .paused_until
                .and_then(|until| until.duration_since(now).ok())
                .map(|left| left.as_secs().div_ceil(60))
                .unwrap_or(0);
            return Decision {
                allow: false,
                level: Level::Deny,
                capability: "all",
                channel: "pause",
                reason: format!("已暂停 {minutes} 分钟，期间不执行任何操作"),
            };
        }

        let verdict = classify(tool, args);
        if verdict.level == Level::Deny {
            return Decision {
                allow: false,
                level: Level::Deny,
                capability: verdict.capability,
                channel: "policy",
                reason: verdict.reason,
            };
        }

        let mut level = self.state.resolve_level(verdict.capability, now);
        // 风险升级（Win+R、rm -rf 之类）默认强制 ask。
        if verdict.escalated {
            level = Level::Ask;
        }

        match level {
            Level::Allow => Decision {
                allow: true,
                level,
                capability: verdict.capability,
                channel: "policy",
                reason: verdict.reason,
            },
            Level::Deny => {
                let label = capability_info(verdict.capability)
                    .map(|info| info.label)
                    .unwrap_or(verdict.capability);
                Decision {
                    allow: false,
                    level,
                    capability: verdict.capability,
                    channel: "policy",
                    reason: format!("「{label}」已被设为拒绝"),
                }
            }
            Level::Ask => {
                let Some(confirm) = confirm else {
                    return Decision {
                        allow: false,
                        level: Level::Deny,
                        capability: verdict.capability,
                        channel: "no-channel",
                        reason: "需要确认，但当前没有可用的确认通道".to_string(),
                    };
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
                let answer = confirm.confirm(request).await;
                let allowed = answer.allow && matches!(answer.channel, "click" | "voice");
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
                }
            }
        }
    }

    /// 该能力是否需要留痕（审计日志用）。
    pub fn should_audit(&self, capability: &str) -> bool {
        capability_info(capability)
            .map(|info| info.audit)
            .unwrap_or(true)
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

/// 能力的中文标签（界面/拒绝原因用）。
pub fn capability_label(capability: &str) -> &str {
    capability_info(capability)
        .map(|info| info.label)
        .unwrap_or(capability)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(mode: PermissionMode) -> PermissionState {
        PermissionState::new(mode)
    }

    /// 固定答复的确认器：PR5 只有点击确认。
    struct Fixed(bool);
    impl Confirmer for Fixed {
        fn confirm<'a>(&'a self, _request: ConfirmRequest) -> ConfirmFuture<'a> {
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
            .check("screenshot", &args(serde_json::json!({})), None)
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
            .check("left_click", &args(serde_json::json!({"x":1,"y":2})), None)
            .await;
        assert!(decision.allow);
        assert!(gate_ref.should_audit("input.pointer"));
    }

    #[tokio::test]
    async fn clipboard_read_requires_confirmation() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check("read_clipboard", &args(serde_json::json!({})), Some(&YES))
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
            )
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "deny");
    }

    #[tokio::test]
    async fn ask_without_confirm_channel_is_denied() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check("read_clipboard", &args(serde_json::json!({})), None)
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "no-channel");
    }

    #[tokio::test]
    async fn unknown_tool_is_always_denied() {
        let state = gate(PermissionMode::Permissive);
        let decision = PermissionGate::new(&state)
            .check("os_shutdown", &args(serde_json::json!({})), Some(&YES))
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.capability, "unknown");
    }

    #[tokio::test]
    async fn risky_key_chord_escalates_to_ask() {
        let state = gate(PermissionMode::Standard);
        let decision = PermissionGate::new(&state)
            .check("key", &args(serde_json::json!({"key":"WIN+R"})), None)
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
            )
            .await;
        assert!(decision.allow);
        assert_eq!(decision.level, Level::Allow);
    }

    #[tokio::test]
    async fn conservative_mode_promotes_input_to_ask() {
        let state = gate(PermissionMode::Conservative);
        let decision = PermissionGate::new(&state)
            .check("left_click", &args(serde_json::json!({"x":1,"y":2})), None)
            .await;
        assert!(!decision.allow, "保守档下点击必须确认（此处无通道→拒绝）");
    }

    #[tokio::test]
    async fn permissive_mode_demotes_clipboard_read() {
        let state = gate(PermissionMode::Permissive);
        let decision = PermissionGate::new(&state)
            .check("read_clipboard", &args(serde_json::json!({})), None)
            .await;
        assert!(decision.allow);
    }

    #[tokio::test]
    async fn pause_blocks_everything() {
        let mut state = gate(PermissionMode::Standard);
        state.pause(10);
        let decision = PermissionGate::new(&state)
            .check("screenshot", &args(serde_json::json!({})), Some(&YES))
            .await;
        assert!(!decision.allow);
        assert_eq!(decision.channel, "pause");
    }

    #[tokio::test]
    async fn grant_once_turns_ask_into_allow() {
        let mut state = gate(PermissionMode::Standard);
        state.grant_once("clipboard.read", 5);
        let decision = PermissionGate::new(&state)
            .check("read_clipboard", &args(serde_json::json!({})), None)
            .await;
        assert!(decision.allow);
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
}
