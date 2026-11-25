//! 两级清单：组（`plugins/<group>/PnC.toml`）与插件（`plugins/<group>/<id>/PnC.toml`）。
//!
//! 插件目录里的 `PnC.toml` 是"加载前的静态信息"——宿主据此在**不 dlopen 的前提下**
//! 就能列出全部插件、算拓扑、做审批预判。dll 内嵌的 `wbp_manifest` 是权威副本，
//! 加载时两者做一致性校验（版本/风险/权限不一致 ⇒ 以 dll 为准并留痕）。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// 清单解析失败。
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("读取失败 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("TOML 解析失败 {path}：{source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("清单既不是 [group] 也不是 [plugin]：{0}")]
    UnknownKind(PathBuf),
    #[error("声明式清单无效 {0}：{1}")]
    Declarative(PathBuf, String),
    #[error("缺字段：{0}")]
    Missing(String),
}

// 清单错误带路径与 io::Error，Box 化收益不抵调用点改动；失败路径都不是热路径
#[allow(clippy::result_large_err)]
fn read_toml(path: &Path) -> Result<RawFile, ManifestError> {
    let text = std::fs::read_to_string(path).map_err(|e| ManifestError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str(&text).map_err(|e| ManifestError::Toml {
        path: path.to_path_buf(),
        source: e,
    })
}

#[derive(Debug, Deserialize)]
struct RawFile {
    group: Option<RawGroup>,
    plugin: Option<RawPlugin>,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default = "one")]
    version: String,
    #[serde(default = "tools_kind")]
    kind: String,
    #[serde(default = "fifty")]
    priority: i32,
    #[serde(default)]
    description: String,
    #[serde(default)]
    requires_groups: Vec<String>,
    #[serde(default)]
    plugins: Vec<String>,
    #[serde(default = "yes")]
    default_enabled: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// 组级默认风险（成员未声明或声明更低时按此兜底）。
    #[serde(default)]
    default_risk: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPlugin {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default = "one")]
    version: String,
    #[serde(default)]
    group: String,
    #[serde(default = "low")]
    risk: String,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    binary: String,
    #[serde(default)]
    summary: String,
    #[serde(default = "yes")]
    default_enabled: bool,
    #[serde(default)]
    tags: Vec<String>,
    /// 能力标签（P0-9）。未声明时调用方回退 `tags`——老清单不用改就能继续用。
    #[serde(default)]
    capabilities: Vec<String>,
}

fn one() -> String {
    "0.1.0".into()
}
fn low() -> String {
    "low".into()
}
fn yes() -> bool {
    true
}
fn fifty() -> i32 {
    50
}
fn tools_kind() -> String {
    "tools".into()
}

/// 组清单。
#[derive(Debug, Clone)]
pub struct GroupManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub priority: i32,
    pub description: String,
    pub requires_groups: Vec<String>,
    /// 非空 = 显式引用（可跨组）；空 = 扫本目录。
    pub plugins: Vec<String>,
    pub default_enabled: bool,
    pub tags: Vec<String>,
    pub default_risk: Option<String>,
    /// 组目录。
    pub dir: PathBuf,
}

/// 插件清单（静态）。
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    /// 归属组（由所在目录决定，缺省即目录名）。
    pub group: String,
    pub risk: String,
    pub permissions: Vec<String>,
    /// dll 文件名（缺省 `<id>.dll`）。
    pub binary: String,
    pub summary: String,
    pub default_enabled: bool,
    pub tags: Vec<String>,
    /// 能力标签；空时调用方回退 `tags`（见 `wpa-engine::base::scan_snapshot`）。
    pub capabilities: Vec<String>,
    pub dir: PathBuf,
    /// 声明式清单（`PnC.toml` 带 `[[tool]]`、无 dll）。None = 常规 dll 插件。
    pub declarative: Option<tree_core::pnc::PnCManifest>,
}

impl PluginManifest {
    pub fn dll_path(&self) -> PathBuf {
        self.dir.join(&self.binary)
    }

    pub fn has_binary(&self) -> bool {
        if self.declarative.is_some() {
            return true; // 声明式没有二进制，清单 + exec 就是实现
        }
        self.dll_path().is_file()
    }
}

/// 从组目录读组清单。目录名与 `id` 不一致时以清单为准并告警（由调用方收集）。
#[allow(clippy::result_large_err)]
pub fn load_group(dir: &Path) -> Result<GroupManifest, ManifestError> {
    let path = dir.join("PnC.toml");
    let raw = read_toml(&path)?;
    let Some(g) = raw.group else {
        return Err(ManifestError::UnknownKind(path));
    };
    if g.id.trim().is_empty() {
        return Err(ManifestError::Missing("group.id".into()));
    }
    Ok(GroupManifest {
        id: g.id,
        name: if g.name.is_empty() {
            dir.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            g.name
        },
        version: g.version,
        kind: g.kind,
        priority: g.priority,
        description: g.description,
        requires_groups: g.requires_groups,
        plugins: g.plugins,
        default_enabled: g.default_enabled,
        tags: g.tags,
        default_risk: g.default_risk,
        dir: dir.to_path_buf(),
    })
}

/// 从插件目录读插件清单。`group_dir_name` 用于补缺 `group` 字段。
#[allow(clippy::result_large_err)]
pub fn load_plugin(dir: &Path, group_dir_name: &str) -> Result<PluginManifest, ManifestError> {
    let path = dir.join("PnC.toml");
    let raw = read_toml(&path)?;
    let Some(p) = raw.plugin else {
        return Err(ManifestError::UnknownKind(path));
    };
    if p.id.trim().is_empty() {
        return Err(ManifestError::Missing("plugin.id".into()));
    }
    let group = if p.group.is_empty() {
        group_dir_name.to_string()
    } else {
        p.group
    };
    Ok(PluginManifest {
        binary: if p.binary.is_empty() {
            format!("{}.dll", p.id)
        } else {
            p.binary
        },
        name: if p.name.is_empty() {
            p.id.clone()
        } else {
            p.name
        },
        id: p.id,
        version: p.version,
        group,
        risk: p.risk.to_ascii_lowercase(),
        permissions: p.permissions,
        summary: p.summary,
        default_enabled: p.default_enabled,
        tags: p.tags,
        capabilities: p.capabilities,
        dir: dir.to_path_buf(),
        declarative: None,
    })
}

/// 声明式插件：`PnC.toml` 不带 `[plugin]` 段、但含非空 `[[tool]]` 表。
///
/// 与 dll 插件走同一条扫描/注册表/工具面链路，只是"实现"不是二进制，
/// 而是 `tree_core::exec` 的四路执行（http / command / python / primitive）。
/// 风险缺省 high（与 `ToolDecl::risk` 的 fail-closed 缺省一致）。
pub fn load_declarative(dir: &Path, group_dir_name: &str) -> Result<PluginManifest, ManifestError> {
    let pnc = tree_core::pnc::PnCManifest::load(dir)
        .map_err(|e| ManifestError::Declarative(dir.join("PnC.toml"), e))?;
    if pnc.tool.is_empty() {
        return Err(ManifestError::UnknownKind(dir.join("PnC.toml")));
    }
    Ok(PluginManifest {
        binary: String::new(),
        name: if pnc.name.is_empty() {
            pnc.id.clone()
        } else {
            pnc.name.clone()
        },
        id: pnc.id.clone(),
        version: pnc.version.clone(),
        group: group_dir_name.to_string(),
        risk: "high".to_string(),
        permissions: vec![],
        summary: pnc.description.clone(),
        default_enabled: false,
        tags: pnc.capabilities.tags.clone(),
        capabilities: pnc.capabilities.tags.clone(),
        dir: dir.to_path_buf(),
        declarative: Some(pnc),
    })
}

/// dll 内嵌清单（`wbp_manifest` 的 JSON）。
#[derive(Debug, Clone)]
pub struct DllManifest {
    pub abi: u32,
    pub id: String,
    pub group: String,
    pub version: String,
    pub kind: String,
    pub risk: String,
    pub permissions: Vec<String>,
    pub summary: String,
    pub schema: Value,
}

impl DllManifest {
    /// 解析并校验必备键。`expected_abi` 不匹配时返回原因。
    pub fn parse(json: &str, expected_abi: u32) -> Result<Self, String> {
        let v: Value =
            serde_json::from_str(json).map_err(|e| format!("manifest 不是合法 JSON：{e}"))?;
        let abi = v.get("abi").and_then(Value::as_u64).unwrap_or(0) as u32;
        if abi != expected_abi {
            return Err(format!(
                "ABI 版本不匹配（插件 {abi}，宿主 {expected_abi}）；请用当前 SDK 重新编译插件"
            ));
        }
        let need = |k: &str| -> Result<String, String> {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("manifest 缺字段 `{k}`"))
        };
        let id = need("id")?;
        if id.trim().is_empty() {
            return Err("manifest 的 id 为空".into());
        }
        let schema = v
            .get("schema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
        Ok(Self {
            abi,
            id,
            group: v
                .get("group")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            version: v
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("0.1.0")
                .to_string(),
            kind: v
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            risk: v
                .get("risk")
                .and_then(Value::as_str)
                .unwrap_or("low")
                .to_ascii_lowercase(),
            permissions: v
                .get("permissions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            summary: v
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            schema,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wbp_manifest_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn group_manifest_roundtrip() {
        let d = tmpdir("group");
        std::fs::write(
            d.join("PnC.toml"),
            r#"
[group]
id = "asset_discovery"
name = "资产发现"
version = "1.0.0"
kind = "tools"
priority = 10
description = "摸清攻击面"
requires_groups = []
plugins = []
tags = ["recon"]
"#,
        )
        .unwrap();
        let g = load_group(&d).unwrap();
        assert_eq!(g.id, "asset_discovery");
        assert_eq!(g.name, "资产发现");
        assert_eq!(g.priority, 10);
        assert!(g.plugins.is_empty());
        assert!(g.default_enabled);
    }

    #[test]
    fn plugin_manifest_defaults() {
        let d = tmpdir("plugin");
        std::fs::write(
            d.join("PnC.toml"),
            r#"
[plugin]
id = "response_diff"
risk = "LOW"
permissions = ["http"]
"#,
        )
        .unwrap();
        let p = load_plugin(&d, "vuln_verify").unwrap();
        assert_eq!(p.group, "vuln_verify");
        assert_eq!(p.binary, "response_diff.dll");
        assert_eq!(p.risk, "low");
        assert_eq!(p.version, "0.1.0");
        assert!(p.name.contains("response_diff"));
        assert!(!p.has_binary());
    }

    #[test]
    fn unknown_kind_rejected() {
        let d = tmpdir("bad");
        std::fs::write(d.join("PnC.toml"), "[other]\nx = 1\n").unwrap();
        assert!(matches!(
            load_group(&d),
            Err(ManifestError::UnknownKind(_))
        ));
    }

    #[test]
    fn dll_manifest_abi_gate() {
        let ok = r#"{"abi":1,"id":"x","group":"g","version":"0.1.0","kind":"tool","risk":"low","permissions":["http"],"summary":"s","schema":{"type":"object"}}"#;
        let m = DllManifest::parse(ok, 1).unwrap();
        assert_eq!(m.id, "x");
        assert_eq!(m.permissions, vec!["http"]);
        assert!(DllManifest::parse(ok, 2).unwrap_err().contains("ABI"));
        assert!(DllManifest::parse(r#"{"abi":1,"id":""}"#, 1).is_err());
    }
}
