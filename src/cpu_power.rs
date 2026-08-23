use std::ffi::CString;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::HANDLE;

use crate::util::with_write_lock;

// PawnIOLib.dll function signatures (STDMETHODCALLTYPE / WINAPI — same on x64).
type PawnioOpen = unsafe extern "system" fn(*mut HANDLE) -> i32;  // HRESULT
type PawnioLoad = unsafe extern "system" fn(HANDLE, *const u8, usize) -> i32;  // HRESULT
type PawnioExecute = unsafe extern "system" fn(
    HANDLE, *const u8, *const u64, usize, *mut u64, usize, *mut usize,
) -> i32;  // HRESULT
type PawnioClose = unsafe extern "system" fn(HANDLE) -> i32;  // HRESULT

const MODULES_DIR_NAME: &str = "modules";
const PAWNIO_MODULES_VERSION: &str = "0.2.10";
const INTEL_MSR_SHA256: &str = "d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f";
const INTEL_MCHBAR_SHA256: &str = "3f82b832d99b4aac37d2a20fdb7c9baa2a3bc0488612c9019c9484eb0e8a6eae";

/// Get the local modules directory path (%APPDATA%/framework-crate/modules/).
fn modules_dir() -> std::path::PathBuf {
    let base = dirs::config_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("framework-crate").join(MODULES_DIR_NAME)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_module_hash(path: &std::path::Path, expected: &str) -> Result<(), &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "module blob missing")?;
    if sha256_hex(&bytes) != expected {
        warn!("PawnIO module hash mismatch: {}", path.display());
        return Err("module hash mismatch");
    }
    Ok(())
}

/// Read a module blob once and verify its hash on the in-memory bytes.
///
/// Do NOT verify-then-re-read: between the two reads an attacker (or a
/// concurrent modules update) could swap the file — the hash would pass on
/// version A while the handle loads version B.
fn read_verified_module(path: &std::path::Path, expected: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "module blob missing")?;
    if sha256_hex(&bytes) != expected {
        warn!("PawnIO module hash mismatch: {}", path.display());
        return Err("module hash mismatch");
    }
    Ok(bytes)
}

fn verify_cached_modules(dir: &std::path::Path) -> Result<(), &'static str> {
    verify_module_hash(&dir.join("IntelMSR.bin"), INTEL_MSR_SHA256)?;
    verify_module_hash(&dir.join("IntelMCHBAR.bin"), INTEL_MCHBAR_SHA256)?;
    Ok(())
}

/// Check if required module blobs are already cached locally and match the pinned hashes.
pub fn modules_downloaded() -> bool {
    verify_cached_modules(&modules_dir()).is_ok()
}

/// Download PawnIO Modules ZIP and extract blobs. Returns Ok or error.
pub fn download_and_extract_modules() -> Result<(), &'static str> {
    let dir = modules_dir();
    std::fs::create_dir_all(&dir).map_err(|_| "failed to create modules directory")?;

    let zip_path = dir.join(format!("pawnio_modules_{}.zip", PAWNIO_MODULES_VERSION));
    // Release asset naming on github.com/namazso/PawnIO.Modules:
    // tag "0.2.10" ships "release_0_2_10.zip".
    let url = format!(
        "https://github.com/namazso/PawnIO.Modules/releases/download/{}/release_{}.zip",
        PAWNIO_MODULES_VERSION,
        PAWNIO_MODULES_VERSION.replace('.', "_")
    );
    debug!("Downloading modules from: {}", url);

    // Remove stale ZIP if it exists and is too small.
    if let Ok(meta) = std::fs::metadata(&zip_path)
        && meta.len() < 1000
    {
        let _ = std::fs::remove_file(&zip_path);
    }

    // Write PowerShell script to a unique temp file (avoids all escaping
    // issues, and concurrent downloads never collide on the same path).
    let script_path = std::env::temp_dir().join(format!(
        "pawnio_download_{}_{}.ps1",
        std::process::id(),
        crate::util::monotonic_ms(),
    ));
    // PowerShell single-quoted strings escape ' as ''.  zip/dir come from
    // %APPDATA% and could contain ' — without escaping the script would
    // break or allow injection.  url is pinned but escaped for completeness.
    let esc = |s: &str| s.replace('\'', "''");
    let script = format!(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12\n\
         Invoke-WebRequest -Uri '{url}' -OutFile '{zip}' -UseBasicParsing\n\
         Remove-Item -Path '{dir}\\IntelMSR.bin' -Force -ErrorAction SilentlyContinue\n\
         Remove-Item -Path '{dir}\\IntelMCHBAR.bin' -Force -ErrorAction SilentlyContinue\n\
         Expand-Archive -Path '{zip}' -DestinationPath '{dir}' -Force\n\
         Remove-Item '{zip}' -ErrorAction SilentlyContinue",
        url = esc(&url),
        zip = esc(&zip_path.display().to_string()),
        dir = esc(&dir.display().to_string()),
    );
    std::fs::write(&script_path, &script).map_err(|_| "failed to write script")?;

    let script_str = script_path
        .to_str()
        .ok_or("temp path is not valid UTF-8")?;
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script_str])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map_err(|_| "failed to run powershell")?;
    let _ = std::fs::remove_file(&script_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit code {}", output.status.code().unwrap_or(-1))
        };
        warn!("PowerShell download failed: {}", detail);
        return Err("download/extraction failed");
    }

    if let Err(e) = verify_cached_modules(&dir) {
        warn!("Downloaded PawnIO modules failed verification: {}", e);
        let _ = std::fs::remove_file(dir.join("IntelMSR.bin"));
        let _ = std::fs::remove_file(dir.join("IntelMCHBAR.bin"));
        return Err(e);
    }

    debug!("PawnIO modules extracted successfully to {}", dir.display());
    invalidate_blob_cache();
    Ok(())
}

static MSR_BLOB_CACHE: std::sync::Mutex<Option<Arc<Vec<u8>>>> = std::sync::Mutex::new(None);
static MCHBAR_BLOB_CACHE: std::sync::Mutex<Option<Arc<Vec<u8>>>> = std::sync::Mutex::new(None);

fn invalidate_blob_cache() {
    let _ = MSR_BLOB_CACHE.lock().map(|mut g| *g = None);
    let _ = MCHBAR_BLOB_CACHE.lock().map(|mut g| *g = None);
}

/// Load IntelMSR module blob (cached after first verified load).
fn load_intel_msr_blob() -> Result<Vec<u8>, &'static str> {
    {
        if let Ok(guard) = MSR_BLOB_CACHE.lock()
            && let Some(cached) = guard.as_ref()
        {
            return Ok((**cached).clone());
        }
    }
    let blob = read_verified_module(&modules_dir().join("IntelMSR.bin"), INTEL_MSR_SHA256)?;
    let arc = Arc::new(blob.clone());
    if let Ok(mut guard) = MSR_BLOB_CACHE.lock() {
        *guard = Some(arc);
    }
    Ok(blob)
}

/// Load IntelMCHBAR module blob (cached after first verified load).
fn load_intel_mchbar_blob() -> Result<Vec<u8>, &'static str> {
    {
        if let Ok(guard) = MCHBAR_BLOB_CACHE.lock()
            && let Some(cached) = guard.as_ref()
        {
            return Ok((**cached).clone());
        }
    }
    let blob = read_verified_module(&modules_dir().join("IntelMCHBAR.bin"), INTEL_MCHBAR_SHA256)?;
    let arc = Arc::new(blob.clone());
    if let Ok(mut guard) = MCHBAR_BLOB_CACHE.lock() {
        *guard = Some(arc);
    }
    Ok(blob)
}

/// CPU power limit information.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuPowerInfo {
    pub pl1_msr: f64,
    pub pl1_msr_enabled: bool,
    pub pl1_msr_clamped: bool,
    pub pl1_time_s: f64,
    pub pl2_msr: f64,
    pub pl2_msr_enabled: bool,
    pub pl2_msr_clamped: bool,
    pub pl2_time_s: f64,
    pub pl1_mmio: f64,
    pub pl1_mmio_enabled: bool,
    pub pl1_mmio_clamped: bool,
    pub pl1_mmio_time_s: f64,
    pub pl2_mmio: f64,
    pub pl2_mmio_enabled: bool,
    pub pl2_mmio_clamped: bool,
    pub pl2_mmio_time_s: f64,
    pub power_unit: f64,
    pub time_unit: f64,
    pub available: bool,
    pub error_msg: Option<&'static str>,
}

impl CpuPowerInfo {
    /// Effective PL1: the CPU enforces the lower of the MSR and MMIO
    /// registers, so the effective limit is whichever is tighter. Falls
    /// back to the value actually read when the other register is
    /// unavailable (0).
    pub fn effective_pl1(&self) -> f64 {
        effective_limit(self.pl1_msr, self.pl1_mmio)
    }

    /// Effective PL2: see [`CpuPowerInfo::effective_pl1`].
    pub fn effective_pl2(&self) -> f64 {
        effective_limit(self.pl2_msr, self.pl2_mmio)
    }

    /// Pre-fill edit fields from current MSR values.
    /// Returns (pl1_watts, pl2_watts, pl1_enabled, pl2_enabled, pl1_clamped, pl2_clamped, pl1_time_s, pl2_time_s).
    pub fn init_edit_fields(&self) -> (String, String, bool, bool, bool, bool, String, String) {
        if self.available {
            (
                format!("{:.1}", self.pl1_msr),
                format!("{:.1}", self.pl2_msr),
                self.pl1_msr_enabled,
                self.pl2_msr_enabled,
                self.pl1_msr_clamped,
                self.pl2_msr_clamped,
                format!("{:.1}", self.pl1_time_s),
                format!("{:.1}", self.pl2_time_s),
            )
        } else {
            (String::new(), String::new(), true, true, false, false, String::new(), String::new())
        }
    }
}

/// Lower of two power limits, ignoring 0 (meaning "not read / unavailable").
fn effective_limit(msr: f64, mmio: f64) -> f64 {
    match (msr > 0.0, mmio > 0.0) {
        (true, true) => msr.min(mmio),
        (true, false) => msr,
        (false, true) => mmio,
        (false, false) => 0.0,
    }
}

/// Decode power limit from raw 32-bit half of the register.
/// Returns (watts, enabled, clamped, time_window_y, time_window_z).
fn decode_power_limit(raw_val: u64, unit: f64) -> (f64, bool, bool, u32, u32) {
    let raw_bits = (raw_val & 0x7FFF) as f64;
    let enabled = (raw_val >> 15) & 1 == 1;
    let clamped = (raw_val >> 16) & 1 == 1;
    // Intel MSR 0x610: Y = 5 bits [21:17], Z = 2 bits [23:22].
    let time_y = ((raw_val >> 17) & 0x1F) as u32;
    let time_z = ((raw_val >> 22) & 0x3) as u32;
    (raw_bits * unit, enabled, clamped, time_y, time_z)
}

/// Decode time window from Y and Z fields into seconds.
/// Time = 2^Y × (1 + Z/4) × Time_Unit
fn decode_time_window(y: u32, z: u32, time_unit: f64) -> f64 {
    (1u64 << y) as f64 * (1.0 + z as f64 / 4.0) * time_unit
}

/// Encode time window in seconds into Y and Z fields.
/// Returns (y, z) where Time ≈ 2^Y × (1 + Z/4) × Time_Unit.
fn encode_time_window(time_s: f64, time_unit: f64) -> (u32, u32) {
    if time_unit <= 0.0 || time_s <= 0.0 {
        return (0, 0);
    }
    let ratio = time_s / time_unit;
    let y = (ratio.ln() / 2.0_f64.ln()).floor() as i32;
    let y = y.clamp(0, 31);
    let remaining = ratio / (1i64 << y) as f64;
    let z = ((remaining - 1.0) * 4.0).round() as i32;
    let z = z.clamp(0, 3);
    (y as u32, z as u32)
}

/// Encode a power limit value into a 32-bit register half.
/// Bits [14:0] = power limit, [15] = enable, [16] = clamp,
/// [21:17] = time window Y (5 bits), [23:22] = time window Z (2 bits).
fn encode_power_limit(watts: f64, enabled: bool, clamped: bool, unit: f64, time_y: u32, time_z: u32) -> u32 {
    let max_raw = ((1u32 << 15) - 1) as f64; // 15-bit field: max 32767
    let raw = (watts / unit).round().clamp(0.0, max_raw) as u32;
    let mut val = raw & 0x7FFF;
    if enabled { val |= 1 << 15; }
    if clamped { val |= 1 << 16; }
    val |= (time_y & 0x1F) << 17;
    val |= (time_z & 0x3) << 22;
    val
}

/// Power limit parameters for PL1/PL2.
#[derive(Clone, Copy)]
struct PowerLimitParams {
    pl1_watts: f64,
    pl1_enabled: bool,
    pl1_clamped: bool,
    pl1_time_s: f64,
    pl2_watts: f64,
    pl2_enabled: bool,
    pl2_clamped: bool,
    pl2_time_s: f64,
    power_unit: f64,
    time_unit: f64,
}

/// Write the MSR_PKG_POWER_LIMIT register (0x610) via IntelMSR module.
///
/// Reads the current MSR value to preserve time window fields, then applies
/// the new PL1/PL2 settings and writes back via ioctl_write_msr.
fn write_msr_pl1_pl2(
    msr_handle: &PawnioHandle,
    params: &PowerLimitParams,
) -> Result<(), &'static str> {
    if params.power_unit <= 0.0 || params.time_unit <= 0.0 {
        return Err("invalid RAPL units (power_unit or time_unit is zero)");
    }
    let (pl1_y, pl1_z) = encode_time_window(params.pl1_time_s, params.time_unit);
    let (pl2_y, pl2_z) = encode_time_window(params.pl2_time_s, params.time_unit);

    let pl1_enc = encode_power_limit(params.pl1_watts, params.pl1_enabled, params.pl1_clamped, params.power_unit, pl1_y, pl1_z);
    let pl2_enc = encode_power_limit(params.pl2_watts, params.pl2_enabled, params.pl2_clamped, params.power_unit, pl2_y, pl2_z);

    let new_val = ((pl2_enc as u64) << 32) | (pl1_enc as u64);

    let mut inp = [0u64; 2];
    inp[0] = 0x610; // MSR_PKG_POWER_LIMIT
    inp[1] = new_val;
    let mut out2 = [0u64; 1];
    exec_ioctl(msr_handle, "ioctl_write_msr", &inp, &mut out2)
        .map_err(|_| "ioctl_write_msr failed — is IntelMSR module loaded?")?;

    // Verify the write actually landed: re-read 0x610 and compare the
    // decoded limits within encoding tolerance. The CPU silently drops
    // the write when the register is BIOS-locked (bit 63); without this
    // check the UI would report success for a limit that was never
    // applied. The clamp bit is a hint that BIOS/EC may rewrite, so it
    // is not compared.
    let tolerance = params.power_unit.max(0.25);
    let mut rb_out = [0u64; 1];
    if exec_ioctl(msr_handle, "ioctl_read_msr", &[0x610], &mut rb_out).is_err() {
        return Err("MSR write succeeded but read-back failed");
    }
    let rb_raw = rb_out[0];
    let (rb_pl1, rb_pl1_en, ..) = decode_power_limit(rb_raw, params.power_unit);
    let (rb_pl2, rb_pl2_en, ..) = decode_power_limit(rb_raw >> 32, params.power_unit);
    // Compare against the clamped/encoded target, not the raw request:
    // encode_power_limit clamps to the 15-bit field, so an out-of-range
    // request decodes back to the clamp point and comparing against the
    // requested watts would falsely report the register as locked.
    let expected_pl1 = ((pl1_enc & 0x7FFF) as f64) * params.power_unit;
    let expected_pl2 = ((pl2_enc & 0x7FFF) as f64) * params.power_unit;
    if (rb_pl1 - expected_pl1).abs() > tolerance
        || (rb_pl2 - expected_pl2).abs() > tolerance
        || rb_pl1_en != params.pl1_enabled
        || rb_pl2_en != params.pl2_enabled
    {
        debug!("MSR read-back mismatch: wrote PL1={:.2}W(en={}) PL2={:.2}W(en={}), read PL1={:.2}W(en={}) PL2={:.2}W(en={})",
            expected_pl1, params.pl1_enabled, expected_pl2, params.pl2_enabled,
            rb_pl1, rb_pl1_en, rb_pl2, rb_pl2_en);
        return Err("MSR write not reflected in read-back — register may be locked");
    }

    debug!("MSR PL1/PL2 written: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) raw=0x{:016X}",
        params.pl1_watts, params.pl1_time_s, params.pl2_watts, params.pl2_time_s, new_val);
    Ok(())
}

/// Write the PACKAGE_POWER_LIMIT_MMIO register (MCHBAR + 0x59A0) via IntelMCHBAR module.
///
/// This is the dynamic (in-effect) power limit register. Writing here takes
/// effect immediately without requiring a MSR write.
fn write_mmio_pl1_pl2(
    mchbar_handle: &PawnioHandle,
    params: &PowerLimitParams,
) -> Result<(), &'static str> {
    if params.power_unit <= 0.0 || params.time_unit <= 0.0 {
        return Err("invalid RAPL units (power_unit or time_unit is zero)");
    }
    let (pl1_y, pl1_z) = encode_time_window(params.pl1_time_s, params.time_unit);
    let (pl2_y, pl2_z) = encode_time_window(params.pl2_time_s, params.time_unit);

    let pl1_enc = encode_power_limit(params.pl1_watts, params.pl1_enabled, params.pl1_clamped, params.power_unit, pl1_y, pl1_z);
    let pl2_enc = encode_power_limit(params.pl2_watts, params.pl2_enabled, params.pl2_clamped, params.power_unit, pl2_y, pl2_z);

    let new_val = ((pl2_enc as u64) << 32) | (pl1_enc as u64);

    let mmio_offset = 0x59A0u64;
    let mut inp = [0u64; 2];
    inp[0] = mmio_offset;
    inp[1] = new_val;
    let mut out2 = [0u64; 1];
    exec_ioctl(mchbar_handle, "ioctl_write_qword", &inp, &mut out2)
        .map_err(|_| "ioctl_write_qword failed — IntelMCHBAR module may not support MMIO write")?;

    debug!("MMIO PL1/PL2 written: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) raw=0x{:016X}",
        params.pl1_watts, params.pl1_time_s, params.pl2_watts, params.pl2_time_s, new_val);
    Ok(())
}

/// Public wrapper: write MSR 0x610 (opens IntelMSR handle internally).
#[allow(clippy::too_many_arguments)]
pub fn write_msr_pl1_pl2_public(
    pl1_watts: f64,
    pl1_enabled: bool,
    pl1_clamped: bool,
    pl1_time_s: f64,
    pl2_watts: f64,
    pl2_enabled: bool,
    pl2_clamped: bool,
    pl2_time_s: f64,
    power_unit: f64,
    time_unit: f64,
) -> Result<(), &'static str> {
    let params = PowerLimitParams {
        pl1_watts,
        pl1_enabled,
        pl1_clamped,
        pl1_time_s,
        pl2_watts,
        pl2_enabled,
        pl2_clamped,
        pl2_time_s,
        power_unit,
        time_unit,
    };
    let msr_blob = load_intel_msr_blob()?;
    let msr_handle = open_handle(&msr_blob)?;
    write_msr_pl1_pl2(&msr_handle, &params)
}

#[allow(clippy::too_many_arguments)]
pub fn write_mmio_pl1_pl2_public(
    pl1_watts: f64,
    pl1_enabled: bool,
    pl1_clamped: bool,
    pl1_time_s: f64,
    pl2_watts: f64,
    pl2_enabled: bool,
    pl2_clamped: bool,
    pl2_time_s: f64,
    power_unit: f64,
    time_unit: f64,
) -> Result<(), &'static str> {
    let params = PowerLimitParams {
        pl1_watts,
        pl1_enabled,
        pl1_clamped,
        pl1_time_s,
        pl2_watts,
        pl2_enabled,
        pl2_clamped,
        pl2_time_s,
        power_unit,
        time_unit,
    };
    let mchbar_blob = load_intel_mchbar_blob()?;
    let mchbar_handle = open_handle(&mchbar_blob)?;
    write_mmio_pl1_pl2(&mchbar_handle, &params)
}

/// Write both MSR and MMIO from a persisted `BiosDefaults` so Reset fully
/// restores the original factory `min(MSR, MMIO)` state.
pub fn write_bios_defaults(bios: &BiosDefaults) -> Result<(), &'static str> {
    write_msr_pl1_pl2_public(
        bios.pl1_watts, bios.pl1_enabled, bios.pl1_clamped, bios.pl1_time_s,
        bios.pl2_watts, bios.pl2_enabled, bios.pl2_clamped, bios.pl2_time_s,
        bios.power_unit, bios.time_unit,
    )?;
    // MMIO may be absent on older persisted files (defaults 0) or on systems
    // where MCHBAR was unreadable at first run — best-effort only.
    if bios.pl1_mmio_watts > 0.0 || bios.pl2_mmio_watts > 0.0 {
        let _ = write_mmio_pl1_pl2_public(
            bios.pl1_mmio_watts, bios.pl1_mmio_enabled, bios.pl1_mmio_clamped, bios.pl1_mmio_time_s,
            bios.pl2_mmio_watts, bios.pl2_mmio_enabled, bios.pl2_mmio_clamped, bios.pl2_mmio_time_s,
            bios.power_unit, bios.time_unit,
        );
    }
    Ok(())
}

/// Loaded PawnIO handle with associated DLL functions.
struct PawnioHandle {
    handle: HANDLE,
    exec_fn: PawnioExecute,
    close_fn: PawnioClose,
}

impl Drop for PawnioHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.close_fn)(self.handle) };
        }
    }
}

/// Global DLL function pointers (loaded once).
static DLL_OPEN: std::sync::OnceLock<PawnioOpen> = std::sync::OnceLock::new();
static DLL_LOAD: std::sync::OnceLock<PawnioLoad> = std::sync::OnceLock::new();
static DLL_EXEC: std::sync::OnceLock<PawnioExecute> = std::sync::OnceLock::new();
static DLL_CLOSE: std::sync::OnceLock<PawnioClose> = std::sync::OnceLock::new();

const DLL_PATH: &str = r"C:\Program Files\PawnIO\PawnIOLib.dll";

/// Try to install PawnIO via winget.
pub fn install_pawnio() -> Result<(), &'static str> {
    use std::process::Command;
    tracing::info!("Installing PawnIO via winget...");
    let status = Command::new("winget")
        .args(["install", "-e", "--id", "namazso.PawnIO", "--accept-package-agreements", "--accept-source-agreements"])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .status()
        .map_err(|_| "failed to run winget")?;
    if status.success() {
        tracing::info!("PawnIO installed successfully");
        Ok(())
    } else {
        Err("winget install failed")
    }
}

/// Check if PawnIO DLL is installed.
pub fn is_pawnio_installed() -> bool {
    std::path::Path::new(DLL_PATH).exists()
}

/// Initialize DLL function pointers (called once).
fn init_dll_fns() -> Result<(), &'static str> {
    use windows_sys::Win32::System::LibraryLoader::{LoadLibraryA, GetProcAddress};

    if DLL_OPEN.get().is_some() {
        return Ok(()); // already initialized
    }

    let dll_path = CString::new(DLL_PATH).map_err(|_| "CString failed")?;
    let dll = unsafe { LoadLibraryA(dll_path.as_ptr() as *const u8) };
    if dll.is_null() {
        return Err("PawnIO not installed");
    }

    unsafe {
        // GetProcAddress returns FARPROC (a generic fn pointer type). rustc
        // does not check ABI compatibility when transmuting between fn
        // pointer types, so use transmute_copy with a compile-time size
        // assertion instead — the accepted pattern for dynamic DLL loading.
        const _: () = assert!(
            std::mem::size_of::<PawnioOpen>()
                == std::mem::size_of::<windows_sys::Win32::Foundation::FARPROC>()
        );
        let addr = GetProcAddress(dll, c"pawnio_open".as_ptr() as *const u8)
            .ok_or("pawnio_open not found")?;
        let open: PawnioOpen = std::mem::transmute_copy(&addr);
        let addr = GetProcAddress(dll, c"pawnio_load".as_ptr() as *const u8)
            .ok_or("pawnio_load not found")?;
        let load: PawnioLoad = std::mem::transmute_copy(&addr);
        let addr = GetProcAddress(dll, c"pawnio_execute".as_ptr() as *const u8)
            .ok_or("pawnio_execute not found")?;
        let exec: PawnioExecute = std::mem::transmute_copy(&addr);
        let addr = GetProcAddress(dll, c"pawnio_close".as_ptr() as *const u8)
            .ok_or("pawnio_close not found")?;
        let close: PawnioClose = std::mem::transmute_copy(&addr);
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
    let open = *DLL_OPEN.get().ok_or("PawnIO DLL not initialized")?;
    let hr = unsafe { open(&mut handle) };
    if hr < 0 || handle.is_null() {
        warn!("pawnio_open returned hr=0x{:X} handle={:?}", hr, handle);
        return Err("pawnio_open failed");
    }

    let load = *DLL_LOAD.get().ok_or("PawnIO DLL not initialized")?;
    let hr = unsafe { load(handle, blob.as_ptr(), blob.len()) };
    if hr < 0 {
        warn!("pawnio_load returned hr=0x{:X}", hr);
        let close = *DLL_CLOSE.get().ok_or("PawnIO DLL not initialized")?;
        unsafe { close(handle) };
        return Err("pawnio_load failed");
    }

    let exec_fn = *DLL_EXEC.get().ok_or("PawnIO DLL not initialized")?;
    let close_fn = *DLL_CLOSE.get().ok_or("PawnIO DLL not initialized")?;
    Ok(PawnioHandle {
        handle,
        exec_fn,
        close_fn,
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

    if hr != 0 {
        return Err("ioctl failed");
    }
    // The driver reports how many u64 entries it wrote (bytes / 8); a short
    // write leaves the output buffer stale/zeroed and must not be treated as
    // valid data.
    if return_size != outputs.len() {
        return Err("ioctl short read");
    }
    Ok(return_size)
}

/// Read all CPU power information via PawnIO modules.
pub fn read_cpu_power() -> CpuPowerInfo {
    let mut info = CpuPowerInfo::default();

    // Load the two module handles independently: a missing MCHBAR module
    // must not discard the MSR data (and vice versa). Each failure is
    // recorded but only reported if neither source ends up working.
    let msr_handle = match load_intel_msr_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            info.error_msg = Some(e);
            None
        }
    };
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            if info.error_msg.is_none() {
                info.error_msg = Some(e);
            }
            None
        }
    };

    // Read MSR 0x606 (MSR_RAPL_POWER_UNIT) — required to decode any limit.
    let mut units_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_msr", &[0x606], &mut out).is_ok() {
            let raw = out[0];
            let unit_bits = (raw & 0xF) as u32;
            info.power_unit = 1.0 / (1u32 << unit_bits) as f64;
            let time_bits = ((raw >> 16) & 0xF) as u32;
            info.time_unit = 1.0 / (1u32 << time_bits) as f64;
            debug!("Power unit: {} W/unit (bits={}), time unit: {} s/unit (bits={})",
                info.power_unit, unit_bits, info.time_unit, time_bits);
            units_ok = true;
        }
    }
    if !units_ok {
        // Keep any earlier root-cause message (e.g. IntelMSR module load
        // failure) instead of masking it with this generic one.
        if info.error_msg.is_none() {
            info.error_msg = Some("Failed to read MSR 0x606");
        }
        return info;
    }

    // Read MSR 0x610 (MSR_PKG_POWER_LIMIT) for static PL1/PL2.
    let mut msr610_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_msr", &[0x610], &mut out).is_ok() {
            msr610_ok = true;
            let raw = out[0];
            debug!("MSR 0x610 raw: 0x{:016X}", raw);
            let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
            let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) = decode_power_limit(raw >> 32, info.power_unit);
            info.pl1_msr = pl1;
            info.pl1_msr_enabled = pl1_en;
            info.pl1_msr_clamped = pl1_cl;
            info.pl1_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
            info.pl2_msr = pl2;
            info.pl2_msr_enabled = pl2_en;
            info.pl2_msr_clamped = pl2_cl;
            info.pl2_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
            debug!("MSR PL1: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s", pl1, pl1_en, pl1_cl, pl1_y, pl1_z, info.pl1_time_s);
            debug!("MSR PL2: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s", pl2, pl2_en, pl2_cl, pl2_y, pl2_z, info.pl2_time_s);
        }
    }

    // Read MMIO at MCHBAR + 0x59A0 (PACKAGE_POWER_LIMIT_MMIO) via IntelMCHBAR.
    let mmio_offset = 0x59A0u64;
    let mut mmio_ok = false;
    if let Some(ref handle) = mchbar_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_qword", &[mmio_offset], &mut out).is_ok() {
            mmio_ok = true;
            let raw = out[0];
            let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
            let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) = decode_power_limit(raw >> 32, info.power_unit);
            info.pl1_mmio = pl1;
            info.pl1_mmio_enabled = pl1_en;
            info.pl1_mmio_clamped = pl1_cl;
            info.pl1_mmio_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
            info.pl2_mmio = pl2;
            info.pl2_mmio_enabled = pl2_en;
            info.pl2_mmio_clamped = pl2_cl;
            info.pl2_mmio_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
            debug!("MMIO PL1: {:.1}W (en={} clamp={}) time={:.1}s", pl1, pl1_en, pl1_cl, info.pl1_mmio_time_s);
            debug!("MMIO PL2: {:.1}W (en={} clamp={}) time={:.1}s", pl2, pl2_en, pl2_cl, info.pl2_mmio_time_s);
        }
    }

    // Both limit registers unreadable: report unavailable instead of
    // presenting all-zero limits as valid data.
    if !msr610_ok && !mmio_ok {
        if info.error_msg.is_none() {
            info.error_msg = Some("Failed to read MSR 0x610 and MMIO power limits");
        }
        return info;
    }
    info.available = true;
    info
}

/// Background sync thread that continuously writes MSR 0x610
/// to counter EC overwriting.
struct SyncThread {
    running: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SyncThread {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Start the sync thread with the given PL1/PL2 parameters.
    /// `external_alive` is `CpuPowerState.sync_alive` — kept in sync with the
    /// thread's real lifecycle so the UI tick can detect thread death
    /// (PawnIO load failure, handle open failure, panic) without locking.
    fn start(&mut self, params: PowerLimitParams, external_alive: Arc<AtomicBool>) -> Result<(), &'static str> {
        self.stop();

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = alive.clone();
        let external_clone = external_alive;

        let handle = std::thread::Builder::new()
            .name("cpu-power-sync".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sync_thread_main(running_clone, params);
                }));
                if result.is_err() {
                    warn!("Sync thread panicked");
                }
                // Clear BOTH liveness flags on exit so the UI cannot show a
                // stale "Syncing" state after an early return or panic.
                alive_clone.store(false, Ordering::Release);
                external_clone.store(false, Ordering::Release);
            })
            .map_err(|_| "failed to spawn sync thread")?;

        self.running = running;
        self.alive = alive;
        self.handle = Some(handle);
        Ok(())
    }

    /// Stop the sync thread.
    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.alive.store(false, Ordering::Release);
    }
}

/// Main loop for the sync thread. Writes MSR 0x610 and MMIO every 250ms.
fn sync_thread_main(running: Arc<AtomicBool>, params: PowerLimitParams) {
    // Load IntelMSR module and open a persistent handle.
    let msr_blob = match load_intel_msr_blob() {
        Ok(b) => b,
        Err(e) => {
            warn!("Sync thread: failed to load IntelMSR module: {}", e);
            return;
        }
    };

    let msr_handle = match open_handle(&msr_blob) {
        Ok(h) => h,
        Err(e) => {
            warn!("Sync thread: failed to open MSR handle: {}", e);
            return;
        }
    };

    // Load IntelMCHBAR module for MMIO write (may fail if module is read-only).
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            warn!("Sync thread: MMIO write unavailable ({}), will write MSR only", e);
            None
        }
    };

    debug!("Sync thread started: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) mmio={}",
        params.pl1_watts, params.pl1_time_s, params.pl2_watts, params.pl2_time_s,
        mchbar_handle.is_some());

    // Consecutive write failures. An EC/firmware override makes the read-back
    // verification fail, but the sync must survive it: keep retrying (the
    // override ends when the user or firmware stops fighting) instead of
    // dying on the first error.
    let mut write_failures: u32 = 0;
    let mut mmio_write_failures: u32 = 0;
    while running.load(Ordering::Relaxed) {
        // Unconditional periodic rewrite: the whole point of this thread is to
        // re-assert the user's limits against EC/firmware overrides. The
        // register is NOT re-read here — a previous "skip when equal"
        // optimization compared against our own last write (never the live
        // hardware value), which silently disabled the sync after its first
        // successful write.
        match write_msr_pl1_pl2(&msr_handle, &params) {
            Ok(()) => {
                write_failures = 0;
            }
            Err(e) => {
                write_failures += 1;
                if write_failures <= 5 {
                    warn!("Sync thread MSR write failed: {}", e);
                } else if write_failures == 6 {
                    warn!("Sync thread MSR write keeps failing ({}), suppressing further warnings", e);
                }
            }
        }
        if let Some(ref mchbar) = mchbar_handle {
            match write_mmio_pl1_pl2(mchbar, &params) {
                Ok(()) => {
                    mmio_write_failures = 0;
                }
                Err(e) => {
                    mmio_write_failures += 1;
                    if mmio_write_failures <= 5 {
                        warn!("Sync thread MMIO write failed: {}", e);
                    } else if mmio_write_failures == 6 {
                        warn!("Sync thread MMIO write keeps failing ({}), suppressing further warnings", e);
                    }
                }
            }
        }
        // Back off on persistent failures: a BIOS-locked register can never
        // succeed, and hammering the driver at 4 Hz forever wastes CPU and
        // kernel I/O for a write that will not land. Cap at 5s between tries.
        let interval_ms = if write_failures >= 10 {
            5000
        } else if write_failures >= 4 {
            1000
        } else {
            250
        };
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }

    debug!("Sync thread stopped");
}

/// BIOS power limit defaults — captured once at first run and persisted to disk,
/// never overwritten by subsequent refreshes so Reset always returns to the
/// original factory values.  Stores both MSR and MMIO so the effective
/// `min(MSR, MMIO)` is fully restored.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BiosDefaults {
    pub pl1_watts: f64,
    pub pl1_enabled: bool,
    pub pl1_clamped: bool,
    pub pl1_time_s: f64,
    pub pl2_watts: f64,
    pub pl2_enabled: bool,
    pub pl2_clamped: bool,
    pub pl2_time_s: f64,
    #[serde(default)]
    pub pl1_mmio_watts: f64,
    #[serde(default)]
    pub pl1_mmio_enabled: bool,
    #[serde(default)]
    pub pl1_mmio_clamped: bool,
    #[serde(default)]
    pub pl1_mmio_time_s: f64,
    #[serde(default)]
    pub pl2_mmio_watts: f64,
    #[serde(default)]
    pub pl2_mmio_enabled: bool,
    #[serde(default)]
    pub pl2_mmio_clamped: bool,
    #[serde(default)]
    pub pl2_mmio_time_s: f64,
    pub power_unit: f64,
    pub time_unit: f64,
    /// Power source at capture time. The snapshot reflects the OEM's limits
    /// for THAT source; restoring an AC snapshot while on battery would
    /// override the firmware's lower battery-mode protection, so resume
    /// only writes when the source matches.
    #[serde(default = "default_true_fn")]
    pub captured_on_ac: bool,
}

fn default_true_fn() -> bool {
    true
}

fn bios_defaults_path() -> Result<std::path::PathBuf, String> {
    let cfg_path = crate::config::config_path()?;
    Ok(cfg_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("bios_defaults.toml"))
}

fn bios_defaults_file_exists() -> bool {
    bios_defaults_path().map(|p| p.exists()).unwrap_or(false)
}

/// Read the current AC-present state from the shared battery snapshot.
/// Defaults to `true` (AC) when no reading exists yet — matching the
/// pre-existing conservative default so the first capture records AC
/// semantics unless the battery path has already published data.
pub fn read_ac_present() -> bool {
    match BATTERY_AC_SNAPSHOT.read() {
        Ok(guard) => *guard,
        Err(poisoned) => {
            warn!("BATTERY_AC_SNAPSHOT poisoned (writer panicked); defaulting to AC");
            *poisoned.into_inner()
        }
    }
}

static BATTERY_AC_SNAPSHOT: std::sync::RwLock<bool> = std::sync::RwLock::new(true);

/// Publish the current AC state for `read_ac_present`. Called by the
/// background thermal loop alongside its battery poll.
pub fn publish_ac_snapshot(ac_present: bool) {
    if let Ok(mut guard) = BATTERY_AC_SNAPSHOT.write() {
        *guard = ac_present;
    }
}

fn load_persisted_bios_defaults() -> Option<BiosDefaults> {    let path = bios_defaults_path().ok()?;
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Missing file = first run, caller may capture current values.
        Err(_) => return None,
    };
    match toml::from_str::<BiosDefaults>(&content) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            // The file EXISTS but is corrupt. Never overwrite the original
            // factory snapshot with live values in this case — back it up
            // and refuse to capture until the user deletes it.
            let backup = path.with_extension(format!(
                "toml.corrupt-{}",
                crate::util::current_time_ms()
            ));
            match std::fs::copy(&path, &backup) {
                Ok(_) => warn!(
                    "bios_defaults.toml is corrupt ({}); backed up to {} — NOT overwriting with live values. Delete the file to re-capture.",
                    e,
                    backup.display()
                ),
                Err(be) => warn!(
                    "bios_defaults.toml is corrupt ({}) and backup failed: {}",
                    e, be
                ),
            }
            None
        }
    }
}

fn persist_bios_defaults(defaults: &BiosDefaults) -> Result<(), String> {
    let path = bios_defaults_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create bios_defaults dir failed: {}", e))?;
    }
    let content = toml::to_string_pretty(defaults).map_err(|e| e.to_string())?;
    // Write via temp file and atomic replace so a crash mid-write never leaves
    // a corrupt bios_defaults.toml (same pattern as config.rs).
    let tmp = path.with_extension(format!("tmp.{}", crate::util::current_time_ms()));
    std::fs::write(&tmp, content.as_bytes())
        .map_err(|e| format!("write tmp bios_defaults failed: {}", e))?;
    // Use MoveFileExW on Windows for atomic replace; fallback to rename.
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        let tmp_w: Vec<u16> = OsStr::new(&tmp).encode_wide().chain(std::iter::once(0)).collect();
        let dst_w: Vec<u16> = OsStr::new(&path).encode_wide().chain(std::iter::once(0)).collect();
        let ok = unsafe { MoveFileExW(tmp_w.as_ptr(), dst_w.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
        if ok != 0 {
            Ok(())
        } else {
            let _ = std::fs::remove_file(&path);
            std::fs::rename(&tmp, &path).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("rename bios_defaults failed: {}", e)
            })?;
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("rename bios_defaults failed: {}", e)
        })?;
        Ok(())
    }
}

/// Shared CPU power state for background task and UI.
#[derive(Clone)]
pub struct CpuPowerState {
    pub info: Arc<parking_lot::RwLock<Arc<CpuPowerInfo>>>,
    pub available: Arc<AtomicBool>,
    pub sync_enabled: Arc<AtomicBool>,
    sync_thread: Arc<parking_lot::Mutex<SyncThread>>,
    /// Lives OUTSIDE the sync_thread mutex so `is_sync_alive()` (called on
    /// the UI tick) never blocks behind a stop/start that is joining the
    /// thread (which can take seconds while the sync thread sleeps between
    /// write attempts).
    sync_alive: Arc<AtomicBool>,
    bios: Arc<parking_lot::RwLock<Arc<Option<BiosDefaults>>>>,
}

impl Default for CpuPowerState {
    fn default() -> Self {
        Self {
            info: Arc::new(parking_lot::RwLock::new(Arc::new(CpuPowerInfo::default()))),
            available: Arc::new(AtomicBool::new(false)),
            sync_enabled: Arc::new(AtomicBool::new(false)),
            sync_thread: Arc::new(parking_lot::Mutex::new(SyncThread::new())),
            sync_alive: Arc::new(AtomicBool::new(false)),
            bios: Arc::new(parking_lot::RwLock::new(Arc::new(None))),
        }
    }
}

impl CpuPowerState {
    pub fn refresh(&self) {
        let info = read_cpu_power();
        let is_available = info.available;
        self.available.store(is_available, Ordering::Release);
        with_write_lock(&self.info, |guard| {
            *guard = Arc::new(info);
        });
        // If we still lack the original BIOS snapshot and now have live data,
        // capture it — on first ever run this persists the true factory values
        // so Reset stays correct after the user has modified them.
        if is_available && self.bios_defaults().is_none() {
            self.init_bios_defaults();
        }
    }

    /// Capture BIOS defaults from current MSR read. On first ever run the
    /// values are persisted to `bios_defaults.toml` alongside `config.toml`;
    /// subsequent runs load the persisted file so Reset always returns to the
    /// original factory values even after the user has modified PL1/PL2.
    /// Safe to call multiple times — only the first successful capture writes.
    pub fn init_bios_defaults(&self) {
        // Already in memory — nothing to do.
        if self.bios_defaults().is_some() {
            return;
        }
        // Try to load previously persisted originals.
        if let Some(persisted) = load_persisted_bios_defaults() {
            with_write_lock(&self.bios, |guard| {
                *guard = Arc::new(Some(persisted));
            });
            info!("Loaded persisted BIOS defaults: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
                persisted.pl1_watts, persisted.pl1_time_s, persisted.pl2_watts, persisted.pl2_time_s);
            return;
        }
        // If the file exists but is corrupt, load_persisted_bios_defaults()
        // already backed it up and logged — do NOT capture live values over
        // it (that would destroy the factory snapshot permanently).
        if bios_defaults_file_exists() {
            warn!("bios_defaults.toml exists but could not be loaded; refusing to overwrite with live values");
            return;
        }
        let info = self.snapshot();
        if !info.available { return; }
        // Record the power source at capture time so resume can refuse to
        // restore an AC snapshot onto battery (or vice versa).
        let captured_on_ac = read_ac_present();
        let defaults = BiosDefaults {
            pl1_watts: info.pl1_msr,
            pl1_enabled: info.pl1_msr_enabled,
            pl1_clamped: info.pl1_msr_clamped,
            pl1_time_s: info.pl1_time_s,
            pl2_watts: info.pl2_msr,
            pl2_enabled: info.pl2_msr_enabled,
            pl2_clamped: info.pl2_msr_clamped,
            pl2_time_s: info.pl2_time_s,
            pl1_mmio_watts: info.pl1_mmio,
            pl1_mmio_enabled: info.pl1_mmio_enabled,
            pl1_mmio_clamped: info.pl1_mmio_clamped,
            pl1_mmio_time_s: info.pl1_mmio_time_s,
            pl2_mmio_watts: info.pl2_mmio,
            pl2_mmio_enabled: info.pl2_mmio_enabled,
            pl2_mmio_clamped: info.pl2_mmio_clamped,
            pl2_mmio_time_s: info.pl2_mmio_time_s,
            power_unit: info.power_unit,
            time_unit: info.time_unit,
            captured_on_ac,
        };
        if let Err(e) = persist_bios_defaults(&defaults) {
            warn!("Failed to persist BIOS defaults: {}", e);
        } else {
            info!("Persisted original BIOS defaults: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
                defaults.pl1_watts, defaults.pl1_time_s, defaults.pl2_watts, defaults.pl2_time_s);
        }
        with_write_lock(&self.bios, |guard| {
            *guard = Arc::new(Some(defaults));
        });
        info!("BIOS defaults captured: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
            defaults.pl1_watts, defaults.pl1_time_s, defaults.pl2_watts, defaults.pl2_time_s);
    }

    /// Get BIOS defaults captured at startup.
    pub fn bios_defaults(&self) -> Option<BiosDefaults> {
        *crate::util::read_lock(&self.bios)
    }

    pub fn snapshot(&self) -> Arc<CpuPowerInfo> {
        crate::util::read_lock(&self.info).clone()
    }

    /// Start the sync thread that continuously writes MSR 0x610.
    #[allow(clippy::too_many_arguments)]
    pub fn start_sync(
        &self,
        pl1_watts: f64,
        pl1_enabled: bool,
        pl1_clamped: bool,
        pl1_time_s: f64,
        pl2_watts: f64,
        pl2_enabled: bool,
        pl2_clamped: bool,
        pl2_time_s: f64,
        power_unit: f64,
        time_unit: f64,
    ) -> Result<(), &'static str> {
        let params = PowerLimitParams {
            pl1_watts,
            pl1_enabled,
            pl1_clamped,
            pl1_time_s,
            pl2_watts,
            pl2_enabled,
            pl2_clamped,
            pl2_time_s,
            power_unit,
            time_unit,
        };
        // Stop any previous thread, joining OUTSIDE the mutex: the old thread
        // may be mid-sleep (up to 5s during write-failure backoff) and holding
        // the lock across that join would block is_sync_alive() on the UI tick.
        let old_handle = {
            let mut thread = self.sync_thread.lock();
            thread.running.store(false, Ordering::Release);
            thread.handle.take()
        };
        if let Some(old) = old_handle {
            let _ = old.join();
        }
        let mut thread = self.sync_thread.lock();
        thread.start(params, Arc::clone(&self.sync_alive))?;
        self.sync_alive.store(true, Ordering::Release);
        self.sync_enabled.store(true, Ordering::Release);
        Ok(())
    }

    /// Check if the sync thread is still alive (hasn't exited on its own).
    /// Lock-free: reads a dedicated atomic, never blocks on the mutex that
    /// stop/start hold while joining.
    pub fn is_sync_alive(&self) -> bool {
        self.sync_alive.load(Ordering::Acquire)
    }

    /// Stop the sync thread.
    pub fn stop_sync(&self) {
        let old_handle = {
            let mut thread = self.sync_thread.lock();
            thread.running.store(false, Ordering::Release);
            thread.handle.take()
        };
        if let Some(old) = old_handle {
            let _ = old.join();
        }
        {
            let thread = self.sync_thread.lock();
            thread.alive.store(false, Ordering::Release);
        }
        self.sync_alive.store(false, Ordering::Release);
        self.sync_enabled.store(false, Ordering::Release);
    }
}

/// Cached PawnIO version.
static PAWNIO_VERSION: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// Read PawnIO version from DLL file metadata (no subprocess needed).
fn fetch_pawnio_version_from_dll() -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};

    let path: Vec<u16> = std::ffi::OsStr::new(DLL_PATH)
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

        // Query the root block to get fixed file info (contains product version).
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
        ) == 0 || ffi_ptr.is_null()
        {
            return None;
        }

        // VS_FIXEDFILEINFO is 52 bytes (13 u32s); the product version fields
        // we read live at offsets 16/20, so require at least 24 bytes.
        if ffi_len < 24 {
            return None;
        }

        // VS_FIXEDFILEINFO layout: first 2 u32 are Signature and StrucVersion,
        // then dwFileVersionMS (offset 8), dwFileVersionLS (offset 12),
        // then dwProductVersionMS (offset 16), dwProductVersionLS (offset 20).
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

/// Get PawnIO version (cached).
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

/// Clear the cached PawnIO version so the next pawnio_version() re-reads
/// the DLL metadata. Call after installing or upgrading PawnIO — the cache
/// would otherwise report the old version for the rest of the process.
pub fn invalidate_pawnio_version() {
    *PAWNIO_VERSION.write() = None;
}

/// Get the embedded PawnIO Modules blob version.
pub fn pawnio_modules_version() -> &'static str {
    PAWNIO_MODULES_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn verify_module_hash_rejects_mismatch() {
        let dir = std::env::temp_dir().join("framework-crate-hash-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.bin");
        std::fs::write(&path, b"not-a-module").unwrap();
        assert!(verify_module_hash(&path, INTEL_MSR_SHA256).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bios_defaults_toml_roundtrip() {
        let defaults = BiosDefaults {
            pl1_watts: 30.0,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 60.0,
            pl2_enabled: true,
            pl2_clamped: true,
            pl2_time_s: 2.5,
            pl1_mmio_watts: 30.0,
            pl1_mmio_enabled: true,
            pl1_mmio_clamped: false,
            pl1_mmio_time_s: 28.0,
            pl2_mmio_watts: 60.0,
            pl2_mmio_enabled: true,
            pl2_mmio_clamped: true,
            pl2_mmio_time_s: 2.5,
            power_unit: 0.125,
            time_unit: 0.00098,
            captured_on_ac: true,
        };
        let toml_str = toml::to_string_pretty(&defaults).unwrap();
        let parsed: BiosDefaults = toml::from_str(&toml_str).unwrap();
        assert_eq!(defaults, parsed);
    }

    #[test]
    fn bios_defaults_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };
        let defaults = BiosDefaults {
            pl1_watts: 15.0,
            pl1_enabled: true,
            pl1_clamped: true,
            pl1_time_s: 56.0,
            pl2_watts: 45.0,
            pl2_enabled: false,
            pl2_clamped: false,
            pl2_time_s: 8.0,
            pl1_mmio_watts: 15.0,
            pl1_mmio_enabled: true,
            pl1_mmio_clamped: true,
            pl1_mmio_time_s: 56.0,
            pl2_mmio_watts: 45.0,
            pl2_mmio_enabled: false,
            pl2_mmio_clamped: false,
            pl2_mmio_time_s: 8.0,
            power_unit: 0.125,
            time_unit: 0.001,
            captured_on_ac: false,
        };
        persist_bios_defaults(&defaults).unwrap();
        let loaded = load_persisted_bios_defaults().expect("should load persisted");
        assert_eq!(defaults, loaded);
        // Second persist should overwrite (but init_bios_defaults would not call it if already loaded)
        let defaults2 = BiosDefaults { pl1_watts: 99.0, ..defaults };
        persist_bios_defaults(&defaults2).unwrap();
        let loaded2 = load_persisted_bios_defaults().unwrap();
        assert_eq!(loaded2.pl1_watts, 99.0);
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v);
            } else {
                std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR");
            }
        }
    }
}
