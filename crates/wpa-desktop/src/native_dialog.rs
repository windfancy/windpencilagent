//! 原生文件/目录选择对话框（#13.1/#16）。
//!
//! 不引第三方 crate（rfd 会拉 gtk，与项目瘦身目标冲突）：Windows 下直接调
//! comdlg32::GetOpenFileNameW（选文件）与 shell32::SHBrowseForFolderW（选目录）。
//!
//! 生命周期审计：
//! - OPENFILENAMEW 的 lpstrFile 指向栈上 Vec<u16>，调用是同步阻塞的，
//!   缓冲区在对话框关闭后才读，生命周期覆盖整个调用；
//! - SHBrowseForFolderW 返回的 ITEMIDLIST 必须 SHFree 释放；
//! - CoInitialize/CoUninitialize 严格配对（同一次调用内）。
//! 非 Windows 平台返回错误（本项目只面向 Windows）。

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::{c_void, OsString};
    use std::os::raw::{c_int, c_ushort};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    /// Windows WCHAR = u16（UTF-16）
    type WChar = u16;

    #[repr(C)]
    struct OpenFileNameW {
        l_struct_size: u32,
        hwnd_owner: *mut c_void,
        h_instance: *mut c_void,
        lpstr_filter: *const WChar,
        lpstr_custom_filter: *mut WChar,
        n_max_cust_filter: u32,
        n_filter_index: u32,
        lpstr_file: *mut WChar,
        n_max_file: u32,
        lpstr_file_title: *mut WChar,
        n_max_file_title: u32,
        lpstr_initial_dir: *const WChar,
        lpstr_title: *const WChar,
        flags: u32,
        n_file_offset: c_ushort,
        n_file_extension: c_ushort,
        lpstr_def_ext: *const WChar,
        l_cust_data: isize,
        lpfn_hook: Option<unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize>,
        lp_template_name: *const WChar,
        pv_reserved: *mut c_void,
        dw_reserved: u32,
        flags_ex: u32,
    }

    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: *mut c_void,
        pidl_root: *mut c_void,
        psz_display_name: *mut WChar,
        lpsz_title: *const WChar,
        ul_flags: u32,
        lpfn: Option<unsafe extern "system" fn(*mut c_void, u32, isize, isize) -> c_int>,
        l_param: isize,
        i_image: c_int,
    }

    #[link(name = "comdlg32")]
    extern "system" {
        fn GetOpenFileNameW(ofn: *mut OpenFileNameW) -> c_int;
    }
    #[link(name = "shell32")]
    extern "system" {
        fn SHBrowseForFolderW(bi: *mut BrowseInfoW) -> *mut c_void;
        fn SHGetPathFromIDListW(pidl: *const c_void, path: *mut WChar) -> c_int;
        fn SHFree(p: *mut c_void);
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(pv: *mut c_void, co_init: u32) -> c_int;
        fn CoUninitialize();
    }

    const OFN_EXPLORER: u32 = 0x0008_0000;
    const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
    const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
    const BIF_RETURNONLYFSDIRS: u32 = 0x0000_0001;
    const BIF_NEWDIALOGSTYLE: u32 = 0x0000_0040;

    fn to_wide(s: &str) -> Vec<u16> {
        OsString::from(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn from_wide(w: &[u16]) -> String {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        OsString::from_wide(&w[..end]).to_string_lossy().into_owned()
    }

    /// 选文件。filter 形如 "可执行文件\0*.exe\0所有文件\0*.*\0\0"（双 NUL 结尾）；传空则不过滤。
    pub fn pick_file(title: &str, filter: Option<&str>) -> Option<String> {
        let mut file_buf = vec![0u16; 1024];
        let title_w = to_wide(title);
        let filter_w = filter.map(to_wide);
        let ofn = OpenFileNameW {
            l_struct_size: std::mem::size_of::<OpenFileNameW>() as u32,
            hwnd_owner: std::ptr::null_mut(),
            h_instance: std::ptr::null_mut(),
            lpstr_filter: filter_w.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
            lpstr_custom_filter: std::ptr::null_mut(),
            n_max_cust_filter: 0,
            n_filter_index: 1,
            lpstr_file: file_buf.as_mut_ptr(),
            n_max_file: file_buf.len() as u32,
            lpstr_file_title: std::ptr::null_mut(),
            n_max_file_title: 0,
            lpstr_initial_dir: std::ptr::null(),
            lpstr_title: title_w.as_ptr(),
            flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
            n_file_offset: 0,
            n_file_extension: 0,
            lpstr_def_ext: std::ptr::null(),
            l_cust_data: 0,
            lpfn_hook: None,
            lp_template_name: std::ptr::null(),
            pv_reserved: std::ptr::null_mut(),
            dw_reserved: 0,
            flags_ex: 0,
        };
        let mut ofn = ofn;
        unsafe {
            if GetOpenFileNameW(&mut ofn) == 0 {
                return None;
            }
            Some(from_wide(&file_buf))
        }
    }

    /// 选目录。
    pub fn pick_dir(title: &str) -> Option<String> {
        let mut display = vec![0u16; 260];
        let title_w = to_wide(title);
        let mut bi = BrowseInfoW {
            hwnd_owner: std::ptr::null_mut(),
            pidl_root: std::ptr::null_mut(),
            psz_display_name: display.as_mut_ptr(),
            lpsz_title: title_w.as_ptr(),
            ul_flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            lpfn: None,
            l_param: 0,
            i_image: 0,
        };
        // SHBrowseForFolderW 需要 COM（BIF_NEWDIALOGSTYLE 的新式对话框）
        unsafe {
            CoInitializeEx(std::ptr::null_mut(), 2); // COINIT_APARTMENTTHREADED
            let pidl = SHBrowseForFolderW(&mut bi);
            let result = if pidl.is_null() {
                None
            } else {
                let mut path = vec![0u16; 1024];
                let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
                SHFree(pidl as *mut c_void);
                if ok == 0 { None } else { Some(from_wide(&path)) }
            };
            CoUninitialize();
            result
        }
    }
}

/// 选文件（供配置路径字段用）。
pub fn pick_file(title: &str, filter: Option<&str>) -> Option<String> {
    #[cfg(target_os = "windows")]
    { win::pick_file(title, filter) }
    #[cfg(not(target_os = "windows"))]
    { let _ = (title, filter); None }
}

/// 选目录。
pub fn pick_dir(title: &str) -> Option<String> {
    #[cfg(target_os = "windows")]
    { win::pick_dir(title) }
    #[cfg(not(target_os = "windows"))]
    { let _ = title; None }
}
