//! # plugin-sdk —— WindBot 原子插件 SDK
//!
//! 一个插件 = 一个目录 = `PnC.toml` + 一个 dll。SDK 负责：
//!
//! - 定义跨 dll 的 [`api::HostApi`]（C ABI，只传 JSON）
//! - [`declare_tool!`] 一次性生成 5 个导出符号，把样板压到 10 行内
//! - `catch_unwind` 兜住 panic（`extern "C"` 里 panic = 整个进程 abort）
//! - 封装宿主能力：`host::http` / `host::tcp` / `host::dns` / `host::kb_search` …
//! - 提供插件公用工具：编解码、哈希、差分、HTML 解析、文本处理
//!
//! ## 两条铁律
//!
//! 1. **插件不许自己联网、落盘、起进程**——全部走 [`host`]，宿主统一做 ScopeGuard / 限流 / 审计。
//! 2. **插件无状态**——同一个 dll 可能被并发调用，不要在 `static mut` 里攒状态。
//!
//! ## 最小插件
//!
//! ```ignore
//! use plugin_sdk::prelude::*;
//!
//! fn run(args: &Value) -> Result<Reply, String> {
//!     let a = args::req_str(args, "a")?;
//!     let b = args::req_str(args, "b")?;
//!     let same = a == b;
//!     Ok(Reply::both(
//!         if same { "两次一致" } else { "两次不同" },
//!         json!({"same": same}),
//!     ))
//! }
//!
//! declare_tool! {
//!     id: "response_diff",
//!     group: "vuln_verify",
//!     version: "0.1.0",
//!     risk: "low",
//!     permissions: ["http"],
//!     summary: "比对两次响应的状态/长度/相似度",
//!     schema: r#"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a","b"]}"#,
//!     run: run,
//! }
//! ```

pub mod api;
pub mod host;

// 通用工具来自 `wpa-kit`：宿主（browser-core / html_parse）与插件用**同一份**实现，
// 避免"两边各写一个 strip_tags，结果一个吃掉 script、一个没吃"。
pub use wpa_kit::{codec, digest, html, shape, textx};

use serde_json::{json, Value};

/// 供宏内部引用，避免插件必须自己 `use serde_json`。
#[doc(hidden)]
pub mod __reexport {
    pub use serde_json;
}

/// 插件返回给宿主的东西。
///
/// `summary` 进 LLM 上下文（有字数上限，插件自己控制长度）；
/// `detail` 进结构化结果与报告，不给模型看全文。
#[derive(Debug, Clone)]
pub struct Reply {
    pub summary: String,
    pub detail: Value,
}

impl Reply {
    pub fn summary(s: impl Into<String>) -> Self {
        Self {
            summary: s.into(),
            detail: Value::Null,
        }
    }

    pub fn both(s: impl Into<String>, detail: Value) -> Self {
        Self {
            summary: s.into(),
            detail,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ok": true,
            "summary": self.summary,
            "detail": self.detail,
        })
    }
}

/// 参数读取（缺键/类型不符都给出可读原因，不让插件写一堆 match）。
pub mod args {
    use serde_json::Value;

    /// 可选字符串。
    pub fn opt_str(v: &Value, key: &str) -> Option<String> {
        v.get(key).and_then(|x| match x {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
    }

    /// 必填字符串。
    pub fn req_str(v: &Value, key: &str) -> Result<String, String> {
        opt_str(v, key).ok_or_else(|| format!("缺少参数 `{key}`（字符串）"))
    }

    /// 必填字符串 + 非空。
    pub fn req_nonempty(v: &Value, key: &str) -> Result<String, String> {
        let s = req_str(v, key)?;
        if s.trim().is_empty() {
            return Err(format!("参数 `{key}` 不能为空"));
        }
        Ok(s)
    }

    pub fn opt_u64(v: &Value, key: &str) -> Option<u64> {
        v.get(key).and_then(|x| match x {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
    }

    pub fn u64_or(v: &Value, key: &str, default: u64) -> u64 {
        opt_u64(v, key).unwrap_or(default)
    }

    /// 上限收敛（防止模型传 10 亿）。
    pub fn u64_cap(v: &Value, key: &str, default: u64, cap: u64) -> u64 {
        u64_or(v, key, default).clamp(1, cap)
    }

    pub fn usize_or(v: &Value, key: &str, default: usize) -> usize {
        u64_or(v, key, default as u64) as usize
    }

    pub fn opt_bool(v: &Value, key: &str) -> Option<bool> {
        v.get(key).and_then(|x| match x {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            Value::Number(n) => Some(n.as_u64().unwrap_or(0) != 0),
            _ => None,
        })
    }

    pub fn bool_or(v: &Value, key: &str, default: bool) -> bool {
        opt_bool(v, key).unwrap_or(default)
    }

    /// 字符串数组（也接受逗号分隔的单串）。
    pub fn str_list(v: &Value, key: &str) -> Vec<String> {
        match v.get(key) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect(),
            Some(Value::String(s)) => s
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 键值对（对象或 `k=v` 数组）。
    pub fn kv_list(v: &Value, key: &str) -> Vec<(String, String)> {
        match v.get(key) {
            Some(Value::Object(o)) => o
                .iter()
                .map(|(k, val)| {
                    (
                        k.clone(),
                        val.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| val.to_string()),
                    )
                })
                .collect(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .filter_map(|s| s.split_once('='))
                .map(|(k, val)| (k.trim().to_string(), val.trim().to_string()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 上限收敛的列表长度。
    pub fn cap_list<T>(mut v: Vec<T>, cap: usize) -> Vec<T> {
        v.truncate(cap);
        v
    }
}

/// 构造 `Reply` 的简写。
#[macro_export]
macro_rules! ok {
    ($summary:expr $(,)?) => {
        Ok($crate::Reply::summary($summary))
    };
    ($summary:expr, $detail:expr $(,)?) => {
        Ok($crate::Reply::both($summary, $detail))
    };
}

/// 提前返回错误。
#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => {
        return Err(format!($($t)*))
    };
}

/// 声明一个原子工具插件，生成全部导出符号。
///
/// `run` 的签名固定为 `fn(&serde_json::Value) -> Result<plugin_sdk::Reply, String>`。
#[macro_export]
macro_rules! declare_tool {
    (
        id: $id:literal,
        group: $group:literal,
        $(version: $version:literal,)?
        risk: $risk:literal,
        permissions: [$($perm:literal),* $(,)?],
        summary: $summary:literal,
        schema: $schema:literal,
        run: $run:path $(,)?
    ) => {
        const __WBP_ID: &str = $id;
        const __WBP_GROUP: &str = $group;
        const __WBP_VERSION: &str = "0.1.0";
        const __WBP_RISK: &str = $risk;

        fn __wbp_manifest_json() -> ::std::string::String {
            let perms: ::std::vec::Vec<&str> = ::std::vec![$($perm),*];
            let schema: $crate::__reexport::serde_json::Value =
                $crate::__reexport::serde_json::from_str($schema).unwrap_or_else(|e| {
                    $crate::__reexport::serde_json::json!({
                        "type": "object",
                        "_schema_error": e.to_string(),
                    })
                });
            let v = $crate::__reexport::serde_json::json!({
                "abi": $crate::api::ABI_VERSION,
                "id": __WBP_ID,
                "group": __WBP_GROUP,
                "version": __WBP_VERSION,
                "kind": "tool",
                "risk": __WBP_RISK,
                "permissions": perms,
                "summary": $summary,
                "schema": schema,
            });
            v.to_string()
        }

        /// 插件清单（静态串，调用方不需要释放）。
        #[no_mangle]
        pub extern "C" fn wbp_manifest() -> *const ::std::os::raw::c_char {
            use ::std::sync::OnceLock;
            static CELL: OnceLock<::std::ffi::CString> = OnceLock::new();
            let c = CELL.get_or_init(|| {
                ::std::ffi::CString::new(__wbp_manifest_json())
                    .unwrap_or_else(|_| ::std::ffi::CString::new("{}").unwrap())
            });
            c.as_ptr()
        }

        /// 注入宿主能力表；0 = 就绪。
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn wbp_init(host: *const $crate::api::HostApi) -> i32 {
            ::std::panic::catch_unwind(|| $crate::host::init(host)).unwrap_or(-9)
        }

        /// 调用一次工具；入参/出参都是 JSON 串。
        ///
        /// 入参指针由宿主按 ABI 契约传入（以 NUL 结尾的 UTF-8），
        /// 无法也不该标 `unsafe fn`——那会让每个插件的导出符号都变成 unsafe。
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn wbp_call(
            args: *const ::std::os::raw::c_char,
        ) -> *mut ::std::os::raw::c_char {
            let out = ::std::panic::catch_unwind(|| -> ::std::string::String {
                if args.is_null() {
                    return r#"{"ok":false,"error":"参数指针为空"}"#.to_string();
                }
                // SAFETY: 宿主保证传入以 NUL 结尾的 UTF-8 串。
                let text = unsafe { ::std::ffi::CStr::from_ptr(args) }
                    .to_string_lossy()
                    .to_string();
                let v: $crate::__reexport::serde_json::Value = if text.trim().is_empty() {
                    $crate::__reexport::serde_json::json!({})
                } else {
                    match $crate::__reexport::serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            return $crate::__reexport::serde_json::json!({
                                "ok": false,
                                "error": format!("参数不是合法 JSON：{e}"),
                            })
                            .to_string()
                        }
                    }
                };
                match $run(&v) {
                    Ok(reply) => reply.to_json().to_string(),
                    Err(e) => $crate::__reexport::serde_json::json!({
                        "ok": false,
                        "error": e,
                    })
                    .to_string(),
                }
            });

            let s = match out {
                Ok(s) => s,
                Err(p) => {
                    let msg = if let Some(s) = p.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = p.downcast_ref::<::std::string::String>() {
                        s.clone()
                    } else {
                        "未知 panic".to_string()
                    };
                    $crate::__reexport::serde_json::json!({
                        "ok": false,
                        "error": format!("插件 panic：{msg}"),
                    })
                    .to_string()
                }
            };

            match ::std::ffi::CString::new(s) {
                Ok(c) => c.into_raw(),
                Err(_) => ::std::ffi::CString::new(r#"{"ok":false,"error":"结果含 NUL 字节"}"#)
                    .unwrap()
                    .into_raw(),
            }
        }

        /// 释放 `wbp_call` 返回的串。
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn wbp_free(s: *mut ::std::os::raw::c_char) {
            if s.is_null() {
                return;
            }
            // SAFETY: 指针来自本 dll 的 CString::into_raw。
            unsafe {
                drop(::std::ffi::CString::from_raw(s));
            }
        }

        /// 卸载钩子（无状态插件无需实现）。
        #[no_mangle]
        pub extern "C" fn wbp_shutdown() {}
    };
}

/// 插件常用预导入。
pub mod prelude {
    pub use crate::args;
    pub use crate::api::HostApi;
    pub use crate::codec;
    pub use crate::digest;
    pub use crate::html;
    pub use crate::host;
    pub use crate::shape;
    pub use crate::textx;
    pub use crate::{bail, declare_tool, ok, Reply};
    pub use serde_json::{json, Value};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_run(args: &Value) -> Result<Reply, String> {
        let a = args::req_str(args, "a")?;
        Ok(Reply::both(
            format!("got {a}"),
            json!({"len": a.len()}),
        ))
    }

    // 在 crate 内部跑一遍宏展开，确保 5 个导出符号与 JSON 契约自洽。
    crate::declare_tool! {
        id: "sdk_selftest",
        group: "selftest",
        risk: "low",
        permissions: ["http", "tcp"],
        summary: "SDK 自检",
        schema: r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]}"#,
        run: echo_run,
    }

    #[test]
    fn manifest_shape() {
        let raw = __wbp_manifest_json();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["abi"], api::ABI_VERSION);
        assert_eq!(v["id"], "sdk_selftest");
        assert_eq!(v["kind"], "tool");
        assert_eq!(v["risk"], "low");
        assert_eq!(v["permissions"].as_array().unwrap().len(), 2);
        assert_eq!(v["schema"]["type"], "object");
    }

    #[test]
    fn call_roundtrip_and_errors() {
        let ok = call_raw(r#"{"a":"xy"}"#);
        let v: Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["summary"], "got xy");
        assert_eq!(v["detail"]["len"], 2);

        let bad = call_raw("{not json");
        let v: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("JSON"));

        let missing = call_raw("{}");
        let v: Value = serde_json::from_str(&missing).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("a"));
    }

    fn call_raw(input: &str) -> String {
        use std::ffi::{CStr, CString};
        let c = CString::new(input).unwrap();
        let out = wbp_call(c.as_ptr());
        assert!(!out.is_null());
        let s = unsafe { CStr::from_ptr(out) }.to_string_lossy().to_string();
        wbp_free(out);
        s
    }

    #[test]
    fn manifest_ptr_is_stable() {
        let a = wbp_manifest();
        let b = wbp_manifest();
        assert_eq!(a, b, "同一插件多次取 manifest 应返回同一块内存");
    }
}
