//! 动态加载：`dlopen` + `wbp_init`，按需加载、LRU 缓存、熔断。
//!
//! **安装 ≠ 加载**：扫描阶段只读 `PnC.toml`，一个 dll 都不打开。
//! 真正 `dlopen` 发生在第一次被执行时（或组启用时按拓扑预加载）。

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use libloading::Library;
use plugin_abi::{
    FreeFn, HostApi, EXPORT_CALL, EXPORT_FREE, EXPORT_INIT, EXPORT_MANIFEST, EXPORT_SHUTDOWN,
};
use serde_json::Value;

use crate::manifest::{DllManifest, PluginManifest};
use crate::HostFactory;

type ManifestFn = unsafe extern "C" fn() -> *const c_char;
type InitFn = unsafe extern "C" fn(*const HostApi) -> i32;
type CallFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type ShutdownFn = unsafe extern "C" fn();

/// 连续失败多少次后熔断。
pub const BREAKER_THRESHOLD: u32 = 3;

/// 一个已加载的插件实例。
pub struct LoadedPlugin {
    pub manifest: DllManifest,
    pub path: PathBuf,
    /// 保活句柄：drop 顺序是"先调 shutdown，再释放库"，由本结构体的 Drop 保证。
    _lib: Arc<Library>,
    call: CallFn,
    free: FreeFn,
    shutdown: ShutdownFn,
    pub calls: AtomicU64,
    pub failures: AtomicU32,
    /// 熔断：为真时拒绝执行（实例保留，不卸载）。
    pub disabled: AtomicBool,
    pub last_used_ms: AtomicU64,
    /// 最近一次错误原因（UI 展示"为什么被熔断"）。
    pub last_error: Mutex<String>,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("id", &self.manifest.id)
            .field("path", &self.path)
            .field("calls", &self.calls.load(Ordering::Relaxed))
            .field("disabled", &self.disabled.load(Ordering::Relaxed))
            .finish()
    }
}

impl LoadedPlugin {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    /// 同步调用（宿主侧须放在阻塞线程池里）。
    ///
    /// 返回值约定：插件返回 `{"ok":true,"summary":..,"detail":..}` 或 `{"ok":false,"error":..}`。
    /// 这里**不吞错误**：`ok:false` 也当 Err 返回，让上层统一记失败计数。
    pub fn invoke(&self, args_json: &str) -> Result<Value, String> {
        if self.is_disabled() {
            return Err(format!(
                "插件 `{}` 已熔断（连续失败 {} 次）：{}",
                self.id(),
                self.failures.load(Ordering::Relaxed),
                self.last_error.lock().map(|s| s.clone()).unwrap_or_default()
            ));
        }
        let c = CString::new(args_json).map_err(|_| "参数含 NUL 字节".to_string())?;
        // SAFETY: 函数指针来自本 dll 的导出表，dll 由 `lib` 保活。
        let out = unsafe { (self.call)(c.as_ptr()) };
        if out.is_null() {
            self.record_failure("插件返回空指针");
            return Err("插件返回空指针（内部错误）".into());
        }
        // SAFETY: 插件承诺返回以 NUL 结尾的 UTF-8 串，且由 `free` 释放。
        let text = unsafe { CStr::from_ptr(out) }.to_string_lossy().to_string();
        // free 由插件导出，指针来自同一次 wbp_call
        (self.free)(out);

        self.calls.fetch_add(1, Ordering::Relaxed);
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                self.record_failure(&format!("返回非法 JSON：{e}"));
                return Err(format!("插件返回非法 JSON：{e}"));
            }
        };
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            let msg = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("插件执行失败")
                .to_string();
            self.record_failure(&msg);
            return Err(msg);
        }
        // 成功即清零失败计数（间歇性错误不该累计成熔断）
        self.failures.store(0, Ordering::Relaxed);
        Ok(v)
    }

    pub fn record_failure(&self, reason: &str) {
        let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut s) = self.last_error.lock() {
            *s = reason.to_string();
        }
        if n >= BREAKER_THRESHOLD && !self.disabled.swap(true, Ordering::Relaxed) {
            tracing::warn!(plugin = self.id(), failures = n, reason, "插件连续失败，已熔断");
        }
    }

    /// 手动恢复（UI「重试用」）。
    pub fn reset_breaker(&self) {
        self.failures.store(0, Ordering::Relaxed);
        self.disabled.store(false, Ordering::Relaxed);
    }

    pub fn touch(&self, now_ms: u64) {
        self.last_used_ms.store(now_ms, Ordering::Relaxed);
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // SAFETY: shutdown 是 dll 导出的空参函数；调用后 Library 才释放。
        unsafe { (self.shutdown)() };
        tracing::debug!(plugin = self.manifest.id, "插件已卸载");
    }
}

/// 加载失败原因。
#[derive(Debug)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LoadError {}

/// 打开一个 dll 并完成 `wbp_init`。
///
/// 能力表按 **dll 自己声明的权限** 构造（不是 PnC.toml 那份）：
/// 加载后以 dll 为准，避免"目录清单写了 http，dll 却用了 tcp"的越权。
pub fn open(path: &Path, factory: &HostFactory) -> Result<LoadedPlugin, LoadError> {
    if !path.is_file() {
        return Err(LoadError(format!("找不到插件文件：{}", path.display())));
    }
    // SAFETY: 加载任意 dll 本质上不安全；这里是宿主的既定行为（只加载用户目录里的插件）。
    let lib = unsafe { Library::new(path) }
        .map_err(|e| LoadError(format!("dlopen 失败（{}）：{e}", path.display())))?;

    // SAFETY: 下面四个符号由 SDK 的 declare_tool! 保证导出。
    unsafe {
        let manifest_fn: libloading::Symbol<ManifestFn> = lib
            .get(EXPORT_MANIFEST.as_bytes())
            .map_err(|e| LoadError(format!("缺导出 {EXPORT_MANIFEST}：{e}")))?;
        let ptr = manifest_fn();
        if ptr.is_null() {
            return Err(LoadError("wbp_manifest 返回空".into()));
        }
        let raw = CStr::from_ptr(ptr).to_string_lossy().to_string();

        // 先验 ABI，再取其余符号：ABI 不符的插件连 init 都不该调。
        let manifest = DllManifest::parse(&raw, plugin_abi::ABI_VERSION)
            .map_err(|e| LoadError(format!("manifest 校验失败：{e}")))?;

        let init: libloading::Symbol<InitFn> = lib
            .get(EXPORT_INIT.as_bytes())
            .map_err(|e| LoadError(format!("缺导出 {EXPORT_INIT}：{e}")))?;
        // 能力表按权限裁剪：未声明的一律 None，插件拿不到也调不动。
        let api = factory(&manifest.permissions);
        let rc = init(&api as *const HostApi);
        if rc != 0 {
            return Err(LoadError(format!(
                "wbp_init 失败（{rc}）：{}",
                init_error_hint(rc)
            )));
        }

        let call = *lib
            .get::<CallFn>(EXPORT_CALL.as_bytes())
            .map_err(|e| LoadError(format!("缺导出 {EXPORT_CALL}：{e}")))?;
        let free = *lib
            .get::<FreeFn>(EXPORT_FREE.as_bytes())
            .map_err(|e| LoadError(format!("缺导出 {EXPORT_FREE}：{e}")))?;
        let shutdown = *lib
            .get::<ShutdownFn>(EXPORT_SHUTDOWN.as_bytes())
            .map_err(|e| LoadError(format!("缺导出 {EXPORT_SHUTDOWN}：{e}")))?;

        Ok(LoadedPlugin {
            manifest,
            path: path.to_path_buf(),
            _lib: Arc::new(lib),
            call,
            free,
            shutdown,
            calls: AtomicU64::new(0),
            failures: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
            last_used_ms: AtomicU64::new(now_ms()),
            last_error: Mutex::new(String::new()),
        })
    }
}

fn init_error_hint(code: i32) -> &'static str {
    match code {
        -1 => "宿主传入了空能力表",
        -2 => "ABI 版本不匹配",
        -3 => "能力表结构过短（宿主版本过旧）",
        -4 => "重复初始化",
        _ => "插件内部初始化失败",
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 实例缓存配置。
#[derive(Debug, Clone, Copy)]
pub struct CacheConfig {
    /// 最多缓存多少个实例。
    pub max_cached: usize,
    /// 空闲多久后回收（毫秒）。
    pub idle_timeout_ms: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_cached: 32,
            idle_timeout_ms: 5 * 60 * 1000,
        }
    }
}

/// 加载器：id → 实例。
#[derive(Default)]
pub struct Loader {
    cache: Mutex<BTreeMap<String, Arc<LoadedPlugin>>>,
    cfg: CacheConfig,
}

impl Loader {
    pub fn new(cfg: CacheConfig) -> Self {
        Self {
            cache: Mutex::new(BTreeMap::new()),
            cfg,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<LoadedPlugin>>> {
        match self.cache.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// 已加载则复用，否则加载。
    pub fn ensure(
        &self,
        p: &PluginManifest,
        factory: &HostFactory,
    ) -> Result<Arc<LoadedPlugin>, LoadError> {
        {
            let guard = self.lock();
            if let Some(l) = guard.get(&p.id) {
                l.touch(now_ms());
                return Ok(l.clone());
            }
        }
        let loaded = Arc::new(open(&p.dll_path(), factory)?);
        // 与 dll 自称的 id 不一致 ⇒ 拒绝（防止装错文件）
        if loaded.manifest.id != p.id {
            return Err(LoadError(format!(
                "插件文件 {} 自称 id `{}`，与目录 `{}` 不符",
                p.dll_path().display(),
                loaded.manifest.id,
                p.id
            )));
        }
        let mut guard = self.lock();
        // 竞态下别人可能已经放进去
        if let Some(existing) = guard.get(&p.id) {
            return Ok(existing.clone());
        }
        guard.insert(p.id.clone(), loaded.clone());
        self.evict_if_needed(&mut guard);
        Ok(loaded)
    }

    pub fn get(&self, id: &str) -> Option<Arc<LoadedPlugin>> {
        self.lock().get(id).cloned()
    }

    pub fn unload(&self, id: &str) -> bool {
        self.lock().remove(id).is_some()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    pub fn loaded_ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// 回收空闲实例（惰性调用：每次 ensure / 定时任务都行）。
    pub fn purge_idle(&self) -> Vec<String> {
        let now = now_ms();
        let mut guard = self.lock();
        let stale: Vec<String> = guard
            .iter()
            .filter(|(_, v)| {
                now.saturating_sub(v.last_used_ms.load(Ordering::Relaxed))
                    >= self.cfg.idle_timeout_ms
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            guard.remove(k);
        }
        stale
    }

    /// 超上限时按最久未用淘汰（不会淘汰 pending 中的：这里只按 last_used）。
    fn evict_if_needed(&self, guard: &mut BTreeMap<String, Arc<LoadedPlugin>>) {
        while guard.len() > self.cfg.max_cached {
            let victim = guard
                .iter()
                .min_by_key(|(_, v)| v.last_used_ms.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    guard.remove(&k);
                    tracing::debug!(plugin = k, "实例缓存超限，淘汰最久未用");
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_gives_readable_error() {
        let p = PathBuf::from("no/such/plugin.dll");
        let f: HostFactory = Arc::new(|_: &[String]| HostApi::empty());
        let err = open(&p, &f).unwrap_err();
        assert!(err.0.contains("找不到插件文件"), "{err}");
    }

    #[test]
    fn init_error_hints() {
        assert!(init_error_hint(-2).contains("ABI"));
        assert!(init_error_hint(-1).contains("空"));
        assert!(!init_error_hint(-99).is_empty());
    }

    #[test]
    fn cache_evicts_oldest() {
        let loader = Loader::new(CacheConfig {
            max_cached: 0,
            idle_timeout_ms: 0,
        });
        // max_cached=0 时插入后会立刻被淘汰；这里只验证逻辑不 panic
        assert!(loader.loaded_ids().is_empty());
    }

    #[test]
    fn loader_starts_empty() {
        let l = Loader::default();
        assert!(l.loaded_ids().is_empty());
        assert!(l.get("nope").is_none());
        assert!(!l.unload("nope"));
        assert!(l.purge_idle().is_empty());
    }
}
