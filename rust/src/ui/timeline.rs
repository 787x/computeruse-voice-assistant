//! 时间线 → 界面模型：对话气泡、工具卡片与思考块。
//!
//! 关键约束（任务书第 4 条）：
//!  - **按真实调用实例**展示工具：连续同名工具各自一张卡片（用 `call_id` 区分），
//!    绝不把"工具侧栏最后状态"当成完整历史；
//!  - 工具卡片带名称、状态、耗时、成功/失败/拒绝、参数摘要与结果；
//!  - 思考内容独立成块（角色「思考」），**不混进可见正文**，因此也绝不会被 TTS 读到；
//!  - 重启/切换会话后仍可从持久化记录重建同样的展示。

use crate::session::{TimelineItem, TimelineKind, TimelineToolStatus};

/// 一条界面上的时间线（气泡 + 工具卡片）。
#[derive(Debug, Clone, Default)]
pub struct TimelineView {
    entries: Vec<crate::ChatEntry>,
    tools: Vec<crate::ToolCard>,
}

impl TimelineView {
    pub fn entries(&self) -> &[crate::ChatEntry] {
        &self.entries
    }

    pub fn tools(&self) -> &[crate::ToolCard] {
        &self.tools
    }

    /// 从持久化记录重建展示。
    ///
    /// 助手正文按回合聚合：同一步的连续增量合并成一条，跨步分开。
    /// 思考单独一段（角色「思考」），这样界面既能区分、也不会与正文混淆。
    pub fn from_items(items: Vec<TimelineItem>) -> Self {
        let mut view = TimelineView::default();
        // 上一条助手气泡归属的 (turn_id, step)：只为"当前这一段"复用。
        let mut last_assistant: Option<(u64, usize)> = None;

        for item in items {
            match item.kind {
                TimelineKind::User => {
                    last_assistant = None;
                    view.entries.push(crate::ChatEntry {
                        role: "你".into(),
                        text: item.text.into(),
                        detail: "".into(),
                        tint: parse_hex_color("#e8f0fe"),
                    });
                }
                TimelineKind::Assistant => {
                    let key = (item.turn_id, item.step);
                    let reuse = last_assistant == Some(key)
                        && view
                            .entries
                            .last()
                            .map(|entry| entry.role.as_str() == "助手")
                            .unwrap_or(false);
                    if reuse {
                        if let Some(last) = view.entries.last_mut() {
                            let mut text = last.text.to_string();
                            text.push_str(&item.text);
                            last.text = text.into();
                        }
                    } else {
                        view.entries.push(crate::ChatEntry {
                            role: "助手".into(),
                            text: item.text.into(),
                            detail: "".into(),
                            tint: parse_hex_color("#ffffff"),
                        });
                    }
                    last_assistant = Some(key);
                }
                TimelineKind::Reasoning => {
                    last_assistant = None;
                    // 思考用独立的浅灰气泡，角色写「思考」：一眼能看出它不播报。
                    view.entries.push(crate::ChatEntry {
                        role: "思考".into(),
                        text: item.text.into(),
                        detail: "模型返回的推理内容（不朗读）".into(),
                        tint: parse_hex_color("#f4f4f7"),
                    });
                }
                TimelineKind::Tool => {
                    last_assistant = None;
                    let status = item.status;
                    let tint = match status {
                        TimelineToolStatus::Ok => "#eef8ef",
                        TimelineToolStatus::Failed => "#fdeeee",
                        TimelineToolStatus::Denied => "#fff4e5",
                        TimelineToolStatus::Cancelled => "#f1f1f3",
                        TimelineToolStatus::Interrupted | TimelineToolStatus::Running => "#f1f1f3",
                    };
                    let mut detail_parts: Vec<String> = Vec::new();
                    if !item.args_summary.trim().is_empty() {
                        detail_parts.push(format!("参数：{}", item.args_summary));
                    }
                    if item.ms > 0 {
                        detail_parts.push(format!("耗时 {} ms", item.ms));
                    }
                    if !item.text.trim().is_empty() {
                        detail_parts.push(item.text.clone());
                    }
                    let name = if item.tool.trim().is_empty() {
                        "（未知工具）".to_string()
                    } else {
                        item.tool.clone()
                    };
                    view.entries.push(crate::ChatEntry {
                        role: format!("#{} {} · {}", item.call_id, name, status.label()).into(),
                        text: detail_parts.join(" · ").into(),
                        detail: "".into(),
                        tint: parse_hex_color(tint),
                    });
                    view.tools.push(crate::ToolCard {
                        name: format!("#{} {}", item.call_id, name).into(),
                        state: format!("{}{}", status.label(), step_suffix(item.step)).into(),
                        detail: detail_parts.join(" · ").into(),
                        tint: parse_hex_color(tint),
                    });
                }
                TimelineKind::System => {
                    last_assistant = None;
                    view.entries.push(crate::ChatEntry {
                        role: "系统".into(),
                        text: item.text.into(),
                        detail: "".into(),
                        tint: parse_hex_color("#fdeeee"),
                    });
                }
            }
        }
        view
    }
}

fn step_suffix(step: usize) -> String {
    if step == 0 {
        String::new()
    } else {
        format!("（第 {step} 步）")
    }
}

/// `#rrggbb` → `slint::Color`；解析失败退回白色（绝不因为配色让界面崩掉）。
pub fn parse_hex_color(value: &str) -> slint::Color {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        ) {
            return slint::Color::from_rgb_u8(r, g, b);
        }
    }
    slint::Color::from_rgb_u8(255, 255, 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: TimelineKind, text: &str) -> TimelineItem {
        TimelineItem {
            kind,
            text: text.to_string(),
            ..TimelineItem::default()
        }
    }

    #[test]
    fn consecutive_same_tools_become_separate_cards() {
        let view = TimelineView::from_items(vec![
            TimelineItem {
                kind: TimelineKind::Tool,
                call_id: 1,
                tool: "screenshot".to_string(),
                status: TimelineToolStatus::Ok,
                ms: 12,
                ..TimelineItem::default()
            },
            TimelineItem {
                kind: TimelineKind::Tool,
                call_id: 2,
                tool: "screenshot".to_string(),
                status: TimelineToolStatus::Failed,
                text: "窗口不可读".to_string(),
                ..TimelineItem::default()
            },
        ]);
        assert_eq!(view.tools().len(), 2, "连续同名工具必须各自一张卡片");
        assert!(view.tools()[0].name.starts_with("#1"));
        assert!(view.tools()[1].name.starts_with("#2"));
        assert!(view.tools()[0].detail.contains("12 ms"));
        assert!(view.tools()[1].detail.contains("窗口不可读"));
        assert_eq!(view.tools()[1].state, "失败");
    }

    #[test]
    fn reasoning_is_a_separate_block_from_visible_text() {
        let view = TimelineView::from_items(vec![
            item(TimelineKind::User, "打开浏览器"),
            item(TimelineKind::Reasoning, "先确认窗口再点"),
            item(TimelineKind::Assistant, "开了。"),
        ]);
        assert_eq!(view.entries().len(), 3);
        assert_eq!(view.entries()[0].role, "你");
        assert_eq!(view.entries()[1].role, "思考");
        assert!(view.entries()[1].detail.contains("不朗读"));
        assert_eq!(view.entries()[2].role, "助手");
        // 思考文本绝不能混进助手正文（否则 TTS 会读到）。
        assert_eq!(view.entries()[2].text.to_string(), "开了。");
    }

    #[test]
    fn assistant_fragments_merge_but_different_steps_do_not() {
        let mut first = item(TimelineKind::Assistant, "开了。");
        first.turn_id = 1;
        first.step = 1;
        let mut more = item(TimelineKind::Assistant, "稳。");
        more.turn_id = 1;
        more.step = 1;
        let mut next_step = item(TimelineKind::Assistant, "还要别的吗？");
        next_step.turn_id = 1;
        next_step.step = 2;
        let view = TimelineView::from_items(vec![first, more, next_step]);
        assert_eq!(view.entries().len(), 2, "同一步合并，跨步分开");
        assert_eq!(view.entries()[0].text.to_string(), "开了。稳。");
        assert_eq!(view.entries()[1].text.to_string(), "还要别的吗？");
    }

    #[test]
    fn denied_and_cancelled_tools_have_their_own_state() {
        let view = TimelineView::from_items(vec![
            TimelineItem {
                kind: TimelineKind::Tool,
                call_id: 5,
                tool: "read_clipboard".to_string(),
                status: TimelineToolStatus::Denied,
                text: "已拒绝：用户没有确认".to_string(),
                ..TimelineItem::default()
            },
            TimelineItem {
                kind: TimelineKind::Tool,
                call_id: 6,
                tool: "left_click".to_string(),
                status: TimelineToolStatus::Cancelled,
                text: "已中断（状态未知）".to_string(),
                ..TimelineItem::default()
            },
        ]);
        assert_eq!(view.tools()[0].state, "已拒绝");
        assert_eq!(view.tools()[1].state, "已取消");
        assert!(view.tools()[1].detail.contains("状态未知"));
    }

    #[test]
    fn args_summary_is_folded_into_the_card_detail() {
        let view = TimelineView::from_items(vec![TimelineItem {
            kind: TimelineKind::Tool,
            call_id: 3,
            tool: "type".to_string(),
            status: TimelineToolStatus::Ok,
            args_summary: "{\"text\":\"你好…\"}".to_string(),
            ..TimelineItem::default()
        }]);
        assert!(view.tools()[0].detail.contains("参数："));
        assert!(view.tools()[0].detail.contains("你好"));
    }

    #[test]
    fn colors_parse_or_fall_back_without_panicking() {
        assert_eq!(
            parse_hex_color("#ffffff"),
            slint::Color::from_rgb_u8(255, 255, 255)
        );
        assert_eq!(
            parse_hex_color("bogus"),
            slint::Color::from_rgb_u8(255, 255, 255)
        );
        assert_eq!(
            parse_hex_color("#f00"),
            slint::Color::from_rgb_u8(255, 255, 255)
        );
    }

    /// 重启恢复：同一份持久化记录必须能重建同样的卡片数量与状态。
    #[test]
    fn persisted_items_restore_identical_view() {
        let items = vec![
            item(TimelineKind::User, "列窗口"),
            TimelineItem {
                kind: TimelineKind::Tool,
                call_id: 1,
                tool: "list_windows".to_string(),
                status: TimelineToolStatus::Ok,
                text: "窗口 3 个".to_string(),
                ..TimelineItem::default()
            },
            item(TimelineKind::Assistant, "3 个窗口。"),
        ];
        let json = serde_json::to_string(&items).unwrap();
        let restored: Vec<TimelineItem> = serde_json::from_str(&json).unwrap();
        let view = TimelineView::from_items(restored);
        assert_eq!(view.entries().len(), 3);
        assert_eq!(view.entries()[1].role, "#1 list_windows · 成功");
        assert_eq!(view.tools().len(), 1);
        assert_eq!(view.tools()[0].state, "成功");
    }
}
