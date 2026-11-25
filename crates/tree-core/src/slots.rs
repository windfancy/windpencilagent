//! 插槽（slots）：工具参数里**不该由 LLM 猜**的值，从上下文取。
//!
//! 装配时把插槽字段从 schema 移除（LLM 看不到、不需要传），调用时自动填值。
//! 收益：减少参数幻觉、减少一次审批往返、保证"目标永远来自授权范围"。

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 插槽取值上下文（由基座在装配/调用时构造）。
#[derive(Debug, Clone, Default)]
pub struct SlotContext {
    /// 授权范围（顺序保留；`scope.primary` 取第一个）。
    pub scope: Vec<String>,
    pub workspace_name: String,
    pub workspace_cwd: String,
    /// 生效模型档案的可读字段（`profile.timeout` 等业务无关键）。
    pub profile: BTreeMap<String, String>,
    pub session_id: String,
}

impl SlotContext {
    /// 解析一个插槽表达式。未知来源返回 `None`（fail-closed：宁可缺参也不猜）。
    pub fn resolve(&self, expr: &str) -> Option<Value> {
        let expr = expr.trim();
        match expr {
            "scope.primary" => self.scope.first().map(|s| Value::String(s.clone())),
            "scope.all" => Some(Value::Array(
                self.scope.iter().cloned().map(Value::String).collect(),
            )),
            "scope.count" => Some(Value::Number(self.scope.len().into())),
            "workspace.name" => Some(Value::String(self.workspace_name.clone())),
            "workspace.cwd" => {
                if self.workspace_cwd.trim().is_empty() {
                    None
                } else {
                    Some(Value::String(self.workspace_cwd.clone()))
                }
            }
            "session.id" => Some(Value::String(self.session_id.clone())),
            other => other
                .strip_prefix("profile.")
                .and_then(|k| self.profile.get(k))
                .map(|v| Value::String(v.clone())),
        }
    }

    /// 是否有任何授权目标（无目标时网络类插槽无法填值）。
    pub fn has_scope(&self) -> bool {
        !self.scope.is_empty()
    }

    /// 工作目录（绝对路径优先；否则退化为当前目录，不做隐式计算）。
    pub fn cwd_path(&self) -> Option<PathBuf> {
        if self.workspace_cwd.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(&self.workspace_cwd))
        }
    }
}

/// 从 JSON Schema 中移除已声明为插槽的属性（LLM 不再需要传）。
///
/// 同时从 `required` 里删掉（否则模型会被告知缺失必填项而编造）。
pub fn strip_from_schema(schema: &mut Value, slots: &BTreeMap<String, String>) {
    if slots.is_empty() {
        return;
    }
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for k in slots.keys() {
            props.remove(k);
        }
    }
    if let Some(req) = schema.get_mut("required").and_then(|r| r.as_array_mut()) {
        req.retain(|v| v.as_str().map(|s| !slots.contains_key(s)).unwrap_or(true));
    }
}

/// 调用时把插槽值填进参数（已有同名字段时以插槽为准——红线不下放给模型）。
pub fn apply_slots(args: &mut Value, slots: &BTreeMap<String, String>, ctx: &SlotContext) -> Vec<String> {
    let mut missing = Vec::new();
    let Some(obj) = args.as_object_mut() else {
        return missing;
    };
    for (key, expr) in slots {
        match ctx.resolve(expr) {
            Some(v) => {
                obj.insert(key.clone(), v);
            }
            None => missing.push(format!("{key}={expr}")),
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> SlotContext {
        SlotContext {
            scope: vec!["10.0.0.5".into(), "example.com".into()],
            workspace_name: "proj-a".into(),
            workspace_cwd: "D:/work/proj-a".into(),
            profile: BTreeMap::from([("timeout".to_string(), "30".to_string())]),
            session_id: "s-1".into(),
        }
    }

    #[test]
    fn resolves_known_slots() {
        let c = ctx();
        assert_eq!(c.resolve("scope.primary"), Some(json!("10.0.0.5")));
        assert_eq!(c.resolve("scope.all"), Some(json!(["10.0.0.5", "example.com"])));
        assert_eq!(c.resolve("workspace.name"), Some(json!("proj-a")));
        assert_eq!(c.resolve("profile.timeout"), Some(json!("30")));
        assert_eq!(c.resolve("session.id"), Some(json!("s-1")));
    }

    #[test]
    fn unknown_slot_is_none_not_guessed() {
        let c = ctx();
        assert_eq!(c.resolve("scope.nope"), None);
        assert_eq!(c.resolve("profile.missing"), None);
        let empty = SlotContext::default();
        assert!(!empty.has_scope());
        assert_eq!(empty.resolve("workspace.cwd"), None);
    }

    #[test]
    fn schema_strip_and_apply() {
        let mut schema = json!({
            "type": "object",
            "properties": {"host": {"type": "string"}, "ports": {"type": "string"}},
            "required": ["host", "ports"]
        });
        let slots = BTreeMap::from([("host".to_string(), "scope.primary".to_string())]);
        strip_from_schema(&mut schema, &slots);
        assert!(schema["properties"].get("host").is_none());
        assert!(schema["properties"].get("ports").is_some());
        assert_eq!(schema["required"], json!(["ports"]));

        let mut args = json!({"ports": "80,443", "host": "spoofed"});
        let missing = apply_slots(&mut args, &slots, &ctx());
        assert!(missing.is_empty());
        assert_eq!(args["host"], "10.0.0.5", "插槽值优先，模型传的必须被覆盖");
    }

    #[test]
    fn missing_scope_reports_missing_slot() {
        let mut args = json!({});
        let slots = BTreeMap::from([("host".to_string(), "scope.primary".to_string())]);
        let missing = apply_slots(&mut args, &slots, &SlotContext::default());
        assert_eq!(missing, vec!["host=scope.primary"]);
        assert!(args.get("host").is_none());
    }
}
