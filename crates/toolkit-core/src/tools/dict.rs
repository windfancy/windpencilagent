//! 统一字典加载器（第 23 轮问题 11）：字典文件统一放程序同级 `dict/` 目录，
//! 按文件名前缀区分用途，**不再从知识库查询字典**。
//!
//! 前缀约定：
//! - `pwd_` 密码字典
//! - `user_` 用户名字典
//! - `dic_` 目录/路径字典
//! - `dns_` DNS 子域 / 虚拟主机字典
//!
//! 每行一个词条：UTF-8 优先，非法 UTF-8（如 GBK）用 lossy 解码；
//! 跳过空行与 `#` 注释行，去首尾空白，跨文件去重并保持首次出现顺序。

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// 把原始字节行按 UTF-8 优先、GBK lossy 兜底解码为字符串。
fn decode_line(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// 单一前缀最多加载的词条数（防止 100MB 级字典撑爆内存）。
/// 部署目录实测有 `pwd_100m.txt`（139MB / 千万级行），全量载入会占用 1GB+ 内存。
/// 可用环境变量 `WFM_DICT_MAX` 覆盖（如需要更大词表时临时调高）。
pub const DEFAULT_DICT_MAX: usize = 200_000;

fn dict_max() -> usize {
    std::env::var("WFM_DICT_MAX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_DICT_MAX)
}

/// 密码字典前缀。
pub const PWD_PREFIX: &str = "pwd_";
/// 用户名字典前缀。
pub const USER_PREFIX: &str = "user_";
/// 目录字典前缀。
pub const DIR_PREFIX: &str = "dic_";
/// DNS 子域 / 虚拟主机字典前缀。
pub const DNS_PREFIX: &str = "dns_";

/// 覆盖字典目录的环境变量（绝对路径或相对当前工作目录）。
pub const DICT_DIR_ENV: &str = "WFM_DICT_DIR";

/// 字典根目录：优先 `WFM_DICT_DIR`，否则 exe 同级 `dict/`（与数据落盘约定一致）。
pub fn dict_dir() -> PathBuf {
    if let Some(p) = std::env::var_os(DICT_DIR_ENV) {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    app_root().join("dict")
}

/// 程序根目录（与 `pysandbox::app_root` 同语义）。
fn app_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 扫描字典目录，加载所有文件名以 `prefix` 开头的文件的词条。
/// 目录不存在 / 无匹配文件 / 全部为空时返回空 Vec。
pub fn load_dict(prefix: &str) -> Vec<String> {
    load_dict_from(&dict_dir(), prefix)
}

/// 加载字典；字典目录无匹配内容时回退到内置默认表（保证无 dict/ 时行为不退化）。
pub fn load_dict_or(prefix: &str, fallback: &[&str]) -> Vec<String> {
    load_dict_or_from(&dict_dir(), prefix, fallback)
}

/// 显式目录版本（供测试使用，避免并发改动环境变量）。
fn load_dict_from(dir: &Path, prefix: &str) -> Vec<String> {
    load_dict_from_capped(dir, prefix, dict_max())
}

fn load_dict_from_capped(dir: &Path, prefix: &str, max: usize) -> Vec<String> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| e.path())
            .collect(),
        Err(_) => return Vec::new(),
    };
    // read_dir 顺序不保证；按文件名排序使结果稳定。
    files.sort();

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(max.min(4096));
    for path in files {
        // 流式逐行读，避免整文件载入内存（100MB 字典实测场景）。
        // UTF-8 非法字节（GBK）用 lossy 解码。
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(file);
        let mut buf = Vec::new();
        while let Ok(n) = reader.read_until(b'\n', &mut buf) {
            if n == 0 {
                break;
            }
            let line = decode_line(&buf);
            buf.clear();
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if seen.insert(line.to_string()) {
                out.push(line.to_string());
            }
            if out.len() >= max {
                return out; // 达到上限，立即停止（保留高频小字典优先：文件已按名排序）
            }
        }
    }
    out
}

/// 显式目录 + fallback 版本（供测试使用）。
fn load_dict_or_from(dir: &Path, prefix: &str, fallback: &[&str]) -> Vec<String> {
    let loaded = load_dict_from(dir, prefix);
    if loaded.is_empty() {
        fallback.iter().map(|s| s.to_string()).collect()
    } else {
        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDict(PathBuf);

    impl TempDict {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wfm_dict_test_{}_{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, content: &str) {
            std::fs::write(self.0.join(name), content).unwrap();
        }
    }

    impl Drop for TempDict {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prefixes_and_comments_and_dedup() {
        let d = TempDict::new();
        d.write("pwd_common.txt", "admin\n\n  # 注释行\n root \nadmin\n123456\n");
        d.write("pwd_extra.txt", "123456\nP@ssw0rd\n");
        d.write("dic_dirs.txt", "admin\nlogin\n");
        d.write("dns_subs.txt", "www\nmail\n");
        d.write("notes.txt", "not_a_dict\n");
        std::fs::create_dir_all(d.path().join("pwd_subdir.txt")).unwrap();

        assert_eq!(load_dict_from(d.path(), PWD_PREFIX), vec!["admin", "root", "123456", "P@ssw0rd"]);
        assert_eq!(load_dict_from(d.path(), USER_PREFIX), Vec::<String>::new());
        assert_eq!(load_dict_from(d.path(), DIR_PREFIX), vec!["admin", "login"]);
        assert_eq!(load_dict_from(d.path(), DNS_PREFIX), vec!["www", "mail"]);
    }

    #[test]
    fn fallback_when_no_matching_files() {
        let d = TempDict::new();
        d.write("unrelated.txt", "x\n");
        assert_eq!(load_dict_or_from(d.path(), PWD_PREFIX, &["admin", "123456"]), vec!["admin", "123456"]);
        d.write("pwd_custom.txt", "# only comments\n\n");
        // 匹配文件但内容为空 → 同样回退
        assert_eq!(load_dict_or_from(d.path(), PWD_PREFIX, &["fallback"]), vec!["fallback"]);
        d.write("pwd_custom.txt", "mypassword\n");
        assert_eq!(load_dict_or_from(d.path(), PWD_PREFIX, &["fallback"]), vec!["mypassword"]);
    }

    #[test]
    fn lossy_decode_for_non_utf8() {
        let d = TempDict::new();
        // GBK 编码的 "密码" + 一个正常词条
        let bytes = [0xc3, 0xdc, 0xc2, 0xeb, b'\n', b'a', b'd', b'm', b'i', b'n', b'\n'];
        std::fs::write(d.path().join("pwd_gbk.txt"), bytes).unwrap();
        let loaded = load_dict_from(d.path(), PWD_PREFIX);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1], "admin");
        assert!(loaded[0].contains('\u{fffd}'), "非法 UTF-8 应 lossy 解码为替换字符");
    }

    #[test]
    fn env_cap_limits_loaded_entries() {
        let d = TempDict::new();
        let content: String = (0..10_000).map(|i| format!("pwd{i}\n")).collect();
        d.write("pwd_big.txt", &content);
        let loaded = load_dict_from_capped(d.path(), PWD_PREFIX, 50);
        assert_eq!(loaded.len(), 50, "达到上限应立即停止读取");
        assert_eq!(loaded[0], "pwd0");
        assert_eq!(loaded[49], "pwd49");
    }
}
