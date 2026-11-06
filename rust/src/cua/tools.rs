//! Computer Use 工具表（复用 `cua.ps1` 的命令协议）。
//!
//! 只接通 `CuaToolkit` 默认的 **12 个**工具，不引入上游全部 25 个动作：
//! 工具越多，模型在短指令上的选择错误率越高，且每个 schema 都常驻上下文。
//!
//! 保留的语义（不能丢）：
//!  - `state_id` 与 `element_index` 必须成对出现；
//!  - `frame_id` 与截图栅格坐标：坐标是**返回图像内的像素**，不是物理屏幕像素；
//!  - 截图按真实 media type（`image/png` / `image/jpeg`）交给多模态模型；
//!  - 参数错误一律在执行前拦下（`.invalid arguments: …`），绝不拿半截参数动作。
//!
//! PR12：工具定义与参数宽化挪到 [`crate::tools::spec`]、错误类型挪到
//! [`crate::tools::error`]，CUA 与本地工具共用同一套形态。本文件重新导出它们，
//! 所以既有调用点（`cua::ToolError` 等）不需要改动。

use std::path::PathBuf;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::cancel::CancelToken;
use crate::cua::bridge::{BridgeError, PwshBridge};
use crate::tools::spec::{
    as_args, coerce_bool, coerce_int, coerce_number, coerce_string, merge_objects, object_schema,
};

pub use crate::tools::error::{ArgError, ToolError};
pub use crate::tools::spec::ToolSpec;

/// 语音场景默认工具白名单（与 `packages/cua/adapter.mjs` 的 `DEFAULT_TOOL_NAMES` 一致）。
pub const DEFAULT_TOOL_NAMES: &[&str] = &[
    "request_access",
    "list_windows",
    "get_app_state",
    "screenshot",
    "open_application",
    "left_click",
    "type",
    "key",
    "scroll",
    "set_value",
    "wait",
    "read_clipboard",
];

/// 截图默认参数：实测 PNG 全屏要 1379ms，语音交互必须用 JPEG 小栅格。
const SCREENSHOT_DEFAULTS: (i64, &str, i64) = (1024, "jpeg", 70);
const SCREENSHOT_BUDGET_BYTES: usize = 240 * 1024;

/// 一次工具执行的结果（模型面文本 + 可选图像）。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 给模型读的文本信封。
    pub text: String,
    /// 截图工具带回的图（**真实** media type）。
    pub image: Option<ImageAttachment>,
    /// 界面用的结构化结果（例如 state_id）。
    pub data: Value,
}

/// 随工具结果一起交给多模态模型的图像。
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub media_type: String,
    pub base64: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

// 工具定义与参数宽化在 `crate::tools::spec`（CUA 与本地工具共用），
// 本模块重新导出它们，所以既有调用点（`cua::ToolSpec` 等）不需要改动。

fn app_selector() -> Value {
    json!({
        "description": "Target application: an appKey/displayName/process name from list_apps, a pid, or an object {appKey|name|process|pid|hwnd}.",
        "oneOf": [
            { "type": "string" },
            { "type": "integer" },
            {
                "type": "object",
                "properties": {
                    "appKey": { "type": "string", "description": "Application key from list_apps." },
                    "name": { "type": "string", "description": "Display or process name." },
                    "process": { "type": "string", "description": "Process name." },
                    "pid": { "type": "integer", "description": "Process id." },
                    "hwnd": { "type": "string", "description": "Window handle, e.g. 0x00120abc." }
                },
                "additionalProperties": true
            }
        ]
    })
}

fn window_selector() -> Value {
    json!({
        "description": "Target window: an hwnd from list_windows, or an object {hwnd|title|index}.",
        "oneOf": [
            { "type": "string" },
            {
                "type": "object",
                "properties": {
                    "hwnd": { "type": "string", "description": "Window handle." },
                    "title": { "type": "string", "description": "Window title substring." },
                    "index": { "type": "integer", "description": "Zero-based index within the app windows from list_windows." }
                },
                "additionalProperties": true
            }
        ]
    })
}

fn element_props() -> Value {
    json!({
        "element_index": { "type": "integer", "description": "Element index from get_app_state (elements[*].index)." },
        "state_id": { "type": "string", "description": "state_id returned by the get_app_state that produced element_index. Required with element_index." }
    })
}

fn coord_props() -> Value {
    json!({
        "x": { "type": "integer", "description": "X in raster pixels of the most recent screenshot." },
        "y": { "type": "integer", "description": "Y in raster pixels of the most recent screenshot." },
        "frame_id": { "type": "string", "description": "Frame id from the screenshot the coordinates were measured on. Omit to use the latest frame." },
        "space": {
            "type": "string",
            "enum": ["raster", "screen"],
            "description": "raster (default) = pixels inside the returned screenshot; screen = absolute physical pixels."
        }
    })
}

/// 构造 12 个工具的定义。
pub fn tool_specs() -> Vec<ToolSpec> {
    let mut specs = Vec::new();

    specs.push(ToolSpec {
        name: "request_access",
        description: "Check whether the desktop backend is ready and report the environment it sees (platform, screen size, DPI awareness, monitor count). Read-only; safe to call first.",
        parameters: object_schema(json!({}), &[]),
        read_only: true,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "list_windows",
        description: "List top-level windows, optionally narrowed to one application. Returns hwnd, title, class and bounds in physical screen pixels.",
        parameters: object_schema(json!({ "app": app_selector() }), &[]),
        read_only: true,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "get_app_state",
        description: "Read the accessibility (UI Automation) tree of one application window: every element with index, role, name, value, bounds, enabled/focused flags and supported actions. This is the primary observation channel — read it before element-targeted actions, then pass element_index with the returned state_id.",
        parameters: object_schema(
            merge_objects(&[
                json!({ "app": app_selector() }),
                json!({ "window": window_selector() }),
                json!({
                    "max_elements": { "type": "integer", "description": "Cap on returned elements (default 120, max 400)." },
                    "max_depth": { "type": "integer", "description": "Cap on tree depth (default 8, max 16)." }
                }),
            ]),
            &[],
        ),
        read_only: true,
        long_timeout: true,
    });

    specs.push(ToolSpec {
        name: "screenshot",
        description: "Capture the screen (or one application window) as an image. Returns the image itself plus a text envelope with the raster size. Coordinates you pass to left_click are pixels inside this returned image.",
        parameters: object_schema(
            merge_objects(&[
                json!({ "app": app_selector() }),
                json!({ "window": window_selector() }),
                json!({
                    "monitor": { "type": "integer", "description": "Monitor index from the environment report; omit for the whole virtual screen." },
                    "max_width": { "type": "integer", "description": "Downscale so the long edge does not exceed this many pixels (0 = no downscale)." },
                    "format": { "type": "string", "enum": ["png", "jpeg"], "description": "Image encoding. Defaults to jpeg." }
                }),
            ]),
            &[],
        ),
        read_only: true,
        long_timeout: true,
    });

    specs.push(ToolSpec {
        name: "open_application",
        description: "Launch an application or open a URL / file path via the shell. Accepts a string, or {url|path|name, args}. Prefer this over trying to click a launcher.",
        parameters: object_schema(
            json!({
                "app": {
                    "description": "What to open: a string, or {url|path|name|args}.",
                    "oneOf": [
                        { "type": "string" },
                        {
                            "type": "object",
                            "properties": {
                                "url": { "type": "string", "description": "URL to open in the default handler." },
                                "path": { "type": "string", "description": "Executable or document path." },
                                "name": { "type": "string", "description": "Application name resolved through the shell (e.g. notepad)." },
                                "args": { "type": "array", "items": { "type": "string" }, "description": "Command-line arguments." }
                            },
                            "additionalProperties": false
                        }
                    ]
                },
                "wait_ms": { "type": "integer", "description": "How long to wait for the new window (default 800)." }
            }),
            &["app"],
        ),
        read_only: false,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "left_click",
        description: "Click with the left mouse button. Target either an element (element_index + state_id from get_app_state — preferred, it survives layout shifts) or raster coordinates x/y from the most recent screenshot. Coordinates are pixels inside that screenshot image, not physical screen pixels.",
        parameters: object_schema(
            merge_objects(&[
                element_props(),
                coord_props(),
                json!({
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button. Defaults to left." },
                    "clicks": { "type": "integer", "description": "Click count (1-3). Defaults to 1." },
                    "interval_ms": { "type": "integer", "description": "Delay between clicks in ms (default 60)." }
                }),
            ]),
            &[],
        ),
        read_only: false,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "type",
        description: "Type text into whatever has keyboard focus, Unicode included. Pass an element target to click it first. Prefer this over per-character key presses.",
        parameters: object_schema(
            merge_objects(&[
                json!({ "text": { "type": "string", "description": "Text to type." } }),
                element_props(),
                json!({ "interval_ms": { "type": "integer", "description": "Delay between characters in ms (default 0)." } }),
            ]),
            &["text"],
        ),
        read_only: false,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "key",
        description: "Press a key or a modifier chord, e.g. \"ctrl+s\", \"alt+tab\", \"enter\", \"f5\", \"ctrl+shift+left\". Modifiers accepted: ctrl, alt, shift, win (aliases: control, meta, cmd, command).",
        parameters: object_schema(
            merge_objects(&[
                json!({
                    "key": { "type": "string", "description": "Chord like \"ctrl+s\"." },
                    "text": { "type": "string", "description": "Single character alternative to key." }
                }),
                element_props(),
            ]),
            &[],
        ),
        read_only: false,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "scroll",
        description: "Scroll the wheel. Give a direction and optionally a point (raster coordinates or element target) to scroll over; without a point the wheel goes to the current pointer position.",
        parameters: object_schema(
            merge_objects(&[
                json!({
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "Wheel direction (default down)." },
                    "amount": { "type": "integer", "description": "Wheel notches 1-20 (default 3)." }
                }),
                element_props(),
                coord_props(),
            ]),
            &[],
        ),
        read_only: false,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "set_value",
        description: "Set the text of an editable element through UI Automation (fills the field without simulating keystrokes). If the element does not support it, fall back to click + type.",
        parameters: object_schema(
            merge_objects(&[
                json!({
                    "value": { "type": "string", "description": "Value to set." },
                    "submit": { "type": "boolean", "description": "Press Enter after setting (default false)." }
                }),
                element_props(),
            ]),
            &["value", "element_index", "state_id"],
        ),
        read_only: false,
        long_timeout: false,
    });

    specs.push(ToolSpec {
        name: "wait",
        description: "Wait, then optionally re-read one application state. Use after an action that needs time (launching, loading, animations) instead of screenshotting in a tight loop.",
        parameters: object_schema(
            merge_objects(&[
                json!({
                    "duration": { "type": "number", "description": "Wait duration in seconds (default 1)." },
                    "duration_ms": { "type": "integer", "description": "Wait duration in ms (overrides duration)." }
                }),
                json!({ "app": app_selector() }),
                json!({ "window": window_selector() }),
            ]),
            &[],
        ),
        read_only: false,
        long_timeout: true,
    });

    specs.push(ToolSpec {
        name: "read_clipboard",
        description:
            "Read the clipboard as text. Returns hasText=false when the clipboard holds no text.",
        parameters: object_schema(json!({}), &[]),
        read_only: true,
        long_timeout: false,
    });

    specs
}

/// 供请求体使用的工具对象数组。
pub fn tools_for_request(names: &[&str]) -> Vec<Value> {
    let specs = tool_specs();
    names
        .iter()
        .filter_map(|name| specs.iter().find(|spec| spec.name == *name))
        .map(ToolSpec::to_request_value)
        .collect()
}

// 参数错误与参数宽化都在 `crate::tools::{error,spec}` 里（CUA 与本地工具共用）。
// 本模块只保留 CUA 特有的 selector → 协议参数转换。

/// app 选择器 → 协议参数。
fn app_selector_to_args(selector: Option<&Value>) -> Result<Value, ArgError> {
    let Some(selector) = selector else {
        return Ok(json!({}));
    };
    if selector.is_null() {
        return Ok(json!({}));
    }
    match selector {
        Value::Number(number) => {
            let pid = number.as_f64().unwrap_or(0.0).round() as i64;
            Ok(json!({ "pid": pid }))
        }
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(json!({}));
            }
            let lower = trimmed.to_ascii_lowercase();
            if lower.starts_with("0x") && lower[2..].chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Ok(json!({ "hwnd": trimmed }));
            }
            if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
                let pid: i64 = trimmed.parse().unwrap_or(0);
                return Ok(json!({ "pid": pid }));
            }
            Ok(json!({ "appKey": trimmed }))
        }
        Value::Object(object) => {
            let mut out = Map::new();
            let key = ["appKey", "name", "process", "displayName"]
                .iter()
                .find_map(|name| object.get(*name).and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if let Some(key) = key {
                out.insert("appKey".to_string(), json!(key));
            }
            if let Some(pid) = object.get("pid").and_then(Value::as_f64) {
                out.insert("pid".to_string(), json!(pid.round() as i64));
            }
            if let Some(hwnd) = object
                .get("hwnd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                out.insert("hwnd".to_string(), json!(hwnd));
            }
            Ok(Value::Object(out))
        }
        _ => Err(ArgError::new(
            "app must be a string, an integer pid, or an object",
        )),
    }
}

/// window 选择器 → 协议参数。
fn window_selector_to_args(selector: Option<&Value>) -> Result<Value, ArgError> {
    let Some(selector) = selector else {
        return Ok(json!({}));
    };
    if selector.is_null() {
        return Ok(json!({}));
    }
    match selector {
        Value::String(text) => Ok(json!({ "hwnd": text.trim() })),
        Value::Number(number) => {
            Ok(json!({ "window_index": number.as_f64().unwrap_or(0.0).round() as i64 }))
        }
        Value::Object(object) => {
            let mut out = Map::new();
            if let Some(hwnd) = object
                .get("hwnd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                out.insert("hwnd".to_string(), json!(hwnd));
            }
            if let Some(title) = object
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                out.insert("title".to_string(), json!(title));
            }
            if let Some(index) = object.get("index").and_then(Value::as_f64) {
                out.insert("window_index".to_string(), json!(index.round() as i64));
            }
            Ok(Value::Object(out))
        }
        _ => Err(ArgError::new("window must be a string hwnd or an object")),
    }
}

/// 取元素目标。`required` 为真时缺 `element_index` 直接报错。
fn require_element(
    args: &Map<String, Value>,
    tool: &str,
    required: bool,
) -> Result<Option<Value>, ArgError> {
    let Some(index) = coerce_int(args, "element_index") else {
        if required {
            return Err(ArgError::new(format!(
                "{tool} needs element_index from get_app_state"
            )));
        }
        return Ok(None);
    };
    let state_id = coerce_string(args, "state_id")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(state_id) = state_id else {
        return Err(ArgError::new(format!(
            "{tool} needs state_id together with element_index (call get_app_state first)"
        )));
    };
    Ok(Some(
        json!({ "element_index": index, "state_id": state_id }),
    ))
}

fn duration_to_ms(args: &Map<String, Value>, default_ms: i64, max_ms: i64) -> i64 {
    if let Some(ms) = coerce_number(args, "duration_ms") {
        return (ms.round() as i64).clamp(1, max_ms);
    }
    if let Some(seconds) = coerce_number(args, "duration") {
        return ((seconds * 1000.0).round() as i64).clamp(1, max_ms);
    }
    default_ms
}

fn quote(value: &str) -> String {
    value.replace('"', "&quot;").replace(['\r', '\n'], " ")
}

fn attr(name: &str, value: Option<&str>) -> String {
    match value {
        Some(text) if !text.is_empty() => format!(" {name}=\"{}\"", quote(text)),
        _ => String::new(),
    }
}

fn bounds_text(bounds: Option<&Value>) -> String {
    let Some(bounds) = bounds else {
        return String::new();
    };
    let get = |key: &str| bounds.get(key).and_then(Value::as_i64).unwrap_or(0);
    format!(
        " @({},{},{},{})",
        get("x"),
        get("y"),
        get("width"),
        get("height")
    )
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn format_list_windows(data: &Value) -> String {
    let windows = data
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if windows.is_empty() {
        return "<windows count=\"0\">\nNo matching windows.\n</windows>".to_string();
    }
    let mut lines = vec![format!("<windows count=\"{}\">", windows.len())];
    for (index, window) in windows.iter().enumerate() {
        let display = window
            .get("displayName")
            .and_then(Value::as_str)
            .or_else(|| window.get("appKey").and_then(Value::as_str))
            .unwrap_or("");
        let foreground = window.get("foreground").and_then(Value::as_bool) == Some(true);
        let minimized = window.get("minimized").and_then(Value::as_bool) == Some(true);
        lines.push(format!(
            "[{index}] hwnd=\"{}\" title=\"{}\" app=\"{}\" pid=\"{}\"{}{}{}{}",
            quote(str_of(window, "hwnd")),
            quote(str_of(window, "title")),
            quote(display),
            window.get("pid").and_then(Value::as_i64).unwrap_or(0),
            attr("class", window.get("className").and_then(Value::as_str)),
            attr("foreground", foreground.then_some("true")),
            attr("minimized", minimized.then_some("true")),
            bounds_text(window.get("bounds")),
        ));
    }
    lines.push("</windows>".to_string());
    lines.join("\n")
}

fn format_app_state(data: &Value, changes: Option<&Value>) -> String {
    let elements = data
        .get("elements")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let app = data.get("app").cloned().unwrap_or(Value::Null);
    let window = data.get("window").cloned().unwrap_or(Value::Null);
    let state_id = str_of(data, "state_id");
    let display = app
        .get("displayName")
        .and_then(Value::as_str)
        .or_else(|| app.get("appKey").and_then(Value::as_str))
        .unwrap_or("");

    let mut lines = vec![format!(
        "<app name=\"{}\" key=\"{}\" pid=\"{}\"{} state_id=\"{}\" elements=\"{}\"{}>",
        quote(display),
        quote(str_of(&app, "appKey")),
        app.get("pid").and_then(Value::as_i64).unwrap_or(0),
        attr("window", window.get("title").and_then(Value::as_str)),
        quote(state_id),
        elements.len(),
        attr(
            "truncated",
            (data.get("truncated").and_then(Value::as_bool) == Some(true)).then_some("true")
        ),
    )];
    if let Some(changes) = changes {
        lines.push(format!(
            "changes: {} added, {} removed since the previous read",
            changes
                .get("added_count")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            changes
                .get("removed_count")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ));
    }
    for element in &elements {
        let mut parts = vec![format!(
            "[{}] {} \"{}\"",
            element.get("index").and_then(Value::as_i64).unwrap_or(0),
            quote(str_of(element, "role")),
            quote(str_of(element, "name")),
        )];
        if let Some(value) = element.get("value") {
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            if !text.is_empty() {
                let clipped: String = text.chars().take(120).collect();
                parts.push(format!("value=\"{}\"", quote(&clipped)));
            }
        }
        let bounds = bounds_text(element.get("bounds"));
        if !bounds.is_empty() {
            parts.push(bounds.trim().to_string());
        }
        let mut flags = Vec::new();
        if element.get("focused").and_then(Value::as_bool) == Some(true) {
            flags.push("focused".to_string());
        }
        if element.get("enabled").and_then(Value::as_bool) == Some(false) {
            flags.push("disabled".to_string());
        }
        if element.get("offscreen").and_then(Value::as_bool) == Some(true) {
            flags.push("offscreen".to_string());
        }
        if let Some(actions) = element.get("actions").and_then(Value::as_array) {
            if !actions.is_empty() {
                let joined = actions
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("|");
                flags.push(format!("actions={joined}"));
            }
        }
        if !flags.is_empty() {
            parts.push(flags.join(" "));
        }
        lines.push(parts.join(" "));
    }
    if elements.is_empty() {
        lines.push("(no interface elements were exposed for this window)".to_string());
    }
    lines.push("</app>".to_string());
    lines.push(format!(
        "Element targets: pass element_index with state_id=\"{}\". A different state_id is stale — call get_app_state again before targeting elements.",
        quote(state_id)
    ));
    lines.join("\n")
}

fn format_screenshot(data: &Value) -> String {
    let image = data.get("image").cloned().unwrap_or(Value::Null);
    let width = image.get("width").and_then(Value::as_i64).unwrap_or(0);
    let height = image.get("height").and_then(Value::as_i64).unwrap_or(0);
    let source = data.get("source").cloned().unwrap_or(Value::Null);
    let scale = data
        .get("scale")
        .and_then(|value| value.get("x"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let mut lines = vec![format!(
        "<screenshot kind=\"full-screen\" frame_id=\"{}\" width=\"{}\" height=\"{}\"{}>",
        quote(str_of(data, "frame_id")),
        width,
        height,
        attr("format", image.get("mediaType").and_then(Value::as_str)),
    )];
    lines.push(format!(
        "source_region=\"{},{},{},{}\" (physical pixels)",
        source.get("x").and_then(Value::as_i64).unwrap_or(0),
        source.get("y").and_then(Value::as_i64).unwrap_or(0),
        source.get("width").and_then(Value::as_i64).unwrap_or(0),
        source.get("height").and_then(Value::as_i64).unwrap_or(0),
    ));
    lines.push(format!(
        "raster=\"{width}x{height}\"  scale=\"{scale:.3}\"{}",
        attr(
            "clamped",
            (data.get("clamped").and_then(Value::as_bool) == Some(true)).then_some("true")
        )
    ));
    lines.push("</screenshot>".to_string());
    lines.push(format!(
        "Coordinate rule: x/y in left_click are **raster pixels inside this image** (0..{} by 0..{}). \
         Do not use window or element bounds as raster coordinates.",
        width.saturating_sub(1).max(0),
        height.saturating_sub(1).max(0)
    ));
    lines.join("\n")
}

fn format_action(data: &Value, fallback: &str) -> String {
    let sent = data.get("action_sent").and_then(Value::as_bool) == Some(true);
    let mut lines = vec![format!(
        "<action status=\"{}\">",
        if sent { "sent" } else { "unknown" }
    )];
    lines.push(
        data.get("text")
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_string(),
    );
    if let Some(target) = data.get("target") {
        if let (Some(x), Some(y)) = (
            target.get("x").and_then(Value::as_i64),
            target.get("y").and_then(Value::as_i64),
        ) {
            lines.push(format!("target_raster=({x},{y})"));
        }
        if let Some(index) = target.get("elementIndex").and_then(Value::as_i64) {
            lines.push(format!("target_element=[{index}]"));
        }
    }
    if let Some(state_id) = data.get("state_id").and_then(Value::as_str) {
        if !state_id.is_empty() {
            lines.push(format!("state_id=\"{}\"", quote(state_id)));
        }
    }
    if let Some(characters) = data.get("characters").and_then(Value::as_i64) {
        lines.push(format!("characters={characters}"));
    }
    lines.push("</action>".to_string());
    lines.join("\n")
}

fn format_clipboard_read(data: &Value) -> String {
    if data.get("hasText").and_then(Value::as_bool) != Some(true) {
        return "<clipboard empty=\"true\">\nNo text on the clipboard.\n</clipboard>".to_string();
    }
    let text = data.get("text").and_then(Value::as_str).unwrap_or("");
    format!(
        "<clipboard characters=\"{}\">\n{text}\n</clipboard>",
        text.chars().count()
    )
}

/// 后端错误码 → 模型能照着修的说明。
pub fn action_error_hint(code: &str) -> &'static str {
    match code {
        "ELEMENT_STALE" => "The interface changed, or the state you measured on is no longer the latest. Call get_app_state again and use the new state_id.",
        "ELEMENT_NOT_FOUND" => "That element index is not in the current state. Call get_app_state again.",
        "ELEMENT_UNAVAILABLE" => "The element is gone, hidden, or has no usable geometry. Call get_app_state again, or fall back to screenshot + x/y raster coordinates.",
        "ACTION_UNAVAILABLE" => "That UI Automation action is not supported by this element. Try click with the element center, or use keyboard navigation.",
        "NOT_SELECTABLE" => "This element does not support text selection. Click it, then use key with ctrl+a instead.",
        "NOT_SETTABLE" => "This element cannot be filled programmatically. Click it, then use type.",
        "FOREGROUND_REQUIRED" => "Windows refused to bring the target window to the foreground. Call open_application to raise it, click one of its elements (element_index + state_id), or ask the user to focus it.",
        "APP_NOT_FOUND" => "No such application. Use list_windows to get a valid app target.",
        "WINDOW_NOT_FOUND" => "No such window. Use list_windows to get a valid window target.",
        "NO_FRAME" => "No screenshot has been taken yet, so raster coordinates have no origin. Call screenshot first, or pass space=\"screen\" with physical screen coordinates.",
        "TIMEOUT" => "The desktop backend did not answer in time and was restarted. Retry the call; if it repeats, the target app may be hung.",
        _ => "Fix the arguments and retry, or take a screenshot to re-observe the screen.",
    }
}

/// 工具执行器。
pub struct CuaToolkit {
    bridge: PwshBridge,
    /// 截图落盘目录（不参与模型输入，仅供排查）。
    frame_dir: Option<PathBuf>,
    /// 上一批 `get_app_state` 的元素签名，用于计算 `changes`。
    /// 用 `Mutex` 而不是 `&mut self`：执行器通过 `&self` 共享（Agent 只持有 `Arc<dyn ToolInvoker>`）。
    state_signatures:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>,
}

impl CuaToolkit {
    pub fn new(bridge: PwshBridge, frame_dir: Option<PathBuf>) -> Self {
        Self {
            bridge,
            frame_dir,
            state_signatures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn bridge(&self) -> &PwshBridge {
        &self.bridge
    }

    /// 预热后端（启动 PowerShell 并等就绪）。
    pub async fn warmup(&self, cancel: &CancelToken) -> Result<(), BridgeError> {
        self.bridge.probe(cancel).await.map(|_| ())
    }

    async fn call(
        &self,
        cmd: &str,
        args: Value,
        long: bool,
        cancel: &CancelToken,
    ) -> Result<Value, BridgeError> {
        let timeout = if long { Some(60_000) } else { None };
        self.bridge.call(cmd, args, timeout, cancel).await
    }

    /// 执行一次工具调用。
    pub async fn invoke(
        &self,
        name: &str,
        raw_args: &Value,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let args = as_args(raw_args);
        let value = match name {
            "request_access" => self.request_access(cancel).await?,
            "list_windows" => self.list_windows(&args, cancel).await?,
            "get_app_state" => self.get_app_state(&args, cancel).await?,
            "screenshot" => self.screenshot(&args, cancel).await?,
            "open_application" => self.open_application(&args, cancel).await?,
            "left_click" => self.click(&args, cancel).await?,
            "type" => self.type_text(&args, cancel).await?,
            "key" => self.press_key(&args, cancel).await?,
            "scroll" => self.scroll(&args, cancel).await?,
            "set_value" => self.set_value(&args, cancel).await?,
            "wait" => self.wait(&args, cancel).await?,
            "read_clipboard" => self.read_clipboard(cancel).await?,
            other => {
                return Err(ToolError::Arg(ArgError::new(format!("未知工具：{other}"))));
            }
        };
        Ok(value)
    }

    async fn request_access(&self, cancel: &CancelToken) -> Result<ToolOutput, ToolError> {
        let probe = self.bridge.probe(cancel).await;
        match probe {
            Ok(info) => {
                let screen = self
                    .call("screen_info", json!({}), false, cancel)
                    .await
                    .unwrap_or(Value::Null);
                let monitors = screen
                    .get("monitors")
                    .and_then(Value::as_array)
                    .map(|items| items.len())
                    .unwrap_or(0);
                let text = format!(
                    "<computer-use environment=\"windows\">\nbackend=\"powershell-native/{}\"\nready=\"true\"\n{}{}monitors=\"{}\"\nvirtual_screen=\"{}\"\nscreen_capture=\"granted\"\ninput_control=\"granted\"\n</computer-use>\n- Windows desktop backend (PowerShell + UI Automation). No macOS-style permission grant is required on Windows.\n- Coordinates for left_click are raster pixels of the most recent screenshot, unless space=\"screen\".",
                    quote(&info.protocol),
                    if info.dpi_aware { "dpi_aware=\"true\"\n" } else { "" },
                    "",
                    monitors,
                    screen
                        .get("virtual")
                        .map(|virtual_screen| format!(
                            "{}x{}",
                            virtual_screen.get("width").and_then(Value::as_i64).unwrap_or(0),
                            virtual_screen.get("height").and_then(Value::as_i64).unwrap_or(0)
                        ))
                        .unwrap_or_else(|| "unknown".to_string()),
                );
                Ok(ToolOutput {
                    text,
                    image: None,
                    data: json!({ "ready": true, "protocol": info.protocol, "monitors": monitors }),
                })
            }
            Err(error) => {
                let text = format!(
                    "<computer-use environment=\"windows\">\nbackend=\"unavailable\"\nready=\"false\"\n</computer-use>\n- Backend is not available: {}\n- Nothing can be observed or controlled until this is fixed.",
                    error.user_message()
                );
                Ok(ToolOutput {
                    text,
                    image: None,
                    data: json!({ "ready": false, "reason": error.user_message() }),
                })
            }
        }
    }

    async fn list_windows(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let payload = app_selector_to_args(args.get("app"))?;
        let data = self.call("list_windows", payload, false, cancel).await?;
        Ok(ToolOutput {
            text: format_list_windows(&data),
            image: None,
            data,
        })
    }

    async fn get_app_state(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let mut payload = app_selector_to_args(args.get("app"))?;
        let window = window_selector_to_args(args.get("window"))?;
        if let Some(source) = window.as_object() {
            if let Some(target) = payload.as_object_mut() {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        if let Some(max_elements) = coerce_int(args, "max_elements") {
            payload["max_elements"] = json!(max_elements);
        }
        if let Some(max_depth) = coerce_int(args, "max_depth") {
            payload["max_depth"] = json!(max_depth);
        }
        let data = self.call("get_app_state", payload, true, cancel).await?;
        let changes = self.compute_changes(&data);
        let text = format_app_state(&data, changes.as_ref());
        Ok(ToolOutput {
            text,
            image: None,
            data,
        })
    }

    /// 以「角色|名字|值|AutomationId」为元素身份，与同一窗口上一次观察做集合差。
    fn compute_changes(&self, data: &Value) -> Option<Value> {
        let key = format!(
            "{}:{}",
            data.get("app")
                .and_then(|app| app.get("appKey"))
                .and_then(Value::as_str)
                .unwrap_or("?"),
            data.get("window")
                .and_then(|window| window.get("hwnd"))
                .and_then(Value::as_str)
                .unwrap_or("?"),
        );
        let elements = data.get("elements").and_then(Value::as_array)?;
        let current: std::collections::HashSet<String> = elements
            .iter()
            .map(|element| {
                format!(
                    "{}|{}|{}|{}",
                    str_of(element, "role"),
                    str_of(element, "name"),
                    element
                        .get("value")
                        .map(|value| match value {
                            Value::String(text) => text.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default(),
                    str_of(element, "automationId"),
                )
            })
            .collect();
        let previous = {
            let mut signatures = self.state_signatures.lock().ok()?;
            let previous = signatures.insert(key, current.clone());
            // 最多记 32 个窗口的历史，避免长时间运行无限增长。
            if signatures.len() > 32 {
                if let Some(oldest) = signatures.keys().next().cloned() {
                    signatures.remove(&oldest);
                }
            }
            previous
        };
        let previous = previous?;
        let added = current.difference(&previous).count();
        let removed = previous.difference(&current).count();
        Some(json!({ "added_count": added, "removed_count": removed }))
    }

    async fn screenshot(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let (default_width, default_format, default_quality) = SCREENSHOT_DEFAULTS;
        let mut payload = Map::new();
        payload.insert("max_width".to_string(), json!(default_width));
        payload.insert("format".to_string(), json!(default_format));
        payload.insert("quality".to_string(), json!(default_quality));

        if let Some(monitor) = coerce_int(args, "monitor") {
            payload.insert("monitor".to_string(), json!(monitor));
        }
        if let Some(max_width) = coerce_int(args, "max_width") {
            payload.insert("max_width".to_string(), json!(max_width));
        }
        if let Some(format) = coerce_string(args, "format") {
            payload.insert("format".to_string(), json!(format));
        }
        // 指定 app/window 时截该窗口区域；拿不到窗口就退回整屏（比直接失败更有用）。
        if let Some(region) = self.resolve_window_region(args, cancel).await {
            if let Some(source) = region.as_object() {
                for (key, value) in source {
                    payload.insert(key.clone(), value.clone());
                }
            }
        }

        let mut data = self
            .call("screenshot", Value::Object(payload.clone()), true, cancel)
            .await?;
        // 超出字节预算时按阶梯降级重拍（png→jpeg→更小栅格），坐标语义随新图更新。
        let bytes = data
            .get("image")
            .and_then(|image| image.get("bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if bytes > SCREENSHOT_BUDGET_BYTES
            && payload.get("format").and_then(Value::as_str) != Some("jpeg")
        {
            {
                payload.insert("format".to_string(), json!("jpeg"));
                payload.insert("quality".to_string(), json!(default_quality));
                if let Ok(next) = self
                    .call("screenshot", Value::Object(payload.clone()), true, cancel)
                    .await
                {
                    let next_bytes = next
                        .get("image")
                        .and_then(|image| image.get("bytes"))
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX) as usize;
                    if next_bytes < bytes {
                        data = next;
                    }
                }
            }
        }

        let image = data.get("image").cloned().unwrap_or(Value::Null);
        let media_type = image
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or("image/jpeg")
            .to_string();
        let base64 = image
            .get("base64")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let width = image.get("width").and_then(Value::as_i64);
        let height = image.get("height").and_then(Value::as_i64);

        // 落盘一份便于排查（不带进模型输入）。
        if let (Some(dir), false) = (self.frame_dir.as_ref(), base64.is_empty()) {
            let extension = if media_type == "image/png" {
                "png"
            } else {
                "jpg"
            };
            let frame_id = str_of(&data, "frame_id").replace(['/', '\\', ':'], "_");
            let _ = std::fs::create_dir_all(dir);
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&base64) {
                let _ = std::fs::write(dir.join(format!("{frame_id}.{extension}")), raw);
            }
        }

        let text = format_screenshot(&data);
        Ok(ToolOutput {
            text,
            image: if base64.is_empty() {
                None
            } else {
                Some(ImageAttachment {
                    media_type,
                    base64,
                    width,
                    height,
                })
            },
            data,
        })
    }

    async fn resolve_window_region(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Option<Value> {
        if args.get("app").is_none() && args.get("window").is_none() {
            return None;
        }
        let mut payload = app_selector_to_args(args.get("app")).ok()?;
        match args.get("window") {
            Some(Value::String(text)) => {
                payload["hwnd"] = json!(text.trim());
            }
            Some(Value::Number(number)) => {
                payload["window_index"] = json!(number.as_f64().unwrap_or(0.0).round() as i64);
            }
            Some(Value::Object(object)) => {
                if let Some(hwnd) = object.get("hwnd").and_then(Value::as_str) {
                    payload["hwnd"] = json!(hwnd.trim());
                }
                if let Some(index) = object.get("index").and_then(Value::as_f64) {
                    payload["window_index"] = json!(index.round() as i64);
                }
                if let Some(title) = object.get("title").and_then(Value::as_str) {
                    payload["title"] = json!(title.trim());
                }
            }
            _ => {}
        }
        if payload
            .as_object()
            .map(|object| object.is_empty())
            .unwrap_or(true)
        {
            return None;
        }
        let listed = self
            .call("list_windows", payload, false, cancel)
            .await
            .ok()?;
        let first = listed.get("windows")?.as_array()?.first()?.clone();
        let bounds = first.get("bounds")?;
        let x = bounds.get("x").and_then(Value::as_i64)?;
        let y = bounds.get("y").and_then(Value::as_i64)?;
        let width = bounds.get("width").and_then(Value::as_i64)?;
        let height = bounds.get("height").and_then(Value::as_i64)?;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some(json!({ "left": x, "top": y, "width": width, "height": height, "clamp": true }))
    }

    async fn open_application(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let Some(selector) = args.get("app") else {
            return Err(ToolError::Arg(ArgError::new("app is required")));
        };
        let mut payload = Map::new();
        match selector {
            Value::String(text) => {
                let lower = text.to_ascii_lowercase();
                if lower.starts_with("http:")
                    || lower.starts_with("https:")
                    || lower.starts_with("file:")
                    || lower.starts_with("mailto:")
                {
                    payload.insert("url".to_string(), json!(text));
                } else {
                    payload.insert("name".to_string(), json!(text));
                }
            }
            Value::Object(object) => {
                if let Some(url) = object.get("url").and_then(Value::as_str) {
                    payload.insert("url".to_string(), json!(url));
                }
                if let Some(path) = object.get("path").and_then(Value::as_str) {
                    payload.insert("path".to_string(), json!(path));
                }
                if let Some(name) = object.get("name").and_then(Value::as_str) {
                    payload.insert("name".to_string(), json!(name));
                }
                if let Some(list) = object.get("args").and_then(Value::as_array) {
                    let items: Vec<String> = list
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    payload.insert("args".to_string(), json!(items));
                }
            }
            _ => {
                return Err(ToolError::Arg(ArgError::new(
                    "app must be a string or an object {url|path|name|args}",
                )));
            }
        }
        if !payload.contains_key("url")
            && !payload.contains_key("path")
            && !payload.contains_key("name")
        {
            return Err(ToolError::Arg(ArgError::new(
                "app must carry url, path, or name",
            )));
        }
        if let Some(wait_ms) = coerce_int(args, "wait_ms") {
            payload.insert("wait_ms".to_string(), json!(wait_ms));
        }
        let data = self
            .call("open_application", Value::Object(payload), false, cancel)
            .await?;
        Ok(ToolOutput {
            text: format_action(&data, "Application launch requested."),
            image: None,
            data,
        })
    }

    async fn click(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let mut payload = Map::new();
        payload.insert(
            "button".to_string(),
            json!(coerce_string(args, "button").unwrap_or_else(|| "left".to_string())),
        );
        payload.insert(
            "clicks".to_string(),
            json!(coerce_int(args, "clicks").unwrap_or(1)),
        );
        match require_element(args, "left_click", false)? {
            Some(element) => {
                if let Some(object) = element.as_object() {
                    for (key, value) in object {
                        payload.insert(key.clone(), value.clone());
                    }
                }
            }
            None => {
                let (Some(x), Some(y)) = (coerce_int(args, "x"), coerce_int(args, "y")) else {
                    return Err(ToolError::Arg(ArgError::new(
                        "left_click needs either element_index+state_id or both x and y",
                    )));
                };
                payload.insert("x".to_string(), json!(x));
                payload.insert("y".to_string(), json!(y));
                if let Some(frame_id) = coerce_string(args, "frame_id") {
                    payload.insert("frame_id".to_string(), json!(frame_id));
                }
                if let Some(space) = coerce_string(args, "space") {
                    payload.insert("space".to_string(), json!(space));
                }
            }
        }
        if let Some(interval) = coerce_int(args, "interval_ms") {
            payload.insert("interval_ms".to_string(), json!(interval));
        }
        let data = self
            .call("click", Value::Object(payload), false, cancel)
            .await?;
        Ok(ToolOutput {
            text: format_action(&data, "left_click dispatched."),
            image: None,
            data,
        })
    }

    async fn type_text(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let Some(text) = coerce_string(args, "text") else {
            return Err(ToolError::Arg(ArgError::new("text is required")));
        };
        let mut payload = Map::new();
        payload.insert("text".to_string(), json!(text));
        if let Some(element) = require_element(args, "type", false)? {
            if let Some(object) = element.as_object() {
                for (key, value) in object {
                    payload.insert(key.clone(), value.clone());
                }
            }
        }
        if let Some(interval) = coerce_int(args, "interval_ms") {
            payload.insert("interval_ms".to_string(), json!(interval));
        }
        let data = self
            .call("type", Value::Object(payload), false, cancel)
            .await?;
        Ok(ToolOutput {
            text: format_action(&data, "Typed."),
            image: None,
            data,
        })
    }

    async fn press_key(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let key = coerce_string(args, "key")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let text = coerce_string(args, "text").filter(|value| !value.is_empty());
        let payload = match (&key, &text) {
            (Some(key), _) => json!({ "key": key }),
            (None, Some(text)) => json!({ "text": text }),
            _ => {
                return Err(ToolError::Arg(ArgError::new(
                    "key needs either key (\"ctrl+s\") or text (a single character)",
                )));
            }
        };
        let mut payload = payload;
        if let Some(element) = require_element(args, "key", false)? {
            if let Some(source) = element.as_object() {
                if let Some(target) = payload.as_object_mut() {
                    for (name, value) in source {
                        target.insert(name.clone(), value.clone());
                    }
                }
            }
        }
        let data = self.call("key", payload, false, cancel).await?;
        Ok(ToolOutput {
            text: format_action(&data, "Key pressed."),
            image: None,
            data,
        })
    }

    async fn scroll(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let mut payload = Map::new();
        payload.insert(
            "direction".to_string(),
            json!(coerce_string(args, "direction").unwrap_or_else(|| "down".to_string())),
        );
        payload.insert(
            "amount".to_string(),
            json!(coerce_int(args, "amount").unwrap_or(3)),
        );
        if let Some(element) = require_element(args, "scroll", false)? {
            if let Some(object) = element.as_object() {
                for (key, value) in object {
                    payload.insert(key.clone(), value.clone());
                }
            }
        } else if let (Some(x), Some(y)) = (coerce_int(args, "x"), coerce_int(args, "y")) {
            payload.insert("x".to_string(), json!(x));
            payload.insert("y".to_string(), json!(y));
            if let Some(frame_id) = coerce_string(args, "frame_id") {
                payload.insert("frame_id".to_string(), json!(frame_id));
            }
            if let Some(space) = coerce_string(args, "space") {
                payload.insert("space".to_string(), json!(space));
            }
        }
        let data = self
            .call("scroll", Value::Object(payload), false, cancel)
            .await?;
        Ok(ToolOutput {
            text: format_action(&data, "Scroll dispatched."),
            image: None,
            data,
        })
    }

    async fn set_value(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let Some(value) = coerce_string(args, "value") else {
            return Err(ToolError::Arg(ArgError::new("value is required")));
        };
        let Some(element) = require_element(args, "set_value", true)? else {
            return Err(ToolError::Arg(ArgError::new(
                "set_value needs element_index from get_app_state",
            )));
        };
        let mut payload = element;
        payload["value"] = json!(value);
        payload["submit"] = json!(coerce_bool(args, "submit").unwrap_or(false));
        let data = self.call("set_value", payload, false, cancel).await?;
        Ok(ToolOutput {
            text: format_action(&data, "Value set."),
            image: None,
            data,
        })
    }

    async fn wait(
        &self,
        args: &Map<String, Value>,
        cancel: &CancelToken,
    ) -> Result<ToolOutput, ToolError> {
        let duration_ms = duration_to_ms(args, 1_000, 30_000);
        let mut payload = Map::new();
        payload.insert("duration_ms".to_string(), json!(duration_ms));
        let app = app_selector_to_args(args.get("app"))?;
        let window = window_selector_to_args(args.get("window"))?;
        if let Some(source) = app.as_object() {
            for (key, value) in source {
                payload.insert(key.clone(), value.clone());
            }
        }
        if let Some(source) = window.as_object() {
            for (key, value) in source {
                payload.insert(key.clone(), value.clone());
            }
        }
        let data = self
            .call("wait", Value::Object(payload), true, cancel)
            .await?;
        let waited = format!("Waited {:.2}s.", duration_ms as f64 / 1000.0);
        if data.get("state_id").is_some() {
            let text = format!(
                "{}\n\n{}",
                format_action(&data, &waited),
                format_app_state(&data, None)
            );
            Ok(ToolOutput {
                text,
                image: None,
                data,
            })
        } else {
            Ok(ToolOutput {
                text: format_action(&data, &waited),
                image: None,
                data,
            })
        }
    }

    async fn read_clipboard(&self, cancel: &CancelToken) -> Result<ToolOutput, ToolError> {
        let data = self
            .call("clipboard_read", json!({}), false, cancel)
            .await?;
        Ok(ToolOutput {
            text: format_clipboard_read(&data),
            image: None,
            data,
        })
    }
}

/// 工具执行错误见 [`crate::tools::error::ToolError`]（已在上方重新导出）。
///
/// 把 `Option<&Value>` 便捷地读成对象。
pub fn args_object(value: &Value) -> Map<String, Value> {
    as_args(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tool_names_match_legacy_whitelist() {
        assert_eq!(DEFAULT_TOOL_NAMES.len(), 12);
        for name in DEFAULT_TOOL_NAMES {
            assert!(
                tool_specs().iter().any(|spec| spec.name == *name),
                "白名单里的工具必须有定义：{name}"
            );
        }
    }

    #[test]
    fn request_tools_only_include_requested_names() {
        let tools = tools_for_request(&["screenshot", "list_windows", "not_a_tool"]);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["function"]["name"], json!("screenshot"));
    }

    #[test]
    fn element_requires_state_id() {
        let args = as_args(&json!({ "element_index": 3 }));
        let error = require_element(&args, "set_value", true).unwrap_err();
        assert!(error
            .to_string()
            .contains("needs state_id together with element_index"));
    }

    #[test]
    fn element_pair_is_accepted() {
        let args = as_args(&json!({ "element_index": "3", "state_id": " s1 " }));
        let element = require_element(&args, "set_value", true).unwrap().unwrap();
        assert_eq!(element["element_index"], json!(3));
        assert_eq!(element["state_id"], json!("s1"));
    }

    #[test]
    fn app_selector_forms_are_normalized() {
        assert_eq!(
            app_selector_to_args(Some(&json!("notepad"))).unwrap(),
            json!({"appKey":"notepad"})
        );
        assert_eq!(
            app_selector_to_args(Some(&json!(1234))).unwrap(),
            json!({"pid":1234})
        );
        assert_eq!(
            app_selector_to_args(Some(&json!("0x1a2b"))).unwrap(),
            json!({"hwnd":"0x1a2b"})
        );
        assert_eq!(
            app_selector_to_args(Some(&json!({"pid": 42, "hwnd": "0x1"}))).unwrap(),
            json!({"pid":42,"hwnd":"0x1"})
        );
        assert!(app_selector_to_args(Some(&json!([1, 2]))).is_err());
    }

    #[test]
    fn numeric_strings_are_coerced() {
        let args = as_args(&json!({ "x": "10", "y": " 20 ", "clicks": "2", "submit": "true" }));
        assert_eq!(coerce_int(&args, "x"), Some(10));
        assert_eq!(coerce_int(&args, "y"), Some(20));
        assert_eq!(coerce_int(&args, "clicks"), Some(2));
        assert_eq!(coerce_bool(&args, "submit"), Some(true));
    }

    #[test]
    fn duration_prefers_ms_over_seconds() {
        let args = as_args(&json!({ "duration": 2.0, "duration_ms": 250 }));
        assert_eq!(duration_to_ms(&args, 1000, 30000), 250);
        let args = as_args(&json!({ "duration": 2.0 }));
        assert_eq!(duration_to_ms(&args, 1000, 30000), 2000);
        let args = as_args(&json!({}));
        assert_eq!(duration_to_ms(&args, 1000, 30000), 1000);
    }

    #[test]
    fn app_state_envelope_carries_state_id_and_coordinate_rule() {
        let data = json!({
            "state_id": "s-42",
            "app": { "appKey": "notepad", "displayName": "记事本", "pid": 100 },
            "window": { "title": "无标题 - 记事本" },
            "elements": [
                { "index": 0, "role": "Edit", "name": "文本编辑器", "bounds": {"x":10,"y":20,"width":300,"height":200} }
            ]
        });
        let text = format_app_state(&data, None);
        assert!(text.contains("state_id=\"s-42\""));
        assert!(text.contains("[0] Edit \"文本编辑器\""));
        assert!(text.contains("pass element_index with state_id=\"s-42\""));
    }

    #[test]
    fn screenshot_envelope_states_raster_coordinate_rule() {
        let data = json!({
            "frame_id": "f-1",
            "image": { "mediaType": "image/jpeg", "width": 1024, "height": 768 },
            "source": { "x": 0, "y": 0, "width": 2560, "height": 1600 },
            "scale": { "x": 0.4, "y": 0.48 }
        });
        let text = format_screenshot(&data);
        assert!(text.contains("frame_id=\"f-1\""));
        assert!(text.contains("0..1023"));
        assert!(text.contains("raster pixels inside this image"));
    }

    #[test]
    fn empty_windows_envelope_is_explicit() {
        let text = format_list_windows(&json!({ "windows": [] }));
        assert!(text.contains("No matching windows"));
    }

    #[test]
    fn changes_are_computed_against_previous_state() {
        let toolkit = CuaToolkit::new(
            PwshBridge::new(crate::cua::bridge::BridgeOptions::new("nope.ps1")),
            None,
        );
        let first = json!({
            "state_id": "s1",
            "app": {"appKey": "a"},
            "window": {"hwnd": "0x1"},
            "elements": [{ "role": "Edit", "name": "n1" }]
        });
        assert!(
            toolkit.compute_changes(&first).is_none(),
            "首次观察没有 changes"
        );

        let second = json!({
            "state_id": "s2",
            "app": {"appKey": "a"},
            "window": {"hwnd": "0x1"},
            "elements": [{ "role": "Edit", "name": "n1" }, { "role": "Button", "name": "n2" }]
        });
        let changes = toolkit.compute_changes(&second).unwrap();
        assert_eq!(changes["added_count"], json!(1));
        assert_eq!(changes["removed_count"], json!(0));
    }

    #[test]
    fn error_hints_are_available_for_known_codes() {
        assert!(action_error_hint("ELEMENT_STALE").contains("get_app_state"));
        assert!(action_error_hint("UNKNOWN_CODE").contains("retry"));
    }
}
