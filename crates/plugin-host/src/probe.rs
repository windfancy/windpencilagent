//! 裸 dll 探测：**子进程**读内嵌清单，按 (路径, 大小, 修改时间) 缓存。
//!
//! 用户把 `xxx.dll` 直接拷到 `plugins/` 根目录时，宿主手上只有文件，没有清单。
//! 要让它"启动就能看见"，就得先读出 dll 自己声明的 id/风险/权限——而这需要
//! `dlopen`，也就意味着会在宿主进程里跑对方 `DllMain`。所以探测单独起进程
//! （`pnc-probe`），超时或崩溃只影响那一次探测。
//!
//! 缓存是必需的：每次启动都探几十个 dll，光进程启动就够呛；而 dll 一旦编译
//! 完基本不改，用 (大小, mtime) 就能可靠判定"还是同一个文件"。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::manifest::DllManifest;

/// 探测超时。编译器拷到一半、DllMain 死循环时不能让启动卡住。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 缓存文件名。放在插件根目录，`.` 开头 —— 扫描会跳过点开头的名字，不会当成组。
pub const CACHE_FILE: &str = ".probe-cache.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheItem {
    size: u64,
    mtime_ms: u128,
    /// 原样存内嵌清单 JSON：省掉「自定义结构 ↔ DllManifest」的字段同步负担。
    manifest: String,
}

/// 探测结果缓存。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProbeCache {
    #[serde(default)]
    items: BTreeMap<String, CacheItem>,
}

fn fingerprint(p: &Path) -> Option<(u64, u128)> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some((md.len(), mtime))
}

fn key_of(p: &Path) -> String {
    // 规范化不了（路径不存在等）就退回原样，缓存命不中只是多探一次
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .to_string()
}

impl ProbeCache {
    /// 从 `<root>/.probe-cache.json` 读；坏文件当空缓存（探测失败不该阻断启动）。
    pub fn load(root: &Path) -> Self {
        let p = root.join(CACHE_FILE);
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// 写回。失败只记日志：缓存丢了只是下次启动慢一点。
    pub fn save(&self, root: &Path) {
        let p = root.join(CACHE_FILE);
        match serde_json::to_string_pretty(self) {
            Ok(t) => {
                if let Err(e) = std::fs::write(&p, t) {
                    tracing::debug!(path = %p.display(), error = %e, "探测缓存写入失败");
                }
            }
            Err(e) => tracing::debug!(error = %e, "探测缓存序列化失败"),
        }
    }

    /// 命中则返回已解析清单；dll 被替换（大小/mtime 变了）自动失效。
    pub fn get(&self, dll: &Path) -> Option<DllManifest> {
        let (size, mtime) = fingerprint(dll)?;
        let it = self.items.get(&key_of(dll))?;
        if it.size != size || it.mtime_ms != mtime {
            return None;
        }
        DllManifest::parse(&it.manifest, plugin_abi::ABI_VERSION).ok()
    }

    pub fn put(&mut self, dll: &Path, raw: &str) -> Option<DllManifest> {
        let (size, mtime) = fingerprint(dll)?;
        let m = DllManifest::parse(raw, plugin_abi::ABI_VERSION).ok()?;
        self.items.insert(
            key_of(dll),
            CacheItem {
                size,
                mtime_ms: mtime,
                manifest: raw.to_string(),
            },
        );
        Some(m)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 找 `pnc-probe`：与**当前进程可执行文件**同目录。
///
/// 测试二进制在 `target/debug/deps/` 下，所以还要往上找两级；发布态两者同在
/// 一个目录，第一次就命中。
pub fn probe_exe() -> Option<PathBuf> {
    let name = if cfg!(windows) { "pnc-probe.exe" } else { "pnc-probe" };
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..3 {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// 起子进程读清单，返回原始 JSON。
pub fn probe_raw(dll: &Path, timeout: Duration) -> Result<String, String> {
    let exe = probe_exe().ok_or_else(|| {
        "找不到 pnc-probe（应与主程序同目录）；请重新构建，或在配置里关掉 plugins.auto_probe"
            .to_string()
    })?;
    let mut child = Command::new(&exe)
        .arg(dll)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动探测进程失败（{}）：{e}", exe.display()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(format!("等待探测进程失败：{e}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "探测超时（>{}s）：{}",
                timeout.as_secs(),
                dll.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("读取探测输出失败：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("探测失败（退出码 {:?}）", out.status.code())
        } else {
            err
        });
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err("探测进程没有输出清单".to_string());
    }
    Ok(text)
}

/// 带缓存的探测。
pub fn probe_cached(cache: &mut ProbeCache, dll: &Path, timeout: Duration) -> Result<DllManifest, String> {
    if let Some(m) = cache.get(dll) {
        return Ok(m);
    }
    let raw = probe_raw(dll, timeout)?;
    cache
        .put(dll, &raw)
        .ok_or_else(|| "探测输出不是合法清单".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wbp_probe_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cache_roundtrip_and_invalidation() {
        let root = tmp("cache");
        let dll = root.join("x.dll");
        std::fs::write(&dll, b"fake").unwrap();
        let raw = r#"{"abi":1,"id":"x","group":"g","version":"0.1.0","kind":"tool","risk":"low","permissions":[],"summary":"s","schema":{"type":"object"}}"#;

        let mut c = ProbeCache::default();
        let m = c.put(&dll, raw).expect("清单应可解析");
        assert_eq!(m.id, "x");
        assert!(c.get(&dll).is_some(), "同文件应命中");
        c.save(&root);

        let loaded = ProbeCache::load(&root);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get(&dll).is_some(), "落盘后应命中");

        // 改内容（大小变）⇒ 缓存失效
        std::fs::write(&dll, b"fake-and-longer").unwrap();
        assert!(loaded.get(&dll).is_none(), "dll 变了就必须重新探测");
    }

    #[test]
    fn bad_cache_file_is_ignored() {
        let root = tmp("badcache");
        std::fs::write(root.join(CACHE_FILE), "not json").unwrap();
        assert!(ProbeCache::load(&root).is_empty());
    }

    #[test]
    fn missing_dll_probes_with_clear_error() {
        let d = tmp("missing").join("nope.dll");
        let err = probe_raw(&d, Duration::from_millis(200)).unwrap_err();
        // 要么找不到探测程序，要么探测进程报告 dlopen 失败；都不能是 panic
        assert!(!err.is_empty(), "{err}");
    }
}
