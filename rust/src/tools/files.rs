//! `files`：本地文件操作（list / read / write / copy / move / delete / info）。
//!
//! 为什么**一个工具带 `action`** 而不是拆成六个：模型对"同一类操作的多个动词"
//! 很容易选错工具名，一个工具 + 枚举让参数校验集中，权限也只需按 `action` 分级
//! （见 `permissions::classify`：list/read/info → `file.read`，
//! write/copy/move → `file.write`，delete → `file.delete`）。**绝不允许**把删除
//! 归到读取能力下。
//!
//! 限制沿旧版：
//!  - 读文件最多 200KB，列目录最多 200 项（多出来的明说"已截断"）；
//!  - 拒绝对盘符根、Windows 目录本身、Program Files 本身、用户目录根动手；
//!  - 路径必须是绝对路径（相对路径会以**进程当前目录**解析，那是随启动方式变化的，
//!    对用户来说不可预测）。
//!
//! 所有文件 I/O 都走 `spawn_blocking`：这些调用会阻塞几毫秒到几百毫秒，
//! 直接在 async 任务里做会把整个运行时的一个 worker 占住（Slint 那边的回合也会跟着卡）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::error::{ArgError, ToolError};
use crate::tools::spec::{as_args, coerce_bool, coerce_string, object_schema, ToolSpec};

/// 单次读取的最大字节数（旧版 `FILES_DEFAULTS.maxBytes`）。
pub const MAX_READ_BYTES: usize = 200 * 1024;
/// 单次列目录的最大条目数（旧版 `FILES_DEFAULTS.maxEntries`）。
pub const MAX_ENTRIES: usize = 200;
/// 递归列目录的最大深度（防止误对 C:\Users 全盘递归）。
pub const MAX_WALK_DEPTH: usize = 6;

/// 工具定义。
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "files",
        description: "本地文件操作：list（列目录）/ read（读文本）/ write（写文件）/ copy / move / delete / info。\
路径用 Windows 绝对路径（如 C:\\Users\\me\\a.txt）。读与列目录默认放行；写入、移动、删除会按程序权限设置处理。\
read 最多返回 200KB，list 最多 200 项。",
        parameters: object_schema(
            json!({
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "write", "copy", "move", "delete", "info"]
                },
                "path": { "type": "string", "description": "目标路径（Windows 绝对路径）" },
                "dest": { "type": "string", "description": "copy / move 的目标路径" },
                "content": { "type": "string", "description": "write 要写入的文本（UTF-8）" },
                "recursive": { "type": "boolean", "description": "list 是否递归子目录（最多 6 层）" }
            }),
            &["action", "path"],
        ),
        read_only: false,
        long_timeout: false,
    }
}

/// 把相对路径按**进程当前目录**解析成绝对路径。
fn absolute(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|dir| dir.join(&path))
        .unwrap_or(path)
}

/// 危险目标：这些路径本身被删除/覆盖就不是"重装软件"能恢复的。
///
/// 只挡目录**本身**（`C:\Windows\System32\drivers\x.sys` 这种具体文件不挡）：
/// 用户确实可能要求清理某个具体文件，但绝不接受"把 C:\Windows 删掉"。
fn assert_safe_target(path: &Path) -> Result<(), String> {
    let resolved = path.to_string_lossy().to_string();
    let trimmed = resolved.trim_end_matches(['\\', '/']).to_string();
    // 盘符根：`C:` / `C:\`。
    let is_drive_root = trimmed.len() == 2
        && trimmed.ends_with(':')
        && trimmed
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic());
    if is_drive_root {
        return Err(format!("拒绝对盘符根目录操作：{resolved}"));
    }
    let lower = trimmed.to_lowercase();
    let mut blocked: Vec<String> = vec![
        "c:\\windows",
        "c:\\program files",
        "c:\\program files (x86)",
        "c:\\programdata",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    for key in [
        "SystemRoot",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "USERPROFILE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            let text = PathBuf::from(value).to_string_lossy().to_string();
            let text = text.trim_end_matches(['\\', '/']).to_lowercase();
            if !text.is_empty() {
                blocked.push(text);
            }
        }
    }
    for item in blocked {
        if lower == item {
            return Err(format!("拒绝对系统/用户目录本身操作：{resolved}"));
        }
    }
    Ok(())
}

/// 执行一次 `files` 调用（异步外壳 + 阻塞核心）。
pub async fn run(args: &Value, cancel: &crate::cancel::CancelToken) -> Result<String, ToolError> {
    let object = as_args(args);
    let action = coerce_string(&object, "action")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let raw_path = coerce_string(&object, "path").unwrap_or_default();
    let raw_path = raw_path.trim().to_string();
    if raw_path.is_empty() {
        return Err(ToolError::Arg(ArgError::new("files 需要 path")));
    }
    if action.is_empty() {
        return Err(ToolError::Arg(ArgError::new(
            "files 需要 action（list/read/write/copy/move/delete/info）",
        )));
    }
    // 参数校验必须**在动作之前**完成：拖到阻塞线程里再报错就可能在报错前已经动过文件。
    let request = Request {
        action: action.clone(),
        path: absolute(&raw_path),
        dest: coerce_string(&object, "dest")
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .map(|text| absolute(&text)),
        content: coerce_string(&object, "content").unwrap_or_default(),
        recursive: coerce_bool(&object, "recursive").unwrap_or(false),
    };
    request.validate()?;
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }

    let handle = tokio::task::spawn_blocking(move || request.execute());
    let joined = tokio::select! {
        result = handle => result,
        _ = cancel.cancelled() => {
            // 阻塞线程无法被取消：文件动作可能已经完成。如实说明。
            return Err(ToolError::failed(
                "已中断：文件操作可能已经执行，请重新检查目标路径的实际状态",
            ));
        }
    };
    match joined {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(ToolError::Arg(error))) => Err(ToolError::Arg(error)),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(ToolError::failed(format!("文件操作线程异常：{error}"))),
    }
}

/// 一次已校验的文件请求。
struct Request {
    action: String,
    path: PathBuf,
    dest: Option<PathBuf>,
    content: String,
    recursive: bool,
}

impl Request {
    fn validate(&self) -> Result<(), ToolError> {
        match self.action.as_str() {
            "list" | "read" | "info" | "write" | "delete" => {}
            "copy" | "move" => {
                if self.dest.is_none() {
                    return Err(ToolError::Arg(ArgError::new(format!(
                        "{} 需要 dest",
                        self.action
                    ))));
                }
            }
            other => {
                return Err(ToolError::Arg(ArgError::new(format!(
                    "未知 action：{other}（可用：list/read/write/copy/move/delete/info）"
                ))));
            }
        }
        // 删除与覆盖类动作先挡危险目标；读/列目录不需要（读 C:\Windows 是合理需求）。
        if matches!(self.action.as_str(), "delete" | "write" | "move") {
            assert_safe_target(&self.path)
                .map_err(|message| ToolError::Arg(ArgError::new(message)))?;
        }
        if matches!(self.action.as_str(), "copy" | "move") {
            if let Some(dest) = self.dest.as_ref() {
                assert_safe_target(dest)
                    .map_err(|message| ToolError::Arg(ArgError::new(message)))?;
            }
        }
        Ok(())
    }

    fn execute(self) -> Result<String, ToolError> {
        match self.action.as_str() {
            "list" => self.list(),
            "read" => self.read(),
            "write" => self.write(),
            "copy" => self.copy_or_move(false),
            "move" => self.copy_or_move(true),
            "delete" => self.delete(),
            "info" => self.info(),
            other => Err(ToolError::Arg(ArgError::new(format!(
                "未知 action：{other}"
            )))),
        }
    }

    fn list(self) -> Result<String, ToolError> {
        let path = self.path;
        let mut entries: Vec<Entry> = Vec::new();
        let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((path.clone(), 0));
        let mut truncated = false;
        while let Some((dir, depth)) = queue.pop_front() {
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Ok(read) = std::fs::read_dir(&dir) else {
                // 子目录读不了（权限）就跳过，但根目录读不了要如实报错。
                if depth == 0 {
                    return Err(ToolError::failed(format!(
                        "无法列出目录：{}",
                        dir.display()
                    )));
                }
                continue;
            };
            let mut items: Vec<(PathBuf, bool, u64)> = Vec::new();
            for item in read.flatten() {
                let full = item.path();
                let is_dir = item.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                let size = if is_dir {
                    0
                } else {
                    item.metadata().map(|meta| meta.len()).unwrap_or(0)
                };
                items.push((full, is_dir, size));
            }
            // 目录在前、同类按名字排序（与旧版一致，便于人读）。
            items.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| {
                    a.0.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                        .cmp(
                            &b.0.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_lowercase(),
                        )
                })
            });
            for (full, is_dir, size) in items {
                if entries.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                if is_dir && self.recursive && depth + 1 < MAX_WALK_DEPTH {
                    queue.push_back((full.clone(), depth + 1));
                }
                entries.push(Entry {
                    is_dir,
                    path: full,
                    size,
                });
            }
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|entry| {
                if entry.is_dir {
                    format!("[目录] {}", entry.path.display())
                } else {
                    format!("[文件] {}  {} 字节", entry.path.display(), entry.size)
                }
            })
            .collect();
        let mut header = format!(
            "列出 {}（{} 项{}）",
            path.display(),
            entries.len(),
            if truncated { "，已截断" } else { "" }
        );
        if entries.is_empty() {
            header.push_str("：\n（空目录）");
        } else {
            header.push_str("：\n");
            header.push_str(&lines.join("\n"));
        }
        Ok(header)
    }

    fn read(self) -> Result<String, ToolError> {
        let path = self.path;
        let meta = std::fs::metadata(&path)
            .map_err(|error| ToolError::failed(format!("读取 {} 失败：{error}", path.display())))?;
        if meta.is_dir() {
            return Err(ToolError::failed(format!(
                "{} 是目录；要看内容请用 action=list",
                path.display()
            )));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| ToolError::failed(format!("读取 {} 失败：{error}", path.display())))?;
        let total = bytes.len();
        let slice = if total > MAX_READ_BYTES {
            &bytes[..MAX_READ_BYTES]
        } else {
            &bytes[..]
        };
        // 截断可能切在多字节字符中间：用 lossy 解码，尾部替换字符不可避免，
        // 但整个文本不会因此变成乱码。
        let text = String::from_utf8_lossy(slice).to_string();
        let truncated = total > MAX_READ_BYTES;
        let head = if truncated {
            format!(
                "读取 {}（{total} 字节，只取前 {MAX_READ_BYTES} 字节）：",
                path.display()
            )
        } else {
            format!("读取 {}（{total} 字节）：", path.display())
        };
        Ok(format!("{head}\n{text}"))
    }

    fn write(self) -> Result<String, ToolError> {
        let path = self.path;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ToolError::failed(format!("无法创建目录 {}：{error}", parent.display()))
                })?;
            }
        }
        std::fs::write(&path, self.content.as_bytes())
            .map_err(|error| ToolError::failed(format!("写入 {} 失败：{error}", path.display())))?;
        Ok(format!(
            "已写入 {}（{} 字节）",
            path.display(),
            self.content.len()
        ))
    }

    fn copy_or_move(self, move_it: bool) -> Result<String, ToolError> {
        let path = self.path;
        let dest = self
            .dest
            .clone()
            .ok_or_else(|| ToolError::Arg(ArgError::new("需要 dest")))?;
        let meta = std::fs::metadata(&path)
            .map_err(|error| ToolError::failed(format!("{} 不可用：{error}", path.display())))?;
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ToolError::failed(format!("无法创建目录 {}：{error}", parent.display()))
                })?;
            }
        }
        if move_it {
            std::fs::rename(&path, &dest).map_err(|error| {
                ToolError::failed(format!(
                    "移动 {} → {} 失败：{error}",
                    path.display(),
                    dest.display()
                ))
            })?;
            return Ok(format!("已移动 {} → {}", path.display(), dest.display()));
        }
        if meta.is_dir() {
            copy_dir(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).map_err(|error| {
                ToolError::failed(format!(
                    "复制 {} → {} 失败：{error}",
                    path.display(),
                    dest.display()
                ))
            })?;
        }
        Ok(format!("已复制 {} → {}", path.display(), dest.display()))
    }

    fn delete(self) -> Result<String, ToolError> {
        let path = self.path;
        let meta = std::fs::metadata(&path).map_err(|error| {
            ToolError::failed(format!("路径不存在：{}（{error}）", path.display()))
        })?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|error| {
                ToolError::failed(format!("删除目录 {} 失败：{error}", path.display()))
            })?;
            Ok(format!("已删除目录 {}", path.display()))
        } else {
            std::fs::remove_file(&path).map_err(|error| {
                ToolError::failed(format!("删除文件 {} 失败：{error}", path.display()))
            })?;
            Ok(format!("已删除文件 {}", path.display()))
        }
    }

    fn info(self) -> Result<String, ToolError> {
        let path = self.path;
        let meta = std::fs::metadata(&path).map_err(|error| {
            ToolError::failed(format!("路径不存在：{}（{error}）", path.display()))
        })?;
        let modified = meta.modified().ok().map(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| crate::clock::from_unix_ms(elapsed.as_millis() as u64).full_stamp())
                .unwrap_or_else(|_| "未知".to_string())
        });
        Ok(format!(
            "路径：{}\n类型：{}\n大小：{} 字节\n修改时间：{}",
            path.display(),
            if meta.is_dir() { "目录" } else { "文件" },
            meta.len(),
            modified.unwrap_or_else(|| "无法读取".to_string())
        ))
    }
}

struct Entry {
    is_dir: bool,
    path: PathBuf,
    size: u64,
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), ToolError> {
    std::fs::create_dir_all(to)
        .map_err(|error| ToolError::failed(format!("无法创建 {}：{error}", to.display())))?;
    let read = std::fs::read_dir(from)
        .map_err(|error| ToolError::failed(format!("无法读取 {}：{error}", from.display())))?;
    for item in read.flatten() {
        let source = item.path();
        let target = to.join(item.file_name());
        let is_dir = item.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if is_dir {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|error| {
                ToolError::failed(format!(
                    "复制 {} → {} 失败：{error}",
                    source.display(),
                    target.display()
                ))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("cuava-files-{name}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn list_read_write_info_delete_round_trip() {
        let root = temp_root("round-trip");
        let file = root.join("a.txt");
        let cancel = crate::cancel::CancelToken::new();

        let text = run(
            &json!({"action":"write","path":file.to_string_lossy(),"content":"你好"}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("已写入"), "{text}");

        let text = run(
            &json!({"action":"list","path":root.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("a.txt"), "{text}");

        let text = run(
            &json!({"action":"read","path":file.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("你好"), "{text}");

        let text = run(
            &json!({"action":"info","path":file.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("类型：文件"), "{text}");

        let text = run(
            &json!({"action":"delete","path":file.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("已删除文件"), "{text}");
        assert!(!file.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn copy_and_move_keep_legacy_semantics() {
        let root = temp_root("copy-move");
        let source = root.join("src.txt");
        let copied = root.join("copy.txt");
        let moved = root.join("moved.txt");
        std::fs::write(&source, b"data").unwrap();
        let cancel = crate::cancel::CancelToken::new();

        let text = run(
            &json!({"action":"copy","path":source.to_string_lossy(),"dest":copied.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("已复制"), "{text}");
        assert!(copied.is_file());

        let text = run(
            &json!({"action":"move","path":copied.to_string_lossy(),"dest":moved.to_string_lossy()}),
            &cancel,
        )
        .await
        .unwrap();
        assert!(text.contains("已移动"), "{text}");
        assert!(!copied.exists());
        assert!(moved.is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn unknown_action_and_missing_dest_are_argument_errors() {
        let cancel = crate::cancel::CancelToken::new();
        let error = run(&json!({"action":"frobnicate","path":"C:\\x"}), &cancel)
            .await
            .unwrap_err();
        assert!(error.is_argument());

        let error = run(&json!({"action":"copy","path":"C:\\x"}), &cancel)
            .await
            .unwrap_err();
        assert!(error.is_argument());
        assert!(error.user_message().contains("需要 dest"));
    }

    #[tokio::test]
    async fn dangerous_roots_are_refused_before_touching_them() {
        let cancel = crate::cancel::CancelToken::new();
        for path in ["C:\\", "C:\\Windows", "C:\\Windows\\", "C:\\Program Files"] {
            let error = run(&json!({"action":"delete","path":path}), &cancel)
                .await
                .unwrap_err();
            assert!(error.is_argument(), "{path} 必须在动作前被拒绝");
        }
    }

    /// 读一个并不存在的路径：必须是"执行失败"，不能是"参数错误"——
    /// 前者说明动作已发起（可能部分生效），后者才能声称"什么都没做"。
    #[tokio::test]
    async fn missing_file_is_an_execution_failure_not_an_argument_error() {
        let cancel = crate::cancel::CancelToken::new();
        let error = run(
            &json!({"action":"read","path":"C:\\nope\\nope\\nope.txt"}),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(!error.is_argument());
        assert!(error.user_message().contains("读取"));
    }

    #[test]
    fn relative_paths_resolve_against_the_process_directory() {
        let resolved = absolute("some/relative.txt");
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("relative.txt"));
    }

    #[test]
    fn safe_target_rules_only_block_the_directories_themselves() {
        assert!(assert_safe_target(Path::new("C:\\")).is_err());
        assert!(assert_safe_target(Path::new("C:\\Windows")).is_err());
        assert!(
            assert_safe_target(Path::new("C:\\Windows\\System32\\drivers\\etc\\hosts")).is_ok()
        );
        assert!(assert_safe_target(Path::new("C:\\Users\\me\\notes.txt")).is_ok());
    }
}
