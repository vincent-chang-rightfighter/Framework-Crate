//! Hardware inventory and OS version queries.
//!
//! Split from `system_info.rs` (P3); the parent re-exports this module so
//! `crate::system_info::*` paths keep working.

use super::to_wide;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC};
use windows_sys::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SPI_GETWORKAREA, SystemParametersInfoW,
};

#[allow(clippy::upper_case_acronyms)]
type HKEY = *mut core::ffi::c_void;

const HKEY_LOCAL_MACHINE: HKEY = 0x80000002 as HKEY;
const KEY_READ: u32 = 0x20019;
const KEY_WOW64_64KEY: u32 = 0x0100;
const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;
const ERROR_SUCCESS: u32 = 0;
const ERROR_MORE_DATA: u32 = 234;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;

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

/// Number of UTF-16 units needed for a registry byte count, rounded up so an
/// odd (malformed) REG_SZ size cannot under-allocate the output buffer.
fn registry_u16_buffer_len(bytes: u32) -> usize {
    (bytes as usize).div_ceil(2)
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

    // Round up: a malformed odd byte count must not make the API write past
    // the allocation. REG_SZ is normally even, but this is unsafe code and
    // the value comes from the registry, so fail safe here.
    let mut buf: Vec<u16> = vec![0u16; registry_u16_buffer_len(buf_size)];
    let mut size = (buf.len() * 2) as u32;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_intel_cpu_does_not_panic() {
        let _ = super::is_intel_cpu();
    }

    #[test]
    fn registry_buffer_rounds_odd_byte_counts_up() {
        assert_eq!(super::registry_u16_buffer_len(0), 0);
        assert_eq!(super::registry_u16_buffer_len(2), 1);
        assert_eq!(super::registry_u16_buffer_len(3), 2);
        assert_eq!(super::registry_u16_buffer_len(4), 2);
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
}
