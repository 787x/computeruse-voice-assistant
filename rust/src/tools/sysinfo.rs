//! `system_info`：CPU / 内存 / 磁盘 / 电池 / 系统信息。
//!
//! 为什么直接调 Win32 而不是像旧版那样 `spawnSync('powershell.exe')`：
//! 旧版 Node 没有这些能力，只能用 PowerShell 兜（一次 12 秒超时、还可能弹窗）。
//! Rust 这边 `GetSystemTimes` / `GlobalMemoryStatusEx` / `GetDiskFreeSpaceExW` /
//! `GetSystemPowerStatus` 全是一次系统调用，几毫秒内完成，也不需要子进程。
//!
//! **无法获得的字段要明确标注**，不能悄悄留空：台式机没有电池、某些磁盘不计入
//! `GetLogicalDrives`（如未挂载的移动硬盘），这些都要让模型与用户看得见是
//! "拿不到"而不是"是 0"。

use serde_json::{json, Value};

use crate::tools::error::ToolError;
use crate::tools::spec::{as_args, coerce_bool, object_schema, ToolSpec};

/// 工具定义。
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "system_info",
        description: "查看本机状态：CPU 占用、内存、磁盘剩余、电池、开机时长、系统版本。\
用户问\"电脑卡不卡\"\"硬盘还剩多少\"\"电池还有多少\"时用它。拿不到的字段会明确说明（例如台式机没有电池）。",
        parameters: object_schema(
            json!({
                "detail": { "type": "boolean", "description": "为 true 时返回每个分区的明细" }
            }),
            &[],
        ),
        read_only: true,
        long_timeout: false,
    }
}

/// 一次采样得到的系统状态。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub hostname: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_percent: Option<u32>,
    pub logical_cores: Option<u32>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_percent: Option<u32>,
    pub disks: Vec<DiskInfo>,
    /// `None` = 拿不到（无电池 / 系统不提供）。
    pub battery: Option<BatteryInfo>,
    pub uptime_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    pub drive: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryInfo {
    pub percent: u8,
    pub charging: bool,
}

fn gb(bytes: u64) -> f64 {
    (bytes as f64 / 1_073_741_824.0 * 10.0).round() / 10.0
}

/// CPU 占用率：`GetSystemTimes` 只给累计值，单次算不出来，要采样两次。
fn cpu_percent(sample: std::time::Duration) -> Option<u32> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::GetSystemTimes;

        fn ticks(time: FILETIME) -> u64 {
            ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
        }

        let read = || -> Option<(u64, u64, u64)> {
            let mut idle = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            // SAFETY：三个入参都是调用方栈上的结构，函数只写入它们。
            unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
            Some((ticks(idle), ticks(kernel), ticks(user)))
        };

        let first = read()?;
        std::thread::sleep(sample);
        let second = read()?;
        // `kernel` 已经包含 idle，所以总时间 = kernel + user。
        let idle_delta = second.0.saturating_sub(first.0);
        let total_delta =
            (second.1.saturating_sub(first.1)).saturating_add(second.2.saturating_sub(first.2));
        if total_delta == 0 {
            return None;
        }
        let busy = total_delta.saturating_sub(idle_delta);
        Some(((busy as f64 / total_delta as f64) * 100.0).round() as u32)
    }
    #[cfg(not(windows))]
    {
        let _ = sample;
        None
    }
}

/// 取一份系统快照。
///
/// 采样 CPU 占用时**阻塞约 300ms**——这是唯一能算出"当前占用"的方式。
/// 调用方必须在后台任务里调它（本模块的 `run` 已经用 `spawn_blocking` 包好）。
pub fn snapshot() -> Snapshot {
    // 用结构体初始化而不是逐个赋值：`Default` + 多次赋值会掩盖"哪个字段没填"。
    let mut snapshot = Snapshot {
        logical_cores: std::thread::available_parallelism()
            .ok()
            .map(|value| value.get() as u32),
        cpu_percent: cpu_percent(std::time::Duration::from_millis(300)),
        ..Snapshot::default()
    };

    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::{
            ComputerNameDnsHostname, GetComputerNameExW, GetTickCount64, GlobalMemoryStatusEx,
            MEMORYSTATUSEX,
        };

        let mut memory = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY：入参是调用方栈上的结构，`dwLength` 已按结构大小填好。
        if unsafe { GlobalMemoryStatusEx(&mut memory) }.is_ok() {
            snapshot.memory_total_bytes = Some(memory.ullTotalPhys);
            snapshot.memory_used_bytes =
                Some(memory.ullTotalPhys.saturating_sub(memory.ullAvailPhys));
            snapshot.memory_percent = Some(memory.dwMemoryLoad);
        }

        // SAFETY：`GetTickCount64` 无入参。
        snapshot.uptime_seconds = Some(unsafe { GetTickCount64() } / 1000);

        let mut buffer = [0u16; 256];
        let mut size = buffer.len() as u32;
        // SAFETY：缓冲区与长度都由调用方提供，函数只按长度写入。
        if unsafe {
            GetComputerNameExW(
                ComputerNameDnsHostname,
                Some(windows::core::PWSTR(buffer.as_mut_ptr())),
                &mut size,
            )
        }
        .is_ok()
        {
            let text = String::from_utf16_lossy(&buffer[..(size as usize).min(buffer.len())]);
            if !text.trim().is_empty() {
                snapshot.hostname = Some(text);
            }
        }

        snapshot.cpu_model = registry_string(
            "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
            "ProcessorNameString",
        );
        snapshot.os_name = registry_string(
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "ProductName",
        );
        snapshot.os_version = registry_string(
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "DisplayVersion",
        )
        .or_else(|| {
            registry_string(
                "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
                "CurrentBuild",
            )
        });

        snapshot.disks = disks();
        snapshot.battery = battery();
    }

    #[cfg(not(windows))]
    {
        // 非 Windows 只用于编译与纯逻辑测试：不伪造系统信息。
    }

    snapshot
}

/// 读一个 `REG_SZ` 注册表值；读不到返回 `None`（**不猜**）。
#[cfg(windows)]
fn registry_string(key: &str, value: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let key_wide: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    let value_wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0u16; 512];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // SAFETY：键名/值名都是 NUL 结尾的宽字符串；缓冲区与长度由调用方提供，
    // `RegGetValueW` 只按 `size` 写入，且不需要我们手动关闭句柄（它内部开关）。
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_wide.as_ptr()),
            PCWSTR(value_wide.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let units = (size as usize / 2).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..units]);
    let trimmed = text.trim_end_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 枚举有容量的本地/固定磁盘（跳过光驱、未挂载的盘符）。
#[cfg(windows)]
fn disks() -> Vec<DiskInfo> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    };
    // `DRIVE_*` 常量在 `System::WindowsProgramming` 里（不是 FileSystem）。
    use windows::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE};

    let mut out = Vec::new();
    // SAFETY：无入参，返回位掩码。
    let mask = unsafe { GetLogicalDrives() };
    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY：`wide` 是 NUL 结尾的宽字符串。
        let kind = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        // 只要固定磁盘、网络盘与内存盘；光驱/可移动盘可能没有介质。
        if kind != DRIVE_FIXED && kind != DRIVE_REMOTE && kind != DRIVE_RAMDISK {
            continue;
        }
        let mut free = 0u64;
        let mut total = 0u64;
        // SAFETY：两个出参都是调用方栈上的 u64，路径是 NUL 结尾的宽字符串。
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide.as_ptr()),
                None,
                Some(&mut total),
                Some(&mut free),
            )
        };
        if ok.is_err() || total == 0 {
            continue;
        }
        out.push(DiskInfo {
            drive: format!("{letter}:"),
            total_bytes: total,
            free_bytes: free,
        });
    }
    out
}

/// 电池状态。台式机 / 无电池 → `None`（界面显示"无法获得"）。
#[cfg(windows)]
fn battery() -> Option<BatteryInfo> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY：出参是调用方栈上的结构。
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return None;
    }
    // `BatteryFlag = 128` 表示"没有系统电池"；`BatteryLifePercent = 255` 表示未知。
    if status.BatteryFlag == 128 || status.BatteryLifePercent == 255 {
        return None;
    }
    Some(BatteryInfo {
        percent: status.BatteryLifePercent.min(100),
        // `ACLineStatus == 1` = 已接入交流电（充电中）。
        charging: status.ACLineStatus == 1,
    })
}

/// 把快照渲染成给模型读的文本。
pub fn render(snapshot: &Snapshot, detail: bool) -> String {
    let mut lines = Vec::new();
    let host = snapshot.hostname.as_deref().unwrap_or("（无法获得主机名）");
    let os = match (snapshot.os_name.as_deref(), snapshot.os_version.as_deref()) {
        (Some(name), Some(version)) => format!("{name} {version}"),
        (Some(name), None) => name.to_string(),
        (None, Some(version)) => format!("Windows（版本号 {version}）"),
        (None, None) => "（无法获得系统版本）".to_string(),
    };
    lines.push(format!("主机：{host}（{os}）"));
    let cpu_model = snapshot.cpu_model.as_deref().unwrap_or("（无法获得型号）");
    let cores = snapshot
        .logical_cores
        .map(|cores| format!("{cores} 个逻辑核"))
        .unwrap_or_else(|| "（无法获得核心数）".to_string());
    let percent = snapshot
        .cpu_percent
        .map(|percent| format!("{percent}% 占用"))
        .unwrap_or_else(|| "占用率无法获得".to_string());
    lines.push(format!("CPU：{cpu_model}，{cores}，{percent}"));

    match (snapshot.memory_total_bytes, snapshot.memory_used_bytes) {
        (Some(total), Some(used)) => {
            let percent = snapshot
                .memory_percent
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string());
            lines.push(format!(
                "内存：{} / {} GB（{percent}% 已用，可用 {} GB）",
                gb(used),
                gb(total),
                gb(total.saturating_sub(used))
            ));
        }
        _ => lines.push("内存：（无法获得）".to_string()),
    }

    if snapshot.disks.is_empty() {
        lines.push("磁盘：（无法获得分区信息）".to_string());
    } else if detail {
        let entries: Vec<String> = snapshot
            .disks
            .iter()
            .map(|disk| {
                format!(
                    "{} 剩 {}GB / 共 {}GB",
                    disk.drive,
                    gb(disk.free_bytes),
                    gb(disk.total_bytes)
                )
            })
            .collect();
        lines.push(format!("磁盘（{} 个分区）：", snapshot.disks.len()));
        lines.push(entries.join("\n"));
    } else {
        let summary: Vec<String> = snapshot
            .disks
            .iter()
            .map(|disk| {
                format!(
                    "{} 剩 {}GB / 共 {}GB",
                    disk.drive,
                    gb(disk.free_bytes),
                    gb(disk.total_bytes)
                )
            })
            .collect();
        lines.push(format!("磁盘：{}", summary.join("；")));
    }

    match &snapshot.battery {
        Some(battery) => lines.push(format!(
            "电池：{}%{}",
            battery.percent,
            if battery.charging {
                "（充电中）"
            } else {
                ""
            }
        )),
        None => lines.push("电池：（无法获得：本机没有电池，或系统未提供该信息）".to_string()),
    }

    match snapshot.uptime_seconds {
        Some(seconds) => lines.push(format!(
            "开机时长：{} 小时 {} 分",
            seconds / 3600,
            (seconds % 3600) / 60
        )),
        None => lines.push("开机时长：（无法获得）".to_string()),
    }
    lines.join("\n")
}

/// 执行一次 `system_info`。
pub async fn run(args: &Value, cancel: &crate::cancel::CancelToken) -> Result<String, ToolError> {
    let object = as_args(args);
    let detail = coerce_bool(&object, "detail").unwrap_or(false);
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    // CPU 占用要采样 300ms（阻塞），不能在 async 任务里直接 sleep。
    let handle = tokio::task::spawn_blocking(snapshot);
    let snapshot = tokio::select! {
        result = handle => result
            .map_err(|error| ToolError::failed(format!("采样系统信息失败：{error}")))?,
        _ = cancel.cancelled() => return Err(ToolError::cancelled()),
    };
    Ok(render(&snapshot, detail))
}

/// 供设置页/诊断使用：一行摘要（不采样，只用便宜的系统调用）。
pub fn summary_line() -> String {
    let cores = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(0);
    format!("本机 {cores} 个逻辑核；system_info 可查 CPU/内存/磁盘/电池/系统版本")
}

/// 结构化结果（供测试断言；不进入模型上下文）。
pub fn to_json(snapshot: &Snapshot) -> Value {
    json!({
        "hostname": snapshot.hostname,
        "os": snapshot.os_name,
        "osVersion": snapshot.os_version,
        "cpuModel": snapshot.cpu_model,
        "cpuPercent": snapshot.cpu_percent,
        "logicalCores": snapshot.logical_cores,
        "memoryTotalBytes": snapshot.memory_total_bytes,
        "memoryUsedBytes": snapshot.memory_used_bytes,
        "disks": snapshot.disks.iter().map(|disk| json!({
            "drive": disk.drive,
            "totalBytes": disk.total_bytes,
            "freeBytes": disk.free_bytes,
        })).collect::<Vec<_>>(),
        "battery": snapshot.battery.as_ref().map(|battery| json!({
            "percent": battery.percent,
            "charging": battery.charging,
        })),
        "uptimeSeconds": snapshot.uptime_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            hostname: Some("PC".into()),
            os_name: Some("Windows 11 Pro".into()),
            os_version: Some("24H2".into()),
            cpu_model: Some("Test CPU".into()),
            cpu_percent: Some(12),
            logical_cores: Some(16),
            memory_total_bytes: Some(32 * 1024 * 1024 * 1024),
            memory_used_bytes: Some(16 * 1024 * 1024 * 1024),
            memory_percent: Some(50),
            disks: vec![DiskInfo {
                drive: "C:".into(),
                total_bytes: 512 * 1024 * 1024 * 1024,
                free_bytes: 128 * 1024 * 1024 * 1024,
            }],
            battery: Some(BatteryInfo {
                percent: 88,
                charging: true,
            }),
            uptime_seconds: Some(3 * 3600 + 25 * 60),
        }
    }

    #[test]
    fn render_includes_every_reported_field() {
        let text = render(&sample(), false);
        assert!(text.contains("主机：PC（Windows 11 Pro 24H2）"), "{text}");
        assert!(text.contains("Test CPU，16 个逻辑核，12% 占用"), "{text}");
        assert!(
            text.contains("内存：16 / 32 GB（50% 已用，可用 16 GB）"),
            "{text}"
        );
        assert!(text.contains("磁盘：C: 剩 128GB / 共 512GB"), "{text}");
        assert!(text.contains("电池：88%（充电中）"), "{text}");
        assert!(text.contains("开机时长：3 小时 25 分"), "{text}");
    }

    /// 无法获得的字段必须说"无法获得"，不能留空或写 0。
    #[test]
    fn unavailable_fields_are_explicitly_marked() {
        let mut snapshot = sample();
        snapshot.battery = None;
        snapshot.cpu_percent = None;
        snapshot.memory_total_bytes = None;
        snapshot.memory_used_bytes = None;
        snapshot.uptime_seconds = None;
        snapshot.disks.clear();
        snapshot.hostname = None;
        snapshot.os_name = None;
        snapshot.os_version = None;
        let text = render(&snapshot, false);
        assert!(text.contains("无法获得主机名"), "{text}");
        assert!(text.contains("无法获得系统版本"), "{text}");
        assert!(text.contains("占用率无法获得"), "{text}");
        assert!(text.contains("内存：（无法获得）"), "{text}");
        assert!(text.contains("磁盘：（无法获得分区信息）"), "{text}");
        assert!(text.contains("本机没有电池"), "{text}");
        assert!(text.contains("开机时长：（无法获得）"), "{text}");
    }

    #[test]
    fn detail_lists_each_partition() {
        let mut snapshot = sample();
        snapshot.disks.push(DiskInfo {
            drive: "D:".into(),
            total_bytes: 1024 * 1024 * 1024 * 1024,
            free_bytes: 256 * 1024 * 1024 * 1024,
        });
        let text = render(&snapshot, true);
        assert!(text.contains("磁盘（2 个分区）"), "{text}");
        assert!(text.contains("C: 剩 128GB / 共 512GB"), "{text}");
        assert!(text.contains("D: 剩 256GB / 共 1024GB"), "{text}");
    }

    #[test]
    fn json_shape_matches_the_public_fields() {
        let value = to_json(&sample());
        assert_eq!(value["hostname"], json!("PC"));
        assert_eq!(value["battery"]["charging"], json!(true));
        assert_eq!(value["disks"][0]["drive"], json!("C:"));
    }

    #[test]
    fn spec_is_read_only_and_has_no_required_fields() {
        let spec = spec();
        assert_eq!(spec.name, "system_info");
        assert!(spec.read_only);
        assert!(spec.parameters.get("required").is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_snapshot_reports_memory_and_uptime() {
        let snapshot = snapshot();
        assert!(snapshot.memory_total_bytes.unwrap_or(0) > 0);
        assert!(snapshot.uptime_seconds.unwrap_or(0) > 0);
        assert!(snapshot.logical_cores.unwrap_or(0) >= 1);
        assert!(!snapshot.disks.is_empty(), "本机至少要有一个固定磁盘");
        // CPU 采样要么给出百分比，要么明确 None（不允许瞎猜）。
        assert!(snapshot
            .cpu_percent
            .map(|value| value <= 100)
            .unwrap_or(true));
    }
}
