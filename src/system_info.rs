use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::sync::OnceLock;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};

#[allow(clippy::upper_case_acronyms)]
type HKEY = *mut core::ffi::c_void;
#[allow(dead_code, clippy::upper_case_acronyms)]
type LPCWSTR = *const u16;

const HKEY_LOCAL_MACHINE: HKEY = 0x80000002 as HKEY;
const KEY_READ: u32 = 0x20019;
const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;
const ERROR_SUCCESS: u32 = 0;
const ERROR_MORE_DATA: u32 = 234;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;

// Tray icon constants
const WM_NULL: u32 = 0x00;
pub const WM_POWERBROADCAST: u32 = 0x0218;
pub const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;
pub const PBT_APMRESUMESUSPEND: u32 = 0x0007;
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;
use crate::tray::event::{ID_QUIT, ID_SHOW};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, RECT};
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC};
use windows_sys::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, keybd_event};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, FindWindowExW, FindWindowW, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowPlacement, IsIconic, IsWindow, IsZoomed, PostMessageW,
    RegisterWindowMessageW, SPI_GETWORKAREA, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPlacement, SetWindowPos, ShowWindow, SystemParametersInfoW, TrackPopupMenu,
    WINDOWPLACEMENT,
};

const VREFRESH: i32 = 116;

#[repr(C)]
struct OsVersionInfoW {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u16; 128],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version_info: *mut OsVersionInfoW) -> u32;
}

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

/// Saved placement for restoring parked window.
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
    let h = hwnd as *mut core::ffi::c_void;
    const VK_MENU: u8 = 0x12; // Alt key
    const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    unsafe {
        // Simulate Alt key press/release to allow SetForegroundWindow to work.
        keybd_event(VK_MENU, 0, KEYEVENTF_EXTENDEDKEY, 0);
        keybd_event(VK_MENU, 0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, 0);
        SetForegroundWindow(h);
    }
}

#[cfg(target_arch = "x86_64")]
pub fn cpu_name() -> String {
    // CPUID leaves 0x80000002-0x80000004 return brand string.
    let mut brand = [0u8; 48];
    for (leaf, offset) in [(0x80000002u32, 0), (0x80000003, 16), (0x80000004, 32)] {
        let result = core::arch::x86_64::__cpuid_count(leaf, 0);
        brand[offset..offset + 4].copy_from_slice(&result.eax.to_le_bytes());
        brand[offset + 4..offset + 8].copy_from_slice(&result.ebx.to_le_bytes());
        brand[offset + 8..offset + 12].copy_from_slice(&result.ecx.to_le_bytes());
        brand[offset + 12..offset + 16].copy_from_slice(&result.edx.to_le_bytes());
    }
    let end = brand.iter().position(|&b| b == 0).unwrap_or(48);
    String::from_utf8_lossy(&brand[..end]).trim().to_string()
}

#[cfg(not(target_arch = "x86_64"))]
pub fn cpu_name() -> String {
    String::from("Unknown CPU")
}

/// CPUID leaf 0 vendor string. Intel RAPL / PawnIO modules are Intel-only.
#[cfg(target_arch = "x86_64")]
pub fn is_intel_cpu() -> bool {
    let r = core::arch::x86_64::__cpuid_count(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&r.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&r.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&r.ecx.to_le_bytes());
    &vendor == b"GenuineIntel"
}

#[cfg(not(target_arch = "x86_64"))]
pub fn is_intel_cpu() -> bool {
    false
}

pub fn total_memory_gb() -> String {
    let mut mem = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: GlobalMemoryStatusEx with valid struct.
    let ok = unsafe { GlobalMemoryStatusEx(&mut mem) };
    if ok != 0 && mem.ullTotalPhys > 0 {
        // Round to nearest GB.
        let gb = ((mem.ullTotalPhys as f64 / 1024.0 / 1024.0 / 1024.0).round()) as u64;
        format!("{} GB", gb)
    } else {
        "N/A".to_string()
    }
}

pub fn os_version() -> String {
    // Query registry once to avoid redundant syscalls.
    let key_path = to_wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let mut hkey: HKEY = std::ptr::null_mut();
    let key_opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key_path.as_ptr(),
            0,
            KEY_READ,
            &mut hkey,
        ) == ERROR_SUCCESS
    };

    let (edition, display, ubr) = if key_opened {
        let edition_wide = to_wide("ProductName");
        let display_wide = to_wide("DisplayVersion");
        let ubr_wide = to_wide("UBR");
        let e = registry_query_value(hkey, edition_wide.as_ptr());
        let d = registry_query_value(hkey, display_wide.as_ptr());
        let u = registry_query_dword_from(hkey, ubr_wide.as_ptr());
        unsafe { RegCloseKey(hkey) };
        (e, d, u)
    } else {
        (None, None, None)
    };

    // SAFETY: RtlGetVersion with valid struct.
    let (major, minor, build) = unsafe {
        let mut info = OsVersionInfoW {
            dw_os_version_info_size: std::mem::size_of::<OsVersionInfoW>() as u32,
            dw_major_version: 0,
            dw_minor_version: 0,
            dw_build_number: 0,
            dw_platform_id: 0,
            sz_csd_version: [0; 128],
        };
        if RtlGetVersion(&mut info) == 0 {
            (
                info.dw_major_version,
                info.dw_minor_version,
                info.dw_build_number,
            )
        } else {
            (0, 0, 0)
        }
    };

    let os_name = match major {
        10 => {
            if build >= 22000 {
                "Windows 11"
            } else {
                "Windows 10"
            }
        }
        6 => {
            if minor >= 3 {
                "Windows 8.1"
            } else if minor >= 2 {
                "Windows 8"
            } else if minor >= 1 {
                "Windows 7"
            } else {
                "Windows Vista"
            }
        }
        _ => "Windows",
    };

    let edition_str = match edition {
        Some(e) if !e.is_empty() => {
            let base = e
                .replace("Windows 10", "")
                .replace("Windows 11", "")
                .trim()
                .to_string();
            if base.is_empty() {
                os_name.to_string()
            } else {
                format!("{} {}", os_name, base)
            }
        }
        _ => os_name.to_string(),
    };

    let full_build = match ubr {
        Some(u) => format!("{}.{}", build, u),
        _ => build.to_string(),
    };

    match display {
        Some(d) if !d.is_empty() => format!("{} {} ({})", edition_str, d, full_build),
        _ => format!("{} ({})", edition_str, full_build),
    }
}

pub fn display_resolution() -> String {
    // SAFETY: GetSystemMetrics returns cached metrics.
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if w > 0 && h > 0 {
        format!("{}x{}", w, h)
    } else {
        String::new()
    }
}

pub fn work_area_size() -> Option<(i32, i32)> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut rect as *mut _ as *mut core::ffi::c_void,
            0,
        )
    };
    if ok != 0 && rect.right > rect.left && rect.bottom > rect.top {
        Some((rect.right - rect.left, rect.bottom - rect.top))
    } else {
        None
    }
}

pub fn display_refresh_rate() -> String {
    // SAFETY: GetDC/GetDeviceCaps/ReleaseDC with screen DC.
    unsafe {
        let hdc = GetDC(std::ptr::null_mut());
        if hdc.is_null() {
            return String::new();
        }
        let hz = GetDeviceCaps(hdc, VREFRESH);
        ReleaseDC(std::ptr::null_mut(), hdc);
        if hz > 0 {
            format!("{}Hz", hz)
        } else {
            String::new()
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn registry_query_value(hkey: HKEY, value_name: *const u16) -> Option<String> {
    let mut buf_size: u32 = 0;
    let mut data_type: u32 = 0;

    // SAFETY: Query required buffer size with null data ptr.
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_name,
            std::ptr::null_mut(),
            &mut data_type,
            std::ptr::null_mut(),
            &mut buf_size,
        )
    };

    if (rc != ERROR_SUCCESS && rc != ERROR_MORE_DATA) || data_type != REG_SZ || buf_size < 4 {
        return None;
    }

    let mut buf: Vec<u16> = vec![0u16; (buf_size / 2) as usize];
    let mut size = buf_size;

    // SAFETY: Second call with correctly sized buffer.
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_name,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut u8,
            &mut size,
        )
    };

    if rc != ERROR_SUCCESS {
        return None;
    }

    let len = (size / 2) as usize;
    if len > 0 && buf[len - 1] == 0 {
        let os_str = OsString::from_wide(&buf[..len - 1]);
        Some(os_str.to_string_lossy().into_owned())
    } else {
        let os_str = OsString::from_wide(&buf[..len]);
        Some(os_str.to_string_lossy().into_owned())
    }
}

fn registry_query_dword_from(hkey: HKEY, value_name: *const u16) -> Option<u32> {
    let mut data_type: u32 = 0;
    let mut buf: [u8; 4] = [0; 4];
    let mut buf_size: u32 = 4;
    // SAFETY: Valid handle and 4-byte buffer for REG_DWORD.
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_name,
            std::ptr::null_mut(),
            &mut data_type,
            buf.as_mut_ptr(),
            &mut buf_size,
        )
    };
    if rc != ERROR_SUCCESS || data_type != REG_DWORD || buf_size != 4 {
        return None;
    }
    Some(u32::from_le_bytes(buf))
}

const STARTUP_TASK_NAME: &str = "FrameworkCrate";
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Process-wide single-instance guard backed by Windows named mutex.
pub struct SingleInstanceGuard {
    _handle: *mut core::ffi::c_void,
}

// Handle is owned, not dereferenced; Move-only across threads.
unsafe impl Send for SingleInstanceGuard {}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if self._handle.is_null() {
            return;
        }
        unsafe {
            CloseHandle(self._handle);
        }
    }
}

impl SingleInstanceGuard {
    /// Tries to acquire named mutex; Err if another instance holds it.
    pub fn acquire(name: &str) -> Result<Self, ()> {
        const ERROR_ALREADY_EXISTS: u32 = 183;
        let wide = to_wide(name);
        // SAFETY: Null-terminated UTF-16 name; non-null handle with ERROR_ALREADY_EXISTS means owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
        if handle.is_null() {
            // Allow instance on rare creation failure.
            tracing::warn!("CreateMutexW failed; single-instance check skipped");
            return Ok(Self { _handle: handle });
        }
        // SAFETY: GetLastError immediately after CreateMutexW.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if exists {
            // Close handle to existing mutex; owning instance keeps it alive.
            unsafe {
                CloseHandle(handle);
            }
            return Err(());
        }
        Ok(Self { _handle: handle })
    }
}

/// Whether app is registered to launch at startup via scheduled task.
pub fn startup_launch_enabled() -> bool {
    std::process::Command::new("schtasks")
        .args(["/Query", "/TN", STARTUP_TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Registers or removes Windows startup scheduled task (requires elevation for HIGHEST).
pub fn set_startup_launch(enabled: bool) -> Result<(), String> {
    let output = if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("cannot resolve exe path: {e}"))?;
        let exe_str = exe.to_str().ok_or("exe path is not valid UTF-8")?;
        // Reject characters that would break schtasks command line or allow injection.
        // schtasks /TR is parsed as a single command line; we wrap the exe in
        // double quotes via raw_arg, so any embedded " would break out.
        if exe_str.contains('"')
            || exe_str.contains('\'')
            || exe_str.contains('&')
            || exe_str.contains('|')
            || exe_str.contains(';')
            || exe_str.contains('%')
            || exe_str.contains('^')
            || exe_str.contains('\n')
            || exe_str.contains('\r')
        {
            return Err("exe path contains invalid characters".to_string());
        }
        // A trailing backslash before the closing quote would escape it
        // (e.g. "C:\path\" --minimized" → the \" becomes an escaped quote).
        if exe_str.ends_with('\\') {
            return Err("exe path must not end with backslash".to_string());
        }
        let mut cmd = std::process::Command::new("schtasks");
        cmd.args(["/Create", "/TN", STARTUP_TASK_NAME, "/TR"]);
        // Use raw_arg to pass the /TR value exactly as "\"<exe>\" --minimized"
        // without Command's extra quoting; exe is already validated to contain
        // no double quotes, so wrapping in quotes is safe.
        cmd.raw_arg(format!("\"{}\" --minimized", exe_str));
        cmd.args(["/SC", "ONLOGON", "/RL", "HIGHEST", "/F"]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.output()
            .map_err(|e| format!("failed to run schtasks: {e}"))?
    } else {
        std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", STARTUP_TASK_NAME, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("failed to run schtasks: {e}"))?
    };
    if output.status.success() {
        return Ok(());
    }
    // schtasks errors may be non-UTF8 (locale/OEM codepage); use lossy conversion.
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let detail = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        format!("exit code {}", output.status.code().unwrap_or(-1))
    };
    if detail.is_empty() {
        Err(format!(
            "schtasks failed (exit code {})",
            output.status.code().unwrap_or(-1)
        ))
    } else {
        Err(detail)
    }
}

// Tray icon functions: SAFETY hwnd is valid handle from FindWindowW/CreateWindowExW.

pub fn find_window_by_title(title: &str) -> Option<isize> {
    let wide = to_wide(title);
    // SAFETY: FindWindowW with null class matches any class.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as isize)
    }
}

/// Hidden tray message window class.
const TRAY_WINDOW_CLASS: &str = "FrameworkControlTray";

/// Message ID for second instance to request restore of parked window.
pub fn show_request_message_id() -> u32 {
    let wide = to_wide("FrameworkCrateShow");
    unsafe { RegisterWindowMessageW(wide.as_ptr()) }
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

/// Adds tray icon; SAFETY hwnd and HICON must be valid.
pub fn shell_notify_add(hwnd: isize, icon: isize, tip: &str, callback_msg: u32) -> bool {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as *mut core::ffi::c_void;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = callback_msg;
    nid.hIcon = icon as *mut core::ffi::c_void;
    let tip_wide = to_wide(tip);
    let copy_len = tip_wide.len().min(127);
    nid.szTip[..copy_len].copy_from_slice(&tip_wide[..copy_len]);
    // Keep buffer NUL-terminated when tip exceeds limit.
    nid.szTip[copy_len] = 0;
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

    #[test]
    fn is_intel_cpu_does_not_panic() {
        let _ = super::is_intel_cpu();
    }

    #[test]
    fn startup_launch_scheduled_task_round_trip() {
        // Round-trip: enable creates the ONLOGON task, disable deletes it.
        // Registering a /RL HIGHEST task requires elevation, so skip
        // silently when running unelevated (debug tests / CI).
        match super::set_startup_launch(true) {
            Ok(()) => {
                assert!(
                    super::startup_launch_enabled(),
                    "task should exist after enable"
                );
                super::set_startup_launch(false).expect("disable should succeed");
                assert!(
                    !super::startup_launch_enabled(),
                    "task should be gone after disable"
                );
            }
            Err(_) => {
                eprintln!("skipping startup task round-trip: not elevated");
            }
        }
    }
}
