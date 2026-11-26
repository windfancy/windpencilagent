//! 系统剪贴板写入（#8 消息复制）。
//!
//! 不引第三方 crate：Windows 下直接调 Win32（user32/kernel32），
//! 其他平台降级返回错误（本项目当前只面向 Windows）。
//! 生命周期审计：
//! - GlobalAlloc(GMEM_MOVEABLE) 出的 HGLOBAL 在 SetClipboardData 成功后
//!   所有权转移给系统，**不能再 GlobalFree**（否则双重释放）；
//! - 失败路径必须自己 GlobalFree，并 GlobalUnlock；
//! - OpenClipboard/CloseClipboard 严格配对，任何提前返回前都先 CloseClipboard。

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_int};

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(handle: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(handle: *mut c_void) -> c_int;
        fn GlobalFree(handle: *mut c_void) -> *mut c_void;
    }
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(hwnd: *mut c_void) -> c_int;
        fn CloseClipboard() -> c_int;
        fn EmptyClipboard() -> c_int;
        fn SetClipboardData(format: u32, handle: *mut c_void) -> *mut c_void;
    }

    /// 把文本写进系统剪贴板。成功返回 Ok(())。
    pub fn copy(text: &str) -> Result<(), String> {
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err("无法打开剪贴板（可能被其他程序占用）".into());
            }
            // 从此处起，任何返回前都要 CloseClipboard
            if EmptyClipboard() == 0 {
                CloseClipboard();
                return Err("无法清空剪贴板".into());
            }
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * std::mem::size_of::<u16>();
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if handle.is_null() {
                CloseClipboard();
                return Err("分配剪贴板内存失败".into());
            }
            let ptr = GlobalLock(handle) as *mut u16;
            if ptr.is_null() {
                GlobalFree(handle);
                CloseClipboard();
                return Err("锁定剪贴板内存失败".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(handle);
            let result = SetClipboardData(CF_UNICODETEXT, handle);
            // SetClipboardData 成功后 handle 归系统；失败则仍归我们，需要释放
            if result.is_null() {
                GlobalFree(handle);
                CloseClipboard();
                return Err("写入剪贴板失败".into());
            }
            CloseClipboard();
            Ok(())
        }
    }

    // 消除未使用导入告警（c_char 在部分平台签名里没用到）
    #[allow(dead_code)]
    type _Char = c_char;
}

#[cfg(target_os = "windows")]
pub fn copy(text: &str) -> Result<(), String> {
    win::copy(text)
}

#[cfg(not(target_os = "windows"))]
pub fn copy(_text: &str) -> Result<(), String> {
    Err("当前平台不支持系统剪贴板".into())
}
