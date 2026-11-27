//! 知识读写组（`kb`）的 10 个工具：本地 sqlite 与向量库的增删改查。
//!
//! 设计口径：
//! - **结构化参数**，不透传任意 SQL/任意脚本；建表走 `schema` 或 `table + columns`。
//! - 更新/删除**必须**带 `where`——一次误操作删空整表是不可接受的失败模式。
//! - 向量写入/检索**必须**有 embedding 模型；没有就报「无法操作」并给出下一步。
//! - 所有路径相对数据目录（`data_root`），越界即拒绝。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Map, Value};

use crate::resolve_under;
use crate::sqlite;
use crate::vector::{self, Embedder, VecItem};
use toolkit_core::tools::{DynTool, Tool, ToolResult};

/// 无模型时的统一提示（向量相关工具共用）。
const NO_EMBEDDER: &str =
    "向量库依赖 embedding 模型：当前没有可用的模型档案（或该档案驱动不支持 embedding），无法进行向量写入/检索。\
     请先在「配置 · 模型」启用一个 OpenAI 兼容或 Gemini 档案后重试。";

/// 构造本组全部工具。
pub fn all(root: PathBuf, embedder: Option<Arc<dyn Embedder>>) -> Vec<DynTool> {
    let root = Arc::new(root);
    vec![
        Arc::new(KbSqliteCreate { root: root.clone() }),
        Arc::new(KbSqliteQuery { root: root.clone() }),
        Arc::new(KbSqliteInsert { root: root.clone() }),
        Arc::new(KbSqliteUpdate { root: root.clone() }),
        Arc::new(KbSqliteDelete { root: root.clone() }),
        Arc::new(KbVecCreate { root: root.clone() }),
        Arc::new(KbVecQuery { root: root.clone(), embedder: embedder.clone() }),
        Arc::new(KbVecInsert { root: root.clone(), embedder: embedder.clone() }),
        Arc::new(KbVecUpdate { root: root.clone(), embedder }),
        Arc::new(KbVecDelete { root }),
    ]
}

fn ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn db_path(root: &Path, args: &Value) -> Result<PathBuf, String> {
    let db = args.get("db").and_then(Value::as_str).unwrap_or("");
    if db.trim().is_empty() {
        return Err("缺少参数 db（相对数据目录的库文件，如 domain/secret_pentest/vuln.db）".into());
    }
    resolve_under(root, db)
}

fn store_path(root: &Path, args: &Value) -> Result<PathBuf, String> {
    let s = args.get("store").and_then(Value::as_str).unwrap_or("");
    vector::store_path(root, s)
}

async fn embed_all(eb: &Option<Arc<dyn Embedder>>, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let Some(e) = eb.as_ref() else {
        return Err(NO_EMBEDDER.to_string());
    };
    if texts.iter().all(|t| t.trim().is_empty()) {
        return Err("没有可嵌入的文本（text 不能全为空）".into());
    }
    e.embed(texts).await
}

fn table_arg(args: &Value) -> Result<String, String> {
    let t = args.get("table").and_then(Value::as_str).unwrap_or("");
    sqlite::ident(t)
}

fn rows_arg(args: &Value) -> Result<Vec<Map<String, Value>>, String> {
    let arr = args
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| "rows 必须是对象数组".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        let obj = r.as_object().ok_or_else(|| "rows 里每一项都必须是对象".to_string())?;
        out.push(obj.clone());
    }
    Ok(out)
}

fn set_arg(args: &Value) -> Result<Map<String, Value>, String> {
    args.get("set")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| "set 必须是对象（列名 → 新值）".to_string())
}

fn where_arg(args: &Value) -> Result<String, String> {
    args.get("where")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "缺少参数 where（如 id = 'CVE-2021-44228'）；不允许整表更新/删除".to_string())
}

// ---------------------------------------------------------------------------
// sqlite
// ---------------------------------------------------------------------------

struct KbSqliteCreate {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbSqliteCreate {
    fn name(&self) -> &str {
        "kb.sqlite.create"
    }
    fn description(&self) -> String {
        "创建/修改本地 sqlite 库（建表、建索引）。参数：db(相对数据目录的库文件)、\
         schema(字符串或字符串数组的 DDL) 或 table+columns([{name,type,pk}])。"
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["db"],
            "properties": {
                "db": { "type": "string", "description": "库文件，相对数据目录，如 domain/x/vuln.db" },
                "schema": { "description": "DDL 语句（字符串或字符串数组）" },
                "table": { "type": "string", "description": "快捷建表：表名" },
                "columns": {
                    "type": "array",
                    "description": "快捷建表的列定义",
                    "items": { "type": "object" }
                }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match db_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let conn = match sqlite::open(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let mut applied = 0usize;
        if let Some(v) = args.get("schema") {
            let stmts: Vec<String> = match v {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            if stmts.is_empty() {
                return ToolResult::error("schema 为空", ms(start));
            }
            match sqlite::exec_schema(&conn, &stmts) {
                Ok(n) => applied += n,
                Err(e) => return ToolResult::error(e, ms(start)),
            }
        }
        if let Some(t) = args.get("table").and_then(Value::as_str) {
            let table = match sqlite::ident(t) {
                Ok(t) => t,
                Err(e) => return ToolResult::error(e, ms(start)),
            };
            let cols: Vec<(String, String, bool)> = args
                .get("columns")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|c| {
                            (
                                c.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                                c.get("type").and_then(Value::as_str).unwrap_or("TEXT").to_string(),
                                c.get("pk").and_then(Value::as_bool).unwrap_or(false),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Err(e) = sqlite::create_table(&conn, &table, &cols) {
                return ToolResult::error(e, ms(start));
            }
            applied += 1;
        }
        if applied == 0 {
            return ToolResult::error("没有可执行的建库动作：请给 schema 或 table+columns", ms(start));
        }
        let tables = sqlite::list_tables(&conn).unwrap_or_default();
        ToolResult::json(
            format!("已执行 {applied} 条建库语句；现有表 {} 张：{}", tables.len(), tables.join("、")),
            json!({ "db": path.display().to_string(), "applied": applied, "tables": tables }),
            ms(start),
        )
    }
}

struct KbSqliteQuery {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbSqliteQuery {
    fn name(&self) -> &str {
        "kb.sqlite.query"
    }
    fn description(&self) -> String {
        "查询本地 sqlite 库（只读 SELECT）。参数：db、sql、limit(可选，最多 500 行)。\
         返回列名与行数据（JSON）。"
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["db", "sql"],
            "properties": {
                "db": { "type": "string" },
                "sql": { "type": "string", "description": "单条 SELECT" },
                "limit": { "type": "number", "description": "最多返回行数，默认 100" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }
    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match db_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        if !path.is_file() {
            return ToolResult::error(format!("数据库不存在：{}", path.display()), ms(start));
        }
        let sql_text = args.get("sql").and_then(Value::as_str).unwrap_or("");
        if !sql_text.trim_start().to_ascii_uppercase().starts_with("SELECT")
            && !sql_text.trim_start().to_ascii_uppercase().starts_with("WITH")
        {
            return ToolResult::error("只接受 SELECT / WITH 查询；写入请用 insert/update/delete", ms(start));
        }
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
        let conn = match sqlite::open(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        match sqlite::query(&conn, sql_text, limit) {
            Ok((cols, rows)) => {
                let summary = format!("查询返回 {} 行（列：{}）", rows.len(), cols.join(", "));
                ToolResult::json(
                    summary,
                    json!({
                        "columns": cols,
                        "rows": rows,
                        "count": rows.len(),
                    }),
                    ms(start),
                )
            }
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbSqliteInsert {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbSqliteInsert {
    fn name(&self) -> &str {
        "kb.sqlite.insert"
    }
    fn description(&self) -> String {
        "向本地 sqlite 表插入若干行。参数：db、table、rows(对象数组，键=列名)。表不存在时先 create。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["db", "table", "rows"],
            "properties": {
                "db": { "type": "string" },
                "table": { "type": "string" },
                "rows": { "type": "array", "items": { "type": "object" } }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match db_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let table = match table_arg(&args) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let rows = match rows_arg(&args) {
            Ok(r) => r,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let conn = match sqlite::open(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        match sqlite::insert_rows(&conn, &table, &rows) {
            Ok(n) => ToolResult::json(
                format!("已插入 {n} 行到 {table}"),
                json!({ "inserted": n, "table": table }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbSqliteUpdate {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbSqliteUpdate {
    fn name(&self) -> &str {
        "kb.sqlite.update"
    }
    fn description(&self) -> String {
        "更新本地 sqlite 表的行。参数：db、table、set(列名→新值)、where(必填，禁止整表更新)。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["db", "table", "set", "where"],
            "properties": {
                "db": { "type": "string" },
                "table": { "type": "string" },
                "set": { "type": "object" },
                "where": { "type": "string" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match db_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let table = match table_arg(&args) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let set = match set_arg(&args) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let wh = match where_arg(&args) {
            Ok(w) => w,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let conn = match sqlite::open(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        match sqlite::update_rows(&conn, &table, &set, &wh) {
            Ok(n) => ToolResult::json(
                format!("已更新 {n} 行（{table} where {wh}）"),
                json!({ "updated": n, "table": table }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbSqliteDelete {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbSqliteDelete {
    fn name(&self) -> &str {
        "kb.sqlite.delete"
    }
    fn description(&self) -> String {
        "删除本地 sqlite 表的行。参数：db、table、where(必填，禁止整表删除)。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["db", "table", "where"],
            "properties": {
                "db": { "type": "string" },
                "table": { "type": "string" },
                "where": { "type": "string" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match db_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let table = match table_arg(&args) {
            Ok(t) => t,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let wh = match where_arg(&args) {
            Ok(w) => w,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let conn = match sqlite::open(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        match sqlite::delete_rows(&conn, &table, &wh) {
            Ok(n) => ToolResult::json(
                format!("已删除 {n} 行（{table} where {wh}）"),
                json!({ "deleted": n, "table": table }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

// ---------------------------------------------------------------------------
// 向量库
// ---------------------------------------------------------------------------

struct KbVecCreate {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbVecCreate {
    fn name(&self) -> &str {
        "kb.vec.create"
    }
    fn description(&self) -> String {
        "新建本地向量库（JSON 文件，余弦检索）。参数：store(相对数据目录，自动补 .vec.json)、dims(可选，0=首条写入时确定)。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["store"],
            "properties": {
                "store": { "type": "string", "description": "如 domain/x/kb/vuln" },
                "dims": { "type": "number", "description": "向量维度，0 或省略=自动" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match store_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let dims = args.get("dims").and_then(Value::as_u64).unwrap_or(0) as usize;
        match vector::create(&path, dims) {
            Ok(_) => ToolResult::json(
                format!("向量库已创建：{}", path.display()),
                json!({ "store": path.display().to_string(), "dims": dims }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbVecInsert {
    root: Arc<PathBuf>,
    embedder: Option<Arc<dyn Embedder>>,
}

#[async_trait]
impl Tool for KbVecInsert {
    fn name(&self) -> &str {
        "kb.vec.insert"
    }
    fn description(&self) -> String {
        "向向量库写入条目（文本由模型 embedding 转成向量）。参数：store、items([{id?,text,meta?,vector?}])。缺 store 时自动新建。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["store", "items"],
            "properties": {
                "store": { "type": "string" },
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["text"],
                        "properties": {
                            "id": { "type": "string" },
                            "text": { "type": "string" },
                            "meta": { "type": "object" },
                            "vector": { "type": "array", "items": { "type": "number" } }
                        }
                    }
                }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match store_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let items = match args.get("items").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.clone(),
            _ => return ToolResult::error("items 不能为空", ms(start)),
        };
        let mut store = if path.is_file() {
            match vector::load(&path) {
                Ok(s) => s,
                Err(e) => return ToolResult::error(e, ms(start)),
            }
        } else {
            vector::VecStore::default()
        };

        // 只有没给 vector 的条目才需要 embedding
        let need: Vec<String> = items
            .iter()
            .filter(|it| it.get("vector").and_then(Value::as_array).is_none())
            .map(|it| it.get("text").and_then(Value::as_str).unwrap_or("").to_string())
            .collect();
        let mut embedded = if need.is_empty() {
            Vec::new()
        } else {
            match embed_all(&self.embedder, &need).await {
                Ok(v) => v,
                Err(e) => return ToolResult::error(e, ms(start)),
            }
        };
        if let Some(eb) = self.embedder.as_ref() {
            if store.model.is_empty() {
                store.model = eb.model();
            }
        }

        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let mut added = 0usize;
        for (i, it) in items.iter().enumerate() {
            let text = it.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            let vec: Vec<f32> = if let Some(arr) = it.get("vector").and_then(Value::as_array) {
                arr.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect()
            } else {
                embedded.remove(0)
            };
            if vec.is_empty() {
                return ToolResult::error("存在空向量（text 为空或 embedding 返回空）", ms(start));
            }
            if let Err(e) = vector::check_dims(&store, vec.len()) {
                return ToolResult::error(e, ms(start));
            }
            if store.dims == 0 {
                store.dims = vec.len();
            }
            let id = it
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{stamp}-{i}"));
            if store.items.iter().any(|x| x.id == id) {
                return ToolResult::error(format!("id 已存在：{id}（更新请用 kb.vec.update）"), ms(start));
            }
            store.items.push(VecItem {
                id,
                text,
                vector: vec,
                meta: it.get("meta").cloned().unwrap_or(Value::Null),
            });
            added += 1;
        }
        match vector::save(&path, &store) {
            Ok(()) => ToolResult::json(
                format!(
                    "已写入 {added} 条；库共 {} 条（{}，{} 维，模型 {}）",
                    store.items.len(),
                    display(&path),
                    store.dims,
                    if store.model.is_empty() { "未记录" } else { &store.model }
                ),
                json!({ "inserted": added, "total": store.items.len(), "dims": store.dims }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbVecUpdate {
    root: Arc<PathBuf>,
    embedder: Option<Arc<dyn Embedder>>,
}

#[async_trait]
impl Tool for KbVecUpdate {
    fn name(&self) -> &str {
        "kb.vec.update"
    }
    fn description(&self) -> String {
        "更新向量库条目（按 id）。参数：store、id、text?(改文本会重新 embedding)、vector?(直接给向量)、meta?。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["store", "id"],
            "properties": {
                "store": { "type": "string" },
                "id": { "type": "string" },
                "text": { "type": "string" },
                "vector": { "type": "array", "items": { "type": "number" } },
                "meta": { "type": "object" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match store_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let id = args.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if id.is_empty() {
            return ToolResult::error("缺少 id", ms(start));
        }
        let mut store = match vector::load(&path) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let Some(pos) = store.items.iter().position(|x| x.id == id) else {
            return ToolResult::error(format!("条目不存在：{id}"), ms(start));
        };

        let new_text = args.get("text").and_then(Value::as_str).map(str::to_string);
        let new_vec: Option<Vec<f32>> = args
            .get("vector")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect());
        let vector = match (new_vec, &new_text) {
            (Some(v), _) => v,
            (None, Some(t)) => match embed_all(&self.embedder, std::slice::from_ref(t)).await {
                Ok(mut v) => v.remove(0),
                Err(e) => return ToolResult::error(e, ms(start)),
            },
            (None, None) => store.items[pos].vector.clone(),
        };
        if let Err(e) = vector::check_dims(&store, vector.len()) {
            return ToolResult::error(e, ms(start));
        }
        if store.dims == 0 {
            store.dims = vector.len();
        }
        if let Some(t) = new_text {
            store.items[pos].text = t;
        }
        store.items[pos].vector = vector;
        if let Some(m) = args.get("meta") {
            store.items[pos].meta = m.clone();
        }
        match vector::save(&path, &store) {
            Ok(()) => ToolResult::json(
                format!("已更新条目 {id}；库共 {} 条", store.items.len()),
                json!({ "updated": 1, "total": store.items.len() }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbVecDelete {
    root: Arc<PathBuf>,
}

#[async_trait]
impl Tool for KbVecDelete {
    fn name(&self) -> &str {
        "kb.vec.delete"
    }
    fn description(&self) -> String {
        "删除向量库条目（按 id）。参数：store、id。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["store", "id"],
            "properties": {
                "store": { "type": "string" },
                "id": { "type": "string" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match store_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let id = args.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if id.is_empty() {
            return ToolResult::error("缺少 id", ms(start));
        }
        let mut store = match vector::load(&path) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let before = store.items.len();
        store.items.retain(|x| x.id != id);
        if store.items.len() == before {
            return ToolResult::error(format!("条目不存在：{id}"), ms(start));
        }
        match vector::save(&path, &store) {
            Ok(()) => ToolResult::json(
                format!("已删除条目 {id}；库剩 {} 条", store.items.len()),
                json!({ "deleted": 1, "total": store.items.len() }),
                ms(start),
            ),
            Err(e) => ToolResult::error(e, ms(start)),
        }
    }
}

struct KbVecQuery {
    root: Arc<PathBuf>,
    embedder: Option<Arc<dyn Embedder>>,
}

#[async_trait]
impl Tool for KbVecQuery {
    fn name(&self) -> &str {
        "kb.vec.query"
    }
    fn description(&self) -> String {
        "向量库语义检索（余弦相似度）。参数：store、query 或 text(检索语句，由模型 embedding)、\
         或 vector(直接给向量)、top_k(默认 5)。"
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["store"],
            "properties": {
                "store": { "type": "string" },
                "query": { "type": "string", "description": "检索语句" },
                "text": { "type": "string", "description": "query 的别名" },
                "vector": { "type": "array", "items": { "type": "number" } },
                "top_k": { "type": "number" }
            }
        })
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }
    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }
    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let path = match store_path(&self.root, &args) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        let store = match vector::load(&path) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(e, ms(start)),
        };
        if store.items.is_empty() {
            return ToolResult::json("向量库为空", json!({ "hits": [] }), ms(start));
        }
        let qvec: Option<Vec<f32>> = args
            .get("vector")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect());
        let qvec = match qvec {
            Some(v) if !v.is_empty() => v,
            _ => {
                let text = args
                    .get("query")
                    .or_else(|| args.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if text.trim().is_empty() {
                    return ToolResult::error("缺少 query（检索语句）或 vector", ms(start));
                }
                match embed_all(&self.embedder, std::slice::from_ref(&text)).await {
                    Ok(mut v) => v.remove(0),
                    Err(e) => return ToolResult::error(e, ms(start)),
                }
            }
        };
        if let Err(e) = vector::check_dims(&store, qvec.len()) {
            return ToolResult::error(e, ms(start));
        }
        let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5).clamp(1, 50) as usize;
        let hits = vector::search(&store, &qvec, top_k);
        let detail: Vec<Value> = hits
            .iter()
            .map(|(score, i)| {
                let it = &store.items[*i];
                json!({ "id": it.id, "score": score, "text": it.text, "meta": it.meta })
            })
            .collect();
        let mut summary = format!("命中 {} 条：\n", detail.len());
        for h in &detail {
            summary.push_str(&format!(
                "- [{:.3}] {}：{}\n",
                h["score"].as_f64().unwrap_or(0.0),
                h["id"].as_str().unwrap_or(""),
                clip(h["text"].as_str().unwrap_or(""), 120)
            ));
        }
        ToolResult::json(summary, json!({ "hits": detail }), ms(start))
    }
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

fn clip(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max { format!("{t}…") } else { t }
}
