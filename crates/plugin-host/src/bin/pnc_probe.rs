//! `pnc-probe <dll>` —— 只读一个 dll 的内嵌清单，把 JSON 打到 stdout。
//!
//! 为什么单开一个进程：`dlopen` 会执行目标 dll 的 `DllMain`。用户刚拷进来的
//! dll 是**未受信任**的，在宿主的地址空间里跑一遍等于把审批链绕过去了。
//! 放进子进程后，最坏情况是这个进程崩掉，宿主只看到"探测超时/失败"。
//!
//! 刻意**不调** `wbp_init`：探测只要清单，不需要能力表，也就不会让插件拿到
//! 任何宿主句柄。
//!
//! 退出码：0 = 成功（stdout 是 JSON）；1 = 不是插件/清单不可读；2 = 用法错误。

use std::ffi::CStr;
use std::os::raw::c_char;

type ManifestFn = unsafe extern "C" fn() -> *const c_char;

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            eprintln!("用法：pnc-probe <插件.dll>");
            std::process::exit(2);
        }
    };

    // SAFETY: 这正是本程序的存在意义——在**独立进程**里加载未受信任的 dll。
    // 即使 DllMain 乱来，也只影响这个进程。
    let lib = match unsafe { libloading::Library::new(&path) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("dlopen 失败：{e}");
            std::process::exit(1);
        }
    };

    // SAFETY: `wbp_manifest` 由 SDK 的 declare_tool! 保证导出，返回 NUL 结尾的静态串。
    let raw = unsafe {
        let f: libloading::Symbol<ManifestFn> = match lib.get(plugin_abi::EXPORT_MANIFEST.as_bytes())
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("缺导出 {}：{e}（不是本宿主的插件）", plugin_abi::EXPORT_MANIFEST);
                std::process::exit(1);
            }
        };
        let ptr = f();
        if ptr.is_null() {
            eprintln!("wbp_manifest 返回空");
            std::process::exit(1);
        }
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    };

    // 在这里就地校验一遍：宿主拿到的一定是能用 DllManifest::parse 解析的串，
    // 免得"探测通过、安装时才发现 ABI 不对"。校验通过后原样输出，不重排字段。
    match plugin_host::DllManifest::parse(&raw, plugin_abi::ABI_VERSION) {
        Ok(_) => println!("{raw}"),
        Err(e) => {
            eprintln!("清单校验失败：{e}");
            std::process::exit(1);
        }
    }
}
