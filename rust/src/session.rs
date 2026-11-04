//! 多会话：稳定 id、原子保存、损坏隔离、时间线恢复。
//!
//! 设计要点（对应任务书第 1 条）：
//!  - **一个 Session 一份文件**：`sessions/<id>.json`。某个文件坏了只影响它自己，
//!    绝不在读取时把别的健康会话一起覆盖；
//!  - **索引与内容分开**：`sessions/index.json` 只存标题/更新时间/当前选中项，
//!    它是"列表"的数据源；条目丢了可以按会话文件重建；
//!  - **原子写**：先写 `.tmp` 再改名，失败不动旧文件（与 `config::save_settings` 同一套规则）；
//!  - **历史与时间线分开存**：`history` 是真正发给模型的消息（兼容 `ChatMessage` /
//!    `context.rs` 压缩与工具配对语义），`timeline` 是用户看到的气泡与工具卡片。
//!    压缩会缩短 `history`，时间线因此**不能**从它推导；
//!  - **附件有界**：截图 base64 不进会话文件（见 `ChatMessage::for_storage`），
//!    只保留文字说明，恢复后结构仍然合法；
//!  - **未完成回合**：保存时把 `status` 标成 `interrupted` / `cancelled`，
//!    恢复界面明确显示"上一轮没有正常结束"，并且**不会自动重放**任何工具调用。
//!
//! 不做的事：不引入数据库、不做跨会话并行 Agent、不把密钥写进会话文件。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{AppPaths, SessionModelChoice};
use crate::llm::ChatMessage;

/// 会话文件结构版本（只用于识别，不做破坏性改写）。
pub const SESSION_SCHEMA_VERSION: u32 = 1;
/// 单条时间线文本上限（防止一条超长工具结果把界面卡死）。
pub const TIMELINE_TEXT_MAX_CHARS: usize = 4_000;
/// 单个会话最多保留的时间线条目（超出从最老的开始丢）。
pub const MAX_TIMELINE_ITEMS: usize = 400;
/// 会话列表最多保留的会话数（超出不会自动删，只提醒）。
pub const MAX_SESSIONS_WARN: usize = 200;

/// 一个回合的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TurnStatus {
    /// 正常结束（模型给出了可见回复）。
    #[default]
    Completed,
    /// 用户取消。
    Cancelled,
    /// 失败（网络/协议/上下文装不下）。
    Failed,
    /// 进程在回合中途退出（恢复时看到这个状态）。
    Interrupted,
}

impl TurnStatus {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Completed => "已完成",
            Self::Cancelled => "已取消",
            Self::Failed => "失败",
            Self::Interrupted => "异常中断",
        }
    }

    /// 是否需要界面明确提示"这一轮没有正常结束"。
    pub fn needs_warning(&self) -> bool {
        !matches!(self, Self::Completed)
    }
}

/// 时间线条目种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineKind {
    /// 用户说的话（文本或语音）。
    User,
    /// 助手可见回复。
    Assistant,
    /// 真实思考内容（模型返回了 `reasoning_content`）。
    Reasoning,
    /// 一次工具调用（按调用实例一条，连续同名工具也是多条）。
    Tool,
    /// 系统提示 / 失败 / 取消说明。
    System,
}

impl TimelineKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Reasoning => "reasoning",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }
}

/// 工具调用在时间线上的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineToolStatus {
    /// 正在执行（进程中途退出时会留下这个状态，恢复时按 interrupted 处理）。
    #[default]
    Running,
    Ok,
    Failed,
    Denied,
    Cancelled,
}

impl TimelineToolStatus {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "执行中",
            Self::Ok => "成功",
            Self::Failed => "失败",
            Self::Denied => "已拒绝",
            Self::Cancelled => "已取消",
        }
    }
}

/// 一条时间线记录（持久化形态）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineItem {
    pub kind: TimelineKind,
    /// 归属回合（0 = 不属于任何回合，例如系统提示）。
    #[serde(default)]
    pub turn_id: u64,
    #[serde(default)]
    pub step: usize,
    /// 工具调用的稳定实例 id（同一回合内唯一）。
    #[serde(default)]
    pub call_id: u64,
    /// 展示文本（用户/助手/思考/系统）。
    #[serde(default)]
    pub text: String,
    /// 工具名（`kind == Tool` 时有值）。
    #[serde(default)]
    pub tool: String,
    /// 工具状态。
    #[serde(default)]
    pub status: TimelineToolStatus,
    /// 折叠展示的参数摘要（JSON 文本，不含截图 base64）。
    #[serde(default)]
    pub args_summary: String,
    /// 耗时（毫秒；思考条目表示该步耗时）。
    #[serde(default)]
    pub ms: u64,
    /// 写入时间（Unix 毫秒）。
    #[serde(default)]
    pub at_ms: u64,
}

impl TimelineItem {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            kind: TimelineKind::User,
            text: text.into(),
            ..Self::default()
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            kind: TimelineKind::System,
            text: text.into(),
            ..Self::default()
        }
    }

    /// 文本按上限截断（保留尾部说明，绝不静默丢内容）。
    pub fn truncated(mut self) -> Self {
        let length = self.text.chars().count();
        if length > TIMELINE_TEXT_MAX_CHARS {
            let head: String = self.text.chars().take(TIMELINE_TEXT_MAX_CHARS).collect();
            self.text = format!("{head}…（原文 {length} 字，已截断）");
        }
        self
    }
}

impl Default for TimelineItem {
    fn default() -> Self {
        Self {
            kind: TimelineKind::System,
            turn_id: 0,
            step: 0,
            call_id: 0,
            text: String::new(),
            tool: String::new(),
            status: TimelineToolStatus::Running,
            args_summary: String::new(),
            ms: 0,
            at_ms: 0,
        }
    }
}

/// 一个会话的完整内容（落盘形态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFile {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    /// 创建时间（Unix 毫秒）。
    #[serde(default)]
    pub created_at_ms: u64,
    /// 最近更新时间（Unix 毫秒）。
    #[serde(default)]
    pub updated_at_ms: u64,
    /// 会话级模型选择（可空 = 跟随全局/时段方案）。
    #[serde(default)]
    pub model_choice: SessionModelChoice,
    /// 最近一次回合的状态（恢复界面据此提示"未完成"）。
    #[serde(default)]
    pub last_turn_status: TurnStatus,
    /// 最近一次回合 id（新回合递增）。
    #[serde(default)]
    pub last_turn_id: u64,
    /// 发送给模型的**有效历史**（已压缩、工具配对完整）。
    #[serde(default)]
    pub history: Vec<ChatMessage>,
    /// 用户可见的完整时间线（不因压缩而缩水）。
    #[serde(default)]
    pub timeline: Vec<TimelineItem>,
}

fn default_schema() -> u32 {
    SESSION_SCHEMA_VERSION
}

impl SessionFile {
    /// 新建一个空会话。
    pub fn new(id: impl Into<String>, title: impl Into<String>, now_ms: u64) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: id.into(),
            title: title.into(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            model_choice: SessionModelChoice::default(),
            last_turn_status: TurnStatus::Completed,
            last_turn_id: 0,
            history: Vec::new(),
            timeline: Vec::new(),
        }
    }

    /// 列表里显示的一行。
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            updated_at_ms: self.updated_at_ms,
            last_turn_status: self.last_turn_status,
            turns: self.history.len(),
        }
    }

    /// 追加一条时间线（按上限裁剪最老的）。
    pub fn push_timeline(&mut self, item: TimelineItem) {
        let item = item.truncated();
        self.timeline.push(item);
        if self.timeline.len() > MAX_TIMELINE_ITEMS {
            self.timeline
                .drain(0..self.timeline.len() - MAX_TIMELINE_ITEMS);
        }
    }

    /// 恢复一个会话后要做的一致性修补：
    ///  - 上一次退出时还在"执行中"的工具卡片 → 标成已取消（绝不假装成功）；
    ///  - 上一次回合不是正常结束 → 状态改为"异常中断"，界面据此提示。
    pub fn repair_after_restore(&mut self) {
        let mut touched = false;
        for item in &mut self.timeline {
            if item.kind == TimelineKind::Tool && item.status == TimelineToolStatus::Running {
                item.status = TimelineToolStatus::Cancelled;
                if item.text.is_empty() {
                    item.text = "（程序在这一次调用中途退出，结果未知）".to_string();
                }
                touched = true;
            }
        }
        if self.last_turn_status == TurnStatus::Completed && touched {
            self.last_turn_status = TurnStatus::Interrupted;
        }
    }

    /// 保存前的有界化：附件不落盘、时间线截断。
    pub fn for_storage(mut self) -> Self {
        for message in &mut self.history {
            *message = message.for_storage();
        }
        self.timeline = self
            .timeline
            .into_iter()
            .map(TimelineItem::truncated)
            .collect();
        self
    }
}

/// 会话列表的一行（索引与列表页共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at_ms: u64,
    pub last_turn_status: TurnStatus,
    /// 历史消息条数（只是粗略指示，不当作 UI 内容）。
    pub turns: usize,
}

/// 会话索引（只存列表元数据 + 当前选中项）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndex {
    #[serde(default)]
    pub current_id: String,
    #[serde(default)]
    pub sessions: Vec<SessionSummary>,
    /// 索引文件损坏 / 缺失时，本次启动按会话文件重建（界面据 library 里的提示说明）。
    #[serde(skip)]
    pub rebuilt: bool,
    /// 重建时发现损坏、被隔离的会话文件（界面要能如实说明"某个会话读不出来"）。
    #[serde(skip)]
    pub corrupt: Vec<String>,
}

/// 会话库：读写、创建、改名、删除、恢复。
pub struct SessionStore {
    paths: AppPaths,
}

/// 一次读取会话库的结果。
#[derive(Debug, Clone)]
pub struct SessionLibrary {
    pub index: SessionIndex,
    /// 已加载的会话（`current_id` 一定在这里，除非库是空的）。
    pub current: Option<SessionFile>,
}

impl SessionStore {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    fn dir(&self) -> PathBuf {
        self.paths.sessions_dir()
    }

    fn index_path(&self) -> PathBuf {
        self.paths.session_index_file()
    }

    fn file_path(&self, session_id: &str) -> PathBuf {
        self.paths.session_file(&safe_session_id(session_id))
    }

    /// 读取整个会话库。
    ///
    /// 损坏的文件**不会**被覆盖或删除：
    ///  - 索引坏了 → 按会话文件重建索引；
    ///  - 某个会话文件坏了 → 只跳过它，并在 `corrupt` 里记下 id，健康会话照常可读。
    pub fn load(&self) -> SessionLibrary {
        let mut index = self.read_index();
        // 以磁盘上的会话文件为准核对列表（索引可能落后或被删）：
        // 只做"补上缺失的会话、去掉已经不存在的条目"，不搬动用户数据。
        let on_disk = self.scan_session_ids();
        let mut summaries: Vec<SessionSummary> = Vec::new();
        let mut corrupt: Vec<String> = Vec::new();
        for id in &on_disk {
            match self.read_session(id) {
                Ok(session) => summaries.push(session.summary()),
                Err(_) => corrupt.push(id.clone()),
            }
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at_ms));
        if summaries.len() > 1 {
            // 索引里已有的标题优先（改名后不必重读全部会话文件）。
            for summary in &mut summaries {
                if let Some(existing) = index.sessions.iter().find(|item| item.id == summary.id) {
                    if !existing.title.trim().is_empty() {
                        summary.title = existing.title.clone();
                    }
                }
            }
        }
        index.sessions = summaries;
        index.corrupt = corrupt;
        index.rebuilt = index.rebuilt || !self.index_path().is_file();

        // 当前选中项：索引里记的仍存在就用它，否则用最近更新的。
        let current_id = if index
            .sessions
            .iter()
            .any(|summary| summary.id == index.current_id)
        {
            index.current_id.clone()
        } else {
            index
                .sessions
                .first()
                .map(|summary| summary.id.clone())
                .unwrap_or_default()
        };
        index.current_id = current_id.clone();

        let current = if current_id.is_empty() {
            None
        } else {
            self.read_session(&current_id).ok()
        };
        SessionLibrary { index, current }
    }

    /// 读取索引（坏了就当空索引，由 `load` 重建）。
    fn read_index(&self) -> SessionIndex {
        let Ok(raw) = std::fs::read_to_string(self.index_path()) else {
            return SessionIndex::default();
        };
        match serde_json::from_str::<SessionIndex>(&raw) {
            Ok(index) => index,
            Err(error) => {
                eprintln!("[session] 索引损坏，将按会话文件重建：{error}");
                SessionIndex {
                    rebuilt: true,
                    ..SessionIndex::default()
                }
            }
        }
    }

    fn scan_session_ids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.dir()) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    return None;
                }
                let name = path.file_stem()?.to_str()?.to_string();
                if name == "index" || name.ends_with(".tmp") || name.ends_with(".bak") {
                    return None;
                }
                Some(name)
            })
            .collect()
    }

    /// 读一个会话（不存在 → `NotFound`；损坏 → `Corrupt`，**不覆盖**）。
    pub fn read_session(&self, session_id: &str) -> Result<SessionFile, SessionError> {
        let path = self.file_path(session_id);
        if !path.is_file() {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            SessionError::Corrupt(format!("读取会话失败（{}）：{error}", path.display()))
        })?;
        let mut session: SessionFile = serde_json::from_str(&raw).map_err(|error| {
            SessionError::Corrupt(format!("会话文件无法解析（{}）：{error}", path.display()))
        })?;
        if session.id != session_id {
            // 文件里的 id 与文件名不一致：以文件名为准并修正内存副本，
            // 避免出现"列表里两个同 id"的重复项。磁盘不静默改写。
            session.id = session_id.to_string();
        }
        Ok(session)
    }

    /// 原子写一个会话（**绝不**先删旧文件）。
    pub fn save_session(&self, session: &SessionFile) -> Result<SessionSummary, SessionError> {
        std::fs::create_dir_all(self.dir()).map_err(|error| {
            SessionError::Io(crate::config::describe_write_error(
                &format!("无法创建会话目录（{}）", self.dir().display()),
                &error,
            ))
        })?;
        let path = self.file_path(&session.id);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(session)
            .map_err(|error| SessionError::Io(format!("序列化会话失败：{error}")))?;
        std::fs::write(&tmp, text).map_err(|error| {
            SessionError::Io(crate::config::describe_write_error(
                &format!("写入会话失败（{}）", tmp.display()),
                &error,
            ))
        })?;

        if !path.exists() {
            std::fs::rename(&tmp, &path)
                .map_err(|error| SessionError::Io(format!("提交会话失败：{error}")))?;
            return Ok(session.summary());
        }
        let backup = path.with_extension("json.bak");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&path, &backup)
            .map_err(|error| SessionError::Io(format!("提交会话失败：{error}")))?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&backup);
                Ok(session.summary())
            }
            Err(error) => {
                let _ = std::fs::rename(&backup, &path);
                let _ = std::fs::remove_file(&tmp);
                Err(SessionError::Io(format!("提交会话失败：{error}")))
            }
        }
    }

    /// 保存索引（原子写；失败不影响会话文件本身）。
    pub fn save_index(&self, index: &SessionIndex) -> Result<(), SessionError> {
        std::fs::create_dir_all(self.dir()).map_err(|error| {
            SessionError::Io(crate::config::describe_write_error(
                &format!("无法创建会话目录（{}）", self.dir().display()),
                &error,
            ))
        })?;
        let path = self.index_path();
        let tmp = path.with_extension("tmp");
        // `rebuilt` / `corrupt` 是运行期状态，不落盘。
        let persisted = SessionIndex {
            current_id: index.current_id.clone(),
            sessions: index.sessions.clone(),
            rebuilt: false,
            corrupt: Vec::new(),
        };
        let text = serde_json::to_string_pretty(&persisted)
            .map_err(|error| SessionError::Io(format!("序列化索引失败：{error}")))?;
        std::fs::write(&tmp, text).map_err(|error| {
            SessionError::Io(crate::config::describe_write_error(
                &format!("写入索引失败（{}）", tmp.display()),
                &error,
            ))
        })?;
        // 索引可以丢（会话文件才是事实源），所以直接覆盖是安全的；
        // 但为了同样的原子性，仍然先删旧再改名（旧内容已可重建）。
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                std::fs::rename(&tmp, &path)
                    .map_err(|error| SessionError::Io(format!("提交索引失败：{error}")))
            }
        }
    }

    /// 删除一个会话（文件与索引条目）。
    pub fn delete_session(&self, session_id: &str) -> Result<(), SessionError> {
        let path = self.file_path(session_id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SessionError::Io(format!(
                    "删除会话失败（{}）：{error}",
                    path.display()
                )))
            }
        }
        // 顺带清掉可能残留的临时/备份文件（它们不属于任何有效会话）。
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
        let _ = std::fs::remove_file(path.with_extension("json.bak"));
        Ok(())
    }

    /// 是否存在。
    pub fn exists(&self, session_id: &str) -> bool {
        self.file_path(session_id).is_file()
    }

    /// 会话目录（供界面显示位置）。
    pub fn dir_display(&self) -> &Path {
        // 借用 `paths` 的根：调用方只用来展示。
        &self.paths.root
    }
}

/// 会话错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// 找不到这个会话。
    NotFound(String),
    /// 文件存在但读不出来/解析不了（**不覆盖**）。
    Corrupt(String),
    /// IO 失败。
    Io(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "会话不存在：{id}"),
            Self::Corrupt(message) => formatter.write_str(message),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

/// 会话 id 用作文件名前先收敛（只保留字母数字与 `-`/`_`）。
pub fn safe_session_id(session_id: &str) -> String {
    let cleaned: String = session_id
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

/// 生成一个新的会话 id（时间戳 + 序号，避免同一毫秒内重复）。
pub fn new_session_id(now_ms: u64, sequence: u64) -> String {
    format!("s{now_ms:x}-{sequence:x}")
}

/// 标题规整：去空白、限长、空标题退回"新会话"。
pub fn normalize_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "新会话".to_string();
    }
    let clipped: String = trimmed.chars().take(60).collect();
    clipped
}

/// 从一条用户输入推测会话标题（首条消息的前 24 字）。
pub fn title_from_input(input: &str) -> String {
    let squeezed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if squeezed.is_empty() {
        return "新会话".to_string();
    }
    let clipped: String = squeezed.chars().take(24).collect();
    if squeezed.chars().count() > 24 {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Content, Role};

    fn unique_root(prefix: &str) -> AppPaths {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("{prefix}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        AppPaths::with_root(dir)
    }

    fn sample(id: &str, text: &str) -> SessionFile {
        let mut session = SessionFile::new(id, text, 1_000);
        session.history.push(ChatMessage::user(text));
        session.timeline.push(TimelineItem::user(text));
        session
    }

    /// 多个会话互不覆盖，重启后都能恢复，当前选中项也保留。
    #[test]
    fn multiple_sessions_round_trip_and_keep_selection() {
        let paths = unique_root("session-roundtrip");
        let store = SessionStore::new(paths.clone());
        for (index, title) in ["会话一", "会话二", "会话三"].iter().enumerate() {
            let mut session = sample(&format!("s{index}"), title);
            session.updated_at_ms = 1_000 + index as u64;
            store.save_session(&session).unwrap();
        }
        let mut index = SessionIndex {
            current_id: "s1".to_string(),
            ..SessionIndex::default()
        };
        store.save_index(&index).unwrap();

        let library = store.load();
        assert_eq!(library.index.sessions.len(), 3);
        assert_eq!(library.index.current_id, "s1");
        let current = library.current.expect("当前会话必须被加载");
        assert_eq!(current.id, "s1");
        assert_eq!(current.title, "会话二");

        // 换选中项 → 重新读取仍然是它。
        index.current_id = "s2".to_string();
        store.save_index(&index).unwrap();
        let library = store.load();
        assert_eq!(library.index.current_id, "s2");

        // 列表按最近更新时间排序（新的在前）。
        assert_eq!(library.index.sessions[0].id, "s2");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// 一个会话文件坏了：只隔离它，别的会话与索引不受影响。
    #[test]
    fn corrupt_session_is_isolated_and_does_not_take_down_others() {
        let paths = unique_root("session-corrupt");
        let store = SessionStore::new(paths.clone());
        store.save_session(&sample("healthy-a", "健康 A")).unwrap();
        store.save_session(&sample("healthy-b", "健康 B")).unwrap();
        // 人为写坏一个文件（截断 JSON）。
        std::fs::write(paths.session_file("broken"), "{\"id\": \"broken\", \"titl").unwrap();

        let library = store.load();
        assert_eq!(library.index.sessions.len(), 2, "健康会话必须照常列出");
        assert!(library
            .index
            .sessions
            .iter()
            .any(|summary| summary.id == "healthy-a"));
        assert!(
            library.index.corrupt.iter().any(|id| id == "broken"),
            "损坏会话要能被如实指出：{:?}",
            library.index.corrupt
        );
        // 坏文件**不被覆盖**（用户也许能手工修复）。
        let raw = std::fs::read_to_string(paths.session_file("broken")).unwrap();
        assert!(
            raw.starts_with("{\"id\": \"broken\""),
            "不能静默重写损坏文件"
        );
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// 索引坏了：按会话文件重建，不丢会话。
    #[test]
    fn broken_index_is_rebuilt_from_session_files() {
        let paths = unique_root("session-index-rebuild");
        let store = SessionStore::new(paths.clone());
        store.save_session(&sample("a", "甲")).unwrap();
        store.save_session(&sample("b", "乙")).unwrap();
        std::fs::write(paths.session_index_file(), "not json at all").unwrap();

        let library = store.load();
        assert_eq!(library.index.sessions.len(), 2);
        assert!(library.index.rebuilt);
        assert!(!library.index.current_id.is_empty(), "必须选中某个会话");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// 删除会话真的删掉文件，并且选中项回落到剩下的。
    #[test]
    fn delete_removes_file_and_reselects() {
        let paths = unique_root("session-delete");
        let store = SessionStore::new(paths.clone());
        store.save_session(&sample("keep", "留下")).unwrap();
        store.save_session(&sample("gone", "删掉")).unwrap();
        let mut index = SessionIndex {
            current_id: "gone".to_string(),
            ..SessionIndex::default()
        };
        store.save_index(&index).unwrap();

        store.delete_session("gone").unwrap();
        assert!(!paths.session_file("gone").is_file());
        index.current_id = "gone".to_string();
        store.save_index(&index).unwrap();

        let library = store.load();
        assert_eq!(library.index.sessions.len(), 1);
        assert_eq!(library.index.current_id, "keep");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    /// 恢复时：还在"执行中"的工具卡片要标成取消，绝不假装成功。
    #[test]
    fn restore_marks_running_tools_as_cancelled() {
        let mut session = SessionFile::new("s", "标题", 1);
        session.timeline.push(TimelineItem {
            kind: TimelineKind::Tool,
            turn_id: 3,
            call_id: 1,
            tool: "left_click".to_string(),
            status: TimelineToolStatus::Running,
            ..TimelineItem::default()
        });
        session.repair_after_restore();
        assert_eq!(session.timeline[0].status, TimelineToolStatus::Cancelled);
        assert!(session.timeline[0].text.contains("未知"));
        assert_eq!(session.last_turn_status, TurnStatus::Interrupted);
    }

    /// 保存时附件不落盘，但消息仍是一条合法的多段消息。
    #[test]
    fn attachments_are_bounded_and_structure_survives() {
        let mut session = SessionFile::new("s", "标题", 1);
        let image = ChatMessage::image_attachment(
            "（这是 screenshot 刚返回的图，16x16）",
            "image/png",
            &"A".repeat(400),
        );
        session.history.push(image);
        let stored = session.for_storage();
        let saved = serde_json::to_string(&stored).unwrap();
        assert!(
            !saved.contains("AAAAAAAAAAAAAAAA"),
            "截图 base64 不能进会话文件"
        );
        assert!(saved.contains("原本附带 1 张截图"), "必须保留可读说明");
        let restored: SessionFile = serde_json::from_str(&saved).unwrap();
        assert!(
            matches!(restored.history[0].content, Content::Parts(_)),
            "结构仍是多段消息，不能被改成普通文本"
        );
        assert_eq!(restored.history[0].role, Role::User);
    }

    /// 时间线超上限时丢最老的，不无限增长。
    #[test]
    fn timeline_is_bounded() {
        let mut session = SessionFile::new("s", "标题", 1);
        for index in 0..MAX_TIMELINE_ITEMS + 20 {
            session.push_timeline(TimelineItem::user(format!("第 {index} 条")));
        }
        assert_eq!(session.timeline.len(), MAX_TIMELINE_ITEMS);
        assert!(
            session.timeline[0].text.contains("第 20 条"),
            "最早的应被丢掉"
        );
    }

    /// 工具实例 id 稳定：连续同名工具各占一条。
    #[test]
    fn repeated_same_tool_calls_are_distinct_items() {
        let mut session = SessionFile::new("s", "标题", 1);
        for call in 1..=3 {
            session.push_timeline(TimelineItem {
                kind: TimelineKind::Tool,
                turn_id: 1,
                call_id: call,
                tool: "screenshot".to_string(),
                status: TimelineToolStatus::Ok,
                ..TimelineItem::default()
            });
        }
        let calls: Vec<u64> = session
            .timeline
            .iter()
            .filter(|item| item.kind == TimelineKind::Tool)
            .map(|item| item.call_id)
            .collect();
        assert_eq!(calls, vec![1, 2, 3], "连续同名工具必须各自独立");
    }

    #[test]
    fn titles_and_ids_are_sanitized() {
        assert_eq!(normalize_title("   "), "新会话");
        assert_eq!(normalize_title("  真实标题  "), "真实标题");
        let long: String = "很".repeat(100);
        assert_eq!(normalize_title(&long).chars().count(), 60);

        assert_eq!(title_from_input("你好"), "你好");
        let long_input: String = "命".repeat(40);
        let title = title_from_input(&long_input);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= 25);

        assert_eq!(safe_session_id("../../etc"), "______etc");
        assert_eq!(safe_session_id("s1"), "s1");
        assert_ne!(new_session_id(1, 1), new_session_id(1, 2));
    }

    /// 历史（发给模型）与时间线（给用户看）是分开的两份，压缩不会让时间线缩水。
    #[test]
    fn timeline_is_independent_of_model_history() {
        let mut session = SessionFile::new("s", "标题", 1);
        for index in 0..5 {
            session
                .history
                .push(ChatMessage::user(format!("历史 {index}")));
            session
                .timeline
                .push(TimelineItem::user(format!("时间线 {index}")));
        }
        // 模拟上下文压缩：历史被删到只剩一条。
        session.history.truncate(1);
        assert_eq!(session.timeline.len(), 5, "时间线不能因压缩而消失");
    }
}
