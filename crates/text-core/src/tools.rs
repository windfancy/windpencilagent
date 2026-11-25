//! 文本原语工具：`text.codec` / `text.hash` / `text.regex` / `text.extract` / `text.transform`。

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use regex::Regex;
use serde_json::{json, Value};

use toolkit_core::tools::{Tool, ToolResult};

use crate::codec;
use crate::hash;

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(|s| s.to_string())
}

/// 取输入：`data`/`text` 内联，或 `file` 路径。
fn input_text(args: &Value) -> Result<String, String> {
    for key in ["data", "text"] {
        if let Some(d) = arg_str(args, key) {
            return Ok(d);
        }
    }
    if let Some(f) = args.get("file").and_then(Value::as_str) {
        return std::fs::read_to_string(f).map_err(|e| format!("读取文件失败 {f}：{e}"));
    }
    Err("缺少 data/text 或 file 参数".into())
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + &format!("…（共 {} 字符）", s.chars().count())
    }
}

// ---------------------------------------------------------------------------
// text.codec
// ---------------------------------------------------------------------------

pub struct TextCodec;

#[async_trait]
impl Tool for TextCodec {
    fn name(&self) -> &str {
        "text.codec"
    }

    fn description(&self) -> String {
        "编解码文本：base64 / base64url / url / hex / html 实体。action = encode | decode。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["encode", "decode"]},
                "scheme": {"type": "string", "enum": ["base64", "base64url", "url", "hex", "html"]},
                "data": {"type": "string", "description": "待处理文本"},
                "file": {"type": "string", "description": "从文件读取（与 data 二选一）"}
            },
            "required": ["action", "scheme"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let action = arg_str(&args, "action").unwrap_or_default();
        let scheme = arg_str(&args, "scheme").unwrap_or_default();
        let input = match input_text(&args) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(e, 0),
        };
        let out = match action.as_str() {
            "encode" => codec::encode(&scheme, &input),
            "decode" => codec::decode(&scheme, &input),
            other => Err(format!("action 只能是 encode/decode，收到：{other}")),
        };
        match out {
            Ok(s) => ToolResult::json(
                format!("{scheme} {action} 完成（{} → {} 字符）", input.chars().count(), s.chars().count()),
                json!({"result": truncate_chars(&s, 20000), "length": s.chars().count()}),
                0,
            ),
            Err(e) => ToolResult::error(e, 0),
        }
    }
}

// ---------------------------------------------------------------------------
// text.hash
// ---------------------------------------------------------------------------

pub struct TextHash;

#[async_trait]
impl Tool for TextHash {
    fn name(&self) -> &str {
        "text.hash"
    }

    fn description(&self) -> String {
        "计算文本/文件的 md5/sha1/sha256/sha512；算法省略时返回全部。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "algo": {"type": "string", "enum": ["md5", "sha1", "sha256", "sha512"]},
                "data": {"type": "string"},
                "file": {"type": "string"}
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let bytes: Vec<u8> = if let Some(f) = args.get("file").and_then(Value::as_str) {
            match std::fs::read(f) {
                Ok(b) => b,
                Err(e) => return ToolResult::error(format!("读取文件失败 {f}：{e}"), 0),
            }
        } else if let Some(d) = arg_str(&args, "data") {
            d.into_bytes()
        } else {
            return ToolResult::error("缺少 data 或 file 参数".to_string(), 0);
        };
        match arg_str(&args, "algo") {
            Some(a) => match hash::hash_bytes(&a, &bytes) {
                Ok(h) => ToolResult::json(format!("{a}: {h}"), json!({"algo": a, "hash": h}), 0),
                Err(e) => ToolResult::error(e, 0),
            },
            None => {
                let all = hash::hash_all(&bytes);
                ToolResult::json(
                    format!("sha256: {}", all["sha256"].as_str().unwrap_or("")),
                    all,
                    0,
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// text.regex
// ---------------------------------------------------------------------------

pub struct TextRegex;

#[async_trait]
impl Tool for TextRegex {
    fn name(&self) -> &str {
        "text.regex"
    }

    fn description(&self) -> String {
        "用正则处理文本：mode = find（首个）| find_all（全部）| capture（捕获组）| replace（替换）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Rust/Python 风格正则"},
                "text": {"type": "string"},
                "file": {"type": "string"},
                "mode": {"type": "string", "enum": ["find", "find_all", "capture", "replace"]},
                "replacement": {"type": "string", "description": "replace 模式的替换串（支持 $1）"},
                "max": {"type": "integer", "description": "最多返回条数（默认 200）"}
            },
            "required": ["pattern"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let pattern = arg_str(&args, "pattern").unwrap_or_default();
        // mode 缺省按 find：这是描述里的第一档，也是"只想看一眼命中没"的最常见用法。
        let mode = arg_str(&args, "mode").unwrap_or_else(|| "find".into());
        let text = match input_text(&args) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e, 0),
        };
        let re = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("正则编译失败：{e}"), 0),
        };
        let max = args.get("max").and_then(Value::as_u64).unwrap_or(200).clamp(1, 5000) as usize;
        match mode.as_str() {
            "find" => match re.find(&text) {
                Some(m) => ToolResult::json(
                    format!("命中：{}", m.as_str()),
                    json!({"match": m.as_str(), "start": m.start(), "end": m.end()}),
                    0,
                ),
                None => ToolResult::json("未命中".to_string(), json!({"match": null}), 0),
            },
            "find_all" => {
                let all: Vec<&str> = re.find_iter(&text).take(max).map(|m| m.as_str()).collect();
                ToolResult::json(
                    format!("命中 {} 处（{}）", all.len(), all.iter().take(5).cloned().collect::<Vec<_>>().join(", ")),
                    json!({"matches": all, "count": all.len()}),
                    0,
                )
            }
            "capture" => {
                let mut rows = Vec::new();
                for caps in re.captures_iter(&text).take(max) {
                    let groups: Vec<Option<String>> = caps
                        .iter()
                        .map(|g| g.map(|m| m.as_str().to_string()))
                        .collect();
                    rows.push(groups);
                }
                ToolResult::json(
                    format!("捕获 {} 组", rows.len()),
                    json!({"groups": rows, "count": rows.len()}),
                    0,
                )
            }
            "replace" => {
                let rep = arg_str(&args, "replacement").unwrap_or_default();
                let out = re.replace_all(&text, rep.as_str()).to_string();
                ToolResult::json(
                    format!("替换完成（结果 {} 字符）", out.chars().count()),
                    json!({"result": truncate_chars(&out, 20000)}),
                    0,
                )
            }
            other => ToolResult::error(format!("mode 非法：{other}"), 0),
        }
    }
}

// ---------------------------------------------------------------------------
// text.extract
// ---------------------------------------------------------------------------

/// 提取种类与对应正则。
fn pattern_of(kind: &str) -> Option<(&'static str, Regex)> {
    let (name, pat) = match kind {
        // 排除中英文标点：否则 `http://a.com/x，` 会把逗号吃进 URL，造成"同址不同串"。
        "url" => ("url", r#"https?://[^\s"'<>\)\]，。；、！？]+"#),
        "email" => ("email", r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"),
        "ip" => ("ip", r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
        "domain" => ("domain", r"\b(?:[a-zA-Z0-9](?:[a-zA-Z0-9\-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,}\b"),
        "path" => ("path", r#"/[A-Za-z0-9_\-./%]{2,}"#),
        "jwt" => ("jwt", r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+"),
        _ => return None,
    };
    Regex::new(pat).ok().map(|r| (name, r))
}

/// path 提取的误报过滤：
/// - `</script>`、`</div>` 这类闭合标签会被 `/[A-Za-z...]` 抓成 `/script`、`/div`；
/// - 位于 URL 内部的片段（`http://h/a`、`//cdn/a.js`）不是独立路径，交给 `url` 种类；
/// - 纯数字/符号的"路径"没有价值。
fn is_real_path(text: &str, start: usize, v: &str) -> bool {
    let before = &text[..start];
    if before.ends_with('<') || before.ends_with('/') {
        return false;
    }
    // 协议相对 URL（`//cdn/a.js`）本身就是一整串，不能当路径
    if v.starts_with("//") {
        return false;
    }
    if !v.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    // 同一个 token（引号/空白/尖括号分隔）里出现过 scheme 或 `//` 开头 ⇒ 是 URL 的一部分
    let token_start = before
        .rfind(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '='))
        .map(|i| i + 1)
        .unwrap_or(0);
    let token = &before[token_start..];
    if token.contains("://") || token.starts_with("//") {
        return false;
    }
    true
}

pub struct TextExtract;
#[async_trait]
impl Tool for TextExtract {
    fn name(&self) -> &str {
        "text.extract"
    }

    fn description(&self) -> String {
        "从文本中批量提取结构化线索：url / email / ip / domain / path / jwt（去重保序）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "file": {"type": "string"},
                "kinds": {"type": "array", "items": {"type": "string"}, "description": "默认全部"},
                "max": {"type": "integer", "description": "每种最多条数（默认 200）"}
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let text = match input_text(&args) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e, 0),
        };
        let kinds: Vec<String> = args
            .get("kinds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_else(|| {
                ["url", "email", "ip", "domain", "path", "jwt"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        let max = args.get("max").and_then(Value::as_u64).unwrap_or(200).clamp(1, 5000) as usize;

        let mut found = serde_json::Map::new();
        let mut counts: Vec<String> = Vec::new();
        for k in &kinds {
            let Some((name, re)) = pattern_of(k) else {
                return ToolResult::error(
                    format!("不支持的提取类型：{k}（可选 url/email/ip/domain/path/jwt）"),
                    0,
                );
            };
            let mut seen = std::collections::BTreeSet::new();
            let mut vals: Vec<String> = Vec::new();
            for m in re.find_iter(&text) {
                let v = m.as_str().to_string();
                if name == "path" && !is_real_path(&text, m.start(), &v) {
                    continue;
                }
                if seen.insert(v.clone()) {
                    vals.push(v);
                    if vals.len() >= max {
                        break;
                    }
                }
            }
            counts.push(format!("{name}={}", vals.len()));
            found.insert(name.to_string(), json!(vals));
        }
        ToolResult::json(
            format!("提取完成：{}", counts.join("，")),
            Value::Object(found),
            0,
        )
    }
}

// ---------------------------------------------------------------------------
// text.transform
// ---------------------------------------------------------------------------

pub struct TextTransform;

#[async_trait]
impl Tool for TextTransform {
    fn name(&self) -> &str {
        "text.transform"
    }

    fn description(&self) -> String {
        "常见文本变换：json_pretty / json_min / uniq / sort / trim / upper / lower / lines_to_csv / csv_to_lines。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["json_pretty","json_min","uniq","sort","trim","upper","lower","lines_to_csv","csv_to_lines"]},
                "data": {"type": "string"},
                "file": {"type": "string"}
            },
            "required": ["op"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let op = arg_str(&args, "op").unwrap_or_default();
        let data = match input_text(&args) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e, 0),
        };
        let out = match op.as_str() {
            "json_pretty" => match serde_json::from_str::<Value>(&data) {
                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                Err(e) => return ToolResult::error(format!("不是合法 JSON：{e}"), 0),
            },
            "json_min" => match serde_json::from_str::<Value>(&data) {
                Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                Err(e) => return ToolResult::error(format!("不是合法 JSON：{e}"), 0),
            },
            "uniq" => {
                let mut seen = std::collections::BTreeSet::new();
                data.lines()
                    .filter(|l| seen.insert(l.to_string()))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "sort" => {
                let mut lines: Vec<&str> = data.lines().collect();
                lines.sort_unstable();
                lines.join("\n")
            }
            "trim" => data
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            "upper" => data.to_uppercase(),
            "lower" => data.to_lowercase(),
            "lines_to_csv" => data
                .lines()
                .map(|l| {
                    if l.contains(',') || l.contains('"') {
                        format!("\"{}\"", l.replace('"', "\"\""))
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            "csv_to_lines" => data.split(',').map(|s| s.trim().trim_matches('"')).collect::<Vec<_>>().join("\n"),
            other => return ToolResult::error(format!("不支持的变换：{other}"), 0),
        };
        ToolResult::json(
            format!("{op} 完成（{} 字符）", out.chars().count()),
            json!({"result": truncate_chars(&out, 20000)}),
            0,
        )
    }
}

/// 全部文本原语。
pub fn all() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![
        std::sync::Arc::new(TextCodec),
        std::sync::Arc::new(TextHash),
        std::sync::Arc::new(TextRegex),
        std::sync::Arc::new(TextExtract),
        std::sync::Arc::new(TextTransform),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kern_core::model::{AgentId, SessionId, TaskId};

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("t"),
            task_id: TaskId::new("t"),
            target: String::new(),
            risk: RiskLevel::None,
            approved: true,
        }
    }

    fn res(r: ToolResult) -> Value {
        assert!(r.ok, "{}", r.summary);
        r.detail.unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn codec_tool_roundtrip() {
        let e = res(TextCodec
            .call(&ctx(), json!({"action":"encode","scheme":"base64","data":"hello"}))
            .await);
        let b64 = e["result"].as_str().unwrap().to_string();
        let d = res(TextCodec
            .call(&ctx(), json!({"action":"decode","scheme":"base64","data":b64}))
            .await);
        assert_eq!(d["result"], "hello");
    }

    #[tokio::test]
    async fn hash_tool_known_value() {
        let v = res(TextHash.call(&ctx(), json!({"algo":"sha256","data":"abc"})).await);
        assert_eq!(v["hash"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[tokio::test]
    async fn extract_path_skips_html_closing_tags() {
        // 实机误报：`</script>` 被抽成 `/script`，渗透树会把标签名当路径去请求 404。
        let html = r#"<a href="/admin/login.php">x</a><script src="/static/app.js"></script></div>"#;
        let v = res(TextExtract
            .call(&ctx(), json!({"text": html, "kinds": ["path"]}))
            .await);
        let paths: Vec<&str> = v["path"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
        assert_eq!(paths, vec!["/admin/login.php", "/static/app.js"], "{paths:?}");
        // 协议相对 URL 的后半段也不算路径
        let v = res(TextExtract
            .call(&ctx(), json!({"text": "src=\"//cdn.example.com/a.js\"", "kinds": ["path"]}))
            .await);
        assert!(v["path"].as_array().unwrap().is_empty(), "{v}");
        // URL 里的路径交给 url 种类，path 不重复报
        let v = res(TextExtract
            .call(&ctx(), json!({"text": "see http://10.0.0.7:8080/internal/api now", "kinds": ["path"]}))
            .await);
        assert!(v["path"].as_array().unwrap().is_empty(), "{v}");
        // 独立出现的路径照常保留
        let v = res(TextExtract
            .call(&ctx(), json!({"text": "try /etc/passwd and /a/b?x=1", "kinds": ["path"]}))
            .await);
        let paths: Vec<&str> = v["path"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
        assert_eq!(paths, vec!["/etc/passwd", "/a/b"], "{paths:?}");
    }

    #[tokio::test]
    async fn regex_modes() {
        let v = res(TextRegex
            .call(&ctx(), json!({"pattern":"\\d+","text":"a1 b22 c333","mode":"find_all"}))
            .await);
        assert_eq!(v["count"], 3);
        assert_eq!(v["matches"][2], "333");

        let v = res(TextRegex
            .call(&ctx(), json!({"pattern":"(\\w)(\\d)","text":"a1 b2","mode":"capture"}))
            .await);
        assert_eq!(v["count"], 2);
        assert_eq!(v["groups"][0][1], "a");

        let v = res(TextRegex
            .call(&ctx(), json!({"pattern":"\\d+","text":"a1b2","mode":"replace","replacement":"#"}))
            .await);
        assert_eq!(v["result"], "a#b#");
    }

    #[tokio::test]
    async fn regex_bad_pattern_errors() {
        let r = TextRegex.call(&ctx(), json!({"pattern":"(","text":"x","mode":"find"})).await;
        assert!(!r.ok);
    }

    #[tokio::test]
    async fn regex_mode_defaults_to_find() {
        // 树包工具可能只传 pattern/data：mode 缺省必须工作，否则业务包装永远调不通。
        let v = res(TextRegex
            .call(&ctx(), json!({"pattern":"OpenSSH_([0-9.p]+)","text":"SSH-2.0-OpenSSH_8.2p1"}))
            .await);
        assert_eq!(v["match"], "OpenSSH_8.2p1");
    }

    #[tokio::test]
    async fn extract_dedups_and_counts() {
        let v = res(TextExtract
            .call(&ctx(), json!({
                "text": "访问 http://a.com/x 与 http://a.com/x，邮箱 ops@a.com，IP 10.0.0.5 和 10.0.0.5",
                "kinds": ["url","email","ip"]
            }))
            .await);
        assert_eq!(v["url"].as_array().unwrap().len(), 1);
        assert_eq!(v["email"][0], "ops@a.com");
        assert_eq!(v["ip"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn transform_json_and_lines() {
        let v = res(TextTransform
            .call(&ctx(), json!({"op":"json_min","data":"{ \"a\" : 1 }"}))
            .await);
        assert_eq!(v["result"], r#"{"a":1}"#);

        let v = res(TextTransform
            .call(&ctx(), json!({"op":"uniq","data":"a\nb\na"}))
            .await);
        assert_eq!(v["result"].as_str().unwrap().lines().count(), 2);
    }

    #[test]
    fn all_tools_are_wired() {
        let names: Vec<String> = all().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            names,
            vec!["text.codec", "text.hash", "text.regex", "text.extract", "text.transform"]
        );
    }
}
