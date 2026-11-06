//! 工具定义的**公共形态**：CUA 工具与本地工具用的是同一份结构与同一套参数宽化规则。
//!
//! 为什么把它从 `cua::tools` 挪出来：PR12 要让本地工具（run_shell / files / …）
//! 和 12 个 CUA 工具一起交给**同一个** Agent。如果各写一份 schema 构造函数，
//! 就会出现"两套 JSON Schema、两种参数校验"的分叉——这正是任务书明确禁止的。
//! 这里只放定义与宽化，执行仍然各在各自的模块里。
//!
//! 参数宽化的理由（沿用 PR5 的结论）：模型常把数字写成 `"10"`、把布尔写成 `"true"`，
//! 一次误报就要重跑一整轮，宽化收益远大于风险。

use serde_json::{json, Map, Value};

/// 工具定义（OpenAI function calling 形态）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// 只读工具（不改变系统状态；仅用于说明与界面展示，**不**参与权限判定）。
    pub read_only: bool,
    /// 长耗时命令用更长的超时（`get_app_state` / `screenshot` / `wait`）。
    pub long_timeout: bool,
}

impl ToolSpec {
    /// 转成请求体里的 tool 对象。
    pub fn to_request_value(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// 构造一个 `additionalProperties: false` 的对象 schema。
pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), properties);
    schema.insert("additionalProperties".to_string(), json!(false));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    Value::Object(schema)
}

/// 把多个 `properties` 片段合并成一个对象（后写的键覆盖先写的）。
pub fn merge_objects(parts: &[Value]) -> Value {
    let mut merged = Map::new();
    for part in parts {
        if let Some(object) = part.as_object() {
            for (key, value) in object {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}

/// 数字（或数字字符串）→ `f64`。
pub fn coerce_number(args: &Map<String, Value>, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            trimmed.parse::<f64>().ok()
        }
        _ => None,
    }
}

/// 数字（或数字字符串）→ 四舍五入的整数。
pub fn coerce_int(args: &Map<String, Value>, key: &str) -> Option<i64> {
    coerce_number(args, key).map(|value| value.round() as i64)
}

/// 布尔（或 `"true"` / `"false"` 字符串）→ `bool`。
pub fn coerce_bool(args: &Map<String, Value>, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 字符串（数字也接受）→ `String`。
pub fn coerce_string(args: &Map<String, Value>, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// 把参数值读成对象（非对象一律当空对象——与旧版一致）。
pub fn as_args(raw: &Value) -> Map<String, Value> {
    raw.as_object().cloned().unwrap_or_default()
}

/// 必填字符串参数：缺失或空白一律报错（**在动作之前**拦下）。
pub fn require_string(args: &Map<String, Value>, key: &str, tool: &str) -> Result<String, String> {
    let value = coerce_string(args, key).unwrap_or_default();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{tool} 需要 {key}"));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_strings_and_bools_are_coerced() {
        let args = as_args(&json!({ "n": "10", "f": " 2.5 ", "b": "true", "s": 3 }));
        assert_eq!(coerce_int(&args, "n"), Some(10));
        assert_eq!(coerce_number(&args, "f"), Some(2.5));
        assert_eq!(coerce_bool(&args, "b"), Some(true));
        assert_eq!(coerce_string(&args, "s").as_deref(), Some("3"));
        assert_eq!(coerce_int(&args, "missing"), None);
    }

    #[test]
    fn required_string_rejects_blank_values() {
        let args = as_args(&json!({ "command": "   " }));
        assert!(require_string(&args, "command", "run_shell").is_err());
        let args = as_args(&json!({ "command": " Get-Date " }));
        assert_eq!(
            require_string(&args, "command", "run_shell").unwrap(),
            "Get-Date"
        );
    }

    #[test]
    fn schema_merging_keeps_later_keys() {
        let merged = merge_objects(&[
            json!({ "a": { "type": "string" } }),
            json!({ "a": { "type": "integer" }, "b": { "type": "boolean" } }),
        ]);
        assert_eq!(merged["a"]["type"], json!("integer"));
        assert_eq!(merged["b"]["type"], json!("boolean"));
    }

    #[test]
    fn spec_serializes_as_openai_function() {
        let spec = ToolSpec {
            name: "demo",
            description: "d",
            parameters: object_schema(json!({}), &[]),
            read_only: true,
            long_timeout: false,
        };
        let value = spec.to_request_value();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["function"]["name"], json!("demo"));
        assert_eq!(
            value["function"]["parameters"]["additionalProperties"],
            json!(false)
        );
    }
}
