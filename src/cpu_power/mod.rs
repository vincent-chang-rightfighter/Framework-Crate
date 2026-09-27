mod bios;
mod limits;
mod modules;
mod read;
mod sync;
mod version;

pub use bios::{BiosDefaults, publish_ac_snapshot, read_ac_present};
// write_mmio_pl1_pl2_public was already unreferenced before the split; it is
// re-exported to keep the module surface unchanged rather than dropped.
#[allow(unused_imports)]
pub use limits::{
    CpuPowerInfo, write_bios_defaults, write_mmio_pl1_pl2_public, write_msr_pl1_pl2_public,
};
pub use modules::{
    download_and_extract_modules, modules_downloaded, open_modules_dir, redetect_modules,
};
pub use read::read_cpu_power;
pub use version::{
    invalidate_modules_version, invalidate_pawnio_version, pawnio_modules_version, pawnio_version,
};

use bios::{bios_defaults_file_exists, load_persisted_bios_defaults, persist_bios_defaults};
use limits::PowerLimitParams;
use modules::sha256_hex;
use sync::SyncThread;

use std::ffi::CString;
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::HANDLE;

use crate::util::with_write_lock;

// PawnIOLib.dll function signatures (STDMETHODCALLTYPE / WINAPI — same on x64).
type PawnioOpen = unsafe extern "system" fn(*mut HANDLE) -> i32; // HRESULT
type PawnioLoad = unsafe extern "system" fn(HANDLE, *const u8, usize) -> i32; // HRESULT
type PawnioExecute = unsafe extern "system" fn(
    HANDLE,
    *const u8,
    *const u64,
    usize,
    *mut u64,
    usize,
    *mut usize,
) -> i32; // HRESULT
type PawnioClose = unsafe extern "system" fn(HANDLE) -> i32; // HRESULT

/// PawnIO handle with DLL functions.
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

/// Global DLL pointers (reinitializable after PawnIO upgrade).
static DLL_OPEN: parking_lot::Mutex<Option<PawnioOpen>> = parking_lot::Mutex::new(None);
static DLL_LOAD: parking_lot::Mutex<Option<PawnioLoad>> = parking_lot::Mutex::new(None);
static DLL_EXEC: parking_lot::Mutex<Option<PawnioExecute>> = parking_lot::Mutex::new(None);
static DLL_CLOSE: parking_lot::Mutex<Option<PawnioClose>> = parking_lot::Mutex::new(None);
/// Loaded module handle for FreeLibrary on reset; single init lock closes check-then-load race.
static DLL_MODULE: parking_lot::Mutex<Option<isize>> = parking_lot::Mutex::new(None);
static DLL_INIT_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

const DLL_PATH: &str = r"C:\Program Files\PawnIO\PawnIOLib.dll";

fn known_program_files() -> Option<std::path::PathBuf> {
    // Query the OS instead of trusting %ProgramFiles% env (a malicious
    // launcher can override env vars for the child process).
    use windows_sys::Win32::UI::Shell::SHGetFolderPathW;
    const CSIDL_PROGRAM_FILES: i32 = 0x0026;
    const MAX_PATH: usize = 260;
    let mut buf = [0u16; MAX_PATH];
    let ok = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            CSIDL_PROGRAM_FILES,
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

fn resolved_dll_path() -> std::path::PathBuf {
    if let Some(pf) = known_program_files() {
        let cand = pf.join("PawnIO").join("PawnIOLib.dll");
        if cand.exists() {
            return cand;
        }
    }
    // Fallback for non-standard layouts; verify_dll_path still enforces prefix.
    if let Ok(pf) = std::env::var("ProgramW6432") {
        let cand = std::path::PathBuf::from(pf)
            .join("PawnIO")
            .join("PawnIOLib.dll");
        if cand.exists() {
            return cand;
        }
    }
    std::path::PathBuf::from(DLL_PATH)
}

/// Installs PawnIO via winget.
pub fn install_pawnio() -> Result<(), String> {
    use std::process::Command;
    tracing::info!("Installing PawnIO via winget...");
    let output = Command::new("winget")
        .args([
            "install",
            "-e",
            "--id",
            "namazso.PawnIO",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| format!("failed to run winget: {}", e))?;
    if output.status.success() {
        tracing::info!("PawnIO installed successfully");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{} {}", stderr, stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit code {}", output.status.code().unwrap_or(-1))
        };
        // winget reports "already installed" / "No newer" as non-zero but is actually success (up to date)
        if combined.contains("already installed")
            || combined.contains("No available upgrade")
            || combined.contains("No newer package")
        {
            tracing::info!("PawnIO already installed and up to date");
            return Ok(());
        }
        // Common causes: winget not installed, not in PATH, or not elevated
        let hint = if detail.contains("not recognized") || detail.contains("not found") {
            " (winget not found — install App Installer from Microsoft Store or download PawnIO from https://github.com/namazso/PawnIO/releases)"
        } else if detail.to_lowercase().contains("elevation") || detail.contains("0x800704C7") {
            " (requires elevation — run as administrator)"
        } else {
            " — you can also download PawnIO manually from https://github.com/namazso/PawnIO/releases"
        };
        Err(format!("winget install failed: {}{}", detail, hint))
    }
}

/// Checks if PawnIO DLL is installed.
pub fn is_pawnio_installed() -> bool {
    resolved_dll_path().exists()
}

/// Updates PawnIO via winget upgrade, fallback to install.
pub fn update_pawnio() -> Result<(), String> {
    use std::process::Command;
    tracing::info!("Updating PawnIO via winget...");
    let output = Command::new("winget")
        .args([
            "upgrade",
            "-e",
            "--id",
            "namazso.PawnIO",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .creation_flags(0x08000000)
        .output();
    if let Ok(ref out) = output
        && out.status.success()
    {
        tracing::info!("PawnIO upgraded successfully");
        invalidate_pawnio_version();
        return Ok(());
    }
    if let Ok(ref out) = output {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{} {}", stderr, stdout);
        if combined.contains("No available upgrade") || combined.contains("No newer package") {
            tracing::info!("PawnIO already up to date");
            invalidate_pawnio_version();
            return Ok(());
        }
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else if !stdout.trim().is_empty() {
            stdout.trim()
        } else {
            ""
        };
        tracing::warn!("winget upgrade output: {}", detail);
    }
    tracing::info!("winget upgrade failed or not available, falling back to install");
    install_pawnio()
}

/// Forces redownload of PawnIO Modules (update).
pub fn update_pawnio_modules() -> Result<(), String> {
    download_and_extract_modules()
}

/// Verifies DLL is at expected location and not a symlink/reparse point before loading.
fn verify_dll_path() -> Result<(), &'static str> {
    let p = resolved_dll_path();
    // Must exist and not be a symlink
    let meta = std::fs::symlink_metadata(&p).map_err(|_| "PawnIO not installed")?;
    if meta.file_type().is_symlink() {
        warn!("PawnIO DLL is a symlink, refusing to load: {}", p.display());
        return Err("DLL is symlink");
    }
    // Canonicalize and require the exact <ProgramFiles>\PawnIO\PawnIOLib.dll
    // suffix instead of a loose substring match.
    // NOTE: canonicalize returns verbatim \\?\ paths; strip the prefix first
    // or the prefix comparison below always fails.
    fn strip_verbatim(s: &str) -> String {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
        s.to_string()
    }
    if let Ok(canon) = std::fs::canonicalize(&p) {
        let canon_str = strip_verbatim(&canon.to_string_lossy().to_lowercase()).replace('/', "\\");
        let suffix = "\\pawnio\\pawniolib.dll";
        // Reconstruct expected prefix from the OS-known Program Files when available.
        let prefix_ok = known_program_files().map(|pf| {
            let mut pre = pf.to_string_lossy().to_lowercase().replace('/', "\\");
            if !pre.ends_with('\\') {
                pre.push('\\');
            }
            canon_str.starts_with(&pre)
        });
        if !canon_str.ends_with(suffix) || prefix_ok == Some(false) {
            warn!("PawnIO DLL canonical path unexpected: {}", canon.display());
            return Err("DLL path mismatch");
        }
    }
    // Authenticode check via PowerShell Get-AuthenticodeSignature (cached per process)
    static AUTHENTICODE_RESULT: parking_lot::Once = parking_lot::Once::new();
    static AUTHENTICODE_OK: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(true);
    AUTHENTICODE_RESULT.call_once(|| {
        if let Ok(out) = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-AuthenticodeSignature '{}').Status -eq 'Valid'",
                    p.display().to_string().replace('\'', "''")
                ),
            ])
            .creation_flags(0x08000000)
            .output()
        {
            let txt = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if txt.contains("false") {
                warn!(
                    "PawnIO DLL authenticode not Valid (may be unsigned/test-signed): {}",
                    p.display()
                );
                AUTHENTICODE_OK.store(false, std::sync::atomic::Ordering::Release);
            }
        }
    });
    if !AUTHENTICODE_OK.load(std::sync::atomic::Ordering::Acquire) {
        // Enforced in all profiles; developers with a self-signed test DLL
        // must set FRAMEWORK_ALLOW_UNSIGNED_PAWNIO=1 explicitly.
        if std::env::var_os("FRAMEWORK_ALLOW_UNSIGNED_PAWNIO").is_none() {
            return Err("DLL authenticode not valid");
        }
        warn!("FRAMEWORK_ALLOW_UNSIGNED_PAWNIO set; loading unsigned DLL");
    }
    // Hash pin: ensure DLL is not zero/truncated and log hash for audit
    if let Ok(bytes) = std::fs::read(&p) {
        if bytes.len() < 10_000 {
            warn!("PawnIO DLL unusually small: {} bytes", bytes.len());
            return Err("DLL too small");
        }
        // log hash for manual pinning; no hard pin yet to allow PawnIO updates
        tracing::debug!("PawnIO DLL sha256: {}", sha256_hex(&bytes));
    }
    Ok(())
}

/// Initializes DLL function pointers (reinitializable).
fn init_dll_fns() -> Result<(), &'static str> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW};

    // Single lock closes the check-then-load race between concurrent first calls.
    let _init = DLL_INIT_LOCK.lock();
    if DLL_OPEN.lock().is_some() {
        return Ok(()); // already initialized
    }

    verify_dll_path()?;

    let resolved = resolved_dll_path();
    let wide: Vec<u16> = resolved
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // Restrict DLL search to the DLL's own directory + System32 to prevent side-loading.
    const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x00000100;
    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x00000800;
    let dll = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if dll.is_null() {
        return Err("PawnIO not installed");
    }

    unsafe {
        // Transmute FARPROC safely with size assertions for each target type.
        const _: () = assert!(
            std::mem::size_of::<PawnioOpen>()
                == std::mem::size_of::<windows_sys::Win32::Foundation::FARPROC>()
        );
        const _: () = assert!(
            std::mem::size_of::<PawnioLoad>()
                == std::mem::size_of::<windows_sys::Win32::Foundation::FARPROC>()
        );
        const _: () = assert!(
            std::mem::size_of::<PawnioExecute>()
                == std::mem::size_of::<windows_sys::Win32::Foundation::FARPROC>()
        );
        const _: () = assert!(
            std::mem::size_of::<PawnioClose>()
                == std::mem::size_of::<windows_sys::Win32::Foundation::FARPROC>()
        );
        let addr = GetProcAddress(dll, c"pawnio_open".as_ptr() as *const u8)
            .ok_or("pawnio_open not found")?;
        let open: PawnioOpen = std::mem::transmute(addr);
        let addr = GetProcAddress(dll, c"pawnio_load".as_ptr() as *const u8)
            .ok_or("pawnio_load not found")?;
        let load: PawnioLoad = std::mem::transmute(addr);
        let addr = GetProcAddress(dll, c"pawnio_execute".as_ptr() as *const u8)
            .ok_or("pawnio_execute not found")?;
        let exec: PawnioExecute = std::mem::transmute(addr);
        let addr = GetProcAddress(dll, c"pawnio_close".as_ptr() as *const u8)
            .ok_or("pawnio_close not found")?;
        let close: PawnioClose = std::mem::transmute(addr);
        *DLL_MODULE.lock() = Some(dll as isize);
        *DLL_OPEN.lock() = Some(open);
        *DLL_LOAD.lock() = Some(load);
        *DLL_EXEC.lock() = Some(exec);
        *DLL_CLOSE.lock() = Some(close);
    }
    Ok(())
}

/// Resets DLL function pointers so next call to init_dll_fns re-loads from disk.
/// Called after PawnIO upgrade to pick up the new DLL.
pub fn reset_dll_fns() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeLibrary(hLibModule: *mut core::ffi::c_void) -> i32;
    }
    let _init = DLL_INIT_LOCK.lock();
    if let Some(raw) = DLL_MODULE.lock().take() {
        unsafe {
            FreeLibrary(raw as *mut core::ffi::c_void);
        }
    }
    *DLL_OPEN.lock() = None;
    *DLL_LOAD.lock() = None;
    *DLL_EXEC.lock() = None;
    *DLL_CLOSE.lock() = None;
    invalidate_pawnio_version();
}

/// Opens PawnIO handle and loads module blob.
fn open_handle(blob: &[u8]) -> Result<PawnioHandle, &'static str> {
    init_dll_fns()?;

    let mut handle: HANDLE = std::ptr::null_mut();
    let open = DLL_OPEN.lock().ok_or("PawnIO DLL not initialized")?;
    let hr = unsafe { open(&mut handle) };
    if hr < 0 || handle.is_null() {
        warn!("pawnio_open returned hr=0x{:X} handle={:?}", hr, handle);
        return Err("pawnio_open failed");
    }

    let load = DLL_LOAD.lock().ok_or("PawnIO DLL not initialized")?;
    let hr = unsafe { load(handle, blob.as_ptr(), blob.len()) };
    if hr < 0 {
        warn!("pawnio_load returned hr=0x{:X}", hr);
        let close = DLL_CLOSE.lock().ok_or("PawnIO DLL not initialized")?;
        unsafe { close(handle) };
        return Err("pawnio_load failed");
    }

    let exec_fn = DLL_EXEC.lock().ok_or("PawnIO DLL not initialized")?;
    let close_fn = DLL_CLOSE.lock().ok_or("PawnIO DLL not initialized")?;
    Ok(PawnioHandle {
        handle,
        exec_fn,
        close_fn,
    })
}

/// Executes named IOCTL in loaded module.
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
    // Short write leaves buffer stale; must not be treated as valid.
    if return_size != outputs.len() {
        return Err("ioctl short read");
    }
    Ok(return_size)
}

/// Shared CPU power state.
#[derive(Clone)]
pub struct CpuPowerState {
    pub info: Arc<parking_lot::RwLock<Arc<CpuPowerInfo>>>,
    pub available: Arc<AtomicBool>,
    pub sync_enabled: Arc<AtomicBool>,
    sync_thread: Arc<parking_lot::Mutex<SyncThread>>,
    /// Outside mutex so liveness check never blocks on join.
    sync_alive: Arc<AtomicBool>,
    sync_start_ms: Arc<std::sync::atomic::AtomicU64>,
    bios: Arc<parking_lot::RwLock<Arc<Option<BiosDefaults>>>>,
    desired_sync: Arc<parking_lot::RwLock<Option<PowerLimitParams>>>,
}

impl Default for CpuPowerState {
    fn default() -> Self {
        Self {
            info: Arc::new(parking_lot::RwLock::new(Arc::new(CpuPowerInfo::default()))),
            available: Arc::new(AtomicBool::new(false)),
            sync_enabled: Arc::new(AtomicBool::new(false)),
            sync_thread: Arc::new(parking_lot::Mutex::new(SyncThread::new())),
            sync_alive: Arc::new(AtomicBool::new(false)),
            sync_start_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            bios: Arc::new(parking_lot::RwLock::new(Arc::new(None))),
            desired_sync: Arc::new(parking_lot::RwLock::new(None)),
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

    /// Captures BIOS defaults; persists on first run for Reset.
    pub fn init_bios_defaults(&self) {
        // Already in memory — nothing to do.
        if self.bios_defaults().is_some() {
            return;
        }
        // Try loading persisted originals.
        if let Some(persisted) = load_persisted_bios_defaults() {
            with_write_lock(&self.bios, |guard| {
                *guard = Arc::new(Some(persisted));
            });
            info!(
                "Loaded persisted BIOS defaults: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
                persisted.pl1_watts,
                persisted.pl1_time_s,
                persisted.pl2_watts,
                persisted.pl2_time_s
            );
            return;
        }
        // If file exists but corrupt, do not overwrite; already backed up.
        if bios_defaults_file_exists() {
            warn!(
                "bios_defaults.toml exists but could not be loaded; refusing to overwrite with live values"
            );
            return;
        }
        let info = self.snapshot();
        if !info.available {
            return;
        }
        // Record power source to prevent cross-source restore on resume.
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
            info!(
                "Persisted original BIOS defaults: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
                defaults.pl1_watts, defaults.pl1_time_s, defaults.pl2_watts, defaults.pl2_time_s
            );
        }
        with_write_lock(&self.bios, |guard| {
            *guard = Arc::new(Some(defaults));
        });
        info!(
            "BIOS defaults captured: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s)",
            defaults.pl1_watts, defaults.pl1_time_s, defaults.pl2_watts, defaults.pl2_time_s
        );
    }

    /// Returns BIOS defaults captured at startup.
    pub fn bios_defaults(&self) -> Option<BiosDefaults> {
        *crate::util::read_lock(&self.bios)
    }

    pub fn snapshot(&self) -> Arc<CpuPowerInfo> {
        crate::util::read_lock(&self.info).clone()
    }

    /// Starts sync thread that continuously writes MSR 0x610.
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
        self.start_sync_with_params(params)
    }

    /// Shared implementation. Holds the `sync_thread` lock across the entire
    /// stop-old + join + start sequence so a concurrent `start_sync` (e.g.
    /// `CpuPowerSyncStart` racing `CpuPowerApplied`, or a resume handler) cannot
    /// slip into the gap between taking the old handle and starting a new one
    /// and spawn two sync threads writing MSR/MMIO at once. The sync loop never
    /// takes this lock, so joining under it cannot deadlock.
    fn start_sync_with_params(&self, params: PowerLimitParams) -> Result<(), &'static str> {
        let mut thread = self.sync_thread.lock();
        thread.running.store(false, Ordering::Release);
        if let Some(old) = thread.handle.take() {
            let _ = old.join();
        }
        thread.start(params, Arc::clone(&self.sync_alive))?;
        self.sync_enabled.store(true, Ordering::Release);
        self.sync_start_ms
            .store(crate::util::monotonic_ms(), Ordering::Release);
        *self.desired_sync.write() = Some(params);
        Ok(())
    }

    /// Checks if sync thread is still alive (lock-free).
    pub fn is_sync_alive(&self) -> bool {
        self.sync_alive.load(Ordering::Acquire)
    }

    /// Whether sync is considered dead, with startup grace period.
    pub fn is_sync_dead(&self) -> bool {
        if !self.sync_enabled.load(Ordering::Acquire) {
            return false;
        }
        if self.sync_alive.load(Ordering::Acquire) {
            return false;
        }
        let start = self.sync_start_ms.load(Ordering::Acquire);
        // Grace period after start_sync before declaring dead (thread startup/handshake)
        if start != 0 && crate::util::monotonic_ms().saturating_sub(start) < 1500 {
            return false;
        }
        true
    }

    /// Attempts to revive a dead sync thread using the last desired params.
    /// Returns true if a restart was actually initiated on this call. Throttled
    /// to once per 5s so a persistently failing handle init does not spawn a
    /// new thread every UI tick. No-ops (returns false) during the 1.5s startup
    /// grace period or while a restart is still in progress.
    pub fn try_restart_if_dead(&self) -> bool {
        if !self.is_sync_dead() {
            return false;
        }
        let now = crate::util::monotonic_ms();
        let last = self.sync_start_ms.load(Ordering::Acquire);
        if last != 0 && now.saturating_sub(last) < 5000 {
            return false;
        }
        let params = match *self.desired_sync.read() {
            Some(p) => p,
            None => return false,
        };
        match self.start_sync_with_params(params) {
            Ok(()) => {
                tracing::info!("Restarted dead CPU power sync thread");
                true
            }
            Err(e) => {
                warn!("Failed to restart CPU power sync thread: {}", e);
                false
            }
        }
    }

    /// Stops sync thread. Holds the `sync_thread` lock across the whole teardown
    /// so a concurrent `start_sync` cannot re-spawn a thread in the gap and then
    /// be incorrectly marked stopped (which would disable a freshly started sync).
    pub fn stop_sync(&self) {
        let mut thread = self.sync_thread.lock();
        thread.running.store(false, Ordering::Release);
        if let Some(old) = thread.handle.take() {
            let _ = old.join();
        }
        thread.alive.store(false, Ordering::Release);
        drop(thread);
        self.sync_alive.store(false, Ordering::Release);
        self.sync_enabled.store(false, Ordering::Release);
        // Keep desired_sync for resume restore.
    }

    pub(crate) fn desired_sync_params(&self) -> Option<PowerLimitParams> {
        *self.desired_sync.read()
    }

    #[allow(dead_code)]
    pub(crate) fn clear_desired_sync(&self) {
        *self.desired_sync.write() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_start_stop_sync_is_safe() {
        // Exercises the start_sync/stop_sync locking path reworked for #1.
        // Without PawnIO present the sync cannot actually initialize, but the
        // calls must serialize cleanly: no panic and no duplicate live threads.
        let state = CpuPowerState::default();
        let p = PowerLimitParams {
            pl1_watts: 15.0,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 35.0,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            power_unit: 0.125,
            time_unit: 0.0009765625,
        };
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = state.clone();
            handles.push(std::thread::spawn(move || {
                let _ = s.start_sync(
                    p.pl1_watts,
                    p.pl1_enabled,
                    p.pl1_clamped,
                    p.pl1_time_s,
                    p.pl2_watts,
                    p.pl2_enabled,
                    p.pl2_clamped,
                    p.pl2_time_s,
                    p.power_unit,
                    p.time_unit,
                );
                s.stop_sync();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(!state.is_sync_alive());
        assert!(
            !state
                .sync_enabled
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn try_restart_if_dead_safe_without_params() {
        let state = CpuPowerState::default();
        // No desired params -> must not restart and must not panic.
        assert!(!state.try_restart_if_dead());
        state.stop_sync();
        assert!(
            !state
                .sync_enabled
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
}
