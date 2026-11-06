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

/// 判断一句话是不是"本地就能答"的**当前时间 / 今天日期**问题。
///
/// 为什么不能用包含匹配：`"几点"` 包含匹配会把"会议几点开始""高铁几点到站"
/// "三个小时后几点"全部截走，用户拿到的是一句自顾自的报时，而不是他要的答案。
/// 这类问题的答案是**别的时间**（会议时间、到站时间、推算结果），必须交给 Agent。
///
/// 因此这里只认**整句就是在问此刻**的简短表达：去掉礼貌前缀与标点后，
/// 句子必须完整等于（或几乎等于）下列固定说法之一。判断不了就交给 Agent——
/// 少答一句时间远比答错一句安全。
pub fn classify(text: &str) -> Option<QuickAnswer> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    // 先排除"今天+别的事"（天气、新闻、日程…），它们不是报时间。
    //
    // 这一层是兜底：即使某个说法恰好整句匹配，只要提到别的主题也不该被截走。
    const OTHER_TOPICS: &[&str] = &[
        "天气", "气温", "温度", "下雨", "雨", "雪", "刮风", "风大", "新闻", "股价", "价格", "日程",
        "安排", "计划", "汇率", "比分", "股票", "限行", "油价", "堵车", "路况", "会议", "高铁",
        "飞机", "火车", "航班", "开始", "结束", "到站", "出发", "之后", "以后", "过后",
    ];
    if OTHER_TOPICS.iter().any(|topic| raw.contains(topic)) {
        return None;
    }
    let normalized = normalize_question(raw);

    // 只要整句在问"此刻是什么时间"。
    const TIME_QUESTIONS: &[&str] = &[
        "现在几点",
        "几点了",
        "现在几点了",
        "现在是几点",
        "现在是什么时间",
        "现在什么时间",
        "现在是什么时刻",
        "现在什么时刻",
        "当前时间",
        "现在时间",
        "现在的时间",
        "现在几点钟",
        "几时了",
    ];
    if TIME_QUESTIONS.contains(&normalized.as_str()) {
        return Some(QuickAnswer::Time);
    }

    // 只要整句在问"今天是几号 / 星期几"。
    const DATE_QUESTIONS: &[&str] = &[
        "今天几号",
        "今天是几号",
        "今天几月几号",
        "今天是几月几号",
        "今天的日期",
        "今天日期",
        "今天星期几",
        "今天是星期几",
        "今天周几",
        "今天是周几",
        "今天礼拜几",
        "今天是礼拜几",
        "今天是什么日子",
        "现在几号",
        "现在是几号",
    ];
    if DATE_QUESTIONS.contains(&normalized.as_str()) {
        return Some(QuickAnswer::Date);
    }
    None
}

/// 把口语问题收敛成一个便于整句比较的形态。
///
/// 去标点、去礼貌前缀（"请问/帮我/告诉我"）与句末语气词（"呢/啊/呀/吧"）。
/// **不做语义改写**：任何多余成分都会让它对不上固定说法，从而交给 Agent。
fn normalize_question(text: &str) -> String {
    let mut text = text.trim();
    // 标点与空白：中英文句号、问号、逗号、感叹号、空格。
    let stripped: String = text
        .chars()
        .filter(|ch| !ch.is_whitespace() && !"，,。.？?！!、；;：:".contains(*ch))
        .collect();
    text = stripped.as_str();
    // 礼貌前缀：模型和用户都常带这些，去掉后不影响"整句在问此刻"的判断。
    for prefix in [
        "请问一下",
        "请问",
        "帮我看看",
        "帮我查一下",
        "帮我查",
        "帮我",
        "告诉我",
        "想问一下",
        "问一下",
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            break;
        }
    }
    // 句末语气词：只在**只剩一个**时去掉，避免把"几点了呢"以外的说法改坏。
    for suffix in ["呢", "啊", "呀", "吧"] {
        if let Some(rest) = text.strip_suffix(suffix) {
            text = rest;
            break;
        }
    }
    text.to_string()
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

    /// 任务书要求的正例：明确的"此刻"问题直接本地回答。
    #[test]
    fn explicit_current_time_questions_are_answered() {
        for text in [
            "现在几点？",
            "现在几点",
            "几点了？",
            "几点了",
            "现在是什么时间？",
            "现在什么时间",
            "现在是什么时刻",
            "当前时间",
            "现在是几点呢",
            "请问现在几点？",
            "告诉我现在几点",
        ] {
            assert_eq!(classify(text), Some(QuickAnswer::Time), "{text}");
        }
    }

    /// 任务书要求的正例：明确的"今天日期/星期"问题。
    #[test]
    fn explicit_today_date_questions_are_answered() {
        for text in [
            "今天几号？",
            "今天几号",
            "今天是几号",
            "今天几月几号",
            "今天星期几？",
            "今天是星期几",
            "今天周几",
            "今天礼拜几",
            "今天的日期",
            "请问今天几号？",
        ] {
            assert_eq!(classify(text), Some(QuickAnswer::Date), "{text}");
        }
    }

    /// **任务书要求的反例**：这些问的是**别的时间**（会议、到站、推算、
    /// 明天、天气），必须交给 Agent，不能被当前时间截走。
    #[test]
    fn questions_about_other_times_are_not_hijacked() {
        for text in [
            "会议几点开始？",
            "高铁几点到站？",
            "三个小时后几点？",
            "明天几号？",
            "今天天气怎么样？",
            "飞机几点起飞",
            "火车几点出发",
            "这个任务几点结束",
            "我几点出发合适",
            "昨天几号",
            "下周星期几",
        ] {
            assert_eq!(classify(text), None, "「{text}」问的不是此刻，不能本地报时");
        }
    }

    /// 时效性问题与无关指令也不能被截走。
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

    /// 规范化只去掉标点、礼貌前缀与句末语气词，不做语义改写。
    #[test]
    fn normalization_strips_only_noise() {
        assert_eq!(normalize_question("现在几点？"), "现在几点");
        assert_eq!(normalize_question("请问现在几点"), "现在几点");
        assert_eq!(normalize_question("几点了呢"), "几点了");
        assert_eq!(normalize_question("  今天 星期几  "), "今天星期几");
        // 多余成分原样保留，因此对不上固定说法 → 交给 Agent。
        assert_eq!(normalize_question("会议几点开始"), "会议几点开始");
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
