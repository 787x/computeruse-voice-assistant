//! 本地时间：只提供"墙上的时间"（星期、时:分）与可读时间戳。
//!
//! 为什么单独一个模块：时段规则（按本地时间切换模型方案）与会话列表的
//! "最近更新时间"都依赖本地时区，而 `std` 没有本地时间 API。
//! Windows 上用 `GetLocalTime`（系统已经按用户时区换算好），
//! 其它平台退化成 UTC——只影响测试与编译，正式发行只在 Windows。
//!
//! PR12 追加两件事，仍然是同一个来源（**绝不问模型、绝不跑 run_shell**）：
//!  - 口语化的时间/日期（"晚上8点9分" / "11月5日，星期四"），
//!    与旧版 `packages/agent/offline.mjs` 的 `formatTime` / `formatDate` 一致；
//!  - 本地时区偏移（"UTC+08:00"），供 `get_current_time` 工具与直接回答使用。

use serde::{Deserialize, Serialize};

/// 一份本地时间快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    /// ISO 星期：1 = 周一 … 7 = 周日（与旧版 `isoWeekday` 一致）。
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Unix 毫秒（用于"最近更新时间"的排序与显示）。
    pub unix_ms: u64,
}

impl LocalTime {
    /// 当天第几分钟（旧版 `minutesOf` 的等价物）。
    pub fn minutes_of_day(&self) -> u32 {
        self.hour as u32 * 60 + self.minute as u32
    }

    /// `HH:mm`（设置页回显 / 会话列表）。
    pub fn hhmm(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// `MM-DD HH:mm`（最近更新时间；同一天看不出来年份也不需要）。
    pub fn short_stamp(&self) -> String {
        format!(
            "{:02}-{:02} {:02}:{:02}",
            self.month, self.day, self.hour, self.minute
        )
    }

    /// `YYYY-MM-DD HH:mm:ss`。
    pub fn full_stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `YYYY-MM-DD`。
    pub fn date_stamp(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// 星期的中文标签（`星期四`）。
    pub fn weekday_label(&self) -> &'static str {
        weekday_label(self.weekday)
    }

    /// "晚上8点9分"——直接照着念的形态（旧版 `formatTime` 的等价物）。
    ///
    /// 与旧版逐字对齐：`0–5` 凌晨、`6–11` 上午、`12` 中午、`13–17` 下午、
    /// `18+` 晚上；0 点显示 12 点、下午起的 12 小时制；整点说"整"。
    pub fn spoken_time(&self) -> String {
        let hour = self.hour;
        let minute = self.minute;
        let period = if hour < 6 {
            "凌晨"
        } else if hour < 12 {
            "上午"
        } else if hour < 13 {
            "中午"
        } else if hour < 18 {
            "下午"
        } else {
            "晚上"
        };
        let display = if hour == 0 {
            12
        } else if hour > 12 {
            hour - 12
        } else {
            hour
        };
        let minute_text = if minute == 0 {
            "整".to_string()
        } else {
            format!("{minute}分")
        };
        format!("{period}{display}点{minute_text}")
    }

    /// "11月5日，星期四"（旧版 `formatDate` 的等价物）。
    pub fn spoken_date(&self) -> String {
        format!("{}月{}日，{}", self.month, self.day, self.weekday_label())
    }
}

/// ISO 星期（1 = 周一 … 7 = 周日）→ 中文标签。越界按周一处理更安全（不会 panic）。
pub fn weekday_label(weekday: u8) -> &'static str {
    match weekday {
        1 => "星期一",
        2 => "星期二",
        3 => "星期三",
        4 => "星期四",
        5 => "星期五",
        6 => "星期六",
        7 => "星期日",
        _ => "星期一",
    }
}

/// 本地时区相对 UTC 的偏移（分钟，东八区 = `+480`）。
///
/// 只用于显示与 `get_current_time`（"UTC+08:00"），不参与时段规则——
/// 那套规则直接比较本地墙钟，不需要偏移。
pub fn utc_offset_minutes() -> i32 {
    platform::utc_offset_minutes()
}

/// `+08:00` / `-05:00` / `UTC` 形态的时区偏移文本。
pub fn utc_offset_label() -> String {
    let minutes = utc_offset_minutes();
    format_offset(minutes)
}

/// 把分钟偏移格式化成 `UTC+08:00`；0 给 `UTC`。
pub fn format_offset(minutes: i32) -> String {
    if minutes == 0 {
        return "UTC".to_string();
    }
    let sign = if minutes < 0 { '-' } else { '+' };
    let absolute = minutes.unsigned_abs();
    let hours = absolute / 60;
    let rest = absolute % 60;
    if rest == 0 {
        format!("UTC{sign}{hours:02}:00")
    } else {
        format!("UTC{sign}{hours:02}:{rest:02}")
    }
}

/// 取当前本地时间（含 ISO 星期）。
pub fn now() -> LocalTime {
    platform::now()
}

/// 从 Unix 毫秒还原本地时间（用于显示历史记录的写入时间）。
pub fn from_unix_ms(ms: u64) -> LocalTime {
    platform::from_unix_ms(ms)
}

/// "HH:mm" → 当天第几分钟；格式不对返回 `None`。
pub fn parse_hhmm(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let (hour, minute) = trimmed.split_once(':')?;
    if hour.is_empty() || minute.is_empty() {
        return None;
    }
    if !hour.chars().all(|ch| ch.is_ascii_digit()) || !minute.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Time::{
        FileTimeToSystemTime, GetTimeZoneInformation, LocalFileTimeToLocalSystemTime,
        TIME_ZONE_INFORMATION,
    };

    use super::LocalTime;

    /// 1601-01-01 到 1970-01-01 之间的 100ns 间隔数。
    const EPOCH_FILETIME_OFFSET: u64 = 116_444_736_000_000_000;
    const TICKS_PER_MS: u64 = 10_000;

    /// `GetTimeZoneInformation` 的返回值：0 = 未知（无夏令时），1 = 标准时间，2 = 夏令时。
    const TZ_ID_UNKNOWN: u32 = 0;
    const TZ_ID_DAYLIGHT: u32 = 2;

    fn snapshot(st: &SYSTEMTIME, unix_ms: u64) -> LocalTime {
        let raw_weekday = st.wDayOfWeek as u8;
        // `wDayOfWeek`：0 = 周日 … 6 = 周六；转成 ISO（1 = 周一 … 7 = 周日）。
        let weekday = if raw_weekday == 0 { 7 } else { raw_weekday };
        LocalTime {
            year: st.wYear,
            month: st.wMonth as u8,
            day: st.wDay as u8,
            weekday,
            hour: st.wHour as u8,
            minute: st.wMinute as u8,
            second: st.wSecond as u8,
            unix_ms,
        }
    }

    pub fn now() -> LocalTime {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        // SAFETY：GetLocalTime 返回按调用方时区换算的墙钟快照，没有指针/生命周期问题。
        let st = unsafe { GetLocalTime() };
        snapshot(&st, unix_ms)
    }

    /// 本地时区偏移（分钟）。
    ///
    /// `Bias` 是"UTC = 本地 + Bias"，所以本地偏移是 `-Bias`；
    /// 夏令时期间再加 `DaylightBias`（同样取反）。失败时返回 0（显示为 UTC），
    /// 不猜一个看起来合理的值。
    pub fn utc_offset_minutes() -> i32 {
        let mut info = TIME_ZONE_INFORMATION::default();
        // SAFETY：入参是调用方栈上的结构，函数只写入它。
        let result = unsafe { GetTimeZoneInformation(&mut info) };
        if result == u32::MAX {
            // TIME_ZONE_ID_INVALID
            return 0;
        }
        let extra = match result {
            TZ_ID_UNKNOWN => 0,
            TZ_ID_DAYLIGHT => info.DaylightBias,
            _ => info.StandardBias,
        };
        -(info.Bias + extra)
    }

    pub fn from_unix_ms(ms: u64) -> LocalTime {
        /*
         * Unix 毫秒 → FILETIME → 本地 SYSTEMTIME。
         *
         * 注意用 `LocalFileTimeToLocalSystemTime`（一步到位），而不是
         * `FileTimeToLocalFileTime` + `FileTimeToSystemTime` 两步：
         * 后者的"本地"转换在部分 Windows 版本上会叠加两次时区偏移。
         */
        let total_ticks = ms.saturating_mul(TICKS_PER_MS) + EPOCH_FILETIME_OFFSET;
        let filetime = FILETIME {
            dwLowDateTime: (total_ticks & 0xFFFF_FFFF) as u32,
            dwHighDateTime: ((total_ticks >> 32) & 0xFFFF_FFFF) as u32,
        };
        let mut st = SYSTEMTIME::default();
        // SAFETY：两个入参都是调用方栈上的结构，函数只写入 `st`。
        // `LocalFileTimeToLocalSystemTime` 返回 `BOOL`（需 `.as_bool()`），
        // `FileTimeToSystemTime` 在本 crate 里已经被包装成 `Result<()>`。
        let converted = unsafe { LocalFileTimeToLocalSystemTime(None, &filetime, &mut st) };
        if !converted.as_bool() {
            // 退化：用 UTC 换算，至少时间戳不会错得离谱。
            // SAFETY：同上。
            if unsafe { FileTimeToSystemTime(&filetime, &mut st) }.is_err() {
                return fallback_from_unix_ms(ms);
            }
        }
        snapshot(&st, ms)
    }

    fn fallback_from_unix_ms(ms: u64) -> LocalTime {
        let seconds = ms / 1000;
        let days = seconds / 86_400;
        // 1970-01-01 是周四（ISO 4）。
        let weekday = ((days + 3) % 7 + 1) as u8;
        LocalTime {
            year: 1970,
            month: 1,
            day: 1,
            weekday,
            hour: ((seconds / 3600) % 24) as u8,
            minute: ((seconds / 60) % 60) as u8,
            second: (seconds % 60) as u8,
            unix_ms: ms,
        }
    }
}

#[cfg(not(windows))]
mod platform {
    //! 非 Windows 只用于编译与纯逻辑测试：按 UTC 计算。
    //! 时段规则的实际匹配只在 Windows 发行版上成立（本项目就是 Windows 应用）。

    use super::LocalTime;

    fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
        // Howard Hinnant 的算法（proleptic Gregorian）。
        let z = days_since_epoch + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
    }

    pub fn from_unix_ms(ms: u64) -> LocalTime {
        let seconds = ms / 1000;
        let days = (seconds / 86_400) as i64;
        let (year, month, day) = civil_from_days(days);
        let weekday = ((days + 3) % 7 + 1) as u8;
        LocalTime {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            weekday,
            hour: ((seconds / 3600) % 24) as u8,
            minute: ((seconds / 60) % 60) as u8,
            second: (seconds % 60) as u8,
            unix_ms: ms,
        }
    }

    pub fn now() -> LocalTime {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        from_unix_ms(ms)
    }

    /// 非 Windows 没有"用户时区"的概念，按本模块的约定退化成 UTC。
    pub fn utc_offset_minutes() -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只用于纯逻辑测试的构造器（不读系统时钟）。
    fn at(year: u16, month: u8, day: u8, weekday: u8, hour: u8, minute: u8) -> LocalTime {
        LocalTime {
            year,
            month,
            day,
            weekday,
            hour,
            minute,
            second: 0,
            unix_ms: 0,
        }
    }

    #[test]
    fn hhmm_parsing_validates_ranges() {
        assert_eq!(parse_hhmm("09:30"), Some(570));
        assert_eq!(parse_hhmm("0:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("09:60"), None);
        assert_eq!(parse_hhmm("9点"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    #[test]
    fn minutes_and_stamps_are_consistent() {
        let time = from_unix_ms(1_700_000_000_000);
        assert!(time.minutes_of_day() < 1440);
        assert!((1..=7).contains(&time.weekday));
        assert_eq!(time.hhmm().len(), 5);
        assert_eq!(time.full_stamp().len(), 19);
        assert_eq!(time.short_stamp().len(), 11);
        assert_eq!(time.date_stamp().len(), 10);
    }

    #[test]
    fn now_is_sane() {
        let time = now();
        assert!(time.year >= 2020);
        assert!((1..=7).contains(&time.weekday));
        assert!(time.month >= 1 && time.month <= 12);
    }

    /// 口语化时间与旧版 `formatTime` 逐段对齐（这是"报时间"的实际输出）。
    #[test]
    fn spoken_time_matches_legacy_wording() {
        assert_eq!(at(2026, 11, 5, 4, 20, 9).spoken_time(), "晚上8点9分");
        assert_eq!(at(2026, 11, 5, 4, 20, 0).spoken_time(), "晚上8点整");
        assert_eq!(at(2026, 11, 5, 4, 0, 5).spoken_time(), "凌晨12点5分");
        assert_eq!(at(2026, 11, 5, 4, 6, 30).spoken_time(), "上午6点30分");
        assert_eq!(at(2026, 11, 5, 4, 12, 0).spoken_time(), "中午12点整");
        assert_eq!(at(2026, 11, 5, 4, 13, 15).spoken_time(), "下午1点15分");
        assert_eq!(at(2026, 11, 5, 4, 17, 59).spoken_time(), "下午5点59分");
        assert_eq!(at(2026, 11, 5, 4, 23, 59).spoken_time(), "晚上11点59分");
    }

    #[test]
    fn spoken_date_uses_chinese_weekday() {
        assert_eq!(at(2026, 11, 5, 4, 9, 0).spoken_date(), "11月5日，星期四");
        assert_eq!(at(2026, 1, 1, 7, 9, 0).spoken_date(), "1月1日，星期日");
        assert_eq!(at(2026, 1, 1, 6, 9, 0).spoken_date(), "1月1日，星期六");
    }

    /// 时区偏移的显示形态：整点不带分、东八区带 `+`、UTC 单独一种写法。
    #[test]
    fn offsets_are_formatted_for_display() {
        assert_eq!(format_offset(0), "UTC");
        assert_eq!(format_offset(480), "UTC+08:00");
        assert_eq!(format_offset(-300), "UTC-05:00");
        assert_eq!(format_offset(330), "UTC+05:30");
        assert_eq!(format_offset(-210), "UTC-03:30");
    }

    /// 星期标签只接受 1–7；越界不 panic（配置里出现过 0 的旧数据也不能炸）。
    #[test]
    fn weekday_label_covers_all_iso_days() {
        assert_eq!(weekday_label(1), "星期一");
        assert_eq!(weekday_label(7), "星期日");
        assert_eq!(weekday_label(0), "星期一");
        assert_eq!(weekday_label(9), "星期一");
    }
}
