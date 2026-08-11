use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};

#[allow(clippy::upper_case_acronyms)]
type HKEY = *mut core::ffi::c_void;
#[allow(clippy::upper_case_acronyms)]
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
const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;
use crate::tray::event::{ID_SHOW, ID_QUIT};

#[repr(C)]
struct MemoryStatusEx {
    dw_length: u32,
    dw_memory_load: u32,
    ull_total_phys: u64,
    ull_avail_phys: u64,
    ull_total_page_file: u64,
    ull_avail_page_file: u64,
    ull_total_virtual: u64,
    ull_avail_virtual: u64,
    ull_avail_extended_virtual: u64,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetSystemMetrics(nIndex: i32) -> i32;
    fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
}

#[allow(clippy::upper_case_acronyms)]
type HDC = *mut core::ffi::c_void;
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
extern "system" {
    fn RtlGetVersion(version_info: *mut OsVersionInfoW) -> u32;
}

#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: *mut core::ffi::c_void) -> HDC;
    fn ReleaseDC(hwnd: *mut core::ffi::c_void, hdc: HDC) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn GetDeviceCaps(hdc: HDC, index: i32) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        hKey: HKEY,
        lpSubKey: LPCWSTR,
        ulOptions: u32,
        samDesired: u32,
        phkResult: *mut HKEY,
    ) -> u32;
    fn RegQueryValueExW(
        hKey: HKEY,
        lpValueName: LPCWSTR,
        lpReserved: *mut u32,
        lpType: *mut u32,
        lpData: *mut u8,
        lpcbData: *mut u32,
    ) -> u32;
    fn RegCloseKey(hKey: HKEY) -> u32;
}

#[link(name = "user32")]
extern "system" {
    fn FindWindowW(lpClassName: LPCWSTR, lpWindowName: LPCWSTR) -> *mut core::ffi::c_void;
    fn ShowWindow(hWnd: *mut core::ffi::c_void, nCmdShow: i32) -> i32;
    fn SetForegroundWindow(hWnd: *mut core::ffi::c_void) -> i32;
    fn IsIconic(hWnd: *mut core::ffi::c_void) -> i32;
    fn IsWindow(hWnd: *mut core::ffi::c_void) -> i32;
    fn PostMessageW(hWnd: *mut core::ffi::c_void, msg: u32, wParam: usize, lParam: isize) -> i32;
    fn CreatePopupMenu() -> *mut core::ffi::c_void;
    fn AppendMenuW(hMenu: *mut core::ffi::c_void, uFlags: u32, uIDNewItem: usize, lpNewItem: LPCWSTR) -> i32;
    fn TrackPopupMenu(hMenu: *mut core::ffi::c_void, uFlags: u32, x: i32, y: i32, nReserved: i32, hWnd: *mut core::ffi::c_void, prcRect: *const core::ffi::c_void) -> i32;
    fn DestroyMenu(hMenu: *mut core::ffi::c_void) -> i32;
    fn GetWindowPlacement(hWnd: *mut core::ffi::c_void, lpwndpl: *mut WINDOWPLACEMENT) -> i32;
    fn SetWindowPlacement(hWnd: *mut core::ffi::c_void, lpwndpl: *const WINDOWPLACEMENT) -> i32;
    fn GetWindowLongPtrW(hWnd: *mut core::ffi::c_void, nIndex: i32) -> isize;
    fn SetWindowLongPtrW(hWnd: *mut core::ffi::c_void, nIndex: i32, dwNewLong: isize) -> isize;
    fn SetFocus(hWnd: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn SetWindowPos(
        hWnd: *mut core::ffi::c_void,
        hWndInsertAfter: *mut core::ffi::c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        uFlags: u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_snake_case)]
struct WINDOWPLACEMENT {
    length: u32,
    flags: u32,
    showCmd: u32,
    ptMinPosition: POINT,
    ptMaxPosition: POINT,
    rcNormalPosition: RECT,
}

const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOOLWINDOW: isize = 0x00000080;
/// winit sets WS_EX_APPWINDOW by default (window_state.rs ON_TASKBAR).
/// It forces a taskbar button even when WS_EX_TOOLWINDOW is set, so it must
/// be cleared while parked and restored afterwards.
const WS_EX_APPWINDOW: isize = 0x0004_0000;
const SW_SHOWNOACTIVATE: u32 = 4;
const SW_RESTORE: u32 = 9;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_FRAMECHANGED: u32 = 0x0020;
/// Classic off-screen parking coordinates (far outside the virtual screen).
const OFFSCREEN: i32 = -32000;

/// Placement saved when the window is parked, used to restore its on-screen
/// position and size.
static SAVED_PLACEMENT: std::sync::Mutex<Option<WINDOWPLACEMENT>> = std::sync::Mutex::new(None);

/// "Hide" the window by parking it off-screen instead of SW_HIDE. The window
/// stays WS_VISIBLE, so WM_PAINT keeps arriving and the swapchain stays valid
/// while hidden — on restore there is no white/blank frame.
pub fn hide_window_to_tray(hwnd: isize) {
    let h = hwnd as *mut core::ffi::c_void;
    // SAFETY: hwnd is a valid window handle from FindWindowW/CreateWindowExW.
    unsafe {
        let mut placement = std::mem::zeroed::<WINDOWPLACEMENT>();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(h, &mut placement) != 0 {
            *SAVED_PLACEMENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(placement);
        }
        let width = (placement.rcNormalPosition.right - placement.rcNormalPosition.left).max(100);
        let height = (placement.rcNormalPosition.bottom - placement.rcNormalPosition.top).max(100);
        let parked = WINDOWPLACEMENT {
            length: placement.length,
            flags: 0,
            showCmd: SW_SHOWNOACTIVATE,
            ptMinPosition: placement.ptMinPosition,
            ptMaxPosition: placement.ptMaxPosition,
            rcNormalPosition: RECT {
                left: OFFSCREEN,
                top: OFFSCREEN,
                right: OFFSCREEN + width,
                bottom: OFFSCREEN + height,
            },
        };
        // Un-minimize/un-maximize at the parked position without activating.
        SetWindowPlacement(h, &parked);
        // Remove taskbar / alt-tab presence while parked. WS_EX_TOOLWINDOW
        // alone is not enough: winit's default WS_EX_APPWINDOW forces a
        // taskbar button even alongside it, so clear that too. SWP_FRAMECHANGED
        // makes the taskbar re-evaluate the extended style.
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW);
        SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        // Drop keyboard focus so keystrokes don't reach the invisible window.
        SetFocus(std::ptr::null_mut());
    }
}

/// Restore a parked window back on screen at its saved position.
pub fn restore_window_from_tray(hwnd: isize) {
    let h = hwnd as *mut core::ffi::c_void;
    // SAFETY: hwnd is a valid window handle from FindWindowW/CreateWindowExW.
    unsafe {
        // Re-add taskbar / alt-tab presence (restore WS_EX_APPWINDOW that
        // winit set at creation). SWP_FRAMECHANGED makes the taskbar
        // re-evaluate the extended style.
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex & !WS_EX_TOOLWINDOW) | WS_EX_APPWINDOW);
        SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        if let Some(mut placement) =
            SAVED_PLACEMENT.lock().unwrap_or_else(|p| p.into_inner()).take()
        {
            placement.showCmd = SW_RESTORE;
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            SetWindowPlacement(h, &placement);
        } else {
            ShowWindow(h, SW_RESTORE as i32);
        }
        SetForegroundWindow(h);
    }
}

#[cfg(target_arch = "x86_64")]
pub fn cpu_name() -> String {
    // SAFETY: CPUID is available on all x86_64 CPUs. Leaves 0x80000002–0x80000004
    // are standard AMD/Intel vendor strings with no side effects.
    unsafe {
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
}

#[cfg(not(target_arch = "x86_64"))]
pub fn cpu_name() -> String {
    String::from("Unknown CPU")
}

pub fn total_memory_gb() -> String {
    let mut mem = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: GlobalMemoryStatusEx is a safe win32 API call with a properly
    // initialised MemoryStatusEx struct (dw_length set). The buffer is stack-allocated.
    let ok = unsafe { GlobalMemoryStatusEx(&mut mem) };
    if ok != 0 && mem.ull_total_phys > 0 {
        let gb = ((mem.ull_total_phys as f64 / 1024.0 / 1024.0 / 1024.0).ceil()) as u64;
        format!("{} GB", gb)
    } else {
        "N/A".to_string()
    }
}

pub fn os_version() -> String {
    // Open the registry key once and query all values to avoid redundant
    // RegOpenKeyExW/RegCloseKey syscalls (was 3 opens = 9 syscalls, now 1 = 3).
    let key_path = to_wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let mut hkey: HKEY = std::ptr::null_mut();
    let key_opened = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_path.as_ptr(), 0, KEY_READ, &mut hkey) == ERROR_SUCCESS
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

    // SAFETY: RtlGetVersion is always available on Windows NT. The struct is
    // zero-initialised with dw_os_version_info_size set to the correct size.
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
            (info.dw_major_version, info.dw_minor_version, info.dw_build_number)
        } else {
            (0, 0, 0)
        }
    };

    let os_name = match major {
        10 => {
            if build >= 22000 { "Windows 11" } else { "Windows 10" }
        }
        6 => {
            if minor >= 3 { "Windows 8.1" }
            else if minor >= 2 { "Windows 8" }
            else if minor >= 1 { "Windows 7" }
            else { "Windows Vista" }
        }
        _ => "Windows",
    };

    let edition_str = match edition {
        Some(e) if !e.is_empty() => {
            let base = e.replace("Windows 10", "").replace("Windows 11", "").trim().to_string();
            if base.is_empty() { os_name.to_string() } else { format!("{} {}", os_name, base) }
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
    // SAFETY: GetSystemMetrics is a side-effect-free win32 user32 call that
    // returns cached system-wide metrics. Safe to call any time.
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if w > 0 && h > 0 {
        format!("{}x{}", w, h)
    } else {
        String::new()
    }
}

pub fn display_refresh_rate() -> String {
    // SAFETY: GetDC with null hwnd returns the screen DC. GetDeviceCaps reads
    // a cached capability value. ReleaseDC releases the DC. No external state
    // is modified.
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

    // SAFETY: First call with null data ptr to query required buffer size.
    // hkey is a valid open handle from RegOpenKeyExW, value_name is a
    // null-terminated UTF-16 string owned by the caller.
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

    // SAFETY: Second call with allocated buffer of the correct size returned
    // by the first call. hkey is valid, value_name is valid. Buffer is
    // properly sized and written as u8 bytes.
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
    // SAFETY: hkey is a valid open handle from RegOpenKeyExW. Buffer is a
    // fixed 4-byte stack array sized for REG_DWORD. value_name is a valid
    // null-terminated UTF-16 string.
    let rc = unsafe {
        RegQueryValueExW(hkey, value_name, std::ptr::null_mut(), &mut data_type, buf.as_mut_ptr(), &mut buf_size)
    };
    if rc != ERROR_SUCCESS || data_type != REG_DWORD || buf_size != 4 {
        return None;
    }
    Some(u32::from_le_bytes(buf))
}

// --- Tray icon functions ---
//
// SAFETY: All tray functions take `hwnd: isize` which is a window handle
// obtained from FindWindowW or CreateWindowExW. The handles are guaranteed
// valid for the lifetime of the tray manager. Window operations are
// side-effect-free or modify only the specified window's state.

pub fn find_window_by_title(title: &str) -> Option<isize> {
    let wide = to_wide(title);
    // SAFETY: FindWindowW searches for a window whose title matches the string.
    // null class name means match any class. wide is null-terminated UTF-16.
    // Returns null if no match found, which we convert to None.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as isize)
    }
}

pub fn is_iconic(hwnd: isize) -> bool {
    // SAFETY: IsIconic checks if the specified window is minimized (iconic).
    // hwnd is a valid window handle. Returns non-zero if iconic.
    unsafe { IsIconic(hwnd as *mut core::ffi::c_void) != 0 }
}

pub fn is_window(hwnd: isize) -> bool {
    // SAFETY: IsWindow checks if the specified handle is a valid window handle.
    // hwnd is a handle we obtained earlier; this validates it's still valid.
    unsafe { IsWindow(hwnd as *mut core::ffi::c_void) != 0 }
}

/// Load an ICO icon from raw bytes and return an HICON handle.
///
/// SAFETY: The returned HICON must be destroyed with DestroyIcon when no longer
/// needed. However, in our case the icon is used for the lifetime of the tray
/// and destroyed implicitly when the process exits.
pub fn load_icon_from_bytes(data: &[u8]) -> Option<isize> {
    // ICO header: reserved(2) + type(2) + count(2) = 6 bytes
    // Each entry: 16 bytes starting at offset 6 + i*16
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

    // Find the entry with the largest dimensions (width * height).
    // ICO stores 0 for 256px entries, so treat 0 as 256.
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
    extern "system" {
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
        CreateIconFromResourceEx(
            owned.as_mut_ptr(),
            size as u32,
            1,
            0x00030000,
            0,
            0,
            0x0000,
        )
    };
    if hicon.is_null() {
        None
    } else {
        Some(hicon as isize)
    }
}

/// Add a tray icon to the system notification area.
///
/// SAFETY: hwnd must be a valid window handle that will receive callback_msg
/// messages. icon must be a valid HICON from LoadIcon/CreateIconFromResourceEx.
/// tip is truncated to 127 characters (Windows limit).
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
    // SAFETY: Shell_NotifyIconW modifies the system tray icon list.
    // nid is properly initialized with all required fields.
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) != 0 }
}

/// Remove a tray icon from the system notification area.
///
/// SAFETY: hwnd must match the handle used in shell_notify_add.
pub fn shell_notify_delete(hwnd: isize) -> bool {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd as *mut core::ffi::c_void;
    nid.uID = 1;
    // SAFETY: Shell_NotifyIconW with NIM_DELETE removes the icon.
    // nid is properly initialized with cbSize, hWnd, and uID.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) != 0 }
}

/// Post a message to a window's message queue (non-blocking).
///
/// SAFETY: hwnd must be a valid window handle. The message is placed in the
/// queue and processed asynchronously by the window's message pump.
pub fn post_message(hwnd: isize, msg: u32, wparam: usize, lparam: isize) {
    // SAFETY: PostMessageW places a message in the message queue of the specified window.
    // hwnd is a valid window handle. Returns immediately without waiting.
    unsafe { PostMessageW(hwnd as *mut core::ffi::c_void, msg, wparam, lparam); }
}

/// Show a tray context menu at the specified screen coordinates.
///
/// Returns the menu command ID (ID_SHOW or ID_QUIT) if the user selects an item,
/// or None if the menu is dismissed without selection.
///
/// SAFETY: hwnd must be a valid window handle for the tray icon.
/// The menu is created, displayed, and destroyed within this function.
pub fn show_tray_menu(hwnd: isize, x: i32, y: i32) -> Option<u32> {
    // SAFETY: CreatePopupMenu creates a new empty popup menu.
    // Returns null if the function fails (out of resources).
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }

    let show_text = to_wide("Show Framework Crate");
    let quit_text = to_wide("Exit");

    // SAFETY: AppendMenuW adds items to the menu. Menu is valid from CreatePopupMenu.
    // ID_SHOW and ID_QUIT are constants used to identify menu items.
    unsafe {
        AppendMenuW(menu, 0, ID_SHOW as usize, show_text.as_ptr());
        AppendMenuW(menu, 0, ID_QUIT as usize, quit_text.as_ptr());
    }

    // SAFETY: TrackPopupMenu displays the menu and returns the selected command.
    // TPM_RIGHTBUTTON: menu appears on right-click.
    // TPM_RETURNCMD: returns command ID instead of sending WM_COMMAND.
    // The menu is destroyed after this call.
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

    // SAFETY: DestroyMenu frees the menu handle. Menu is valid from CreatePopupMenu.
    unsafe { DestroyMenu(menu); }

    // Send WM_NULL to ensure the tray icon callback is processed
    post_message(hwnd, WM_NULL, 0, 0);

    if cmd > 0 { Some(cmd as u32) } else { None }
}

