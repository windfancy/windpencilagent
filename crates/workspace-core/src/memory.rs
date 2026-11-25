//! 空间记忆（WorkBuddy workspace memory 语义）：
//! - `YYYY-MM-DD.md` 每日工作日志，**append-only**（只追加不覆盖，自动建 `# 日期` 头）
//! - `MEMORY.md` 精炼长期笔记，**限额**（append 超限报错，提示人工精炼）
//!
//! 目录：`workspaces/<name>/data/memory/`。时间统一 UTC+8（部署环境时区）。

use std::path::{Path, PathBuf};

use thiserror::Error;

/// MEMORY.md 精炼笔记字符限额。
pub const CURATED_LIMIT: usize = 3000;

#[derive(Debug, Error)]
pub enum MemError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("精炼笔记超限：当前 {cur} 字符 + 追加 {add} > 限额 {limit}，请先人工精炼 MEMORY.md")]
    CuratedOverflow { cur: usize, add: usize, limit: usize },
    #[error("非法参数: {0}")]
    Invalid(String),
}

/// 记忆目录：root/<name>/data/memory/。
pub fn memory_dir(root: impl AsRef<Path>, name: &str) -> PathBuf {
    root.as_ref().join(name).join("data").join("memory")
}

/// 当前 UTC+8 日期（YYYY-MM-DD）。civil_from_days（Howard Hinnant 算法）。
pub fn today_date() -> String {
    let secs = now_secs();
    date_from_secs(secs)
}

/// 当前 UTC+8 时间（HH:MM）。
pub fn now_hhmm() -> String {
    let secs = now_secs();
    let day_secs = secs % 86400;
    format!("{:02}:{:02}", day_secs / 3600, (day_secs % 3600) / 60)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() + 8 * 3600) // UTC+8
        .unwrap_or(0)
}

/// 秒 → (Y, M, D)。反推自 days = floor(secs/86400)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn date_from_secs(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 校验日期参数格式（YYYY-MM-DD）。
fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..].iter().all(u8::is_ascii_digit)
}

/// 追加每日日志（append-only）。已存在文件直接追加；新文件先写 `# 日期` 头。
/// 每条自动带 `HH:MM` 时间戳。
pub fn append_daily(root: impl AsRef<Path>, name: &str, date: &str, text: &str) -> Result<String, MemError> {
    if !valid_date(date) {
        return Err(MemError::Invalid(format!("日期格式须为 YYYY-MM-DD: {date:?}")));
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(MemError::Invalid("日志内容为空".into()));
    }
    let dir = memory_dir(&root, name);
    std::fs::create_dir_all(&dir)?;
    let f = dir.join(format!("{date}.md"));
    let mut body = String::new();
    if !f.is_file() {
        body.push_str(&format!("# {date}\n"));
    }
    for line in text.lines() {
        body.push_str(&format!("- {} {}\n", now_hhmm(), line.trim()));
    }
    use std::io::Write;
    let mut fh = std::fs::OpenOptions::new().create(true).append(true).open(&f)?;
    fh.write_all(body.as_bytes())?;
    Ok(f.display().to_string())
}

/// 追加精炼长期笔记（MEMORY.md，限额 CURATED_LIMIT 字符）。
pub fn append_curated(root: impl AsRef<Path>, name: &str, text: &str) -> Result<String, MemError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(MemError::Invalid("笔记内容为空".into()));
    }
    let dir = memory_dir(&root, name);
    std::fs::create_dir_all(&dir)?;
    let f = dir.join("MEMORY.md");
    let cur_len = std::fs::read_to_string(&f)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let add = text.chars().count() + 1; // 换行
    if cur_len + add > CURATED_LIMIT {
        return Err(MemError::CuratedOverflow { cur: cur_len, add, limit: CURATED_LIMIT });
    }
    use std::io::Write;
    let mut fh = std::fs::OpenOptions::new().create(true).append(true).open(&f)?;
    fh.write_all(text.as_bytes())?;
    fh.write_all(b"\n")?;
    Ok(f.display().to_string())
}

/// 读取每日日志（date 缺省 = 今天）。
pub fn read_daily(root: impl AsRef<Path>, name: &str, date: &str) -> Result<String, MemError> {
    if !valid_date(date) {
        return Err(MemError::Invalid(format!("日期格式须为 YYYY-MM-DD: {date:?}")));
    }
    let f = memory_dir(&root, name).join(format!("{date}.md"));
    if !f.is_file() {
        return Ok(String::new());
    }
    Ok(std::fs::read_to_string(f)?)
}

/// 读取精炼笔记。
pub fn read_curated(root: impl AsRef<Path>, name: &str) -> Result<String, MemError> {
    let f = memory_dir(&root, name).join("MEMORY.md");
    if !f.is_file() {
        return Ok(String::new());
    }
    Ok(std::fs::read_to_string(f)?)
}

/// 列出已有日志日期（升序）。
pub fn list_dates(root: impl AsRef<Path>, name: &str) -> Vec<String> {
    let dir = memory_dir(&root, name);
    let mut dates: Vec<String> = std::fs::read_dir(&dir)
        .into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.len() == 13 && n.ends_with(".md") && n != "MEMORY.md" {
                Some(n[..10].to_string())
            } else {
                None
            }
        })
        .collect();
    dates.sort();
    dates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wpa_mem_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn civil_date_math() {
        // 1970-01-01
        assert_eq!(date_from_secs(0), "1970-01-01");
        // 2026-01-01 00:00 UTC+8（已偏移秒 = 2026-01-01T00:00Z + 8h）
        assert_eq!(date_from_secs(1767225600 + 8 * 3600), "2026-01-01");
        // 闰日 2024-02-29 00:00 UTC+8（= 2024-02-28T16:00Z，epoch 1709136000）
        assert_eq!(date_from_secs(1709136000 + 8 * 3600), "2024-02-29");
        // 偏移跨越日界：2025-12-31T16:00Z = 北京时间 2026-01-01 00:00
        assert_eq!(date_from_secs(1767196800 + 8 * 3600), "2026-01-01");
    }

    #[test]
    fn daily_append_only_and_header() {
        let root = tmp("daily");
        let d = today_date();
        let p1 = append_daily(&root, "ws1", &d, "完成扫描一轮").unwrap();
        let p2 = append_daily(&root, "ws1", &d, "发现 3 个开放端口").unwrap();
        assert_eq!(p1, p2, "同日同文件");
        let content = read_daily(&root, "ws1", &d).unwrap();
        assert!(content.starts_with(&format!("# {d}")));
        assert!(content.contains("完成扫描一轮"));
        assert!(content.contains("发现 3 个开放端口"));
        // 时间戳行
        assert!(content.lines().filter(|l| l.starts_with("- ")).count() == 2);
        // 重复 append 不覆盖（append-only）
        let before = content.chars().count();
        append_daily(&root, "ws1", &d, "第三条").unwrap();
        assert!(read_daily(&root, "ws1", &d).unwrap().chars().count() > before);
        // 非法日期
        assert!(append_daily(&root, "ws1", "bad", "x").is_err());
        // list_dates
        assert_eq!(list_dates(&root, "ws1"), vec![d]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn curated_limit_enforced() {
        let root = tmp("curated");
        let long_line = "甲".repeat(1500);
        append_curated(&root, "ws2", &long_line).unwrap();
        // 1500 + 1500 + 1 = 3001 > 3000 → 拒绝
        let r = append_curated(&root, "ws2", &long_line);
        assert!(matches!(r, Err(MemError::CuratedOverflow { .. })));
        // 精炼后（人为缩短）可继续追加
        let f = memory_dir(&root, "ws2").join("MEMORY.md");
        std::fs::write(&f, "short").unwrap();
        append_curated(&root, "ws2", "可追加").unwrap();
        assert!(read_curated(&root, "ws2").unwrap().contains("可追加"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_missing_empty() {
        let root = tmp("missing");
        assert_eq!(read_daily(&root, "nope", "2026-01-01").unwrap(), "");
        assert_eq!(read_curated(&root, "nope").unwrap(), "");
        assert!(list_dates(&root, "nope").is_empty());
        assert!(read_daily(&root, "nope", "bad").is_err());
    }
}
