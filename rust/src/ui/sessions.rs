//! 工作线程里的会话运行时：当前会话、列表、切换/新建/改名/删除、持久化。
//!
//! 职责边界（刻意保持扁平的几件事）：
//!  - 持有"会话库"的内存视图（列表 + 当前选中 + 时间线 + 模型选择）；
//!  - 回合结束/取消时把结果写回磁盘（原子写，失败只提示不静默）；
//!  - 维护 UI 侧的时间线推送（按回合/调用实例归属，旧事件不串到别的会话）。
//!
//! 不做的事：不启动 Agent（回合仍由 `worker` 里的同一条执行路径驱动），
//! 不做跨会话并行，不把密钥写进会话文件。

use slint::{Model, ModelRc, VecModel};

use crate::config::{AppPaths, SessionModelChoice};
use crate::llm::ChatMessage;
use crate::permissions::AuditRecord;
use crate::session::{
    new_session_id, normalize_title, safe_session_id, title_from_input, SessionFile, SessionIndex,
    SessionStore, SessionSummary, TimelineItem, TimelineKind, TimelineToolStatus, TurnStatus,
};
use crate::ui::timeline::TimelineView;
use crate::AppWindow;

/// 会话运行时（工作线程持有）。
pub struct SessionRuntime {
    store: SessionStore,
    /// 列表元数据（按最近更新时间倒序）。
    summaries: Vec<SessionSummary>,
    /// 当前选中会话。
    current: Option<SessionFile>,
    /// 索引里的当前选中 id。
    current_id: String,
    /// 会话 id 生成序号（同一毫秒内也不重复）。
    next_sequence: u64,
    /// 索引损坏/丢失时重建过（界面提示用）。
    rebuilt: bool,
    /// 读不出来的会话文件（界面提示用；**不覆盖**）。
    corrupt: Vec<String>,
    /// 上一次保存失败的说明（界面保留提示，不冒充"已保存"）。
    last_save_error: Option<String>,
    /// 有已保存 Key 的提供商 id（只存 id，不存明文）。
    key_presence: Vec<String>,
}

impl SessionRuntime {
    /// 打开会话库：没有会话时**不自动新建**（由调用方决定）。
    pub fn open(paths: &AppPaths) -> Self {
        let store = SessionStore::new(paths.clone());
        let library = store.load();
        // 恢复时就要做一致性修补：上一次退出时还在"执行中"的工具卡片必须
        // 标成已取消（绝不假装成功），回合标成异常中断。放在这里而不是只在
        // `select` 里做，是因为"启动时选中的那个会话"也要经过同一条修补。
        let mut current = library.current;
        if let Some(session) = current.as_mut() {
            session.repair_after_restore();
        }
        Self {
            store,
            summaries: library.index.sessions,
            current,
            current_id: library.index.current_id,
            next_sequence: 0,
            rebuilt: library.index.rebuilt,
            corrupt: library.index.corrupt,
            last_save_error: None,
            key_presence: Vec::new(),
        }
    }

    pub fn summaries(&self) -> &[SessionSummary] {
        &self.summaries
    }

    pub fn current(&self) -> Option<&SessionFile> {
        self.current.as_ref()
    }

    pub fn current_id(&self) -> &str {
        &self.current_id
    }

    pub fn corrupt(&self) -> &[String] {
        &self.corrupt
    }

    pub fn was_rebuilt(&self) -> bool {
        self.rebuilt
    }

    pub fn last_save_error(&self) -> Option<&str> {
        self.last_save_error.as_deref()
    }

    /// 当前会话的模型选择（空 = 跟随全局/时段方案）。
    pub fn model_choice(&self) -> SessionModelChoice {
        self.current
            .as_ref()
            .map(|session| session.model_choice.clone())
            .unwrap_or_default()
    }

    /// 当前会话的"发给模型的"历史。
    pub fn history(&self) -> Vec<ChatMessage> {
        self.current
            .as_ref()
            .map(|session| session.history.clone())
            .unwrap_or_default()
    }

    /// 当前会话的时间线。
    pub fn timeline(&self) -> Vec<TimelineItem> {
        self.current
            .as_ref()
            .map(|session| session.timeline.clone())
            .unwrap_or_default()
    }

    /// 新建一个会话并选中它。返回新会话 id。
    ///
    /// **立刻落盘**：新建的会话必须在进程意外退出后仍然存在，
    /// 否则用户"刚建了一个会话"却重启后找不到。
    pub fn create(&mut self, now_ms: u64, title: &str) -> String {
        self.next_sequence += 1;
        let id = new_session_id(now_ms, self.next_sequence);
        let title = normalize_title(title);
        let mut session = SessionFile::new(&id, title, now_ms);
        // 新会话继承当前会话的模型选择（"继续用同一套模型"是常见期望）。
        if let Some(previous) = &self.current {
            session.model_choice = previous.model_choice.clone();
        }
        let summary = session.summary();
        self.summaries.insert(0, summary);
        self.current = Some(session);
        self.current_id = id.clone();
        // 写盘失败只记录原因：会话仍在内存里可用，界面会提示"没保存"。
        if let Err(error) = self.save_current() {
            self.last_save_error = Some(error);
        }
        id
    }

    /// 确保至少有一个会话（启动时调用）。返回是否新建了。
    pub fn ensure_one(&mut self, now_ms: u64) -> bool {
        if self.current.is_some() {
            return false;
        }
        self.create(now_ms, "新会话");
        true
    }

    /// 切换到指定会话。
    ///
    /// **总是从磁盘重读**：磁盘是事实源，内存副本可能是刚被别的路径改过的。
    /// 重读顺带修好"上次退出时还在执行中"的工具状态。
    /// **忙碌判断由调用方负责**：这里只管文件能不能读出来。
    pub fn select(&mut self, session_id: &str) -> Result<(), String> {
        let session_id = safe_session_id(session_id);
        let mut session = self
            .store
            .read_session(&session_id)
            .map_err(|error| error.to_string())?;
        session.repair_after_restore();
        self.current_id = session_id;
        self.current = Some(session);
        Ok(())
    }

    /// 改名（当前会话与列表同时更新）。
    pub fn rename(&mut self, session_id: &str, title: &str) -> Result<(), String> {
        let title = normalize_title(title);
        let target = safe_session_id(session_id);
        let is_current = self
            .current
            .as_ref()
            .map(|session| session.id == target)
            .unwrap_or(false);
        // 改名也刷新更新时间：用户会期望"刚改过"的会话排在前面。
        let now = crate::clock::now().unix_ms;
        if is_current {
            let session = self
                .current
                .as_mut()
                .ok_or_else(|| "当前会话不存在".to_string())?;
            session.title = title;
            session.updated_at_ms = now;
            let stored = session.clone().for_storage();
            self.store
                .save_session(&stored)
                .map_err(|error| error.to_string())?;
            self.sync_summaries();
            return Ok(());
        }
        let mut session = self
            .store
            .read_session(&target)
            .map_err(|error| error.to_string())?;
        session.title = title;
        session.updated_at_ms = now;
        self.store
            .save_session(&session.for_storage())
            .map_err(|error| error.to_string())?;
        self.sync_summaries();
        Ok(())
    }

    /// 删除会话。删除当前会话后自动选中最近更新的那个。
    pub fn delete(&mut self, session_id: &str) -> Result<(), String> {
        let target = safe_session_id(session_id);
        self.store
            .delete_session(&target)
            .map_err(|error| error.to_string())?;
        self.summaries.retain(|summary| summary.id != target);
        if self.current_id == target {
            self.current = None;
            self.current_id.clear();
            self.select_next();
        }
        self.persist_index();
        Ok(())
    }

    /// 选中列表里的下一个（最近更新的那个）。
    fn select_next(&mut self) {
        let Some(first) = self.summaries.first() else {
            return;
        };
        let id = first.id.clone();
        if let Ok(mut session) = self.store.read_session(&id) {
            session.repair_after_restore();
            self.current = Some(session);
            self.current_id = id;
        }
    }

    /// 保存当前会话。`Err` 时**不冒充成功**。
    pub fn save_current(&mut self) -> Result<(), String> {
        let Some(session) = self.current.as_ref() else {
            return Ok(());
        };
        let stored = session.clone().for_storage();
        match self.store.save_session(&stored) {
            Ok(summary) => {
                self.last_save_error = None;
                if let Some(existing) = self.summaries.iter_mut().find(|item| item.id == summary.id)
                {
                    *existing = summary;
                } else {
                    self.summaries.push(summary);
                }
                self.summaries
                    .sort_by_key(|item| std::cmp::Reverse(item.updated_at_ms));
                self.persist_index();
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                self.last_save_error = Some(message.clone());
                Err(message)
            }
        }
    }

    /// 更新模型"发给 LLM 的有效历史"（回合结束时由 Agent 返回值驱动）。
    pub fn set_history(&mut self, history: Vec<ChatMessage>) {
        if let Some(session) = self.current.as_mut() {
            session.history = history;
        }
    }

    /// 记下一次回合的终态（恢复界面据此提示"上一轮没有正常结束"）。
    pub fn set_last_turn(&mut self, turn_id: u64, status: TurnStatus) {
        if let Some(session) = self.current.as_mut() {
            session.last_turn_id = turn_id;
            session.last_turn_status = status;
            session.updated_at_ms = crate::clock::now().unix_ms;
        }
    }

    /// 追加一条时间线。
    pub fn push_timeline(&mut self, turn_id: u64, item: TimelineItem) {
        if let Some(session) = self.current.as_mut() {
            let mut item = item;
            item.turn_id = turn_id;
            if item.at_ms == 0 {
                item.at_ms = crate::clock::now().unix_ms;
            }
            session.push_timeline(item);
            session.updated_at_ms = crate::clock::now().unix_ms;
        }
    }

    /// 更新一条已存在的时间线（按 `call_id` + `turn_id` 定位）。
    pub fn update_tool_item(
        &mut self,
        turn_id: u64,
        call_id: u64,
        status: TimelineToolStatus,
        text: &str,
        ms: u64,
    ) {
        let Some(session) = self.current.as_mut() else {
            return;
        };
        if let Some(item) = session.timeline.iter_mut().rev().find(|item| {
            item.kind == TimelineKind::Tool && item.call_id == call_id && item.turn_id == turn_id
        }) {
            item.status = status;
            item.ms = ms;
            if !text.is_empty() {
                item.text = text.to_string();
            }
        }
        session.updated_at_ms = crate::clock::now().unix_ms;
    }

    /// 追加助手正文（同一回合的连续增量合并到同一条，跨回合分开）。
    pub fn push_or_extend_assistant(&mut self, turn_id: u64, delta: &str) {
        let Some(session) = self.current.as_mut() else {
            return;
        };
        if let Some(item) = session
            .timeline
            .iter_mut()
            .rev()
            .find(|item| item.kind == TimelineKind::Assistant && item.turn_id == turn_id)
        {
            item.text.push_str(delta);
        } else {
            let mut item = TimelineItem {
                kind: TimelineKind::Assistant,
                text: delta.to_string(),
                ..TimelineItem::default()
            };
            item.turn_id = turn_id;
            item.at_ms = crate::clock::now().unix_ms;
            session.push_timeline(item);
        }
        session.updated_at_ms = crate::clock::now().unix_ms;
    }

    /// 这一回合是否已经有助手正文（用于"纯工具回合"的最后补一条结果）。
    pub fn has_assistant_text(&self, turn_id: u64) -> bool {
        self.current
            .as_ref()
            .map(|session| {
                session
                    .timeline
                    .iter()
                    .any(|item| item.kind == TimelineKind::Assistant && item.turn_id == turn_id)
            })
            .unwrap_or(false)
    }

    /// 累积思考片段（同一回合合并到一条，最终文本由 `settle_reasoning` 覆盖）。
    pub fn push_reasoning_fragment(&mut self, turn_id: u64, delta: &str) {
        let Some(session) = self.current.as_mut() else {
            return;
        };
        if let Some(item) = session
            .timeline
            .iter_mut()
            .rev()
            .find(|item| item.kind == TimelineKind::Reasoning && item.turn_id == turn_id)
        {
            item.text.push_str(delta);
        } else {
            let mut item = TimelineItem {
                kind: TimelineKind::Reasoning,
                text: delta.to_string(),
                ..TimelineItem::default()
            };
            item.turn_id = turn_id;
            item.at_ms = crate::clock::now().unix_ms;
            session.push_timeline(item);
        }
        session.updated_at_ms = crate::clock::now().unix_ms;
    }

    /// 用最终文本替换某一步的思考条目（去重：流式片段 → 最终结果）。
    ///
    /// 找不到就**新增**一条：这样"只有流式、没收到汇总"时也不会丢内容。
    pub fn settle_reasoning(&mut self, turn_id: u64, step: usize, text: &str) {
        let Some(session) = self.current.as_mut() else {
            return;
        };
        let existing = session.timeline.iter_mut().rev().find(|item| {
            item.kind == TimelineKind::Reasoning && item.turn_id == turn_id && item.step == step
        });
        match existing {
            Some(item) => item.text = text.to_string(),
            None => {
                let mut item = TimelineItem {
                    kind: TimelineKind::Reasoning,
                    step,
                    text: text.to_string(),
                    ..TimelineItem::default()
                };
                item.turn_id = turn_id;
                item.at_ms = crate::clock::now().unix_ms;
                session.push_timeline(item);
            }
        }
        session.updated_at_ms = crate::clock::now().unix_ms;
    }

    /// 切换当前会话的模型选择（持久化到该会话文件）。
    pub fn set_model_choice(&mut self, choice: SessionModelChoice) -> Result<(), String> {
        let Some(session) = self.current.as_mut() else {
            return Err("还没有会话".to_string());
        };
        session.model_choice = choice;
        session.updated_at_ms = crate::clock::now().unix_ms;
        self.save_current()
    }

    /// 把列表元数据重建一份（改名/保存后调用）。
    fn sync_summaries(&mut self) {
        if let Some(session) = self.current.as_ref() {
            let summary = session.summary();
            self.summaries.retain(|item| item.id != summary.id);
            self.summaries.push(summary);
            self.summaries
                .sort_by_key(|item| std::cmp::Reverse(item.updated_at_ms));
        }
        self.persist_index();
    }

    /// 写索引（失败只记日志：会话文件才是事实源）。
    fn persist_index(&self) {
        let index = SessionIndex {
            current_id: self.current_id.clone(),
            sessions: self.summaries.clone(),
            rebuilt: false,
            corrupt: Vec::new(),
        };
        if let Err(error) = self.store.save_index(&index) {
            eprintln!("[session] 索引保存失败（会话文件仍然有效）：{error}");
        }
    }

    /// 首条用户消息决定会话标题（只在标题仍是默认值时）。
    pub fn adopt_title_from(&mut self, input: &str) {
        let Some(session) = self.current.as_mut() else {
            return;
        };
        if session.title.trim() == "新会话" || session.title.trim().is_empty() {
            session.title = title_from_input(input);
        }
    }

    /// 这个提供商有没有已保存的 Key（只探测存在性，不读明文）。
    pub fn has_key_for(&self, provider_id: &str) -> bool {
        self.key_presence.iter().any(|id| id == provider_id)
    }

    /// 记住"哪些提供商有 Key"（启动/保存后刷新）。
    pub fn set_key_presence(&mut self, providers: Vec<String>) {
        self.key_presence = providers;
    }
}

// ---------------------------------------------------------------- 界面推送

/// 把会话列表推给界面（标题 + 最近更新时间 + 未完成标记）。
pub fn publish_session_list(weak: &slint::Weak<AppWindow>, runtime: &SessionRuntime) {
    let mut rows: Vec<crate::SessionRow> = Vec::new();
    for summary in runtime.summaries() {
        let updated = crate::clock::from_unix_ms(summary.updated_at_ms).short_stamp();
        let status = summary.last_turn_status;
        rows.push(crate::SessionRow {
            id: summary.id.clone().into(),
            title: summary.title.clone().into(),
            detail: format!(
                "{updated} · {}{}",
                status.label(),
                if status.needs_warning() {
                    "（未正常结束）"
                } else {
                    ""
                }
            )
            .into(),
            warning: status.needs_warning(),
            selected: summary.id == runtime.current_id(),
        });
    }
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_sessions(ModelRc::new(VecModel::from(rows)));
        }
    });
}

/// 把当前会话的时间线推给界面（对话气泡 + 工具卡片）。
pub fn publish_timeline(weak: &slint::Weak<AppWindow>, runtime: &SessionRuntime) {
    let view = TimelineView::from_items(runtime.timeline());
    let entries: Vec<crate::ChatEntry> = view.entries().to_vec();
    let tools: Vec<crate::ToolCard> = view.tools().to_vec();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_entries(ModelRc::new(VecModel::from(entries)));
            window.set_tools(ModelRc::new(VecModel::from(tools)));
        }
    });
}

/// 把审计记录推给设置页（最近 N 条，最新在前）。
pub fn publish_audit(weak: &slint::Weak<AppWindow>, records: &[AuditRecord]) {
    let lines: Vec<slint::SharedString> = if records.is_empty() {
        vec!["（还没有记录）".into()]
    } else {
        records
            .iter()
            .rev()
            .map(|record| slint::SharedString::from(record.label()))
            .collect()
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_audit_lines(ModelRc::new(VecModel::from(lines)));
        }
    });
}

/// 把"当前选中的是哪个会话"同步给界面的标题与改名草稿。
pub fn publish_session_title(weak: &slint::Weak<AppWindow>, runtime: &SessionRuntime) {
    let title = runtime
        .current()
        .map(|session| session.title.clone())
        .unwrap_or_else(|| "（没有会话）".to_string());
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_session_title(title.into());
        }
    });
}

/// 会话行里"选中项"的 id（供测试与界面复用）。
pub fn rows_have_one_selected(window: &AppWindow) -> bool {
    let rows = window.get_sessions();
    let selected = (0..rows.row_count()).filter(|index| {
        rows.row_data(*index)
            .map(|row| row.selected)
            .unwrap_or(false)
    });
    selected.count() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_paths(prefix: &str) -> AppPaths {
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

    #[test]
    fn runtime_creates_and_switches_between_sessions() {
        let paths = unique_paths("runtime-sessions");
        let mut runtime = SessionRuntime::open(&paths);
        assert!(runtime.current().is_none());
        assert!(runtime.ensure_one(1_000));
        let first = runtime.current_id().to_string();

        let second = runtime.create(2_000, "第二");
        assert_ne!(first, second);
        assert_eq!(runtime.current_id(), second);
        assert_eq!(runtime.summaries().len(), 2);

        runtime.select(&first).unwrap();
        assert_eq!(runtime.current().unwrap().title, "新会话");
        runtime.rename(&first, "改过的名字").unwrap();
        assert_eq!(runtime.current().unwrap().title, "改过的名字");

        // 重启：仍然能读回来，当前选中项保留。
        runtime.save_current().unwrap();
        let reopened = SessionRuntime::open(&paths);
        assert_eq!(reopened.current_id(), first);
        assert_eq!(reopened.summaries().len(), 2);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn runtime_persists_timeline_and_marks_interrupted() {
        let paths = unique_paths("runtime-timeline");
        let mut runtime = SessionRuntime::open(&paths);
        runtime.ensure_one(1_000);
        let id = runtime.current_id().to_string();

        runtime.push_timeline(7, TimelineItem::user("打开浏览器"));
        runtime.push_timeline(
            7,
            TimelineItem {
                kind: TimelineKind::Tool,
                call_id: 42,
                tool: "left_click".to_string(),
                status: TimelineToolStatus::Running,
                ..TimelineItem::default()
            },
        );
        runtime.set_history(vec![ChatMessage::user("打开浏览器")]);
        runtime.set_last_turn(7, TurnStatus::Completed);
        runtime.save_current().unwrap();

        // 模拟进程中途退出：重新打开后，执行中的工具被标为已取消，回合标为异常中断。
        let mut reopened = SessionRuntime::open(&paths);
        reopened.select(&id).unwrap();
        let timeline = reopened.timeline();
        let tool = timeline
            .iter()
            .find(|item| item.kind == TimelineKind::Tool)
            .unwrap();
        assert_eq!(tool.status, TimelineToolStatus::Cancelled);
        assert_eq!(
            reopened.current().unwrap().last_turn_status,
            TurnStatus::Interrupted
        );
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn deleting_current_session_selects_the_next() {
        let paths = unique_paths("runtime-delete");
        let mut runtime = SessionRuntime::open(&paths);
        runtime.ensure_one(1_000);
        let first = runtime.current_id().to_string();
        let second = runtime.create(2_000, "第二个");
        runtime.select(&second).unwrap();
        runtime.delete(&second).unwrap();
        assert_eq!(runtime.current_id(), first, "删除后应选中剩下的会话");
        assert_eq!(runtime.summaries().len(), 1);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn model_choice_is_per_session_and_persisted() {
        let paths = unique_paths("runtime-model-choice");
        let mut runtime = SessionRuntime::open(&paths);
        runtime.ensure_one(1_000);
        let first = runtime.current_id().to_string();
        runtime
            .set_model_choice(SessionModelChoice {
                provider_id: "deepseek".to_string(),
                model: "deepseek-flash".to_string(),
                thinking: None,
                reasoning_effort: Some(crate::config::ReasoningEffort::High),
            })
            .unwrap();

        // 新会话继承当前选择（同一套模型），但之后各自独立。
        let second = runtime.create(2_000, "第二个");
        assert_eq!(runtime.model_choice().provider_id, "deepseek");

        runtime.select(&first).unwrap();
        runtime
            .set_model_choice(SessionModelChoice {
                provider_id: "mimo".to_string(),
                model: "mimo-v2.6-flash".to_string(),
                thinking: None,
                reasoning_effort: None,
            })
            .unwrap();
        runtime.select(&second).unwrap();
        assert_eq!(
            runtime.model_choice().provider_id,
            "deepseek",
            "另一个会话的选择不能被改动"
        );
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn first_message_sets_the_title_only_once() {
        let paths = unique_paths("runtime-title");
        let mut runtime = SessionRuntime::open(&paths);
        runtime.ensure_one(1_000);
        runtime.adopt_title_from("帮我把这份表格转成图表");
        let title = runtime.current().unwrap().title.clone();
        assert!(title.contains("表格"), "{title}");
        runtime.adopt_title_from("第二条消息");
        assert_eq!(
            runtime.current().unwrap().title,
            title,
            "第二条消息不该改标题"
        );
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn reasoning_settle_dedupes_streaming_fragments() {
        let paths = unique_paths("runtime-reasoning");
        let mut runtime = SessionRuntime::open(&paths);
        runtime.ensure_one(1_000);
        runtime.settle_reasoning(3, 1, "先看窗口列表");
        let count = runtime
            .timeline()
            .iter()
            .filter(|item| item.kind == TimelineKind::Reasoning)
            .count();
        assert_eq!(count, 1);
        // 同一 step 再来一次：替换而不是新增（不重复展示）。
        runtime.settle_reasoning(3, 1, "先看窗口列表，再点按钮");
        let items: Vec<TimelineItem> = runtime
            .timeline()
            .into_iter()
            .filter(|item| item.kind == TimelineKind::Reasoning)
            .collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("再点按钮"));
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn tool_updates_target_the_right_call_instance() {
        let paths = unique_paths("runtime-tool-update");
        let mut runtime = SessionRuntime::open(&paths);
        runtime.ensure_one(1_000);
        for call in 1..=2 {
            runtime.push_timeline(
                5,
                TimelineItem {
                    kind: TimelineKind::Tool,
                    call_id: call,
                    tool: "screenshot".to_string(),
                    status: TimelineToolStatus::Running,
                    ..TimelineItem::default()
                },
            );
        }
        runtime.update_tool_item(5, 2, TimelineToolStatus::Ok, "窗口列表", 12);
        let items: Vec<TimelineItem> = runtime
            .timeline()
            .into_iter()
            .filter(|item| item.kind == TimelineKind::Tool)
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].status,
            TimelineToolStatus::Running,
            "第一条不受影响"
        );
        assert_eq!(items[1].status, TimelineToolStatus::Ok);
        assert_eq!(items[1].ms, 12);
        let _ = std::fs::remove_dir_all(&paths.root);
    }
}
