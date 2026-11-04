//! 本地时间：只提供"墙上的时间"（星期、时:分）与可读时间戳。
//!
//! 为什么单独一个模块：时段规则（按本地时间切换模型方案）与会话列表的
//! "最近更新时间"都依赖本地时区，而 `std` 没有本地时间 API。
//! Windows 上用 `GetLocalTime`（系统已经按用户时区换算好），
//! 其它平台退化成 UTC——只影响测试与编译，正式发行只在 Windows。

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
    use windows::Win32::System::Time::{FileTimeToSystemTime, LocalFileTimeToLocalSystemTime};

    use super::LocalTime;

    /// 1601-01-01 到 1970-01-01 之间的 100ns 间隔数。
    const EPOCH_FILETIME_OFFSET: u64 = 116_444_736_000_000_000;
    const TICKS_PER_MS: u64 = 10_000;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn now_is_sane() {
        let time = now();
        assert!(time.year >= 2020);
        assert!((1..=7).contains(&time.weekday));
        assert!(time.month >= 1 && time.month <= 12);
    }
}
