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
const KEY_WOW64_64KEY: u32 = 0x0100;
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

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, RECT};
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TokenElevation};
use windows_sys::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, FindWindowExW, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowPlacement, IsIconic, IsWindow, IsZoomed, PostMessageW, RegisterWindowMessageW,
    SPI_GETWORKAREA, SetForegroundWindow, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    ShowWindow, SystemParametersInfoW, TrackPopupMenu, WINDOWPLACEMENT,
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
    // Pin the 64-bit view: a 32-bit build would otherwise be redirected to
    // Wow6432Node and report the wrong system version.
    let key_opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key_path.as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
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

    if (rc != ERROR_SUCCESS && rc != ERROR_MORE_DATA) || data_type != REG_SZ {
        return None;
    }
    // An empty REG_SZ is just the nul terminator (2 bytes); it is a valid
    // empty string, not a read failure. Anything smaller is malformed.
    if buf_size < 2 {
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
    /// Tries to acquire the named mutex.
    ///
    /// `Ok(Some(_))` acquired it, `Ok(None)` another instance already holds it.
    /// `Err` means the mutex could not be created at all (e.g. handle
    /// exhaustion), which is deliberately distinct from "already running": the
    /// caller must not exit silently in that case, or a scheduled logon launch
    /// looks like it did nothing.
    pub fn acquire(name: &str) -> Result<Option<Self>, std::io::Error> {
        const ERROR_ALREADY_EXISTS: u32 = 183;
        let wide = to_wide(name);
        // SAFETY: Null-terminated UTF-16 name; non-null handle with ERROR_ALREADY_EXISTS means owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
        if handle.is_null() {
            // SAFETY: null handle, nothing to close.
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: GetLastError immediately after CreateMutexW.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if exists {
            // Close handle to existing mutex; owning instance keeps it alive.
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Self { _handle: handle }))
    }
}

/// Whether app is registered to launch at startup via scheduled task.
pub fn startup_launch_enabled() -> bool {
    schtasks_output(&["/Query", "/TN", STARTUP_TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Runs schtasks with a 5s timeout so a hung Task Scheduler service cannot
/// freeze the UI thread (toggle handler) or delay startup.
/// Decodes console child-process output. schtasks errors on zh-TW systems
/// are in the OEM codepage (cp950), not UTF-8 — from_utf8_lossy would
/// render them as mojibake and hide the real cause.
pub(crate) fn decode_console_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    // SAFETY: GetOEMCP takes no args; MultiByteToWideChar with valid buffers.
    unsafe {
        let cp = GetOEMCP();
        let len = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if len <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; len as usize];
        let written = MultiByteToWideChar(
            cp,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            len,
        );
        if written <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&wide[..written as usize])
    }
}

/// Runs schtasks with a 5s timeout so a hung Task Scheduler service cannot
/// freeze the UI thread (toggle handler) or delay startup.
/// The task action is registered through `/XML`, so no argument value has to
/// survive schtasks' own command-line re-parsing.
fn schtasks_output(args: &[&str]) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    // Own the arguments so nothing borrows the caller across the wait below.
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut cmd = std::process::Command::new(crate::util::system32_exe("schtasks.exe"));
    cmd.args(&args);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run schtasks: {e}"))?;
    // Drain pipes on threads so a chatty child can never block on a full pipe
    // while the main thread polls for exit.
    let mut stdout = child.stdout.take();
    let out_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(ref mut o) = stdout {
            let _ = o.read_to_end(&mut buf);
        }
        buf
    });
    let mut stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(ref mut e) = stderr {
            let _ = e.read_to_end(&mut buf);
        }
        buf
    });
    let start = std::time::Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("failed to run schtasks: {e}"))?
        {
            Some(status) => {
                let stdout = out_thread.join().unwrap_or_default();
                let stderr = err_thread.join().unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None => {
                if start.elapsed() >= std::time::Duration::from_secs(5) {
                    // Kill on timeout: a /Create that lands after we already
                    // reported failure would enable startup the UI claims is
                    // off (or vice versa).
                    let _ = child.kill();
                    let reap_deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(1);
                    while child.try_wait().ok().flatten().is_none()
                        && std::time::Instant::now() < reap_deadline
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    return Err("schtasks timed out after 5s".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Escapes text for inclusion in the startup task XML.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Task Scheduler's `/XML` importer only accepts UTF-16. A UTF-8 file is
/// rejected with a parse error that points at the XML declaration, so the
/// document is encoded to UTF-16LE with a BOM here.
fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(s.len() * 2 + 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Builds the logon startup task XML. `schtasks.exe` cannot express power or
/// retry policy, so the task is registered from XML to apply them in one
/// atomic operation, instead of a follow-up PowerShell call that can fail
/// silently and leave the task refusing to start on battery.
/// Settings that matter for a long-lived tray app:
/// - battery conditions cleared: schtasks defaults them to true, which makes a
///   laptop on battery silently skip the task.
/// - `ExecutionTimeLimit` `PT0S`: schtasks defaults to `PT72H`, which stops the
///   tray app three days after logon and never restarts it.
/// - `RestartOnFailure`: retry a transient startup failure instead of giving up.
///
/// Child element order follows the schema; Task Scheduler rejects a document
/// whose `Settings` children are out of order.
fn build_task_xml(command: &str, args: &str, user_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Framework Crate</Author>
    <Description>Launch Framework Crate minimized to the notification area at logon.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
  </Settings>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user_id),
        command = xml_escape(command),
        args = xml_escape(args),
    )
}

/// Whether the process runs with an elevated (administrator) token.
///
/// The startup task is registered with `RunLevel=HighestAvailable`, so
/// registering it requires elevation. Reported to the user as a readable
/// message instead of the localized schtasks "access denied" text.
pub fn is_elevated() -> bool {
    const TOKEN_QUERY: u32 = 0x0008;
    // SAFETY: pseudo-handle is always valid; the token handle is closed below.
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned: u32 = 0;
    // SAFETY: token is a live handle and the buffer matches TOKEN_ELEVATION.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            std::ptr::from_mut(&mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    } != 0;
    // SAFETY: token was opened above and is not used afterwards.
    unsafe { CloseHandle(token) };
    ok && elevation.TokenIsElevated != 0
}

/// Account for the task principal. Task Scheduler resolves `DOMAIN\user` to a
/// SID while registering, so a later account rename does not break the task.
///
/// Resolved from the process token, never from `USERNAME`/`USERDOMAIN`:
/// environment variables are caller-controlled and must not name the account
/// an elevated task runs as.
fn current_user_id() -> Result<String, String> {
    use windows_sys::Win32::Security::{LookupAccountSidW, SID_NAME_USE, TOKEN_USER, TokenUser};
    use windows_sys::core::{PCWSTR, PWSTR};

    const TOKEN_QUERY: u32 = 0x0008;
    // SAFETY: pseudo-handle is always valid; the token handle is closed below.
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err("cannot open process token for startup task".to_string());
    }
    // Size query first: expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let mut len: u32 = 0;
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        // SAFETY: token was opened above and is not used afterwards.
        unsafe { CloseHandle(token) };
        return Err("cannot size token user for startup task".to_string());
    }
    // u64 backing keeps the TOKEN_USER struct aligned.
    let mut buf = vec![0u64; len as usize / 8 + 1];
    // SAFETY: buffer sized by the query above; token is live.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            (buf.len() * 8) as u32,
            &mut len,
        )
    };
    if ok == 0 {
        // SAFETY: token was opened above and is not used afterwards.
        unsafe { CloseHandle(token) };
        return Err("cannot read token user for startup task".to_string());
    }
    // SAFETY: buffer holds a valid TOKEN_USER per the successful call above.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    // SAFETY: token was opened above and is not used afterwards.
    unsafe { CloseHandle(token) };

    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut sid_use: SID_NAME_USE = 0;
    // SAFETY: null buffers query the required lengths; may fail, ignored.
    unsafe {
        LookupAccountSidW(
            std::ptr::null() as PCWSTR,
            sid,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut sid_use,
        )
    };
    if name_len == 0 {
        return Err("cannot resolve account name for startup task".to_string());
    }
    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: buffers sized by the query above; sid came from our own token.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null() as PCWSTR,
            sid,
            name.as_mut_ptr() as PWSTR,
            &mut name_len,
            domain.as_mut_ptr() as PWSTR,
            &mut domain_len,
            &mut sid_use,
        )
    };
    if ok == 0 {
        return Err("cannot resolve account name for startup task".to_string());
    }
    let trim_nul = |v: &[u16]| {
        let n = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        String::from_utf16_lossy(&v[..n])
    };
    let user = trim_nul(&name);
    if user.is_empty() {
        return Err("cannot determine current user for startup task".to_string());
    }
    let domain = trim_nul(&domain);
    if domain.is_empty() {
        Ok(user)
    } else {
        Ok(format!("{domain}\\{user}"))
    }
}

fn known_common_app_data() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::UI::Shell::SHGetFolderPathW;
    const CSIDL_COMMON_APPDATA: i32 = 0x0023;
    const MAX_PATH: usize = 260;
    let mut buf = [0u16; MAX_PATH];
    let ok = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            CSIDL_COMMON_APPDATA,
            std::ptr::null_mut(),
            0,
            buf.as_mut_ptr(),
        )
    };
    if ok != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(MAX_PATH);
    String::from_utf16(&buf[..len])
        .ok()
        .map(std::path::PathBuf::from)
}

fn task_dir() -> std::io::Result<std::path::PathBuf> {
    #[cfg(any(debug_assertions, test))]
    if !is_elevated() {
        return Ok(std::env::temp_dir());
    }
    let base = known_common_app_data()
        .ok_or_else(|| std::io::Error::other("cannot resolve ProgramData"))?
        .join("FrameworkCrate");
    create_admin_directory(&base)?;
    let dir = base.join("tasks");
    create_admin_directory(&dir)?;
    Ok(dir)
}

/// Creates an Administrators-owned directory with an admin/SYSTEM-only DACL.
fn create_admin_directory(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAceEx, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
        InitializeAcl, InitializeSecurityDescriptor, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::core::PCWSTR;

    enable_restore_privilege()?;
    let mut admin_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut admin_len = admin_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            admin_sid.as_mut_ptr() as PSID,
            &mut admin_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut system_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut system_len = system_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            std::ptr::null_mut(),
            system_sid.as_mut_ptr() as PSID,
            &mut system_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }

    let mut acl_buf = [0u32; 64];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    if unsafe { InitializeAcl(acl, std::mem::size_of_val(&acl_buf) as u32, ACL_REVISION) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    for sid in [admin_sid.as_ptr() as PSID, system_sid.as_ptr() as PSID] {
        if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, NO_INHERITANCE, GENERIC_ALL, sid) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let sd_ptr = &mut sd as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR;
    if unsafe { InitializeSecurityDescriptor(sd_ptr, 1) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetSecurityDescriptorOwner(sd_ptr, admin_sid.as_ptr() as PSID, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetSecurityDescriptorDacl(sd_ptr, 1, acl, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd_ptr,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &sa) };
    if created != 0 {
        if !task_file_owner_is_administrators(path)? {
            let _ = std::fs::remove_dir(path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "task directory owner is not Administrators",
            ));
        }
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(183) {
        if crate::config::dir_has_reparse_point(path) || !path.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "task directory is not a plain admin directory: {}",
                    path.display()
                ),
            ));
        }
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr() as PCWSTR,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                admin_sid.as_ptr() as PSID,
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        if !task_file_owner_is_administrators(path)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "task directory owner is not Administrators",
            ));
        }
        return Ok(());
    }
    Err(err)
}

/// Prefix of the temporary task document written for `schtasks /Create /XML`.
const TASK_XML_PREFIX: &str = "framework-crate-task-";

/// Per-process counter so concurrent toggles and PID reuse never map two
/// task documents onto one path.
static TASK_XML_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(any(debug_assertions, test))]
fn create_plain_task_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    let meta = f.metadata()?;
    if !meta.is_file() || meta.len() != bytes.len() as u64 {
        return Err(std::io::Error::other(
            "startup task xml failed post-write verification",
        ));
    }
    Ok(())
}

fn enable_restore_privilege() -> std::io::Result<()> {
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        SE_RESTORE_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let result = (|| {
        let mut luid = windows_sys::Win32::Foundation::LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_RESTORE_NAME, &mut luid) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &state,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let err = unsafe { GetLastError() };
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err as i32));
        }
        Ok(())
    })();
    unsafe { CloseHandle(token) };
    result
}

fn task_file_owner_is_administrators(path: &std::path::Path) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, PSID, SECURITY_MAX_SID_SIZE, WinBuiltinAdministratorsSid,
    };
    let mut expected = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut expected_len = expected.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            expected.as_mut_ptr() as PSID,
            &mut expected_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSID = std::ptr::null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 || owner.is_null() {
        if !sd.is_null() {
            unsafe { LocalFree(sd) };
        }
        return Err(if rc != 0 {
            std::io::Error::from_raw_os_error(rc as i32)
        } else {
            std::io::Error::other("startup task xml owner SID missing")
        });
    }
    let matches = unsafe { EqualSid(owner, expected.as_ptr() as PSID) } != 0;
    unsafe { LocalFree(sd) };
    Ok(matches)
}

fn create_admin_only_task_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAceEx, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
        InitializeAcl, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES, WRITE_DAC,
        WRITE_OWNER,
    };

    enable_restore_privilege()?;
    let mut admin_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut admin_len = admin_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            admin_sid.as_mut_ptr() as PSID,
            &mut admin_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut system_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut system_len = system_sid.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            std::ptr::null_mut(),
            system_sid.as_mut_ptr() as PSID,
            &mut system_len,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }

    let mut acl_buf = [0u32; 64];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    if unsafe { InitializeAcl(acl, std::mem::size_of_val(&acl_buf) as u32, ACL_REVISION) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    for sid in [admin_sid.as_ptr() as PSID, system_sid.as_ptr() as PSID] {
        if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, NO_INHERITANCE, GENERIC_ALL, sid) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE | FILE_READ_ATTRIBUTES | WRITE_DAC | WRITE_OWNER,
            0,
            std::ptr::null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    let rc = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            admin_sid.as_ptr() as PSID,
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() != bytes.len() as u64 {
            return Err(std::io::Error::other(
                "startup task xml failed post-write verification",
            ));
        }
        Ok(())
    })();
    drop(file);
    let result = result.and_then(|()| {
        if task_file_owner_is_administrators(path)? {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "startup task xml owner is not Administrators",
            ))
        }
    });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn create_task_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(any(debug_assertions, test))]
    if !is_elevated() {
        return create_plain_task_file(path, bytes);
    }
    create_admin_only_task_file(path, bytes)
}

/// Writes the task document to a uniquely-named temp file.
fn write_task_xml(xml: &str) -> Result<String, String> {
    let dir = task_dir().map_err(|e| format!("cannot resolve startup task directory: {e}"))?;
    for _ in 0..5 {
        // Unpredictable suffix: timestamp-nanos + pid + counter hashed, so a
        // same-user attacker can't pre-plant the path.
        let nonce = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            crate::util::monotonic_ms().hash(&mut h);
            crate::util::current_time_ms().hash(&mut h);
            std::process::id().hash(&mut h);
            TASK_XML_COUNTER
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                .hash(&mut h);
            (&TASK_XML_COUNTER as *const _ as usize).hash(&mut h);
            format!("{:016x}{:016x}", h.finish(), crate::util::monotonic_ms())
        };
        let xml_path = dir.join(format!("{TASK_XML_PREFIX}{nonce}.xml"));
        let bytes = utf16le_with_bom(xml);
        match create_task_file(&xml_path, &bytes) {
            Ok(()) => return Ok(xml_path.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                let _ = std::fs::remove_file(&xml_path);
                return Err(format!("cannot write startup task xml: {e}"));
            }
        }
    }
    Err("cannot allocate a unique startup task xml path".to_string())
}

/// Removes task documents left behind by a previous run.
///
/// The release profile uses `panic = "abort"`, so a process killed mid-write
/// (or by Task Scheduler) never reaches the removal and the file would
/// otherwise accumulate in the temp directory forever.
fn cleanup_task_xml_in(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(TASK_XML_PREFIX) && name.ends_with(".xml") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn cleanup_stale_task_xml() {
    let temp = std::env::temp_dir();
    cleanup_task_xml_in(&temp);
    if let Ok(dir) = task_dir()
        && dir != temp
    {
        cleanup_task_xml_in(&dir);
    }
}

/// Validates an executable path destined for the task action.
///
/// Rejects characters that would break the task XML or allow injection. The
/// path is embedded as XML text and wrapped in quotes for the action Command,
/// so quotes, cmd metacharacters and control characters are all refused here.
fn validate_task_command_path(exe: &str) -> Result<(), String> {
    const FORBIDDEN: &[char] = &[
        '"', '\'', '&', '|', ';', '%', '^', '`', '$', '(', ')', '<', '>', '*', '?', '{', '}', '!',
        '\n', '\r',
    ];
    if exe.contains(FORBIDDEN) {
        return Err("exe path contains invalid characters".to_string());
    }
    // Block non-ASCII control/bidi override that could hide injection.
    if exe
        .chars()
        .any(|c| c.is_control() || c == '\u{202E}' || c == '\u{202D}')
    {
        return Err("exe path contains invalid unicode".to_string());
    }
    // A trailing backslash would escape the closing quote of the action
    // Command (e.g. "C:\path\" → the \" becomes an escaped quote).
    if exe.ends_with('\\') {
        return Err("exe path must not end with backslash".to_string());
    }
    // HighestAvailable runs elevated at logon: an exe in a user-writable
    // dir (Downloads/Desktop/Temp) can be replaced by anyone to gain
    // elevation. Warn loudly so portable runs are a conscious choice.
    let lower = exe.to_lowercase();
    for risky in [
        "downloads",
        "desktop",
        "\\temp\\",
        "\\tmp\\",
        "appdata\\local\\temp",
    ] {
        if lower.contains(risky) {
            tracing::warn!(
                "startup exe is in a user-writable dir ({}); move to Program Files for HighestAvailable safety",
                exe
            );
            break;
        }
    }
    Ok(())
}

/// Builds the startup task document for `exe`.
///
/// Pure: no I/O and no environment lookups, so the startup settings are
/// verifiable by `cargo test` without administrator rights. The caller is
/// responsible for writing the UTF-16 encoded document and invoking schtasks.
pub(crate) fn build_startup_task(exe: &str, user_id: &str) -> Result<String, String> {
    validate_task_command_path(exe)?;
    // Command and Arguments are separate XML elements, so the exe path is never
    // re-parsed as a command line and cannot split into extra arguments the way
    // a schtasks /TR value did.
    Ok(build_task_xml(
        &format!("\"{exe}\""),
        "--minimized",
        user_id,
    ))
}

/// Registers or removes Windows startup scheduled task (requires elevation for HIGHEST).
pub fn set_startup_launch(enabled: bool) -> Result<(), String> {
    let output = if enabled {
        cleanup_stale_task_xml();
        let exe = std::env::current_exe().map_err(|e| format!("cannot resolve exe path: {e}"))?;
        let exe_str = exe.to_str().ok_or("exe path is not valid UTF-8")?;
        // Bounded by schtasks_output timeout so a hung Task Scheduler cannot freeze the UI.
        let xml = build_startup_task(exe_str, &current_user_id()?)?;
        let xml_path = write_task_xml(&xml)?;
        let created =
            schtasks_output(&["/Create", "/TN", STARTUP_TASK_NAME, "/XML", &xml_path, "/F"]);
        // Remove the temp document whether or not registration succeeded.
        let _ = std::fs::remove_file(&xml_path);
        let out = created.map_err(|e| format!("failed to run schtasks: {e}"))?;
        if out.status.success() && !startup_launch_enabled() {
            // Registration reported success but the task is not queryable, so
            // startup would silently not happen. Surface it instead of
            // returning a green toggle.
            return Err("task was created but cannot be queried; startup may not work".to_string());
        }
        out
    } else {
        schtasks_output(&["/Delete", "/TN", STARTUP_TASK_NAME, "/F"])
            .map_err(|e| format!("failed to run schtasks: {e}"))?
    };
    if output.status.success() {
        return Ok(());
    }
    // The task is registered with RunLevel=HighestAvailable, so a non-elevated
    // process is the common cause. Report that directly instead of surfacing
    // the localized schtasks text, which does not say what to do about it.
    if enabled && !is_elevated() {
        return Err(
            "launch at startup needs administrator privileges; restart Framework Crate as administrator and try again"
                .to_string(),
        );
    }
    // schtasks errors follow the OEM codepage (cp950 on zh-TW); decode
    // accordingly so the message is readable instead of mojibake.
    let stderr = decode_console_bytes(&output.stderr);
    let stdout = decode_console_bytes(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        // schtasks reported nothing at all, so name the command and the code
        // rather than surfacing a bare "exit code 1" with no context.
        format!(
            "schtasks failed with no output (exit code {})",
            output.status.code().unwrap_or(-1)
        )
    };
    Err(detail)
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
    fn task_xml_sets_startup_critical_settings() {
        let xml = build_task_xml("\"C:\\app\\framework-crate.exe\"", "--minimized", "DOM\\u");

        // schtasks defaults would silently skip a battery-powered logon.
        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"));
        // PT72H would stop the tray app three days after logon.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S<"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<RunLevel>HighestAvailable<"));
        assert!(xml.contains("<LogonType>InteractiveToken<"));
        // Command and Arguments stay separate so the path is never re-parsed.
        assert!(xml.contains("<Command>&quot;C:\\app\\framework-crate.exe&quot;<"));
        assert!(xml.contains("<Arguments>--minimized<"));
        assert!(xml.contains("<UserId>DOM\\u<"));
    }

    #[test]
    fn startup_task_rejects_dangerous_paths_offline() {
        // Runs without elevation, unlike the schtasks round-trip test below.
        let base = r"C:\app\framework-crate.exe";
        assert!(
            super::build_startup_task(base, r"DOM\u").is_ok(),
            "a plain path must be accepted"
        );

        for bad in [
            r#"C:\a"b.exe"#,
            r"C:\a&b.exe",
            r"C:\a|b.exe",
            r"C:\a;b.exe",
            r"C:\a%b.exe",
            r"C:\a^b.exe",
            r"C:\a`b.exe",
            r"C:\a$b.exe",
            r"C:\a(b).exe",
            r"C:\a<b>.exe",
            r"C:\a*b.exe",
            r"C:\a?b.exe",
            r"C:\a{b}.exe",
            r"C:\a!b.exe",
            r"C:\a'b.exe",
            "C:\\a\nb.exe",
            "C:\\a\rb.exe",
            "C:\\a\u{202E}b.exe",
            "C:\\a\u{7}b.exe",
            r"C:\app\",
        ] {
            assert!(
                super::build_startup_task(bad, r"DOM\u").is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn startup_task_xml_is_verifiable_without_elevation() {
        let xml =
            super::build_startup_task(r"C:\app\framework-crate.exe", r"DOM\u").expect("valid path");

        // schtasks defaults would silently skip a battery-powered logon.
        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"));
        // PT72H would stop the tray app three days after logon.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S<"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<RunLevel>HighestAvailable<"));
        assert!(xml.contains("<LogonType>InteractiveToken<"));
        // Command and Arguments stay separate so the path is never re-parsed.
        assert!(xml.contains(r"<Command>&quot;C:\app\framework-crate.exe&quot;<"));
        assert!(xml.contains("<Arguments>--minimized<"));
        assert!(xml.contains(r"<UserId>DOM\u<"));
    }

    #[test]
    fn task_xml_escapes_metacharacters() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        let xml = build_task_xml("\"C:\\a&b\\app.exe\"", "--minimized", "D&u");
        assert!(xml.contains("<UserId>D&amp;u<"));
        assert!(!xml.contains("<UserId>D&u<"));
    }

    #[test]
    fn task_xml_encoding_has_utf16le_bom() {
        let bytes = utf16le_with_bom("<?xml version=\"1.0\" encoding=\"UTF-16\"?>");
        // schtasks rejects a UTF-8 document with a parse error on the declaration.
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let text = String::from_utf16(
            &bytes[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect::<Vec<u16>>(),
        )
        .expect("valid UTF-16");
        assert_eq!(text, "<?xml version=\"1.0\" encoding=\"UTF-16\"?>");
    }

    #[test]
    fn current_user_id_ignores_spoofed_env() {
        // The task principal must come from the token: poison the env
        // first and prove the result is unaffected.
        let prev_user = std::env::var_os("USERNAME");
        let prev_domain = std::env::var_os("USERDOMAIN");
        unsafe {
            std::env::set_var("USERNAME", "spoofed-user-xyz");
            std::env::set_var("USERDOMAIN", "SPOOFED-DOMAIN-XYZ");
        }
        let id = super::current_user_id().expect("token user must resolve");
        assert!(!id.is_empty());
        assert!(
            !id.to_lowercase().contains("spoofed"),
            "principal must not trust env, got {id}"
        );
        unsafe {
            if let Some(v) = prev_user {
                std::env::set_var("USERNAME", v);
            } else {
                std::env::remove_var("USERNAME");
            }
            if let Some(v) = prev_domain {
                std::env::set_var("USERDOMAIN", v);
            } else {
                std::env::remove_var("USERDOMAIN");
            }
        }
    }

    #[test]
    fn registry_empty_sz_reads_as_empty_string() {
        // Round-trips an empty REG_SZ through HKCU: exercises the
        // sub-nul-size path that used to be misreported as missing.
        use windows_sys::Win32::System::Registry::{
            RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW,
        };
        const HKEY_CURRENT_USER: HKEY = 0x80000001 as HKEY;
        const KEY_SET_VALUE: u32 = 0x0002;
        let sub = to_wide(&format!(
            "SOFTWARE\\FrameworkCrateRegTest{}",
            std::process::id()
        ));
        let name = to_wide("EmptyValue");
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: null class/security/disposition; handle closed + key deleted below.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                0,
                std::ptr::null_mut(),
                0,
                KEY_READ | KEY_SET_VALUE,
                std::ptr::null_mut(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "create test key");
        let nul = [0u16; 1];
        let rc =
            unsafe { RegSetValueExW(hkey, name.as_ptr(), 0, REG_SZ, nul.as_ptr() as *const u8, 2) };
        assert_eq!(rc, ERROR_SUCCESS, "write empty REG_SZ");
        assert_eq!(
            registry_query_value(hkey, name.as_ptr()),
            Some(String::new()),
            "empty REG_SZ is empty, not missing"
        );
        unsafe { RegCloseKey(hkey) };
        unsafe {
            RegDeleteKeyW(HKEY_CURRENT_USER, sub.as_ptr());
        }
    }

    #[test]
    fn elevated_task_xml_is_owned_by_administrators() {
        if !super::is_elevated() {
            return;
        }
        let path = super::write_task_xml("<a/>").expect("write");
        assert!(
            super::task_file_owner_is_administrators(std::path::Path::new(&path))
                .expect("owner query")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn task_xml_paths_are_unique_per_call() {
        // create_new refuses planted paths, so uniqueness must come from
        // the name itself (pid + counter), not from overwriting. Assert
        // only the names: file-existence checks would race the stale-file
        // sweeper that other tests trigger in parallel.
        let a = super::write_task_xml("<a/>").expect("first write");
        let b = super::write_task_xml("<b/>").expect("second write");
        assert_ne!(a, b, "two task documents must not share a path");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
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
