//! 包清单 `PnC.toml`：一个"包"装进某个插口的自描述。
//!
//! 格式沿用既有 `base-plugins` 的 `id / version / permissions` 语义并扩展：
//! - `kind`：插口类型（knowledge / tools / logic / skills）；
//! - `min_base_version`：版本闸（不满足即拒绝该包，不影响其它包）；
//! - `[capabilities]`：能力标签与阶段，供 UI 展示与工具面收敛；
//! - `[[tool]]`：声明式工具表（`exec` 四路：http / command / python / primitive）；
//! - `slots`：不该由 LLM 猜的参数，装配时从上下文取值并从 schema 移除。
//!
//! **向后兼容**：所有新增字段都有默认值，只有 `id` 必填。

use serde::{Deserialize, Serialize};

use wpa_api::{GroupKind, SemVer};

/// 权限声明（沿用旧格式；当前仅用于审计与 UI 展示，执行面由基座链路统一约束）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Permissions {
    pub net_connect: bool,
    pub fs_read_tmp: bool,
    pub fs_write_workspace: bool,
    pub proc_spawn: bool,
    pub max_memory_mb: Option<u64>,
    pub max_steps: Option<u64>,
}

/// 能力标签与阶段归属。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Capabilities {
    /// 标签（`network` / `recon` / `web`）：UI 筛选与上下文收敛。
    pub tags: Vec<String>,
    /// 该包在哪些阶段可用（对接 `Planner::phases()` 的 id）；空 = 全阶段。
    pub phases: Vec<String>,
}

/// 声明式工具。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    /// 风险档（low / medium / high / critical）；缺省 **high**（fail-closed）。
    pub risk: Option<String>,
    /// 执行方式。
    pub exec: ExecSpec,
    /// 插槽：参数名 → 取值来源（`scope.primary` 等）。
    pub slots: std::collections::BTreeMap<String, String>,
    /// 参数 schema（内联表，转成 JSON Schema）。插槽参数会被自动移除。
    pub parameters: Option<toml::Value>,
    /// 是否并发安全（只读探测建议 true）。
    pub concurrency_safe: Option<bool>,
    /// 超时秒数（默认 30）。
    pub timeout_secs: Option<u64>,
}

/// 四种执行方式。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ExecSpec {
    /// HTTP 请求（走基座共享连接池）。
    Http {
        #[serde(default = "default_method")]
        method: String,
        url: String,
        #[serde(default)]
        headers: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        body: Option<String>,
        /// 命中这些状态码视为成功（默认 2xx/3xx）。
        #[serde(default)]
        expect: Vec<u16>,
    },
    /// 外部程序（argv 直传，不经 shell）。
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        stdin: Option<String>,
    },
    /// Python 脚本（走基座沙箱：解释器与脚本根受限）。
    Python {
        script: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// 基座原语（`net.scan` / `text.codec` …）：把通用能力包装成业务命名工具。
    Primitive {
        primitive: String,
        /// 是否把调用参数原样传给原语（默认 true）。
        #[serde(default = "default_true")]
        passthrough: bool,
    },
}

impl Default for ExecSpec {
    fn default() -> Self {
        // 未声明 exec 的工具不可能是可执行工具；给一个必然失败的占位，
        // 由 validate 拒绝（fail-closed：绝不"默认跑点什么"）。
        Self::Command { program: String::new(), args: vec![], stdin: None }
    }
}

fn default_method() -> String {
    "GET".into()
}

fn default_true() -> bool {
    true
}

/// 知识包声明。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct KnowledgeDecl {
    /// 知识条目文件（相对群组目录）：`.jsonl` / `.json` / `.md`。
    pub entries: Vec<String>,
    /// 约束文件（`.toml`：`[[constraint]] id/text/severity`）。
    pub constraints: Option<String>,
}

/// 技能条目声明（一个包可含多个技能）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillDecl {
    pub name: String,
    pub trigger: String,
    /// 正文文件（相对群组目录；缺省 = 用 `body` 内联）。
    pub body_file: Option<String>,
    pub body: Option<String>,
    pub keywords: Vec<String>,
    pub requires: Vec<String>,
}

/// 阶段声明（逻辑包用）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PhaseDecl {
    pub id: String,
    pub label: String,
    pub entry_hint: String,
    pub exit_condition: String,
    pub allowed_groups: Vec<String>,
}

/// 一个包的清单。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PnCManifest {
    pub id: String,
    pub kind: Option<String>,
    pub name: String,
    pub version: String,
    pub min_base_version: Option<String>,
    pub description: String,
    pub permissions: Permissions,
    /// 跨插口依赖（组级拓扑）。
    pub deps: Vec<String>,
    pub capabilities: Capabilities,

    /// 工具包：工具表。
    pub tool: Vec<ToolDecl>,
    /// 知识包。
    pub knowledge: Option<KnowledgeDecl>,
    /// 技能包。
    pub skill: Vec<SkillDecl>,
    /// 逻辑包：阶段与工作范式。
    pub phase: Vec<PhaseDecl>,
    /// 逻辑包：注入的 prompt 片段（内联或文件）。
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    /// 领域角色（覆盖基座默认通用角色）。
    pub persona: Option<String>,
}

impl PnCManifest {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("PnC.toml 解析失败：{e}"))
    }

    pub fn load(dir: &std::path::Path) -> Result<Self, String> {
        let path = dir.join("PnC.toml");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取清单失败 {}：{e}", path.display()))?;
        Self::from_toml(&text)
    }

    /// 插口类型（缺省 tools；未知值拒绝）。
    pub fn group_kind(&self) -> Result<GroupKind, String> {
        match self.kind.as_deref().unwrap_or("tools").trim().to_ascii_lowercase().as_str() {
            "knowledge" => Ok(GroupKind::Knowledge),
            "tools" => Ok(GroupKind::Tools),
            "logic" => Ok(GroupKind::Logic),
            "skills" => Ok(GroupKind::Skills),
            other => Err(format!("未知插口类型 kind = {other}")),
        }
    }

    pub fn parsed_version(&self) -> SemVer {
        SemVer::parse(&self.version).unwrap_or(SemVer { major: 0, minor: 0, patch: 0, pre: None })
    }

    pub fn min_version(&self) -> Option<SemVer> {
        self.min_base_version.as_deref().and_then(|v| SemVer::parse(v).ok())
    }

    /// 清单自检（字段级；任一错误即拒绝装配）。
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.id.trim().is_empty() {
            errs.push("包缺少 id".into());
        }
        if self.version.trim().is_empty() {
            errs.push(format!("包 {} 缺少 version", self.id));
        } else if SemVer::parse(&self.version).is_err() {
            errs.push(format!("包 {} 的 version 非法：{}", self.id, self.version));
        }
        if let Some(v) = &self.min_base_version {
            if SemVer::parse(v).is_err() {
                errs.push(format!("包 {} 的 min_base_version 非法：{v}", self.id));
            }
        }
        if let Err(e) = self.group_kind() {
            errs.push(format!("包 {}：{e}", self.id));
        }
        for t in &self.tool {
            if t.name.trim().is_empty() {
                errs.push(format!("包 {} 存在无名工具", self.id));
                continue;
            }
            if !t.name.contains('.') {
                errs.push(format!("工具 {} 名应为 `命名空间.动作`", t.name));
            }
            if let ExecSpec::Command { program, .. } = &t.exec {
                if program.trim().is_empty() {
                    errs.push(format!("工具 {} 未声明 exec（或 program 为空）", t.name));
                }
            }
            if let ExecSpec::Primitive { primitive, .. } = &t.exec {
                if primitive.trim().is_empty() {
                    errs.push(format!("工具 {} 的 exec.primitive 为空", t.name));
                }
            }
            if let ExecSpec::Python { script, .. } = &t.exec {
                if script.trim().is_empty() || script.contains("..") {
                    errs.push(format!("工具 {} 的 python 脚本路径非法：{script}", t.name));
                }
            }
        }
        for s in &self.skill {
            if s.name.trim().is_empty() {
                errs.push(format!("包 {} 存在无名技能", self.id));
            }
        }
        for p in &self.phase {
            if p.id.trim().is_empty() {
                errs.push(format!("包 {} 存在无 id 阶段", self.id));
            }
        }
        errs
    }
}

/// TOML 值 → JSON 值（schema 声明用）。
pub fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => serde_json::Value::Object(
            t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect(),
        ),
    }
}

/// 字符串风险档 → 契约风险档。
///
/// - 缺省（`None`）：按 **High** 处理（未声明风险不能当低风险）；
/// - 写了但无法识别：按 **Critical** 处理（拼错比不写更危险）。
pub fn parse_risk(s: Option<&str>) -> wpa_api::RiskTier {
    use wpa_api::RiskTier;
    match s {
        None => RiskTier::High,
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "none" => RiskTier::None,
            "low" => RiskTier::Low,
            "medium" => RiskTier::Medium,
            "high" => RiskTier::High,
            "critical" => RiskTier::Critical,
            _ => RiskTier::Critical,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOLS_PNC: &str = r#"
id = "tools.scan"
kind = "tools"
name = "端口与服务扫描"
version = "1.0.0"
min_base_version = "0.1.0"
description = "主机存活与端口识别"

[permissions]
net-connect = true

[capabilities]
tags = ["network", "recon"]
phases = ["recon"]

[[tool]]
name = "scan.port"
description = "TCP 端口扫描"
risk = "medium"
exec = { kind = "primitive", primitive = "net.scan" }
slots = { host = "scope.primary" }
concurrency_safe = true
parameters = { type = "object", properties = { ports = { type = "string" } } }

[[tool]]
name = "scan.http"
description = "HTTP 指纹"
risk = "low"
exec = { kind = "http", method = "GET", url = "http://{{host}}/" }
"#;

    #[test]
    fn parses_tools_package() {
        let p = PnCManifest::from_toml(TOOLS_PNC).expect("应解析");
        assert!(p.validate().is_empty(), "{:?}", p.validate());
        assert_eq!(p.group_kind().unwrap(), GroupKind::Tools);
        assert_eq!(p.tool.len(), 2);
        assert_eq!(p.tool[0].name, "scan.port");
        assert!(p.tool[0].slots.contains_key("host"));
        assert!(matches!(p.tool[0].exec, ExecSpec::Primitive { .. }));
        assert!(matches!(p.tool[1].exec, ExecSpec::Http { .. }));
        assert_eq!(p.capabilities.phases, vec!["recon"]);
        assert!(p.permissions.net_connect);
    }

    #[test]
    fn rejects_incomplete_tool() {
        let bad = r#"
id = "tools.bad"
version = "1.0.0"
[[tool]]
name = "noperiod"
exec = { kind = "command", program = "" }
"#;
        let p = PnCManifest::from_toml(bad).unwrap();
        let errs = p.validate();
        assert!(errs.iter().any(|e| e.contains("命名空间")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("program 为空")), "{errs:?}");
    }

    #[test]
    fn unknown_kind_and_risk_are_fail_closed() {
        let p = PnCManifest::from_toml("id=\"x\"\nversion=\"1.0.0\"\nkind=\"weird\"").unwrap();
        assert!(p.group_kind().is_err());
        assert_eq!(parse_risk(Some("bogus")), wpa_api::RiskTier::Critical);
        assert_eq!(parse_risk(None), wpa_api::RiskTier::High);
    }

    #[test]
    fn toml_schema_to_json() {
        let v: toml::Value = toml::from_str("type=\"object\"\nrequired=[\"a\"]").unwrap();
        let j = toml_to_json(&v);
        assert_eq!(j["type"], "object");
        assert_eq!(j["required"][0], "a");
    }
}
