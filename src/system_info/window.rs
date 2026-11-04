//! Window parking, tray icon primitives, and related Win32 helpers.
//!
//! Split from `system_info.rs` (P3); the parent re-exports this module so
//! `crate::system_info::*` paths keep working.

use super::to_wide;
use crate::tray::event::{ID_QUIT, ID_SHOW};
use std::sync::OnceLock;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, FindWindowExW, GetWindowLongPtrW,
    GetWindowPlacement, IsIconic, IsWindow, IsZoomed, PostMessageW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, ShowWindow,
    TrackPopupMenu, WINDOWPLACEMENT,
};

#[allow(dead_code, clippy::upper_case_acronyms)]
type LPCWSTR = *const u16;

// Tray icon constants
const WM_NULL: u32 = 0x00;
pub const WM_POWERBROADCAST: u32 = 0x0218;
pub const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;
pub const PBT_APMRESUMESUSPEND: u32 = 0x0007;
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;

// Win32 constants for window management (POINT/RECT/WINDOWPLACEMENT now from windows-sys).

const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOOLWINDOW: isize = 0x00000080;
/// Winit's WS_EX_APPWINDOW forces taskbar button; must be cleared while parked.
const WS_EX_APPWINDOW: isize = 0x0004_0000;
const SW_SHOWMAXIMIZED: u32 = 3;
const SW_RESTORE: u32 = 9;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_FRAMECHANGED: u32 = 0x0020;
const SWP_SHOWWINDOW: u32 = 0x0040;
/// Classic off-screen parking coordinates (far outside the virtual screen).
const OFFSCREEN: i32 = -32000;

/// WINDOWPLACEMENT is 44 bytes; SetWindowPlacement requires exact length.
const _: () = assert!(std::mem::size_of::<WINDOWPLACEMENT>() == 44);

/// Saved placement for restoring a parked window. Single-window assumption:
/// the app owns exactly one main window, so one global slot is enough. A
/// crash between hide and restore loses the slot and restore falls back to
/// `ShowWindow` (see `restore_window_from_tray`); double-hide while parked is
/// guarded by the `left > -10000` check in `hide_window_to_tray`, which keeps
/// the off-screen position from overwriting the good one.
static SAVED_PLACEMENT: std::sync::Mutex<Option<WINDOWPLACEMENT>> = std::sync::Mutex::new(None);

/// Hides window by parking off-screen to keep WM_PAINT and swapchain valid.
pub fn hide_window_to_tray(hwnd: isize) {
    if !is_window(hwnd) {
        // Skip if HWND is stale; operating on dead handle could mutate another window.
        tracing::debug!(
            "hide_window_to_tray: HWND {} no longer valid, skipping",
            hwnd
        );
        return;
    }
    let h = hwnd as *mut core::ffi::c_void;
    // SAFETY: hwnd is valid window handle.
    unsafe {
        let mut placement = std::mem::zeroed::<WINDOWPLACEMENT>();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(h, &mut placement) != 0 {
            // Only save if the window is on-screen. A second hide while already
            // parked would save the off-screen (-32000) position and make the
            // next restore unrecoverable.
            if placement.rcNormalPosition.left > -10000 {
                *SAVED_PLACEMENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(placement);
            }
        }
        // Park off-screen with SetWindowPos. SetWindowPlacement clamps negative
        // coordinates to (0,0) leaving the window visible, so it cannot park.
        // Maximized windows ignore SetWindowPos, so unmaximize first; the saved
        // placement still carries SW_SHOWMAXIMIZED for restore.
        if IsZoomed(h) != 0 {
            ShowWindow(h, SW_RESTORE as i32);
        }
        SetWindowPos(
            h,
            std::ptr::null_mut(),
            OFFSCREEN,
            OFFSCREEN,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // Remove taskbar/alt-tab presence while parked.
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW);
        SetWindowPos(
            h,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        // Drop focus so keystrokes do not reach invisible window.
        SetFocus(std::ptr::null_mut());
    }
}

/// Restores a window previously parked off-screen to its saved position
/// and re-adds taskbar / Alt-Tab presence.
pub fn restore_window_from_tray(hwnd: isize) {
    if !is_window(hwnd) {
        tracing::debug!(
            "restore_window_from_tray: HWND {} no longer valid, skipping",
            hwnd
        );
        return;
    }
    let h = hwnd as *mut core::ffi::c_void;
    // SAFETY: hwnd is valid window handle.
    unsafe {
        // Re-add taskbar / Alt-Tab presence. WS_EX_APPWINDOW (set by winit)
        // forces a taskbar button, so clear WS_EX_TOOLWINDOW and restore
        // WS_EX_APPWINDOW, then SWP_FRAMECHANGED to re-evaluate.
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex & !WS_EX_TOOLWINDOW) | WS_EX_APPWINDOW);
        SetWindowPos(
            h,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        // Clear iconic state before repositioning. SetWindowPlacement with
        // SW_RESTORE alone is unreliable when the window was parked off-screen
        // with WS_EX_TOOLWINDOW while iconic.
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE as i32);
        }

        if let Some(mut placement) = SAVED_PLACEMENT
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            // Preserve maximized state if it was maximized before parking;
            // never restore as minimized, which would re-trigger auto-minimize.
            placement.showCmd = if placement.showCmd == SW_SHOWMAXIMIZED {
                SW_SHOWMAXIMIZED
            } else {
                SW_RESTORE
            };
            if SetWindowPlacement(h, &placement) == 0 {
                tracing::error!(
                    "SetWindowPlacement (restore) failed: {}",
                    std::io::Error::last_os_error()
                );
                // Fallback: move back with SetWindowPos using the saved rect.
                let r = placement.rcNormalPosition;
                SetWindowPos(
                    h,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        } else {
            ShowWindow(h, SW_RESTORE as i32);
        }
    }
    // Use force_foreground_window outside the unsafe block so it can also
    // simulate the Alt key workaround without nesting unsafe.
    force_foreground_window(hwnd);
}

/// Forces window to foreground via Alt key trick to bypass foreground restriction.
pub fn force_foreground_window(hwnd: isize) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        SendInput, VK_MENU,
    };
    let h = hwnd as *mut core::ffi::c_void;
    // Simulate Alt key press/release via SendInput with OS-provided INPUT
    // layout (a hand-rolled struct had the wrong size on x64).
    unsafe {
        let mut inputs = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        wScan: 0,
                        dwFlags: KEYEVENTF_EXTENDEDKEY,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_MENU,
                        wScan: 0,
                        dwFlags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ];
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
        SetForegroundWindow(h);
    }
}

// Tray icon functions: SAFETY hwnd is valid handle from FindWindowW/CreateWindowExW.

/// Finds our own main window by process ID instead of title, so another app
/// with the same title can never be mistaken for ours. Skips invisible
/// windows such as the hidden tray message window.
pub fn find_main_window_by_pid() -> Option<isize> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };
    use windows_sys::core::BOOL;

    struct EnumCtx {
        pid: u32,
        found: Option<isize>,
    }

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the caller's EnumCtx, alive for the whole call.
        let ctx = unsafe { &mut *(lparam as *mut EnumCtx) };
        let mut pid = 0u32;
        // SAFETY: hwnd comes from the enumerator; pid is a valid out-param.
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        // SAFETY: same hwnd as above.
        if pid == ctx.pid && ctx.found.is_none() && unsafe { IsWindowVisible(hwnd) } != 0 {
            ctx.found = Some(hwnd as isize);
            return 0; // stop enumeration
        }
        1 // continue
    }

    let mut ctx = EnumCtx {
        // SAFETY: takes no arguments.
        pid: unsafe { GetCurrentProcessId() },
        found: None,
    };
    // SAFETY: the callback only touches the stack EnumCtx above.
    unsafe { EnumWindows(Some(enum_cb), &mut ctx as *mut EnumCtx as LPARAM) };
    ctx.found
}

/// Hidden tray message window class.
const TRAY_WINDOW_CLASS: &str = "FrameworkControlTray";

/// Message ID for second instance to request restore of parked window.
/// Cached like [`taskbar_created_msg`]: `RegisterWindowMessageW` is process-
/// wide and idempotent for the same string, but the wndproc hot path must
/// not pay a syscall + UTF-16 encode on every message.
pub fn show_request_message_id() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| {
        let wide = to_wide("FrameworkCrateShow");
        unsafe { RegisterWindowMessageW(wide.as_ptr()) }
    })
}

/// Finds running instance's hidden tray window.
pub fn find_tray_window() -> Option<isize> {
    let wide = to_wide(TRAY_WINDOW_CLASS);
    let hwnd = unsafe {
        FindWindowExW(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wide.as_ptr(),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as isize)
    }
}

/// Asks running instance to restore its parked window (only owner has saved placement).
pub fn request_show_running_instance() {
    // The running instance's tray window is created a moment after its main
    // window appears, so a second instance launched immediately (or while the
    // first is still booting) may find nothing. Retry briefly so the restore
    // request is not silently dropped.
    for _ in 0..20 {
        if let Some(hwnd) = find_tray_window() {
            post_message(hwnd, show_request_message_id(), 0, 0);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Registered message sent by Explorer when the taskbar is recreated (e.g.
/// Explorer crash/restart). `RegisterWindowMessageW("TaskbarCreated")` returns
/// a dynamic atom, not a fixed constant, so it must be queried at runtime.
pub fn taskbar_created_msg() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| {
        let wide = to_wide("TaskbarCreated");
        unsafe { RegisterWindowMessageW(wide.as_ptr()) }
    })
}

pub fn is_iconic(hwnd: isize) -> bool {
    // SAFETY: IsIconic checks if window is minimized.
    unsafe { IsIconic(hwnd as *mut core::ffi::c_void) != 0 }
}

pub fn is_window(hwnd: isize) -> bool {
    // SAFETY: IsWindow validates handle.
    unsafe { IsWindow(hwnd as *mut core::ffi::c_void) != 0 }
}

/// Loads ICO icon from bytes and returns HICON handle.
pub fn load_icon_from_bytes(data: &[u8]) -> Option<isize> {
    // ICO: 6-byte header + 16-byte entries.
    if data.len() < 22 {
        return None;
    }

    let _reserved = u16::from_le_bytes([data[0], data[1]]);
    let icon_type = u16::from_le_bytes([data[2], data[3]]);
    if icon_type != 1 {
        return None;
    }
    let count = u16::from_le_bytes([data[4], data[5]]);
    if count == 0 {
        return None;
    }

    // Find largest entry; 0 means 256px.
    let mut best_entry: Option<(u32, u32)> = None; // (bytes_in_res, image_offset)
    let mut best_area: u32 = 0;

    for i in 0..count as usize {
        let entry_offset = 6 + i * 16;
        if entry_offset + 16 > data.len() {
            break;
        }
        let w = data[entry_offset];
        let h = data[entry_offset + 1];
        let w_px = if w == 0 { 256 } else { w as u32 };
        let h_px = if h == 0 { 256 } else { h as u32 };
        let area = w_px * h_px;

        let bytes_in_res = u32::from_le_bytes([
            data[entry_offset + 8],
            data[entry_offset + 9],
            data[entry_offset + 10],
            data[entry_offset + 11],
        ]);
        let image_offset = u32::from_le_bytes([
            data[entry_offset + 12],
            data[entry_offset + 13],
            data[entry_offset + 14],
            data[entry_offset + 15],
        ]);

        if area >= best_area {
            best_area = area;
            best_entry = Some((bytes_in_res, image_offset));
        }
    }

    let (bytes_in_res, image_offset) = best_entry?;
    let offset = image_offset as usize;
    let size = bytes_in_res as usize;

    if offset + size > data.len() {
        return None;
    }

    let mut owned = data[offset..offset + size].to_vec();

    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateIconFromResourceEx(
            presbits: *mut u8,
            dwResSize: u32,
            fIcon: i32,
            dwVer: u32,
            cxDesired: i32,
            cyDesired: i32,
            uFlags: u32,
        ) -> *mut core::ffi::c_void;
    }

    let hicon = unsafe {
        CreateIconFromResourceEx(owned.as_mut_ptr(), size as u32, 1, 0x00030000, 0, 0, 0x0000)
    };
    if hicon.is_null() {
        None
    } else {
        Some(hicon as isize)
    }
}

/// Copies a tooltip into a fixed `szTip` buffer, truncating at 127 UTF-16
/// units and keeping it NUL-terminated. Shared by add/modify so the two
/// call sites cannot drift apart.
fn fill_tip(sz_tip: &mut [u16; 128], tip: &str) {
    let tip_wide = to_wide(tip);
    let copy_len = tip_wide.len().min(127);
    sz_tip[..copy_len].copy_from_slice(&tip_wide[..copy_len]);
    // Keep buffer NUL-terminated when tip exceeds limit.
    sz_tip[copy_len] = 0;
}

/// Adds tray icon; SAFETY hwnd and HICON must be valid.
pub fn shell_notify_add(hwnd: isize, icon: isize, tip: &str, callback_msg: u32) -> bool {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as *mut core::ffi::c_void;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = callback_msg;
    nid.hIcon = icon as *mut core::ffi::c_void;
    fill_tip(&mut nid.szTip, tip);
    // SAFETY: Shell_NotifyIconW with valid NID.
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) != 0 }
}

/// Removes tray icon; SAFETY hwnd must match add handle.
pub fn shell_notify_delete(hwnd: isize) -> bool {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as *mut core::ffi::c_void;
    nid.uID = 1;
    // SAFETY: Shell_NotifyIconW with NIM_DELETE.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) != 0 }
}

/// Rewrites the tray hover text; SAFETY hwnd must match add handle.
pub fn shell_notify_modify_tip(hwnd: isize, tip: &str) -> bool {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as *mut core::ffi::c_void;
    nid.uID = 1;
    nid.uFlags = NIF_TIP;
    fill_tip(&mut nid.szTip, tip);
    // SAFETY: Shell_NotifyIconW with valid NID.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) != 0 }
}

/// Posts message to window queue; SAFETY hwnd must be valid.
pub fn post_message(hwnd: isize, msg: u32, wparam: usize, lparam: isize) {
    // SAFETY: PostMessageW with valid hwnd.
    unsafe {
        PostMessageW(hwnd as *mut core::ffi::c_void, msg, wparam, lparam);
    }
}

/// Shows tray context menu at coordinates; returns selected command ID.
pub fn show_tray_menu(hwnd: isize, x: i32, y: i32) -> Option<u32> {
    // SAFETY: CreatePopupMenu; null on failure.
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }

    let show_text = to_wide("Show Framework Crate");
    let quit_text = to_wide("Exit");

    // SAFETY: AppendMenuW with valid menu.
    unsafe {
        AppendMenuW(menu, 0, ID_SHOW as usize, show_text.as_ptr());
        AppendMenuW(menu, 0, ID_QUIT as usize, quit_text.as_ptr());
    }

    // SAFETY: TrackPopupMenu with TPM_RETURNCMD.
    let cmd = unsafe {
        SetForegroundWindow(hwnd as *mut core::ffi::c_void);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            x,
            y,
            0,
            hwnd as *mut core::ffi::c_void,
            std::ptr::null(),
        )
    };

    // SAFETY: DestroyMenu with valid handle.
    unsafe {
        DestroyMenu(menu);
    }

    // Ensure tray callback is processed.
    post_message(hwnd, WM_NULL, 0, 0);

    if cmd > 0 { Some(cmd as u32) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS_OVERLAPPED: u32 = 0x0000_0000;
    const SW_SHOW: i32 = 5;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateWindowExW(
            dwExStyle: u32,
            lpClassName: LPCWSTR,
            lpWindowName: LPCWSTR,
            dwStyle: u32,
            x: i32,
            y: i32,
            nWidth: i32,
            nHeight: i32,
            hWndParent: *mut core::ffi::c_void,
            hMenu: *mut core::ffi::c_void,
            hInstance: *mut core::ffi::c_void,
            lpParam: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn DestroyWindow(hWnd: *mut core::ffi::c_void) -> i32;
    }

    fn placement_of(hwnd: *mut core::ffi::c_void) -> WINDOWPLACEMENT {
        let mut p = unsafe { std::mem::zeroed::<WINDOWPLACEMENT>() };
        p.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        let rc = unsafe { GetWindowPlacement(hwnd, &mut p) };
        assert_ne!(
            rc,
            0,
            "GetWindowPlacement failed: {}",
            std::io::Error::last_os_error()
        );
        p
    }

    #[test]
    fn parking_moves_window_offscreen_and_restores() {
        unsafe {
            let class = to_wide("STATIC");
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_OVERLAPPED,
                100,
                100,
                320,
                240,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW failed");
            ShowWindow(hwnd, SW_SHOW);

            let before = placement_of(hwnd);
            assert_eq!(
                before.rcNormalPosition.left, 100,
                "expected window at x=100 before parking"
            );

            hide_window_to_tray(hwnd as isize);

            let parked = placement_of(hwnd);
            assert_eq!(
                parked.rcNormalPosition.left, OFFSCREEN,
                "window should have been parked off-screen"
            );

            restore_window_from_tray(hwnd as isize);

            let restored = placement_of(hwnd);
            assert_eq!(
                restored.rcNormalPosition.left, 100,
                "window should be restored to x=100"
            );

            DestroyWindow(hwnd);
        }
    }
}
