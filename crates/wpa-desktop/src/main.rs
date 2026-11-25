//! windpencilagent 桌面端入口。
//!
//! 两件事和"能不能当产品用"直接相关，都在这里做：
//! 1. **不弹黑窗**：`windows_subsystem = "windows"`（否则双击必开一个控制台）；
//! 2. **日志写文件**：既然没有控制台了，日志必须落到 `<数据目录>/logs/`，
//!    否则出问题时用户手里一片空白。按天滚动、按配置保留天数清理。

// 没有这一行，双击 exe 会先弹一个黑色控制台窗口（PE Subsystem = Console）。
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    // 缺省读 exe 同级的 config.toml（与 data/、domains/ 同处），
    // 这样"整体搬移目录即可迁移"对桌面端也成立；从任意目录启动都不受 CWD 影响。
    let mut config = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.toml")))
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            if let Some(p) = args.next() {
                config = PathBuf::from(p);
            }
        }
    }
    init_logging(&config);
    tracing::info!(config = %config.display(), "windpencilagent 启动");
    wpa_desktop::run(config)
}

/// 日志落盘：`<logs>/windpencilagent-YYYYMMDD.log`，append，按天分文件。
fn init_logging(cfg_path: &Path) {
    use tracing_subscriber::EnvFilter;

    // 配置读不出来也要能记日志：此时用 exe 同级 data/logs
    let cfg = wpa_engine::AppConfig::load(cfg_path).unwrap_or_default();
    let dir = cfg.logs_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    prune_old_logs(&dir, cfg.logging.keep_days);
    let path = dir.join(format!("windpencilagent-{}.log", today_ymd()));
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let level = cfg.logging.level.clone();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,windpencilagent={level}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .try_init();
}

/// 只保留最近 `keep_days` 个日志文件（文件名 YYYYMMDD 天然按字典序=时间序）。
fn prune_old_logs(dir: &Path, keep_days: u32) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("windpencilagent-") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    let keep = keep_days.max(1) as usize;
    while files.len() > keep {
        let oldest = files.remove(0);
        let _ = std::fs::remove_file(oldest);
    }
}

/// 本地日期 YYYYMMDD（不引 chrono：只为文件名，用 civil-from-days 反算即可）。
fn today_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 以 UTC 计（服务器/本机时区差异只影响跨零点的那一小时，日志文件名不值得引时区库）
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}")
}

/// Howard Hinnant 的 `civil_from_days`：天数 → (年, 月, 日)，纯整数运算。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
        assert_eq!(civil_from_days(20_067), (2024, 12, 10));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn ymd_is_eight_digits() {
        let s = today_ymd();
        assert_eq!(s.len(), 8, "{s}");
        assert!(s.chars().all(|c| c.is_ascii_digit()));
    }
}
