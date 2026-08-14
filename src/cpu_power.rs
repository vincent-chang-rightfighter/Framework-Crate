use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};
use windows_sys::Win32::Foundation::HANDLE;

use crate::util::with_write_lock;

// PawnIOLib.dll function signatures (win32 ABI).
type PawnioOpenWin32 = unsafe extern "system" fn(*mut HANDLE) -> i32;
type PawnioLoadWin32 = unsafe extern "system" fn(HANDLE, *const u8, usize) -> i32;
type PawnioExecuteWin32 = unsafe extern "system" fn(
    HANDLE, *const u8, *const u64, usize, *mut u64, usize, *mut usize,
) -> i32;
type PawnioCloseWin32 = unsafe extern "system" fn(HANDLE) -> i32;

// Embedded module blobs (PawnIO.Modules v0.2.10).
const INTEL_MSR_BLOB: &[u8] = include_bytes!("../assets/IntelMSR.bin");
const INTEL_MCHBAR_BLOB: &[u8] = include_bytes!("../assets/IntelMCHBAR.bin");

/// CPU power limit information.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuPowerInfo {
    pub pl1_msr: f64,
    pub pl1_msr_enabled: bool,
    pub pl1_msr_clamped: bool,
    pub pl2_msr: f64,
    pub pl2_msr_enabled: bool,
    pub pl2_msr_clamped: bool,
    pub pl1_mmio: f64,
    pub pl1_mmio_enabled: bool,
    pub pl1_mmio_clamped: bool,
    pub pl2_mmio: f64,
    pub pl2_mmio_enabled: bool,
    pub pl2_mmio_clamped: bool,
    pub power_unit: f64,
    pub mchbar_base: u64,
    pub available: bool,
    pub error_msg: Option<&'static str>,
}

/// Decode power limit from raw 64-bit value.
/// Returns (watts, enabled, clamped).
fn decode_power_limit(raw_val: u64, unit: f64) -> (f64, bool, bool) {
    let raw_bits = (raw_val & 0x7FFF) as f64;
    let enabled = (raw_val >> 15) & 1 == 1;
    let clamped = (raw_val >> 16) & 1 == 1;
    (raw_bits * unit, enabled, clamped)
}

/// Loaded PawnIO handle with associated DLL functions.
struct PawnioHandle {
    handle: HANDLE,
    exec_fn: PawnioExecuteWin32,
    close_fn: PawnioCloseWin32,
}

impl Drop for PawnioHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.close_fn)(self.handle) };
        }
    }
}

/// Global DLL function pointers (loaded once).
static DLL_OPEN: std::sync::OnceLock<PawnioOpenWin32> = std::sync::OnceLock::new();
static DLL_LOAD: std::sync::OnceLock<PawnioLoadWin32> = std::sync::OnceLock::new();
static DLL_EXEC: std::sync::OnceLock<PawnioExecuteWin32> = std::sync::OnceLock::new();
static DLL_CLOSE: std::sync::OnceLock<PawnioCloseWin32> = std::sync::OnceLock::new();

/// Initialize DLL function pointers (called once).
fn init_dll_fns() -> Result<(), &'static str> {
    use windows_sys::Win32::System::LibraryLoader::{LoadLibraryA, GetProcAddress};

    if DLL_OPEN.get().is_some() {
        return Ok(()); // already initialized
    }

    let dll_path = CString::new(r"C:\Program Files\PawnIO\PawnIOLib.dll")
        .map_err(|_| "CString failed")?;
    let dll = unsafe { LoadLibraryA(dll_path.as_ptr() as *const u8) };
    if dll.is_null() {
        return Err("PawnIOLib.dll not found");
    }

    unsafe {
        let open: PawnioOpenWin32 = std::mem::transmute(
            GetProcAddress(dll, c"pawnio_open_win32".as_ptr() as *const u8)
                .ok_or("pawnio_open_win32 not found")?
        );
        let load: PawnioLoadWin32 = std::mem::transmute(
            GetProcAddress(dll, c"pawnio_load_win32".as_ptr() as *const u8)
                .ok_or("pawnio_load_win32 not found")?
        );
        let exec: PawnioExecuteWin32 = std::mem::transmute(
            GetProcAddress(dll, c"pawnio_execute_win32".as_ptr() as *const u8)
                .ok_or("pawnio_execute_win32 not found")?
        );
        let close: PawnioCloseWin32 = std::mem::transmute(
            GetProcAddress(dll, c"pawnio_close_win32".as_ptr() as *const u8)
                .ok_or("pawnio_close_win32 not found")?
        );
        let _ = DLL_OPEN.set(open);
        let _ = DLL_LOAD.set(load);
        let _ = DLL_EXEC.set(exec);
        let _ = DLL_CLOSE.set(close);
    }
    Ok(())
}

/// Open a PawnIO handle and load a module blob.
fn open_handle(blob: &[u8]) -> Result<PawnioHandle, &'static str> {
    init_dll_fns()?;

    let mut handle: HANDLE = std::ptr::null_mut();
    unsafe { (DLL_OPEN.get().unwrap())(&mut handle) };
    if handle.is_null() {
        return Err("pawnio_open failed");
    }

    let hr = unsafe { (DLL_LOAD.get().unwrap())(handle, blob.as_ptr(), blob.len()) };
    if hr < 0 {
        unsafe { (DLL_CLOSE.get().unwrap())(handle) };
        return Err("pawnio_load failed");
    }

    Ok(PawnioHandle {
        handle,
        exec_fn: *DLL_EXEC.get().unwrap(),
        close_fn: *DLL_CLOSE.get().unwrap(),
    })
}

/// Execute a named IOCTL in a loaded module.
fn exec_ioctl(
    handle: &PawnioHandle,
    name: &str,
    inputs: &[u64],
    outputs: &mut [u64],
) -> Result<usize, &'static str> {
    let name_c = CString::new(name).map_err(|_| "CString failed")?;
    let mut return_size: usize = 0;

    let hr = unsafe {
        (handle.exec_fn)(
            handle.handle,
            name_c.as_ptr() as *const u8,
            inputs.as_ptr(),
            inputs.len(),
            outputs.as_mut_ptr(),
            outputs.len(),
            &mut return_size,
        )
    };

    if hr < 0 {
        return Err("ioctl failed");
    }
    Ok(return_size)
}

/// Read all CPU power information via PawnIO modules.
pub fn read_cpu_power() -> CpuPowerInfo {
    let mut info = CpuPowerInfo::default();

    // Load IntelMCHBAR module to get MCHBAR base address.
    let mchbar_handle = match open_handle(INTEL_MCHBAR_BLOB) {
        Ok(h) => h,
        Err(e) => {
            info.error_msg = Some(e);
            return info;
        }
    };

    // Get MCHBAR base address.
    let mut out = [0u64; 1];
    match exec_ioctl(&mchbar_handle, "ioctl_get_mchbar_addr", &[], &mut out) {
        Ok(_) => {
            info.mchbar_base = out[0];
            debug!("MCHBAR base: 0x{:X}", info.mchbar_base);
        }
        Err(e) => {
            info.error_msg = Some(e);
            warn!("Failed to get MCHBAR address");
            return info;
        }
    }

    // Load IntelMSR module to read MSRs.
    let msr_handle = match open_handle(INTEL_MSR_BLOB) {
        Ok(h) => h,
        Err(e) => {
            info.error_msg = Some(e);
            return info;
        }
    };

    // Read MSR 0x606 (MSR_RAPL_POWER_UNIT).
    let mut out = [0u64; 1];
    if exec_ioctl(&msr_handle, "ioctl_read_msr", &[0x606], &mut out).is_ok() {
        let raw = out[0];
        let unit_bits = (raw & 0xF) as u32;
        info.power_unit = 1.0 / (1u32 << unit_bits) as f64;
        debug!("Power unit: {} W/unit (bits={})", info.power_unit, unit_bits);
    } else {
        info.error_msg = Some("Failed to read MSR 0x606");
        return info;
    }

    // Read MSR 0x610 (MSR_PKG_POWER_LIMIT) for static PL1/PL2.
    let mut out = [0u64; 1];
    if exec_ioctl(&msr_handle, "ioctl_read_msr", &[0x610], &mut out).is_ok() {
        let raw = out[0];
        let (pl1, pl1_en, pl1_cl) = decode_power_limit(raw, info.power_unit);
        let (pl2, pl2_en, pl2_cl) = decode_power_limit(raw >> 32, info.power_unit);
        info.pl1_msr = pl1;
        info.pl1_msr_enabled = pl1_en;
        info.pl1_msr_clamped = pl1_cl;
        info.pl2_msr = pl2;
        info.pl2_msr_enabled = pl2_en;
        info.pl2_msr_clamped = pl2_cl;
        debug!("MSR PL1: {:.1}W (en={} clamp={})", pl1, pl1_en, pl1_cl);
        debug!("MSR PL2: {:.1}W (en={} clamp={})", pl2, pl2_en, pl2_cl);
    }

    // Read MMIO at MCHBAR + 0x59A0 (PACKAGE_POWER_LIMIT_MMIO) via IntelMCHBAR.
    let mmio_offset = 0x59A0u64;
    let mut out = [0u64; 1];
    if exec_ioctl(&mchbar_handle, "ioctl_read_qword", &[mmio_offset], &mut out).is_ok() {
        let raw = out[0];
        let (pl1, pl1_en, pl1_cl) = decode_power_limit(raw, info.power_unit);
        let (pl2, pl2_en, pl2_cl) = decode_power_limit(raw >> 32, info.power_unit);
        info.pl1_mmio = pl1;
        info.pl1_mmio_enabled = pl1_en;
        info.pl1_mmio_clamped = pl1_cl;
        info.pl2_mmio = pl2;
        info.pl2_mmio_enabled = pl2_en;
        info.pl2_mmio_clamped = pl2_cl;
        debug!("MMIO PL1: {:.1}W (en={} clamp={})", pl1, pl1_en, pl1_cl);
        debug!("MMIO PL2: {:.1}W (en={} clamp={})", pl2, pl2_en, pl2_cl);
    }

    info.available = true;
    info
}

/// Shared CPU power state for background task and UI.
#[derive(Clone)]
pub struct CpuPowerState {
    pub info: Arc<parking_lot::RwLock<Arc<CpuPowerInfo>>>,
    pub available: Arc<AtomicBool>,
}

impl Default for CpuPowerState {
    fn default() -> Self {
        Self {
            info: Arc::new(parking_lot::RwLock::new(Arc::new(CpuPowerInfo::default()))),
            available: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl CpuPowerState {
    pub fn refresh(&self) {
        let info = read_cpu_power();
        self.available.store(info.available, Ordering::Release);
        with_write_lock(&self.info, |guard| {
            *guard = Arc::new(info);
        });
    }

    pub fn snapshot(&self) -> Arc<CpuPowerInfo> {
        crate::util::read_lock(&self.info).clone()
    }
}
