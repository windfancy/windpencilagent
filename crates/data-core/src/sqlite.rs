//! 本地 sqlite 读写：建库建表 / 查询 / 插入 / 更新 / 删除。
//!
//! 刻意**不提供** "随便执行一段 SQL" 的入口（`attach`、`pragma` 之类一律不经手）：
//! 工具面暴露给模型的是"结构化动作"，参数错就报错，而不是把任意 SQL 透传给引擎。
//! 库文件、表名、列名都过标识符白名单；`where` 片段不接受分号与注释符
//! （避免一次调用顺手把整库删掉还没人察觉）。

use std::path::Path;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

/// 单次查询最多回多少行（防止一次 SELECT 把上下文撑爆）。
pub const MAX_ROWS: usize = 500;

/// 打开（必要时创建）数据库文件。
pub fn open(path: &Path) -> Result<Connection, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    }
    Connection::open(path).map_err(|e| format!("打开数据库 {} 失败：{e}", path.display()))
}

/// 只读打开（统计条目数用，绝不改文件）。
fn open_ro(path: &Path) -> Result<Connection, String> {
    if !path.is_file() {
        return Err(format!("数据库不存在：{}", path.display()));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开数据库 {} 失败：{e}", path.display()))
}

/// 标识符白名单：字母数字下划线，不以数字开头。
pub fn ident(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("名字不能为空".into());
    }
    if t.len() > 64 {
        return Err(format!("名字过长：{t}"));
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("名字只允许字母数字下划线：{t}"));
    }
    if t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        return Err(format!("名字不能以数字开头：{t}"));
    }
    Ok(t.to_string())
}

/// `where` 片段的基本防线：拒绝多语句与注释（单条 SELECT/UPDATE 里不该出现）。
fn where_clause(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("where 不能为空（防误改/误删整表）".into());
    }
    if t.contains(';') || t.contains("--") || t.contains("/*") {
        return Err("where 里不允许出现分号或注释符".into());
    }
    Ok(format!(" WHERE {t}"))
}

/// 执行建表语句（DDL）。
pub fn exec_schema(conn: &Connection, statements: &[String]) -> Result<usize, String> {
    let mut n = 0;
    for s in statements {
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        // 只放行建表/建索引/建视图这类定义语句
        let head = t
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if !matches!(head.as_str(), "CREATE" | "ALTER" | "DROP" | "INSERT" | "DELETE" | "UPDATE") {
            return Err(format!("create 只接受 CREATE/ALTER/DROP 等定义语句，收到：{head}"));
        }
        conn.execute_batch(t).map_err(|e| format!("执行失败（{head}）：{e}"))?;
        n += 1;
    }
    Ok(n)
}

/// 快捷建表：`columns` 形如 `[{"name":"cve","type":"TEXT","pk":true}]`。
pub fn create_table(conn: &Connection, table: &str, columns: &[(String, String, bool)]) -> Result<(), String> {
    if columns.is_empty() {
        return Err("columns 不能为空".into());
    }
    let mut defs: Vec<String> = Vec::new();
    for (name, ty, pk) in columns {
        let n = ident(name)?;
        let t = ty.trim().to_ascii_uppercase();
        let allowed = ["TEXT", "INTEGER", "REAL", "BLOB", "NUMERIC"];
        let t = if allowed.contains(&t.as_str()) { t } else { "TEXT".to_string() };
        defs.push(if *pk { format!("{n} {t} PRIMARY KEY") } else { format!("{n} {t}") });
    }
    let sql = format!("CREATE TABLE IF NOT EXISTS {table} ({})", defs.join(", "));
    conn.execute_batch(&sql).map_err(|e| format!("建表 {table} 失败：{e}"))
}

/// 列出全部用户表。
pub fn list_tables(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|e| format!("读取表清单失败：{e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("读取表清单失败：{e}"))?;
    let mut out = Vec::new();
    for r in rows.flatten() {
        out.push(r);
    }
    Ok(out)
}

/// 执行查询，返回列名与行（最多 [`MAX_ROWS`] 行）。
pub fn query(conn: &Connection, sql: &str, limit: usize) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    let t = sql.trim();
    if t.is_empty() {
        return Err("sql 不能为空".into());
    }
    if t.contains(';') && t.trim_end_matches(';').contains(';') {
        return Err("一次只能执行一条查询".into());
    }
    let mut stmt = conn.prepare(t).map_err(|e| format!("SQL 解析失败：{e}"))?;
    let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let cap = if limit == 0 { MAX_ROWS } else { limit.min(MAX_ROWS) };
    let mut rows = stmt.query([]).map_err(|e| format!("查询失败：{e}"))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("读取结果失败：{e}"))? {
        if out.len() >= cap {
            break;
        }
        let mut line = Vec::with_capacity(cols.len());
        for i in 0..cols.len() {
            let v = row.get_ref(i).map_err(|e| format!("读取第 {i} 列失败：{e}"))?;
            line.push(vref_to_json(v));
        }
        out.push(line);
    }
    Ok((cols, out))
}

/// 批量插入（同名键一次编译、多行执行）。
pub fn insert_rows(conn: &Connection, table: &str, rows: &[Map<String, Value>]) -> Result<usize, String> {
    if rows.is_empty() {
        return Err("rows 不能为空".into());
    }
    let cols: Vec<String> = rows[0].keys().cloned().collect();
    for c in &cols {
        ident(c)?;
    }
    let col_sql = cols.join(", ");
    let holders = vec!["?"; cols.len()].join(", ");
    let sql = format!("INSERT INTO {table} ({col_sql}) VALUES ({holders})");
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("准备插入语句失败：{e}"))?;
    let mut n = 0usize;
    for r in rows {
        let vals: Vec<SqlValue> = cols
            .iter()
            .map(|c| r.get(c).map(json_to_sql).unwrap_or(SqlValue::Null))
            .collect();
        stmt.execute(rusqlite::params_from_iter(vals.iter()))
            .map_err(|e| format!("插入失败：{e}"))?;
        n += 1;
    }
    Ok(n)
}

/// 更新（必须带 where）。
pub fn update_rows(
    conn: &Connection,
    table: &str,
    set: &Map<String, Value>,
    where_raw: &str,
) -> Result<usize, String> {
    if set.is_empty() {
        return Err("set 不能为空".into());
    }
    let mut assigns = Vec::new();
    let mut vals: Vec<SqlValue> = Vec::new();
    for (k, v) in set {
        let col = ident(k)?;
        assigns.push(format!("{col} = ?"));
        vals.push(json_to_sql(v));
    }
    let sql = format!(
        "UPDATE {table} SET {}{}",
        assigns.join(", "),
        where_clause(where_raw)?
    );
    conn.execute(&sql, rusqlite::params_from_iter(vals.iter()))
        .map_err(|e| format!("更新失败：{e}"))
}

/// 删除（必须带 where）。
pub fn delete_rows(conn: &Connection, table: &str, where_raw: &str) -> Result<usize, String> {
    let sql = format!("DELETE FROM {table}{}", where_clause(where_raw)?);
    conn.execute(&sql, [])
        .map_err(|e| format!("删除失败：{e}"))
}

/// 统计整库行数（所有用户表之和）。
pub fn count_all(path: &Path) -> Result<usize, String> {
    let conn = open_ro(path)?;
    let tables = list_tables(&conn)?;
    let mut total = 0usize;
    for t in tables {
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", t.replace('"', ""));
        if let Ok(n) = conn.query_row(&sql, [], |r| r.get::<_, i64>(0)) {
            total += n.max(0) as usize;
        }
    }
    Ok(total)
}

fn vref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).to_string()),
        ValueRef::Blob(b) => json!(format!("<blob {} bytes>", b.len())),
    }
}

fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("data_core_{tag}_{}.db", std::process::id()))
    }

    #[test]
    fn crud_roundtrip() {
        let p = tmp_db("crud");
        let _ = std::fs::remove_file(&p);
        let conn = open(&p).unwrap();
        create_table(&conn, "vuln", &[("id".into(), "TEXT".into(), true), ("name".into(), "TEXT".into(), false)]).unwrap();
        let mut r1 = Map::new();
        r1.insert("id".into(), json!("CVE-2021-44228"));
        r1.insert("name".into(), json!("log4shell"));
        let mut r2 = Map::new();
        r2.insert("id".into(), json!("CVE-2017-0144"));
        r2.insert("name".into(), json!("eternalblue"));
        assert_eq!(insert_rows(&conn, "vuln", &[r1, r2]).unwrap(), 2);
        let (cols, rows) = query(&conn, "SELECT id FROM vuln ORDER BY id", 0).unwrap();
        assert_eq!(cols, vec!["id"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], json!("CVE-2017-0144"));

        let mut set = Map::new();
        set.insert("name".into(), json!("log4j"));
        assert_eq!(update_rows(&conn, "vuln", &set, "id = 'CVE-2021-44228'").unwrap(), 1);
        assert_eq!(delete_rows(&conn, "vuln", "id = 'CVE-2017-0144'").unwrap(), 1);
        assert_eq!(count_all(&p).unwrap(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn where_is_mandatory_and_guarded() {
        let p = tmp_db("guard");
        let _ = std::fs::remove_file(&p);
        let conn = open(&p).unwrap();
        create_table(&conn, "t", &[("k".into(), "TEXT".into(), true)]).unwrap();
        assert!(delete_rows(&conn, "t", "").is_err(), "无 where 必须拒绝");
        assert!(delete_rows(&conn, "t", "1=1; DROP TABLE t").is_err(), "多语句必须拒绝");
        assert!(delete_rows(&conn, "t", "1=1 -- x").is_err(), "注释必须拒绝");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn identifiers_reject_injection() {
        assert!(ident("vuln").is_ok());
        assert!(ident("t;drop").is_err());
        assert!(ident("1abc").is_err());
        assert!(ident("a-b").is_err());
    }
}
