//! 工作空间（workspace-core）：多空间隔离的任务环境，属基座能力，无业务语义。
//!
//! `workspace-manager` 组件 + 8 个 `ws.*` 工具：create/list/switch/remove/info/record/
//! memory.append/memory.read。每个空间 = exe 同级 `workspaces/<name>/` 独立目录（data/
//! 供记忆与统计）+ 权限集（read/write/network/exec）+ 授权范围 + LLM profile 引用 +
//! 启用技能 / 树包 / 工具白名单。空间定义持久化于 `workspaces/workspaces.toml`；
//! 会话 token 用量落 `workspaces/<name>/data/stats.json`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use kern_core::{Component, ComponentKind, ComponentStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use toolkit_core::tools::{Tool, ToolResult};

pub mod memory;

#[derive(Debug, Error)]
pub enum WsError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("序列化错误: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("非法参数: {0}")]
    Invalid(String),
}

/// 操作权限集：四类能力开关。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspacePerms {
    /// 允许读取文件 / 检索
    pub read: bool,
    /// 允许写入文件 / 落盘
    pub write: bool,
    /// 允许网络访问（扫描 / HTTP / DNS）
    pub network: bool,
    /// 允许执行命令 / 脚本
    pub exec: bool,
}

impl Default for WorkspacePerms {
    fn default() -> Self {
        // 缺省全开；由创建者按需收紧
        Self { read: true, write: true, network: true, exec: true }
    }
}

impl WorkspacePerms {
    fn parse(map: &Value) -> Self {
        Self {
            read: map.get("read").and_then(Value::as_bool).unwrap_or(true),
            write: map.get("write").and_then(Value::as_bool).unwrap_or(true),
            network: map.get("network").and_then(Value::as_bool).unwrap_or(true),
            exec: map.get("exec").and_then(Value::as_bool).unwrap_or(true),
        }
    }
}

/// 工作空间定义（v2：dsh Profile 装配语义）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    /// ScopeGuard 授权目标（空 = 继承全局 config）
    #[serde(default)]
    pub scope: Vec<String>,
    /// 显式拒绝目标
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub perms: WorkspacePerms,
    /// 关联 LLM profile 名（空 = 用全局 active）
    #[serde(default)]
    pub llm_profile: String,
    /// 启用技能名列表（空 = 用全局技能设置）
    #[serde(default)]
    pub skills: Vec<String>,
    /// 空间工作目录（dsh profile 目录语义：shell 初始 cwd / 文件工具默认落点；空 = 继承全局）
    #[serde(default)]
    pub cwd: String,
    /// 启用工具/组件 id 清单（Profile bundle 装配语义：非空时 executor 白名单过滤；空 = 全部注册）。
    /// 允许两种写法：**组 id**（展开为组内全部工具）与**工具名**（精确匹配）。
    #[serde(default)]
    pub plugins: Vec<String>,
    /// 该空间启用哪些树包 id（空 = 全部已启用树包）。构成"树包 → 空间 → 工具白名单"三级生效面。
    #[serde(default)]
    pub trees: Vec<String>,
    /// 简述
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_active: u64,
}

impl Workspace {
    /// 工具面白名单展开（B-3）：`plugins` 里的条目既可以是**组 id** 也可以是**工具名**。
    ///
    /// - `plugins` 为空 → 返回 `all_tools`（不设白名单）。
    /// - `group_tools` = 当前已挂载的「组 id → 组内工具名」，用于把组 id 展开成具体工具。
    /// - 无法识别（既不是已挂载组、也不是已知工具）的条目原样保留，由装配层给出警告
    ///   （不静默丢配置，也不擅自放行）。
    pub fn effective_tools(
        &self,
        group_tools: &BTreeMap<String, Vec<String>>,
        all_tools: &[String],
    ) -> Vec<String> {
        if self.plugins.is_empty() {
            return all_tools.to_vec();
        }
        let mut out: Vec<String> = Vec::new();
        for item in &self.plugins {
            let mut hit = false;
            if let Some(list) = group_tools.get(item) {
                for t in list {
                    if !out.contains(t) {
                        out.push(t.clone());
                    }
                }
                hit = true;
            }
            if !hit && all_tools.iter().any(|t| t == item) && !out.contains(item) {
                out.push(item.clone());
            }
            if !hit && !all_tools.iter().any(|t| t == item) {
                // 未识别条目：保留以便上层告警（装配层据此提示"空间引用了不存在的组/工具"）
                out.push(item.clone());
            }
        }
        out
    }

    /// 该空间是否启用某个树包（`trees` 为空 = 全部启用）。
    pub fn uses_tree(&self, tree_id: &str) -> bool {
        self.trees.is_empty() || self.trees.iter().any(|t| t == tree_id)
    }
}

/// 会话用量统计（token 速度 / 上下文使用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub sessions: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub elapsed_ms: u64,
}

impl UsageStats {
    /// 综合补全速度 tok/s。
    pub fn tok_per_sec(&self) -> f64 {
        if self.elapsed_ms == 0 { 0.0 } else {
            self.completion_tokens as f64 / (self.elapsed_ms as f64 / 1000.0)
        }
    }
}

/// 空间集合的持久化形态。
#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspaceStore {
    active: String,
    workspaces: BTreeMap<String, Workspace>,
}

/// 工作空间注册表：TOML 持久化 + 活跃空间运行时视图。
pub struct WorkspaceRegistry {
    root: PathBuf,
    inner: RwLock<WorkspaceStore>,
}

impl WorkspaceRegistry {
    /// 打开（或初始化）注册表。root = exe 同级 `workspaces/`。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, WsError> {
        let root = root.as_ref().to_path_buf();
        let file = root.join("workspaces.toml");
        let store: WorkspaceStore = if file.is_file() {
            let text = std::fs::read_to_string(&file)?;
            toml::from_str(&text)?
        } else {
            WorkspaceStore::default()
        };
        let reg = Self { root, inner: RwLock::new(store) };
        // 活跃空间目录自愈：确保存在
        if let Ok(active) = reg.active_name() {
            let _ = std::fs::create_dir_all(reg.workspace_dir(&active));
        }
        reg.save()?;
        Ok(reg)
    }

    fn save(&self) -> Result<(), WsError> {
        std::fs::create_dir_all(&self.root)?;
        let inner = self.inner.read().expect("lock");
        let text = toml::to_string(&*inner).map_err(|e| WsError::Invalid(format!("{e}")))?;
        std::fs::write(self.root.join("workspaces.toml"), text)?;
        Ok(())
    }

    pub fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn sanitize_name(name: &str) -> Result<(), WsError> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            && name != "." && name != "..";
        if ok { Ok(()) } else {
            Err(WsError::Invalid(format!("空间名只允许字母数字-_., 长度 1-64: {name:?}")))
        }
    }

    /// 空间目录：root/<name>/。
    pub fn workspace_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// 空间数据分库目录：root/<name>/data/。
    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.workspace_dir(name).join("data")
    }

    /// 空间记忆目录：root/<name>/data/memory/。
    pub fn memory_dir_of(&self, name: &str) -> PathBuf {
        memory::memory_dir(&self.root, name)
    }

    /// 读取空间精炼笔记 MEMORY.md（无文件或无空间返回空串）。
    pub fn read_memory_curated(&self, name: &str) -> String {
        memory::read_curated(&self.root, name).unwrap_or_default()
    }

    /// 读取空间某日工作日志（无文件返回空串）。
    pub fn read_memory_daily(&self, name: &str, date: &str) -> Result<String, WsError> {
        memory::read_daily(&self.root, name, date)
            .map_err(|e| WsError::Invalid(format!("读取日志失败: {e}")))
    }

    /// 追加空间每日日志（UI 快捷记录用；自动带 HH:MM 时间戳）。
    pub fn append_memory_daily(&self, name: &str, text: &str) -> Result<String, WsError> {
        memory::append_daily(&self.root, name, &memory::today_date(), text)
            .map_err(|e| WsError::Invalid(format!("写入日志失败: {e}")))
    }

    /// 追加空间精炼笔记（3000 字符限额）。
    pub fn append_memory_curated(&self, name: &str, text: &str) -> Result<String, WsError> {
        memory::append_curated(&self.root, name, text)
            .map_err(|e| WsError::Invalid(format!("写入精炼笔记失败: {e}")))
    }

    /// 空间每日日志日期列表（新的在前）。
    pub fn list_memory_dates(&self, name: &str) -> Vec<String> {
        let mut dates = memory::list_dates(&self.root, name);
        dates.sort();
        dates.reverse();
        dates
    }

    /// 活跃空间名（无空间时为空串）。
    pub fn active_name(&self) -> Result<String, WsError> {
        Ok(self.inner.read().expect("lock").active.clone())
    }

    /// 活跃空间定义（无空间时 None）。
    pub fn active(&self) -> Option<Workspace> {
        let inner = self.inner.read().expect("lock");
        inner.workspaces.get(&inner.active).cloned()
    }

    /// 创建空间（重名报错）。目录与 data/ 子目录立即创建。
    pub fn create(&self, mut w: Workspace) -> Result<Workspace, WsError> {
        Self::sanitize_name(&w.name)?;
        {
            let inner = self.inner.read().expect("lock");
            if inner.workspaces.contains_key(&w.name) {
                return Err(WsError::Invalid(format!("空间已存在: {}", w.name)));
            }
        }
        w.created_at = Self::now_secs();
        w.last_active = 0;
        std::fs::create_dir_all(self.data_dir(&w.name))?;
        self.inner.write().expect("lock").workspaces.insert(w.name.clone(), w.clone());
        self.save()?;
        Ok(w)
    }

    /// 删除空间（目录保留，定义移除；活跃空间被删则清空活跃态）。
    pub fn remove(&self, name: &str) -> Result<bool, WsError> {
        let removed = {
            let mut inner = self.inner.write().expect("lock");
            let r = inner.workspaces.remove(name).is_some();
            if r && inner.active == name { inner.active.clear(); }
            r
        };
        if removed { self.save()?; }
        Ok(removed)
    }

    /// 退出空间回到全局模式（仅清空活跃标记，空间定义与数据保留）。
    pub fn deactivate(&self, name: &str) -> Result<(), WsError> {
        let changed = {
            let mut inner = self.inner.write().expect("lock");
            if inner.active == name {
                inner.active.clear();
                true
            } else {
                false
            }
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// 切换活跃空间（更新 last_active）。
    pub fn switch(&self, name: &str) -> Result<Workspace, WsError> {
        let snapshot = {
            let mut inner = self.inner.write().expect("lock");
            if !inner.workspaces.contains_key(name) {
                return Err(WsError::Invalid(format!("空间不存在: {name}")));
            }
            inner.active = name.to_string();
            let w = inner.workspaces.get_mut(name).expect("checked");
            w.last_active = Self::now_secs();
            w.clone()
        };
        std::fs::create_dir_all(self.data_dir(name))?;
        self.save()?;
        Ok(snapshot)
    }

    /// 列出全部空间。
    pub fn list(&self) -> Vec<Workspace> {
        self.inner.read().expect("lock").workspaces.values().cloned().collect()
    }

    /// 空间详情。
    pub fn get(&self, name: &str) -> Option<Workspace> {
        self.inner.read().expect("lock").workspaces.get(name).cloned()
    }

    /// 读取空间用量统计（不存在则空表）。
    pub fn usage(&self, name: &str) -> UsageStats {
        let f = self.data_dir(name).join("stats.json");
        std::fs::read_to_string(f)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// 累加用量统计（活跃空间；无活跃空间则忽略）。
    pub fn record_usage(&self, prompt: u64, completion: u64, elapsed_ms: u64) {
        let active = self.active_name().unwrap_or_default();
        if active.is_empty() { return; }
        let dir = self.data_dir(&active);
        if std::fs::create_dir_all(&dir).is_err() { return; }
        let f = dir.join("stats.json");
        let mut s: UsageStats = std::fs::read_to_string(&f)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.sessions += 1;
        s.prompt_tokens += prompt;
        s.completion_tokens += completion;
        s.elapsed_ms += elapsed_ms;
        if let Ok(text) = serde_json::to_string_pretty(&s) {
            let _ = std::fs::write(f, text);
        }
    }
}

/// 组件：workspace-manager（kind=Plugin）。
pub struct WorkspaceComponent {
    pub registry: Arc<WorkspaceRegistry>,
}

impl Component for WorkspaceComponent {
    fn id(&self) -> &str { "workspace-manager" }
    fn kind(&self) -> ComponentKind { ComponentKind::Plugin }
    fn version(&self) -> &str { "1.0.0" }
    fn description(&self) -> String {
        "工作空间管理：多空间隔离目录/权限/LLM profile/技能集，token 用量与上下文统计".into()
    }
    fn status(&self) -> ComponentStatus { ComponentStatus::Loaded }
    fn as_any(&self) -> &dyn std::any::Any { self }
}

// ---- Tools（共享 registry 句柄；一切皆插件）----

fn ws_json(w: &Workspace, active: &str) -> Value {
    json!({
        "name": w.name, "active": w.name == active,
        "scope": w.scope, "deny": w.deny,
        "perms": { "read": w.perms.read, "write": w.perms.write,
                   "network": w.perms.network, "exec": w.perms.exec },
        "llm_profile": w.llm_profile, "skills": w.skills,
        "cwd": w.cwd, "plugins": w.plugins, "trees": w.trees,
        "note": w.note, "created_at": w.created_at, "last_active": w.last_active,
    })
}

/// ws.create：创建空间。
pub struct WsCreate(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsCreate {
    fn name(&self) -> &str { "ws.create" }
    fn description(&self) -> String {
        "创建工作空间（Profile 装配清单）：独立目录、Scope 范围、LLM profile、技能列表、空间工作目录 cwd、启用工具白名单 plugins（dsh 语义：非空时按清单装配，空=全部）。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["name"],"properties":{
            "name":{"type":"string"},
            "scope":{"type":"array","items":{"type":"string"}},
            "deny":{"type":"array","items":{"type":"string"}},
            "perms":{"type":"object"},
            "llm_profile":{"type":"string"},
            "skills":{"type":"array","items":{"type":"string"}},
            "cwd":{"type":"string","description":"空间工作目录"},
            "plugins":{"type":"array","items":{"type":"string"},"description":"启用工具白名单：可写组 id（展开组内全部工具）或工具名；空=全部"},
            "trees":{"type":"array","items":{"type":"string"},"description":"该空间启用的树包 id；空=全部已启用树包"},
            "note":{"type":"string"}
        }})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let name = args.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let perms = args.get("perms").map(WorkspacePerms::parse).unwrap_or_default();
        let w = Workspace {
            name,
            scope: args.get("scope").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            deny: args.get("deny").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            perms,
            llm_profile: args.get("llm_profile").and_then(Value::as_str).unwrap_or("").into(),
            skills: args.get("skills").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            cwd: args.get("cwd").and_then(Value::as_str).unwrap_or("").into(),
            plugins: args.get("plugins").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            trees: args.get("trees").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            note: args.get("note").and_then(Value::as_str).unwrap_or("").into(),
            created_at: 0, last_active: 0,
        };
        let reg = self.0.clone();
        let name2 = w.name.clone();
        match reg.create(w) {
            Ok(w) => ToolResult::json(
                format!("工作空间 `{}` 已创建（{}）", w.name, reg.workspace_dir(&w.name).display()),
                ws_json(&w, ""), start.elapsed().as_millis() as u64),
            Err(e) => ToolResult::error(format!("创建空间 `{name2}` 失败: {e}"), start.elapsed().as_millis() as u64),
        }
    }
}

/// ws.list：列出全部空间。
pub struct WsList(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsList {
    fn name(&self) -> &str { "ws.list" }
    fn description(&self) -> String { "列出全部工作空间（含活跃标记、权限、LLM profile、技能）。".into() }
    fn input_schema(&self) -> Value { json!({"type":"object","properties":{}}) }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, _args: Value) -> ToolResult {
        let start = Instant::now();
        let reg = self.0.clone();
        let active = reg.active_name().unwrap_or_default();
        let rows: Vec<Value> = reg.list().iter().map(|w| ws_json(w, &active)).collect();
        ToolResult::json(
            format!("共 {} 个工作空间，活跃: {}", rows.len(), if active.is_empty() { "（无）" } else { &active }),
            json!({"count": rows.len(), "active": active, "workspaces": rows}),
            start.elapsed().as_millis() as u64)
    }
}

/// ws.switch：切换活跃空间（下一会话生效）。
pub struct WsSwitch(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsSwitch {
    fn name(&self) -> &str { "ws.switch" }
    fn description(&self) -> String {
        "切换活跃工作空间：ScopeGuard、LLM profile、技能集与数据分库随空间生效（下一次会话装配）。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"}}})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let reg = self.0.clone();
        let name2 = name.to_string();
        match reg.switch(name) {
            Ok(w) => {
                let mut v = ws_json(&w, name);
                v["active"] = json!(true);
                ToolResult::json(
                    format!("已切换到工作空间 `{}`（Scope {} 条 · LLM {} · 技能 {} 个）",
                        w.name, w.scope.len(),
                        if w.llm_profile.is_empty() { "全局" } else { &w.llm_profile },
                        w.skills.len()),
                    v, start.elapsed().as_millis() as u64)
            }
            Err(e) => ToolResult::error(format!("切换空间 `{name2}` 失败: {e}"), start.elapsed().as_millis() as u64),
        }
    }
}

/// ws.remove：删除空间定义。
pub struct WsRemove(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsRemove {
    fn name(&self) -> &str { "ws.remove" }
    fn description(&self) -> String { "删除工作空间定义（目录与数据保留，可重建同名空间接回）。".into() }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"}}})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Medium }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let reg = self.0.clone();
        let name2 = name.to_string();
        match reg.remove(name) {
            Ok(true) => ToolResult::json(
                format!("空间 `{name2}` 已移除（目录保留）"), json!({"name": name2}),
                start.elapsed().as_millis() as u64),
            Ok(false) => ToolResult::error(format!("空间不存在: {name2}"), start.elapsed().as_millis() as u64),
            Err(e) => ToolResult::error(format!("移除失败: {e}"), start.elapsed().as_millis() as u64),
        }
    }
}

/// ws.info：空间详情 + 用量统计（token 速度 / 上下文累计）。
pub struct WsInfo(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsInfo {
    fn name(&self) -> &str { "ws.info" }
    fn description(&self) -> String {
        "工作空间详情：权限、Scope、LLM profile、技能、目录、token 用量（tok/s 与上下文累计）。name 缺省=活跃空间。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"name":{"type":"string"}}})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let reg = self.0.clone();
        let name = args.get("name").and_then(Value::as_str).map(String::from)
            .unwrap_or_else(|| reg.active_name().unwrap_or_default());
        if name.is_empty() {
            return ToolResult::error("未指定空间且无活跃空间", start.elapsed().as_millis() as u64);
        }
        let Some(w) = reg.get(&name) else {
            return ToolResult::error(format!("空间不存在: {name}"), start.elapsed().as_millis() as u64);
        };
        let u = reg.usage(&name);
        let mut v = ws_json(&w, &name);
        v["dir"] = json!(reg.workspace_dir(&name).display().to_string());
        v["usage"] = json!({
            "sessions": u.sessions,
            "prompt_tokens": u.prompt_tokens,
            "completion_tokens": u.completion_tokens,
            "tok_per_sec": (u.tok_per_sec() * 100.0).round() / 100.0,
            "elapsed_ms": u.elapsed_ms,
        });
        let mem_dates = memory::list_dates(reg.root.clone(), &name);
        v["memory"] = json!({
            "dir": memory::memory_dir(reg.root.clone(), &name).display().to_string(),
            "daily_dates": mem_dates,
            "curated_chars": memory::read_curated(reg.root.clone(), &name)
                .map(|c| c.chars().count()).unwrap_or(0),
            "curated_limit": memory::CURATED_LIMIT,
        });
        ToolResult::json(format!("工作空间 `{name}` 详情"), v, start.elapsed().as_millis() as u64)
    }
}

/// ws.stats：写回本轮会话 token 用量（供聊天引擎在完成时调用）。
pub struct WsRecord(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsRecord {
    fn name(&self) -> &str { "ws.record" }
    fn description(&self) -> String {
        "记录本轮会话 token 用量到活跃工作空间（prompt/completion tokens 与耗时 ms）。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["prompt_tokens","completion_tokens"],"properties":{
            "prompt_tokens":{"type":"number"},"completion_tokens":{"type":"number"},
            "elapsed_ms":{"type":"number"}
        }})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let p = args.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let c = args.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
        let ms = args.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0);
        let reg = self.0.clone();
        reg.record_usage(p, c, ms);
        let active = reg.active_name().unwrap_or_default();
        let u = reg.usage(&active);
        ToolResult::json(
            format!("用量已记录（活跃空间 {}，累计 {} tok，{:.1} tok/s）", active, u.completion_tokens, u.tok_per_sec()),
            json!({"active": active, "usage": {
                "sessions": u.sessions, "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens, "tok_per_sec": u.tok_per_sec(),
            }}),
            start.elapsed().as_millis() as u64)
    }
}

/// ws.memory.append：空间记忆写入（daily 日志 append-only / curated 精炼笔记限额）。
pub struct WsMemoryAppend(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsMemoryAppend {
    fn name(&self) -> &str { "ws.memory.append" }
    fn description(&self) -> String {
        "写入活跃工作空间记忆：kind=daily 追加每日工作日志（append-only，自动带时间戳）；kind=curated 追加精炼长期笔记（3000 字符限额，超限报错提示精炼）。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["text","kind"],"properties":{
            "text":{"type":"string","description":"要记录的内容（多行均可）"},
            "kind":{"type":"string","enum":["daily","curated"],"description":"daily=每日日志 curated=精炼笔记"},
            "date":{"type":"string","description":"daily 专用日期（YYYY-MM-DD），缺省=今天"}
        }})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = start.elapsed().as_millis() as u64;
        let text = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("daily");
        let active = self.0.active_name().unwrap_or_default();
        if active.is_empty() {
            return ToolResult::error("无活跃工作空间，记忆无处落盘（先 ws.switch）", ms);
        }
        let reg = self.0.clone();
        let res = match kind {
            "daily" => {
                let date = args.get("date").and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(memory::today_date);
                memory::append_daily(reg.root.clone(), &active, &date, &text)
            }
            "curated" => memory::append_curated(reg.root.clone(), &active, &text),
            other => return ToolResult::error(format!("非法 kind: {other}（daily/curated）"), ms),
        };
        match res {
            Ok(path) => ToolResult::json(
                format!("记忆已写入（{kind}）→ {path}"),
                json!({"kind": kind, "active": active, "path": path}),
                ms),
            Err(memory::MemError::CuratedOverflow { cur, add, limit }) => ToolResult::error(
                format!("精炼笔记超限：当前 {cur} + 追加 {add} > 限额 {limit}；请先人工精炼 MEMORY.md（保留跨会话长期有效的硬事实）"),
                ms),
            Err(e) => ToolResult::error(format!("记忆写入失败: {e}"), ms),
        }
    }
}

/// ws.memory.read：空间记忆读取。
pub struct WsMemoryRead(pub Arc<WorkspaceRegistry>);
#[async_trait]
impl Tool for WsMemoryRead {
    fn name(&self) -> &str { "ws.memory.read" }
    fn description(&self) -> String {
        "读取活跃工作空间记忆：kind=daily 读指定日期日志（缺省今天）、kind=curated 读精炼笔记、kind=list 列出全部日志日期。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["kind"],"properties":{
            "kind":{"type":"string","enum":["daily","curated","list"]},
            "date":{"type":"string","description":"daily 专用日期（YYYY-MM-DD），缺省=今天"}
        }})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = start.elapsed().as_millis() as u64;
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("daily");
        let active = self.0.active_name().unwrap_or_default();
        if active.is_empty() {
            return ToolResult::error("无活跃工作空间", ms);
        }
        let reg = self.0.clone();
        let res = match kind {
            "daily" => {
                let date = args.get("date").and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(memory::today_date);
                memory::read_daily(reg.root.clone(), &active, &date)
                    .map(|c| (format!("日志 {date}（{} 字符）", c.chars().count()), json!({"kind":"daily","date":date,"content":c})))
            }
            "curated" => memory::read_curated(reg.root.clone(), &active)
                .map(|c| (format!("精炼笔记（{} 字符 / 限额 {}）", c.chars().count(), memory::CURATED_LIMIT),
                    json!({"kind":"curated","content":c}))),
            "list" => {
                let dates = memory::list_dates(reg.root.clone(), &active);
                Ok((format!("共 {} 天日志", dates.len()), json!({"kind":"list","dates":dates})))
            }
            other => return ToolResult::error(format!("非法 kind: {other}（daily/curated/list）"), ms),
        };
        match res {
            Ok((summary, detail)) => ToolResult::json(summary, detail, ms),
            Err(e) => ToolResult::error(format!("记忆读取失败: {e}"), ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wpa_ws_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn create_switch_list_roundtrip() {
        let root = tmp_root("basic");
        let reg = WorkspaceRegistry::open(&root).unwrap();
        let w = reg.create(Workspace {
            name: "proj-a".into(),
            scope: vec!["10.0.0.0/24".into()],
            deny: vec!["10.0.0.1".into()],
            perms: WorkspacePerms { read: true, write: false, network: true, exec: false },
            llm_profile: "deepseek".into(),
            skills: vec!["recon-chain".into()],
            cwd: String::new(),
            plugins: vec![],
            trees: vec![],
            note: "A 项目授权段".into(),
            created_at: 0, last_active: 0,
        }).unwrap();
        assert_eq!(w.name, "proj-a");
        assert!(reg.workspace_dir("proj-a").join("data").is_dir());

        reg.switch("proj-a").unwrap();
        assert_eq!(reg.active_name().unwrap(), "proj-a");
        assert!(reg.active().unwrap().last_active > 0);
        // 退出空间回到全局（v3.7 GUI 工作区下拉的「全局」项）
        reg.deactivate("proj-a").unwrap();
        assert_eq!(reg.active_name().unwrap(), "");
        assert_eq!(reg.list().len(), 1, "退出空间不删定义");
        reg.switch("proj-a").unwrap();

        // 重名拒绝
        assert!(reg.create(Workspace { name: "proj-a".into(), ..Default::default() }).is_err());
        // 持久化往返
        let reg2 = WorkspaceRegistry::open(&root).unwrap();
        let loaded = reg2.get("proj-a").unwrap();
        assert!(!loaded.perms.write);
        assert_eq!(loaded.llm_profile, "deepseek");
        assert_eq!(reg2.active_name().unwrap(), "proj-a");
        assert_eq!(reg2.list().len(), 1);
    }

    #[test]
    fn name_sanitize() {
        assert!(WorkspaceRegistry::sanitize_name("a-b_c1").is_ok());
        assert!(WorkspaceRegistry::sanitize_name("").is_err());
        assert!(WorkspaceRegistry::sanitize_name("..").is_err());
        assert!(WorkspaceRegistry::sanitize_name("a/b").is_err());
        assert!(WorkspaceRegistry::sanitize_name("太长".repeat(40).as_str()).is_err());
    }

    #[test]
    fn remove_clears_active() {
        let root = tmp_root("remove");
        let reg = WorkspaceRegistry::open(&root).unwrap();
        reg.create(Workspace { name: "ws1".into(), ..Default::default() }).unwrap();
        reg.switch("ws1").unwrap();
        assert!(reg.remove("ws1").unwrap());
        assert_eq!(reg.active_name().unwrap(), "");
        assert!(!reg.remove("ws1").unwrap());
        // 目录仍在，可重建接回
        assert!(reg.workspace_dir("ws1").is_dir());
    }

    #[test]
    fn usage_accumulates() {
        let root = tmp_root("usage");
        let reg = Arc::new(WorkspaceRegistry::open(&root).unwrap());
        reg.create(Workspace { name: "u1".into(), ..Default::default() }).unwrap();
        reg.switch("u1").unwrap();
        reg.record_usage(100, 50, 2000);
        reg.record_usage(200, 150, 3000);
        let u = reg.usage("u1");
        assert_eq!(u.sessions, 2);
        assert_eq!(u.prompt_tokens, 300);
        assert_eq!(u.completion_tokens, 200);
        assert_eq!(u.elapsed_ms, 5000);
        assert!((u.tok_per_sec() - 40.0).abs() < 1e-9);
    }

    #[test]
    fn v2_profile_fields_persist() {
        let root = tmp_root("v2fields");
        let reg = WorkspaceRegistry::open(&root).unwrap();
        reg.create(Workspace {
            name: "prof-a".into(),
            cwd: "D:\\proj\\a".into(),
            plugins: vec!["scan.port".into(), "net.dns".into()],
            trees: vec!["pentest".into()],
            ..Default::default()
        }).unwrap();
        // 持久化往返（旧文件无新字段 → serde default 兼容）
        let reg2 = WorkspaceRegistry::open(&root).unwrap();
        let w = reg2.get("prof-a").unwrap();
        assert_eq!(w.cwd, "D:\\proj\\a");
        assert_eq!(w.plugins, vec!["scan.port", "net.dns"]);
        assert_eq!(w.trees, vec!["pentest"]);
        assert!(w.uses_tree("pentest"));
        assert!(!w.uses_tree("other"), "声明了 trees 则只认声明项");
    }

    #[test]
    fn effective_tools_expands_group_ids() {
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        groups.insert("tools.scan".into(), vec!["scan.port".into(), "scan.banner".into()]);
        let all = vec!["scan.port".into(), "scan.banner".into(), "file.read".into()];

        // 空白名单 → 全部
        let w = Workspace::default();
        assert_eq!(w.effective_tools(&groups, &all), all);

        // 组 id 展开 + 工具名精确命中 + 未识别项保留（上层告警）
        let w = Workspace {
            plugins: vec!["tools.scan".into(), "file.read".into(), "no.such".into()],
            ..Default::default()
        };
        assert_eq!(
            w.effective_tools(&groups, &all),
            vec!["scan.port", "scan.banner", "file.read", "no.such"]
        );

        // 去重
        let w = Workspace {
            plugins: vec!["scan.port".into(), "tools.scan".into()],
            ..Default::default()
        };
        assert_eq!(w.effective_tools(&groups, &all), vec!["scan.port", "scan.banner"]);

        // trees 为空 = 全部树包
        assert!(Workspace::default().uses_tree("anything"));
    }

    #[tokio::test]
    async fn memory_tools_roundtrip() {
        use toolkit_core::tools::Tool;
        let root = tmp_root("memtools");
        let reg = Arc::new(WorkspaceRegistry::open(&root).unwrap());
        reg.create(Workspace { name: "m1".into(), ..Default::default() }).unwrap();
        reg.switch("m1").unwrap();

        let ctx = tests_ctx();
        let app = WsMemoryAppend(reg.clone());
        let rd = WsMemoryRead(reg.clone());

        // 无活跃空间报错分支已被 switch 覆盖；daily 写入
        let r = app.call(&ctx, json!({"kind": "daily", "text": "完成第一轮扫描"})).await;
        assert!(r.ok, "{r:?}");
        // curated 写入与读取
        let r = app.call(&ctx, json!({"kind": "curated", "text": "目标段 10.0.0.0/24 已授权"})).await;
        assert!(r.ok);
        let r = rd.call(&ctx, json!({"kind": "curated"})).await;
        assert!(r.ok);
        assert!(r.detail.unwrap()["content"].as_str().unwrap().contains("已授权"));
        // list
        let r = rd.call(&ctx, json!({"kind": "list"})).await;
        assert_eq!(r.detail.unwrap()["dates"].as_array().unwrap().len(), 1);
        // 非法 kind
        assert!(!app.call(&ctx, json!({"kind": "bad", "text": "x"})).await.ok);
        // 无活跃空间（remove 后）
        reg.remove("m1").unwrap();
        let r = app.call(&ctx, json!({"kind": "daily", "text": "x"})).await;
        assert!(!r.ok);
    }

    #[test]
    fn tool_meta() {
        let root = tmp_root("meta");
        let reg = Arc::new(WorkspaceRegistry::open(&root).unwrap());
        let comp = WorkspaceComponent { registry: reg.clone() };
        assert_eq!(comp.id(), "workspace-manager");
        assert_eq!(comp.kind(), ComponentKind::Plugin);
        let names: Vec<String> = [
            WsCreate(reg.clone()).name().to_string(),
            WsList(reg.clone()).name().to_string(),
            WsSwitch(reg.clone()).name().to_string(),
            WsRemove(reg.clone()).name().to_string(),
            WsInfo(reg.clone()).name().to_string(),
            WsRecord(reg.clone()).name().to_string(),
            WsMemoryAppend(reg.clone()).name().to_string(),
            WsMemoryRead(reg).name().to_string(),
        ]
        .to_vec();
        assert_eq!(names, vec!["ws.create", "ws.list", "ws.switch", "ws.remove", "ws.info", "ws.record",
            "ws.memory.append", "ws.memory.read"]);
    }

    // 手工构造 ExecutionContext（kern-core 无 Default，与 feat-nuclei/feat-tasks 同款）
    fn tests_ctx() -> ExecutionContext {
        use kern_core::model::{AgentId, SessionId, TaskId};
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("a-test"),
            task_id: TaskId::new("t-test"),
            target: String::new(),
            risk: RiskLevel::Low,
            approved: true,
        }
    }
}
