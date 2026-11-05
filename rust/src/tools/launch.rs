//! `launch_app`：按**名字**启动已安装的程序（开始菜单级解析）。
//!
//! 为什么需要它：CUA 的 `open_application` 接受的是真实路径或 URL，
//! 而"打开无畏契约"需要的是一次 Start 菜单级的名字解析。旧版实测：模型只能自己摸索
//! "Win 键 → 打字 → 截图 → 点结果"，六七个动作还经常在搜索结果出来前就用完步数。
//!
//! 索引来自两处，缺一不可（沿用旧版 `packages/tools/app-launch.mjs`）：
//!  - `Get-StartApps`：UWP/商店应用 + 一部分桌面应用（AppID 是 `shell:AppsFolder` 形式）；
//!  - 两个开始菜单目录里的 `.lnk`：桌面应用的**中文显示名**往往只在这里。
//!
//! 索引按 TTL 缓存（默认 10 分钟）：装了新软件不需要重启应用。
//! 所有 PowerShell 调用都在后台任务里，绝不占用界面线程。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cancel::CancelToken;
use crate::tools::error::{ArgError, ToolError};
use crate::tools::process::run_powershell;
use crate::tools::spec::{as_args, coerce_string, object_schema, require_string, ToolSpec};

/// 索引多久重建一次（装了新软件不用重启应用）。
pub const INDEX_TTL: Duration = Duration::from_secs(10 * 60);
/// 匹配不上时最多返回几个候选。
pub const MAX_CANDIDATES: usize = 5;
/// 建索引 / 启动的超时（旧版 8 秒）。
const PS_TIMEOUT: Duration = Duration::from_secs(8);

/// 工具定义。
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "launch_app",
        description: "按名字启动已安装的程序（支持中文名，如\"无畏契约\"\"微信\"）。\
打开软件优先用它，不要猜 exe 路径、也不要靠 Win 键搜索一步步点。\
返回实际启动的名字；没匹配上会给出候选，可以换个更接近的名字再调。",
        parameters: object_schema(
            json!({
                "query": { "type": "string", "description": "程序名，中文或英文都行，比如\"无畏契约\"或\"VALORANT\"" }
            }),
            &["query"],
        ),
        read_only: false,
        long_timeout: false,
    }
}

/// 一条索引项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchEntry {
    /// 显示名（`.lnk` 的 BaseName 或 `Get-StartApps` 的 Name）。
    pub name: String,
    /// `lnk` = 开始菜单快捷方式；`uwp` = 商店应用 AppID。
    pub kind: &'static str,
    /// lnk 的完整路径，或 UWP 的 AppID。
    pub target: String,
    /// exe 名（不含 `.exe`），只有 lnk 有。
    pub alias: String,
}

/// 去掉空格、标点与"帮我打开"这类动词，便于"无畏契约"/"VALORANT"互相命中。
pub fn normalize_query(text: &str) -> String {
    let lower = text.to_lowercase();
    let stripped = strip_leading_verb(&lower);
    stripped
        .chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '-' | '_'
                        | '·'
                        | '・'
                        | '.'
                        | ','
                        | '，'
                        | '。'
                        | ':'
                        | '：'
                        | '('
                        | ')'
                        | '（'
                        | '）'
                        | '['
                        | ']'
                        | '【'
                        | '】'
                )
        })
        .collect()
}

/// 模型经常把动词一起塞进 query（"帮我打开无畏契约"）。
fn strip_leading_verb(text: &str) -> String {
    for prefix in ["请", "帮我", "帮忙", "麻烦", "给我"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return strip_action(rest);
        }
    }
    strip_action(text)
}

fn strip_action(text: &str) -> String {
    for verb in ["打开", "启动", "运行", "开启", "开一下", "开个", "开"] {
        if let Some(rest) = text.strip_prefix(verb) {
            return rest.to_string();
        }
    }
    text.to_string()
}

/// 编辑距离（只用来兜住同音错字：无谓契约 / 无畏契约）。
fn edit_distance(a: &[char], b: &[char]) -> usize {
    if a == b {
        return 0;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.iter().enumerate() {
            let cost = usize::from(left != right);
            let value = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
            current.push(value);
        }
        previous = current;
    }
    previous[b.len()]
}

/// 打分：精确 > 前缀 > 包含 > 反向包含 > 英文缩写 > 近似（同音/错字）。0 = 不匹配。
pub fn score_match(name: &str, query: &str) -> u32 {
    let name = normalize_query(name);
    let query = normalize_query(query);
    if name.is_empty() || query.is_empty() {
        return 0;
    }
    if name == query {
        return 100;
    }
    if name.starts_with(&query) {
        return 85;
    }
    if name.contains(&query) {
        return 70;
    }
    if query.contains(&name) && name.chars().count() >= 2 {
        return 55;
    }
    // 英文缩写：VLR 之类。
    if query.chars().all(|ch| ch.is_ascii_alphabetic())
        && name
            .split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|word| !word.is_empty() && word.starts_with(&query))
    {
        return 45;
    }
    // 近似匹配：ASR 把「无畏契约」听成「无谓契约」时编辑距离只有 1。
    let a: Vec<char> = name.chars().collect();
    let b: Vec<char> = query.chars().collect();
    if a.len() >= 2 && b.len() >= 2 {
        let distance = edit_distance(&a, &b);
        let similarity = 1.0 - distance as f64 / a.len().max(b.len()) as f64;
        if similarity >= 0.6 {
            return (60.0 * similarity).round() as u32;
        }
    }
    0
}

/// 应用索引（带 TTL 的进程内缓存）。
#[derive(Debug, Default)]
pub struct AppIndex {
    entries: Vec<LaunchEntry>,
    built_at: Option<Instant>,
}

impl AppIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_ready(&self) -> bool {
        self.built_at.is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 到 TTL 就重建；构建失败时**保留旧索引**（旧索引仍然能启动上次能启动的程序）。
    pub async fn ensure(&mut self, cancel: &CancelToken, force: bool) -> Result<usize, ToolError> {
        let fresh = match self.built_at {
            Some(built) => built.elapsed() < INDEX_TTL,
            None => false,
        };
        if fresh && !force {
            return Ok(self.entries.len());
        }
        match build_index(cancel).await {
            Ok(entries) => {
                self.entries = entries;
                self.built_at = Some(Instant::now());
                Ok(self.entries.len())
            }
            Err(error) => {
                if self.entries.is_empty() {
                    Err(error)
                } else {
                    // 有旧索引就先用着：装索引失败不该让"打开微信"直接失败。
                    Ok(self.entries.len())
                }
            }
        }
    }

    /// 只查不启动（模型据此挑候选）。
    pub fn find(&self, query: &str, limit: usize) -> Vec<(u32, &LaunchEntry)> {
        let mut scored: Vec<(u32, &LaunchEntry)> = self
            .entries
            .iter()
            .map(|entry| {
                let score = score_match(&entry.name, query).max(score_match(&entry.alias, query));
                (score, entry)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
        });
        scored.truncate(limit);
        scored
    }
}

/// 从 PowerShell 建索引（开始菜单 `.lnk` + `Get-StartApps`）。
async fn build_index(cancel: &CancelToken) -> Result<Vec<LaunchEntry>, ToolError> {
    let script = r#"
$sh = New-Object -ComObject WScript.Shell;
$roots = @($env:ProgramData + "\Microsoft\Windows\Start Menu\Programs",
  $env:APPDATA + "\Microsoft\Windows\Start Menu\Programs") | Where-Object { Test-Path $_ };
$links = @(Get-ChildItem $roots -Recurse -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object {
  $target = "";
  try { $target = $sh.CreateShortcut($_.FullName).TargetPath } catch { };
  [pscustomobject]@{ name = $_.BaseName; path = $_.FullName; target = $target }
});
$apps = @(Get-StartApps -ErrorAction SilentlyContinue | ForEach-Object {
  [pscustomobject]@{ name = $_.Name; appId = $_.AppID }
});
[pscustomobject]@{ links = $links; startApps = $apps } | ConvertTo-Json -Compress -Depth 4
"#;
    let outcome = run_powershell(script, None, PS_TIMEOUT, cancel, 400_000).await?;
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    let text = outcome.stdout.trim().trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Err(ToolError::failed(format!(
            "无法建立应用索引：PowerShell 没有输出{}",
            if outcome.stderr.trim().is_empty() {
                String::new()
            } else {
                format!("（{}）", outcome.stderr.trim())
            }
        )));
    }
    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| ToolError::failed(format!("应用索引不是合法 JSON：{error}")))?;
    let mut entries: Vec<LaunchEntry> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let links = items_of(&parsed, "links");
    for link in links {
        let name = link
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let path = link
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() || path.is_empty() {
            continue;
        }
        if !seen.insert(("lnk".to_string(), path.clone())) {
            continue;
        }
        let alias = link
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .rsplit('\\')
            .next()
            .unwrap_or("")
            .trim_end_matches(".exe")
            .trim_end_matches(".EXE")
            .to_string();
        entries.push(LaunchEntry {
            name,
            kind: "lnk",
            target: path,
            alias,
        });
    }
    for app in items_of(&parsed, "startApps") {
        let name = app
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let app_id = app
            .get("appId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() || app_id.is_empty() {
            continue;
        }
        if !seen.insert(("uwp".to_string(), app_id.clone())) {
            continue;
        }
        entries.push(LaunchEntry {
            name,
            kind: "uwp",
            target: app_id,
            alias: String::new(),
        });
    }
    if entries.is_empty() {
        return Err(ToolError::failed(
            "开始菜单里没有可启动的条目（索引为空）".to_string(),
        ));
    }
    Ok(entries)
}

/// `ConvertTo-Json` 在只有一个元素时给对象、多个时给数组；两种都要认。
fn items_of<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
    match value.get(key) {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![other],
    }
}

/// 启动一条索引项。
async fn launch_entry(entry: &LaunchEntry, cancel: &CancelToken) -> Result<(), ToolError> {
    let script = match entry.kind {
        "uwp" => format!(
            "Start-Process explorer.exe -ArgumentList 'shell:AppsFolder\\{}'",
            entry.target.replace('\'', "''")
        ),
        _ => format!(
            "Start-Process -FilePath '{}'",
            entry.target.replace('\'', "''")
        ),
    };
    let outcome = run_powershell(&script, None, PS_TIMEOUT, cancel, 4_000).await?;
    if cancel.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    if outcome.code != 0 {
        let reason = if outcome.stderr.trim().is_empty() {
            format!("PowerShell 退出码 {}", outcome.code)
        } else {
            outcome.stderr.trim().to_string()
        };
        return Err(ToolError::failed(reason));
    }
    Ok(())
}

/// 执行一次 `launch_app`。
pub async fn run(
    index: &mut AppIndex,
    args: &Value,
    cancel: &CancelToken,
) -> Result<String, ToolError> {
    let object = as_args(args);
    let query = require_string(&object, "query", "launch_app")?;
    let started = Instant::now();
    let total = index.ensure(cancel, false).await?;
    let candidates = index.find(&query, MAX_CANDIDATES);
    if candidates.is_empty() {
        return Ok(format!(
            "没有找到叫「{query}」的程序（已索引 {total} 个）。\n\
可以换个更接近的名字再调一次；或者直接用 Computer Use 在开始菜单里搜索，\
但注意搜索结果出来后还需要点一下才算打开。"
        ));
    }
    let (_, best) = candidates[0];
    let others: Vec<String> = candidates
        .iter()
        .skip(1)
        .map(|(_, entry)| entry.name.clone())
        .collect();
    if let Err(error) = launch_entry(best, cancel).await {
        if error.is_cancelled() {
            return Err(error);
        }
        return Ok(format!(
            "找到「{}」但启动失败：{}\n（相近的还有：{}）",
            best.name,
            error.user_message(),
            if others.is_empty() {
                "无".to_string()
            } else {
                others.join("、")
            }
        ));
    }
    let mut text = format!(
        "已启动「{}」（{}，{}ms）",
        best.name,
        if best.kind == "uwp" {
            "商店应用"
        } else {
            "开始菜单快捷方式"
        },
        started.elapsed().as_millis()
    );
    if !others.is_empty() {
        text.push_str(&format!("（相近的还有：{}）", others.join("、")));
    }
    text.push_str("\n如果窗口没起来，可以用 list_windows 或截图确认；本工具只保证启动请求已发出。");
    Ok(text)
}

/// 供设置页/诊断使用：索引里有多少条、来自哪里。
pub fn describe_index(index: &AppIndex) -> String {
    if !index.is_ready() {
        return "应用索引尚未建立（第一次用 launch_app 时会建，约 1~2 秒）".to_string();
    }
    format!(
        "应用索引：{} 条（开始菜单快捷方式 + 商店应用）",
        index.len()
    )
}

/// 数据目录里一份可选的应用索引缓存路径（仅为诊断与测试，不参与启动）。
pub fn cache_hint(paths: &crate::config::AppPaths) -> PathBuf {
    paths.app_index_file()
}

/// 参数校验辅助：`query` 缺失时在**动作之前**报错。
pub fn validate_args(raw: &Value) -> Result<String, ToolError> {
    let object = as_args(raw);
    match coerce_string(&object, "query") {
        Some(text) if !text.trim().is_empty() => Ok(text),
        _ => Err(ToolError::Arg(ArgError::new("launch_app 需要 query"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, alias: &str) -> LaunchEntry {
        LaunchEntry {
            name: name.to_string(),
            kind: "lnk",
            target: format!("C:\\Start\\{name}.lnk"),
            alias: alias.to_string(),
        }
    }

    #[test]
    fn query_normalization_strips_verbs_and_punctuation() {
        assert_eq!(normalize_query("帮我打开无畏契约"), "无畏契约");
        assert_eq!(normalize_query("打开 VALORANT"), "valorant");
        assert_eq!(normalize_query("WeChat"), "wechat");
        assert_eq!(normalize_query("微信（WeChat）"), "微信wechat");
    }

    #[test]
    fn scoring_prefers_exact_then_prefix_then_contains() {
        assert_eq!(score_match("微信", "微信"), 100);
        assert_eq!(score_match("微信 WeChat", "微信"), 85);
        assert_eq!(score_match("腾讯微信", "微信"), 70);
        assert!(
            score_match("无畏契约", "无谓契约") >= 30,
            "同音错字必须能兜住"
        );
        assert_eq!(score_match("微信", "记事本"), 0);
        assert_eq!(score_match("", "x"), 0);
    }

    #[test]
    fn english_alias_matches_chinese_entry() {
        assert_eq!(score_match("VALORANT", "valorant"), 100);
        // 索引里中文名是"无畏契约"、exe 是 VALORANT：别名命中就够。
        let index = AppIndex {
            entries: vec![entry("无畏契约", "VALORANT"), entry("记事本", "notepad")],
            built_at: Some(Instant::now()),
        };
        let found = index.find("valorant", 5);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.name, "无畏契约");
    }

    #[test]
    fn find_returns_candidates_sorted_and_capped() {
        let index = AppIndex {
            entries: vec![
                entry("微信", "WeChat"),
                entry("微信开发者工具", "wechatdevtools"),
                entry("微信输入法", ""),
                entry("记事本", "notepad"),
            ],
            built_at: Some(Instant::now()),
        };
        let found = index.find("微信", 2);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].1.name, "微信", "精确匹配必须排第一");
    }

    #[tokio::test]
    async fn missing_query_is_an_argument_error() {
        let mut index = AppIndex::new();
        let cancel = CancelToken::new();
        let error = run(&mut index, &json!({}), &cancel).await.unwrap_err();
        assert!(error.is_argument());
    }

    #[test]
    fn index_status_is_explicit_before_first_build() {
        let index = AppIndex::new();
        assert!(!index.is_ready());
        assert!(describe_index(&index).contains("尚未建立"));
    }
}
