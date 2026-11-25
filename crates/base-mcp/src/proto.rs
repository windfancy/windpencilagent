//! JSON-RPC 2.0 消息构造与解析（MCP stdio transport：newline-delimited JSON）。
//!
//! 纯函数层，无 IO，全部可单测。

use serde_json::Value;

/// 构造请求（带 id）。
pub fn request(id: i64, method: &str, params: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

/// 构造通知（无 id，不期待响应）。
pub fn notification(method: &str, params: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
    .to_string()
}

/// 解析响应行：返回 `(id, result|error)`。
///
/// - 成功 → `Ok((id, Ok(result)))`
/// - 协议错误 → `Ok((id, Err(message)))`
/// - 不是响应（通知/请求/坏 JSON）→ `None`
pub fn parse_response(line: &str) -> Option<(i64, Result<Value, String>)> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let id = v.get("id")?.as_i64()?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown rpc error")
            .to_string();
        return Some((id, Err(msg)));
    }
    Some((id, Ok(v.get("result").cloned().unwrap_or(Value::Null))))
}

/// 从 `tools/list` 结果提取工具定义数组。
pub fn extract_tools(result: &Value) -> Vec<McpToolDef> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?.to_string();
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let schema = t.get("inputSchema").cloned().unwrap_or(Value::Null);
                    Some(McpToolDef { name, description, input_schema: schema })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 从 `tools/call` 结果提取文本回显与错误标志。
pub fn extract_call_result(result: &Value) -> (bool, String) {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let mut text = String::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }
    }
    (!is_error, text)
}

/// MCP 工具定义（`tools/list` 单条）。
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_and_parse_roundtrip() {
        let line = request(7, "tools/list", json!({}));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "tools/list");

        let resp = r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[{"name":"read_file","description":"d","inputSchema":{"type":"object"}}]}}"#;
        let (id, r) = parse_response(resp).unwrap();
        assert_eq!(id, 7);
        let result = r.unwrap();
        let tools = extract_tools(&result);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].input_schema["type"], "object");
    }

    #[test]
    fn rpc_error_extracted() {
        let resp = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found"}}"#;
        let (id, r) = parse_response(resp).unwrap();
        assert_eq!(id, 3);
        assert_eq!(r.unwrap_err(), "method not found");
    }

    #[test]
    fn notifications_have_no_id() {
        let line = notification("notifications/initialized", json!({}));
        assert!(parse_response(&line).is_none(), "通知不应被当作响应");
    }

    #[test]
    fn call_result_text_and_error_flag() {
        let ok = json!({"content":[{"type":"text","text":"22/tcp open"},{"type":"text","text":"80/tcp open"}],"isError":false});
        let (is_ok, text) = extract_call_result(&ok);
        assert!(is_ok);
        assert_eq!(text, "22/tcp open\n80/tcp open");

        let bad = json!({"content":[{"type":"text","text":"boom"}],"isError":true});
        let (is_ok, text) = extract_call_result(&bad);
        assert!(!is_ok);
        assert_eq!(text, "boom");
    }
}
