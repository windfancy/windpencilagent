//! # plugin-abi —— 插件与宿主之间唯一的二进制契约
//!
//! 只放 `#[repr(C)]` 定义与常量，**零依赖**。两个使用者：
//! - `plugin-sdk`（插件侧，编译进 dll）
//! - `plugin-host`（宿主侧，dlopen 后按同一布局解释）
//!
//! 一份定义、两处引用，杜绝"两边结构体漂移导致宿主读到垃圾指针"。
//!
//! ## 版本规则
//!
//! - `ABI_VERSION` 提升 = 不兼容，宿主拒绝加载老插件；
//! - 新增能力字段 = 不改版本号，只在结构体末尾追加，插件用 `size` 判断是否存在；
//! - 永远不要重排既有字段。

use std::os::raw::{c_char, c_int};

/// 当前 ABI 版本。
pub const ABI_VERSION: u32 = 1;

/// `char*(const char*)`：入参 JSON，返回 JSON。
pub type JsonCallFn = extern "C" fn(*const c_char) -> *mut c_char;
/// `void(const char*)`：单向事件（审计/日志）。
pub type JsonSinkFn = extern "C" fn(*const c_char);
/// `void(int32, const char*)`：日志。
pub type LogFn = extern "C" fn(c_int, *const c_char);
/// `void(char*)`：释放跨边界分配的串。
pub type FreeFn = extern "C" fn(*mut c_char);

/// 宿主能力表。
///
/// `Option<fn>` 在 `repr(C)` 下与 C 的可空函数指针同布局。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApi {
    /// 结构体字节数（插件据此判断宿主支持到哪个字段）。
    pub size: u32,
    /// ABI 版本。
    pub abi_version: u32,

    // ---- 无头浏览器 ----
    /// `{method,url,headers,body,timeout_ms,follow}` → `{ok,status,headers,body,final_url,elapsed_ms}`
    pub http: Option<JsonCallFn>,
    /// `{requests:[...],concurrency}` → `{ok,results:[...]}`
    pub http_batch: Option<JsonCallFn>,
    /// `{name,base_url,ua}` → `{ok,session}`
    pub session_open: Option<JsonCallFn>,
    /// `{session,method,url,...}` → 同 `http`
    pub session_request: Option<JsonCallFn>,
    /// `{session,url,fields,method}` → 同 `http`
    pub session_form: Option<JsonCallFn>,
    /// `{session}` → `{ok,cookies:[...]}`
    pub session_cookies: Option<JsonCallFn>,
    /// `{session}` → `{ok}`
    pub session_close: Option<JsonCallFn>,
    /// `{html,base_url}` → `{ok,title,links,forms,...}`
    pub html_parse: Option<JsonCallFn>,
    /// `{html,selector}` → `{ok,nodes:[...]}`
    pub html_query: Option<JsonCallFn>,

    // ---- 网络原语 ----
    /// `{host,port,tls,payload_b64,read_ms,expect_bytes}` → `{ok,open,banner_b64,elapsed_ms,error}`
    pub tcp: Option<JsonCallFn>,
    /// `{targets:[{host,port}],read_ms,concurrency}` → `{ok,results:[...]}`
    pub tcp_batch: Option<JsonCallFn>,
    /// `{name,rtype,server}` → `{ok,records,cname,ttl}`
    pub dns: Option<JsonCallFn>,

    // ---- 数据与审计 ----
    /// `{query,top}` → `{ok,hits:[{title,body,score}]}`
    pub kb_search: Option<JsonCallFn>,
    /// `{ns,key}` → `{ok,value}`
    pub kv_get: Option<JsonCallFn>,
    /// `{ns,key,value}` → `{ok}`
    pub kv_put: Option<JsonCallFn>,
    /// `{name,content,mime}` → `{ok,handle,sha256,bytes}`
    pub raw_store: Option<JsonCallFn>,
    /// `{event,payload}`（单向）
    pub audit: Option<JsonSinkFn>,

    // ---- 杂项 ----
    /// level：0=debug 1=info 2=warn 3=error
    pub log: Option<LogFn>,
    /// 释放宿主返回的串。
    pub host_free: Option<FreeFn>,

    // ---- 追加能力（ABI 1 的扩展：老插件用 `size` 判断是否可用）----
    /// UDP 探测：`{host,port,payload_b64,read_ms,expect_bytes}` → `{ok,responded,data_b64,elapsed_ms,error}`
    pub udp: Option<JsonCallFn>,
    /// TLS 证书：`{host,port,server_name}` → `{ok,subject,issuer,alt_names,not_after,not_before,sig_alg,days_left}`
    pub tls_cert: Option<JsonCallFn>,
    /// 本机信息：`{what:"net"}` → `{ok,interfaces:[{name,addr,prefix}],routes:[..]}`；
    /// `{what:"host"}` → `{ok,hostname,user,os}`
    pub sysinfo: Option<JsonCallFn>,
}

// SAFETY: 整张表只含函数指针与整数，跨线程只读共享安全。
unsafe impl Send for HostApi {}
unsafe impl Sync for HostApi {}

impl HostApi {
    /// 结构体字节数（初始化时填进 `size` 字段）。
    pub const SIZE: u32 = std::mem::size_of::<HostApi>() as u32;

    /// 一份全部为空的表（宿主按插件权限逐项填充）。
    pub fn empty() -> Self {
        Self {
            size: Self::SIZE,
            abi_version: ABI_VERSION,
            http: None,
            http_batch: None,
            session_open: None,
            session_request: None,
            session_form: None,
            session_cookies: None,
            session_close: None,
            html_parse: None,
            html_query: None,
            tcp: None,
            tcp_batch: None,
            dns: None,
            kb_search: None,
            kv_get: None,
            kv_put: None,
            raw_store: None,
            audit: None,
            log: None,
            host_free: None,
            udp: None,
            tls_cert: None,
            sysinfo: None,
        }
    }
}

/// 插件必须导出的 5 个符号名（宿主据此查找；缺一个即拒绝加载）。
pub const EXPORT_MANIFEST: &str = "wbp_manifest";
pub const EXPORT_INIT: &str = "wbp_init";
pub const EXPORT_CALL: &str = "wbp_call";
pub const EXPORT_FREE: &str = "wbp_free";
pub const EXPORT_SHUTDOWN: &str = "wbp_shutdown";

/// 权限位：`PnC.toml` 里 `permissions` 的合法取值。
pub const PERMISSIONS: &[&str] = &[
    "http",
    "session",
    "html",
    "tcp",
    "udp",
    "dns",
    "tls",
    "sys",
    "kb",
    "kv",
    "raw_store",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        // 指针 8 字节平台上：2 个 u32（8）+ 22 个函数指针（176）= 184。
        // 这条断言是"两边结构体漂移"的守门员——改了布局必须同步改这里并升 ABI 版本。
        let ptr = std::mem::size_of::<usize>();
        assert_eq!(HostApi::SIZE as usize, 8 + 22 * ptr);
        assert_eq!(HostApi::empty().size, HostApi::SIZE);
        assert_eq!(HostApi::empty().abi_version, ABI_VERSION);
    }

    #[test]
    fn empty_table_has_no_capabilities() {
        let e = HostApi::empty();
        assert!(e.http.is_none());
        assert!(e.host_free.is_none());
    }

    #[test]
    fn permission_list_covers_abi() {
        assert!(PERMISSIONS.contains(&"http"));
        assert!(PERMISSIONS.contains(&"raw_store"));
        assert_eq!(PERMISSIONS.len(), 11);
    }
}
