//! PawnIO driver and Modules version reporting for the ABOUT panel.

use super::{
    INTEL_MCHBAR_SHA256, INTEL_MSR_SHA256, LAST_KNOWN_MODULES_VERSION, is_pawnio_installed,
    local_modules_version, modules_dir, persist_local_modules_version, resolved_dll_path,
    sha256_hex,
};

/// Cached PawnIO version.
static PAWNIO_VERSION: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// Reads PawnIO version from DLL metadata.
fn fetch_pawnio_version_from_dll() -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    let path: Vec<u16> = resolved_dll_path()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut dummy: u32 = 0;
        let size = GetFileVersionInfoSizeW(path.as_ptr(), &mut dummy);
        if size == 0 {
            return None;
        }

        let mut buf: Vec<u8> = vec![0; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
            return None;
        }

        // Query root block for fixed file info.
        let mut ffi_ptr: *mut std::ffi::c_void = ptr::null_mut();
        let mut ffi_len: u32 = 0;
        let root: Vec<u16> = std::ffi::OsStr::new("\\")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if VerQueryValueW(
            buf.as_ptr() as *const _,
            root.as_ptr(),
            &mut ffi_ptr,
            &mut ffi_len,
        ) == 0
            || ffi_ptr.is_null()
        {
            return None;
        }

        // VS_FIXEDFILEINFO: need 24 bytes for product version at offsets 16/20.
        if ffi_len < 24 {
            return None;
        }

        // VS_FIXEDFILEINFO: product version at offsets 16 and 20.
        let ffi = ffi_ptr as *const u8;
        let ms = ptr::read_unaligned(ffi.add(16) as *const u32);
        let ls = ptr::read_unaligned(ffi.add(20) as *const u32);

        let major = (ms >> 16) & 0xFFFF;
        let minor = ms & 0xFFFF;
        let build = (ls >> 16) & 0xFFFF;
        let patch = ls & 0xFFFF;
        Some(format!("{}.{}.{}.{}", major, minor, build, patch))
    }
}

/// Returns cached PawnIO version.
pub fn pawnio_version() -> Option<String> {
    {
        let guard = PAWNIO_VERSION.read();
        if let Some(ref v) = *guard {
            return Some(v.clone());
        }
    }
    let ver = if is_pawnio_installed() {
        fetch_pawnio_version_from_dll().or_else(|| Some("installed".to_string()))
    } else {
        None
    };
    *PAWNIO_VERSION.write() = ver.clone();
    ver
}

/// Clears cached PawnIO version so next call re-reads DLL metadata.
pub fn invalidate_pawnio_version() {
    *PAWNIO_VERSION.write() = None;
}

/// Cached Modules version display string; cleared after successful download.
static MODULES_VERSION: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

pub fn invalidate_modules_version() {
    *MODULES_VERSION.write() = None;
}

/// Returns the locally installed PawnIO Modules version for display:
/// recorded tag, hash-inferred tag for unmarked bins, `unknown` for
/// unrecognized manual installs, `not installed` when absent.
pub fn pawnio_modules_version() -> String {
    if let Some(v) = MODULES_VERSION.read().clone() {
        return v;
    }
    let dir = modules_dir();
    let v = if let Some(local) = local_modules_version() {
        local
    } else if dir.join("IntelMSR.bin").is_file() && dir.join("IntelMCHBAR.bin").is_file() {
        // Self-heal: bins match the known release hashes but carry no marker
        // (manual copy); record the inferred tag so the number shows up.
        let msr = std::fs::read(dir.join("IntelMSR.bin"))
            .map(|b| sha256_hex(&b))
            .unwrap_or_default();
        let mchbar = std::fs::read(dir.join("IntelMCHBAR.bin"))
            .map(|b| sha256_hex(&b))
            .unwrap_or_default();
        if msr == INTEL_MSR_SHA256 && mchbar == INTEL_MCHBAR_SHA256 {
            persist_local_modules_version(&dir, LAST_KNOWN_MODULES_VERSION);
            LAST_KNOWN_MODULES_VERSION.to_string()
        } else {
            "unknown (manual install?)".to_string()
        }
    } else {
        "not installed".to_string()
    };
    *MODULES_VERSION.write() = Some(v.clone());
    v
}
