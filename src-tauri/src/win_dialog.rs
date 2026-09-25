//! 原生对话框：给「启动阶段的致命错误」一个用户可见的出口。
//!
//! 为什么必须有：release 版在 Windows 上是 GUI 子系统
//! （`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`），**没有控制台**。
//! 启动阶段任何致命错误（WebView2 运行时缺失、WebView 创建失败、panic）都会让进程静默消失，
//! 用户只能报告「装完打不开」—— 这正是这类故障排查困难的根源。
//!
//! 本模块刻意直接用 `user32!MessageBoxW` 的 FFI，而不是引入 `windows-sys`：
//! 只用到这一个函数，零依赖更省心，也不会拖慢编译。
//!
//! 非 Windows 平台编译成 stderr 版本（macOS/Linux 从终端启动时一样能看到）。
//!
//! 文案取自字典的 `dlg.*`：这条路径不经过 WebView，前端的 `data-i18n` 帮不上它 ——
//! 想在崩溃对话框里看到别的语言，只能靠后端字典。取的是**弹出那一刻**的语言，
//! 与「一条日志保住它写下时的语言」是同一类时点文本。

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::{c_void, OsStr};
    // `OsStrExt::encode_wide` 是 **Windows 专属** trait，所以这个 helper 必须待在本
    // 模块内部：放在文件顶层会让整个 crate 在 macOS / Linux 上直接编译失败（E0599）。
    use std::os::windows::ffi::OsStrExt;

    /// 把 Rust 字符串转成 Win32 需要的 NUL 结尾 UTF-16。
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, utype: u32) -> i32;
    }

    const MB_OK: u32 = 0x0000_0000;
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_ICONWARNING: u32 = 0x0000_0030;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    const MB_TOPMOST: u32 = 0x0004_0000;

    /// 弹一个阻塞式系统对话框。`hwnd` 传 NULL：没有可用的父窗口。
    /// 同时置 `MB_TOPMOST | MB_SETFOREGROUND`，避免对话框被别的窗口盖住 ——
    /// 用户「看不到任何反应」和「没有对话框」是一样糟的故障。
    pub fn show(title: &str, body: &str, is_error: bool) {
        let t = wide(title);
        let b = wide(body);
        let icon = if is_error { MB_ICONERROR } else { MB_ICONWARNING };
        let flags = MB_OK | MB_SETFOREGROUND | MB_TOPMOST | icon;
        // 安全：两个指针都指向以 NUL 结尾的 UTF-16 缓冲区，且在本调用期间一直存活。
        unsafe {
            MessageBoxW(std::ptr::null_mut(), b.as_ptr(), t.as_ptr(), flags);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    pub fn show(title: &str, body: &str, _is_error: bool) {
        eprintln!("[netsense] {}\n{}", title, body);
    }
}

/// 致命错误对话框（红色错误图标）。调用方随后应结束进程。
pub fn fatal(title: &str, body: &str) {
    imp::show(title, body, true);
}

/// 警告对话框（黄色警告图标）。
pub fn warn(title: &str, body: &str) {
    imp::show(title, body, false);
}
