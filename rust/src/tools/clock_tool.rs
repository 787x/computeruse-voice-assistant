//! `get_current_time`：只读的本地时间工具，以及"时间/日期"的**直接回答**路径。
//!
//! 为什么要有直接回答路径：旧版实测过代价——模型为了"现在几点"去调 `run_shell`
//! （`Get-Date`），而 `system.shell` 在标准档是"每次询问"，屏幕上弹出一张确认卡；
//! 用户没理它，等满拒绝超时，一轮对话白等三分钟（审计日志里有记录）。
//!
//! 因此分两层，**都不经过 LLM、不需要 Key、不需要 CUA 后端**：
//!  1. [`direct_answer`]：识别"现在几点 / 今天几号 / 星期几"，直接用 [`crate::clock`] 回答；
//!  2. [`run`]：给复杂任务提供一个只读工具，返回本地日期、时间、星期、时区偏移，
//!     参数为空，能力是 `none`（默认放行，不会弹确认）。
//!
//! 两条路径都**绝不**让模型猜系统时间。

use serde_json::{json, Value};

use crate::cancel::CancelToken;
use crate::clock::{self, LocalTime};
use crate::tools::error::ToolError;
use crate::tools::spec::{as_args, object_schema, ToolSpec};

/// 工具名（提示词、权限表与测试共用）。
pub const TOOL_NAME: &str = "get_current_time";

/// 工具定义。
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: TOOL_NAME,
        description: "读取本机当前本地时间（只读、无需确认）：日期、时间、星期与时区偏移。\
用户问\"现在几点\"\"今天几号\"\"星期几\"，或任务需要准确时间（例如计算时长、写带日期的文件名）时用它，\
不要用 run_shell 跑 Get-Date，也不要凭记忆猜当前时间。",
        parameters: object_schema(json!({}), &[]),
        read_only: true,
        long_timeout: false,
    }
}

/// 结构化时间（供工具结果与测试共用）。
pub fn payload(time: &LocalTime, offset_minutes: i32) -> Value {
    json!({
        "date": time.date_stamp(),
        "time": format!("{:02}:{:02}:{:02}", time.hour, time.minute, time.second),
        "weekday": time.weekday_label(),
        "weekdayIso": time.weekday,
        "timezoneOffset": clock::format_offset(offset_minutes),
        "timezoneOffsetMinutes": offset_minutes,
        "unixMs": time.unix_ms,
    })
}

/// 工具结果文本：一行一个字段，模型读起来比读 JSON 稳。
pub fn render(time: &LocalTime, offset_minutes: i32) -> String {
    format!(
        "本地时间：{} {:02}:{:02}:{:02}（{}）\n时区偏移：{}\nUnix 毫秒：{}",
        time.date_stamp(),
        time.hour,
        time.minute,
        time.second,
        time.weekday_label(),
        clock::format_offset(offset_minutes),
        time.unix_ms
    )
}

/// 执行一次 `get_current_time`（无参数，永远不需要权限确认）。
pub async fn run(args: &Value, cancel: &CancelToken) -> Result<String, ToolError> {
    // 有参数也不报错：模型偶尔会传一个空对象或无关字段，
    // 为此让一句"现在几点"失败不值得（工具本身没有任何副作用）。
    let _ = as_args(args);
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    let time = clock::now();
    Ok(render(&time, clock::utc_offset_minutes()))
}

/// 直接回答的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickAnswer {
    /// "现在几点" → 现在是晚上8点9分。
    Time,
    /// "今天几号 / 星期几" → 今天是11月5日，星期四。
    Date,
}

/// 判断一句话是不是"本地就能答"的时间/日期问题。
///
/// 规则来自旧版 `parseOfflineIntent`，但**更严格**：只认明确问时间/日期的说法，
/// 不把"今天天气怎么样""我今天有点累"接过来——那些必须交给模型。
pub fn classify(text: &str) -> Option<QuickAnswer> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // 先排除"今天+别的事"（天气、新闻、日程…），它们不是报时间。
    //
    // 这一层比旧版更严格，是**故意**的：把"今天几号有雨吗"截成一句日期，
    // 用户拿到的不是他要的答案。"雨/雪/风/温度"这类单字也列进来，
    // 因为口语里"有雨吗""冷不冷"不会带上"天气"两个字。
    const OTHER_TOPICS: &[&str] = &[
        "天气", "气温", "温度", "下雨", "雨", "雪", "刮风", "风大", "新闻", "股价", "价格", "日程",
        "安排", "计划", "汇率", "比分", "股票", "限行", "油价", "堵车", "路况",
    ];
    if OTHER_TOPICS.iter().any(|topic| text.contains(topic)) {
        return None;
    }
    // 时间：几点 / 什么时间 / 什么时刻 / 几点了。
    const TIME_PATTERNS: &[&str] = &["几点", "什么时间", "什么时刻", "啥时候"];
    if TIME_PATTERNS.iter().any(|pattern| text.contains(pattern)) {
        return Some(QuickAnswer::Time);
    }
    // 日期：几号 / 几月几号 / 日期 / 星期几 / 周几 / 今天是什么日子。
    const DATE_PATTERNS: &[&str] = &["几号", "几月几", "日期", "星期几", "周几", "礼拜几"];
    if DATE_PATTERNS.iter().any(|pattern| text.contains(pattern)) {
        return Some(QuickAnswer::Date);
    }
    None
}

/// 直接回答的文本（与旧版 `replyOffline` 的措辞一致）。
pub fn direct_answer(kind: QuickAnswer, time: &LocalTime) -> String {
    match kind {
        QuickAnswer::Time => format!("现在是{}。", time.spoken_time()),
        QuickAnswer::Date => format!("今天是{}。", time.spoken_date()),
    }
}

/// 便捷入口：识别 + 回答（`None` = 这句话该交给模型）。
pub fn try_direct_answer(text: &str) -> Option<String> {
    let kind = classify(text)?;
    Some(direct_answer(kind, &clock::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LocalTime {
        LocalTime {
            year: 2026,
            month: 11,
            day: 5,
            weekday: 4,
            hour: 20,
            minute: 9,
            second: 7,
            unix_ms: 1_800_000_000_000,
        }
    }

    #[test]
    fn time_questions_are_recognized() {
        for text in ["现在几点", "几点了", "现在什么时间", "告诉我现在什么时刻"]
        {
            assert_eq!(classify(text), Some(QuickAnswer::Time), "{text}");
        }
    }

    #[test]
    fn date_questions_are_recognized() {
        for text in [
            "今天几号",
            "今天几月几号",
            "今天星期几",
            "今天周几",
            "现在的日期",
        ] {
            assert_eq!(classify(text), Some(QuickAnswer::Date), "{text}");
        }
    }

    /// 时效性问题不能被"报时间"截走：那会让用户拿到一句时间而不是答案。
    #[test]
    fn other_topics_are_not_hijacked() {
        for text in [
            "今天天气怎么样",
            "今天几号有雨吗",
            "今天的新闻",
            "今天股价多少",
            "我今天有点累",
            "帮我打开记事本",
            "",
        ] {
            assert_eq!(classify(text), None, "{text} 不该被本地时间路径接走");
        }
    }

    #[test]
    fn direct_answers_match_legacy_wording() {
        let time = sample();
        assert_eq!(
            direct_answer(QuickAnswer::Time, &time),
            "现在是晚上8点9分。"
        );
        assert_eq!(
            direct_answer(QuickAnswer::Date, &time),
            "今天是11月5日，星期四。"
        );
    }

    #[test]
    fn try_direct_answer_wraps_classification() {
        assert!(try_direct_answer("现在几点").unwrap().starts_with("现在是"));
        assert!(try_direct_answer("今天几号").unwrap().starts_with("今天是"));
        assert!(try_direct_answer("帮我写个函数").is_none());
    }

    #[test]
    fn payload_carries_every_field_the_model_needs() {
        let value = payload(&sample(), 480);
        assert_eq!(value["date"], json!("2026-11-05"));
        assert_eq!(value["time"], json!("20:09:07"));
        assert_eq!(value["weekday"], json!("星期四"));
        assert_eq!(value["weekdayIso"], json!(4));
        assert_eq!(value["timezoneOffset"], json!("UTC+08:00"));
        assert_eq!(value["timezoneOffsetMinutes"], json!(480));
    }

    #[tokio::test]
    async fn tool_returns_the_local_time_and_ignores_parameter_noise() {
        let cancel = CancelToken::new();
        let text = run(&json!({"unexpected": true}), &cancel).await.unwrap();
        assert!(text.contains("本地时间："), "{text}");
        assert!(text.contains("时区偏移："), "{text}");
        assert!(text.contains("星期"), "{text}");
    }

    #[tokio::test]
    async fn cancelled_token_stops_the_tool() {
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(run(&json!({}), &cancel).await.unwrap_err().is_cancelled());
    }

    #[test]
    fn spec_is_read_only_and_parameterless() {
        let spec = spec();
        assert_eq!(spec.name, TOOL_NAME);
        assert!(spec.read_only);
        assert!(spec.parameters.get("required").is_none());
    }
}
