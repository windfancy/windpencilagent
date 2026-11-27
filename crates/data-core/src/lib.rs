//! 基座数据原语：本地 **sqlite** 与 **向量库** 的读写能力（能力组 `kb`）。
//!
//! 为什么放在独立 crate 而不是塞进 `toolkit-core`：
//! `rusqlite(bundled)` 会带进一份 C 源码与编译期构建步骤，只有需要数据库的
//! 装配路径才该付这个代价；工具层与存储实现分开，测试也不必拉起整套内核。
//!
//! 安全边界：
//! - 所有路径都相对 `data_root` 解析，**拒绝绝对路径与 `..`**（不能顺着参数爬出数据目录）；
//! - sqlite 的库名、表名、列名走标识符白名单校验；`where` 片段不接受分号；
//! - 向量库的向量**只从模型 embedding 来**，没有模型就报错——不做"假装向量"。

pub mod sqlite;
pub mod tools;
pub mod vector;

use std::path::{Component, Path, PathBuf};

/// 把相对路径解析到数据根目录下（越界即拒绝）。
pub fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("路径不能为空".into());
    }
    if rel.contains('\0') {
        return Err("路径包含非法字符".into());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(format!("只接受相对数据目录的路径：{rel}"));
    }
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return Err(format!("路径不能包含 `..` 或盘符：{rel}")),
        }
    }
    Ok(root.join(p))
}

/// 统计一个知识库的条目数（供领域页"知识管理"只读展示）。
///
/// `kind` 取值：`sqlite` / `vector` / `csv` / `file`。返回 `Err` 时由调用方
/// 原样展示原因（文件不存在、表结构坏了等）。
pub fn count_entries(kind: &str, path: &Path) -> Result<usize, String> {
    match kind {
        "sqlite" => sqlite::count_all(path),
        "vector" => Ok(vector::load(path)?.items.len()),
        "csv" => count_lines(path, 1),
        _ => count_lines(path, 0),
    }
}

fn count_lines(path: &Path, skip: usize) -> Result<usize, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    Ok(text.lines().filter(|l| !l.trim().is_empty()).count().saturating_sub(skip))
}

/// 知识库类型的中文名（界面展示用）。
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "sqlite" => "sqlite 数据",
        "vector" => "向量数据",
        "csv" => "csv 数据",
        _ => "文件数据",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_paths_escaping_root() {
        let root = Path::new("D:/data");
        assert!(resolve_under(root, "kb/a.db").is_ok());
        assert!(resolve_under(root, "../etc/passwd").is_err());
        assert!(resolve_under(root, "D:/x").is_err());
        assert!(resolve_under(root, "").is_err());
        assert!(resolve_under(root, "/abs").is_err());
    }

    #[test]
    fn labels_cover_known_kinds() {
        assert_eq!(kind_label("sqlite"), "sqlite 数据");
        assert_eq!(kind_label("weird"), "文件数据");
    }
}
