//! PawnIOLib.dll binding: path resolution and verification, function pointers,
//! driver install/update, and the handle and ioctl wrappers.

use std::ffi::CString;
use std::os::windows::process::CommandExt;

use tracing::warn;
use windows_sys::Win32::Foundation::HANDLE;

use super::modules::{
    download_and_extract_modules, latest_modules_download_url, local_modules_version,
    modules_update_needed, sha256_hex, tag_from_modules_url,
};

// PawnIOLib.dll function signatures (STDMETHODCALLTYPE / WINAPI - same on x64).
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
pub(super) struct PawnioHandle {
    pub(super) handle: HANDLE,
    pub(super) exec_fn: PawnioExecute,
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

/// Outcome of a winget invocation, classified from process output.
///
/// Exit status decides first: winget's human-readable text is localized, so
/// text matching is only a fallback for the known non-zero up-to-date
/// reports. Unknown output is a failure with the raw detail preserved, never
/// silently treated as success.
#[derive(Debug, PartialEq)]
enum WingetOutcome {
    Success,
    UpToDate,
    NotInstalled,
    Failed(String),
}

fn classify_winget_output(output: &std::process::Output) -> WingetOutcome {
    if output.status.success() {
        return WingetOutcome::Success;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr} {stdout}");
    // Best-effort English signals; other locales fall through to Failed with
    // the raw detail preserved for the error message.
    if combined.contains("already installed")
        || combined.contains("No available upgrade")
        || combined.contains("No newer package")
    {
        return WingetOutcome::UpToDate;
    }
    if combined.contains("not recognized")
        || combined.contains("not found")
        || combined.contains("No installed package found")
        || combined.contains("No package found")
    {
        return WingetOutcome::NotInstalled;
    }
    WingetOutcome::Failed(super::modules::proc_failure_detail(output))
}

fn winget_package_version(dir_name: &str) -> Option<Vec<u64>> {
    let version = dir_name
        .strip_prefix("Microsoft.DesktopAppInstaller_")?
        .split('_')
        .next()?;
    let parts: Vec<u64> = version
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then_some(parts)
}

fn trusted_winget_path(path: &std::path::Path) -> bool {
    let Ok(canon) = std::fs::canonicalize(path) else {
        return false;
    };
    let Some(pf) = known_program_files() else {
        return false;
    };
    let windows_apps = pf.join("WindowsApps");
    let prefix = std::fs::canonicalize(&windows_apps).unwrap_or(windows_apps);
    let canon_str = canon.to_string_lossy().replace('/', "\\").to_lowercase();
    let prefix_str = prefix.to_string_lossy().replace('/', "\\").to_lowercase();
    canon_str.starts_with(&prefix_str)
        && canon_str
            .rsplit('\\')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("winget.exe"))
}

fn winget_package_from_appx() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new(crate::util::powershell_exe())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-AppxPackage -Name Microsoft.DesktopAppInstaller -ErrorAction SilentlyContinue | Select-Object -First 1).InstallLocation",
        ])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let location = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if location.is_empty() {
        return None;
    }
    let candidate = std::path::Path::new(&location).join("winget.exe");
    candidate.is_file().then_some(candidate)
}

/// Resolves winget to a trusted absolute path. PATH lookup in an elevated
/// process can pick a user-writable shim, so only Program Files\WindowsApps
/// packages are accepted; the per-user alias directory is user-writable.
fn resolve_winget() -> Result<std::path::PathBuf, String> {
    let mut candidates: Vec<(Vec<u64>, std::path::PathBuf)> = Vec::new();
    if let Some(pf) = known_program_files()
        && let Ok(entries) = std::fs::read_dir(pf.join("WindowsApps"))
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(version) = winget_package_version(&name.to_string_lossy()) else {
                continue;
            };
            let candidate = entry.path().join("winget.exe");
            if candidate.is_file() {
                candidates.push((version, candidate));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, candidate) in candidates {
        if trusted_winget_path(&candidate) {
            return Ok(candidate);
        }
    }
    if let Some(candidate) = winget_package_from_appx()
        && trusted_winget_path(&candidate)
    {
        return Ok(candidate);
    }
    Err(
        "winget not found in a trusted location; install App Installer from the Microsoft Store, or download PawnIO manually from https://github.com/namazso/PawnIO/releases"
            .to_string(),
    )
}

fn run_command_with_timeout(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    command
        .creation_flags(0x08000000)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to run winget: {e}"))?;
    let mut stdout = child.stdout.take();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut out) = stdout {
            let _ = out.read_to_end(&mut buf);
        }
        let _ = out_tx.send(buf);
    });
    let mut stderr = child.stderr.take();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut err) = stderr {
            let _ = err.read_to_end(&mut buf);
        }
        let _ = err_tx.send(buf);
    });

    let start = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("failed to wait for winget: {e}"))?
        {
            Some(status) => {
                let stdout = out_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_default();
                let stderr = err_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let reap_deadline = Instant::now() + Duration::from_secs(1);
                    while child.try_wait().ok().flatten().is_none()
                        && Instant::now() < reap_deadline
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    return Err(format!("winget timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Installs PawnIO via winget.
pub fn install_pawnio() -> Result<(), String> {
    use std::process::Command;
    tracing::info!("Installing PawnIO via winget...");
    let winget = resolve_winget()?;
    let mut command = Command::new(winget);
    command.args([
        "install",
        "-e",
        "--id",
        "namazso.PawnIO",
        "--accept-package-agreements",
        "--accept-source-agreements",
    ]);
    let output = run_command_with_timeout(command, std::time::Duration::from_secs(55))?;
    match classify_winget_output(&output) {
        WingetOutcome::Success => {
            tracing::info!("PawnIO installed successfully");
            Ok(())
        }
        WingetOutcome::UpToDate => {
            tracing::info!("PawnIO already installed and up to date");
            Ok(())
        }
        WingetOutcome::NotInstalled => Err(
            "winget install failed: package or source not found — install App Installer from Microsoft Store or download PawnIO from https://github.com/namazso/PawnIO/releases"
                .to_string(),
        ),
        WingetOutcome::Failed(detail) => {
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
}

/// Checks if PawnIO DLL is installed.
pub fn is_pawnio_installed() -> bool {
    resolved_dll_path().exists()
}

/// Cached PawnIO version, read from the DLL's version resource.
static PAWNIO_VERSION: parking_lot::RwLock<Option<Option<String>>> = parking_lot::RwLock::new(None);

/// Reads the PawnIO version from the DLL's VS_FIXEDFILEINFO resource.
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

/// Returns the installed PawnIO version, cached for the process lifetime.
///
/// `None` when PawnIO is not installed. A negative result is cached too, so
/// that a machine without PawnIO does not re-resolve the DLL path on every
/// view rebuild.
pub fn pawnio_version() -> Option<String> {
    if let Some(cached) = PAWNIO_VERSION.read().clone() {
        return cached;
    }
    let ver = if is_pawnio_installed() {
        Some(fetch_pawnio_version_from_dll().unwrap_or_else(|| "installed".to_string()))
    } else {
        None
    };
    *PAWNIO_VERSION.write() = Some(ver.clone());
    ver
}

/// Clears the cached PawnIO version so the next call re-reads DLL metadata.
pub fn invalidate_pawnio_version() {
    *PAWNIO_VERSION.write() = None;
}

/// Updates PawnIO via winget upgrade, fallback to install.
pub fn update_pawnio() -> Result<(), String> {
    use std::process::Command;
    tracing::info!("Updating PawnIO via winget...");
    let winget = resolve_winget()?;
    let mut command = Command::new(winget);
    command.args([
        "upgrade",
        "-e",
        "--id",
        "namazso.PawnIO",
        "--accept-package-agreements",
        "--accept-source-agreements",
    ]);
    let output = run_command_with_timeout(command, std::time::Duration::from_secs(55))?;
    match classify_winget_output(&output) {
        WingetOutcome::Success => {
            tracing::info!("PawnIO upgraded successfully");
            invalidate_pawnio_version();
            Ok(())
        }
        WingetOutcome::UpToDate => {
            tracing::info!("PawnIO already up to date");
            invalidate_pawnio_version();
            Ok(())
        }
        // Upgrade of a package winget does not know is just an install.
        WingetOutcome::NotInstalled => {
            tracing::info!("winget upgrade found no installed package, falling back to install");
            install_pawnio()
        }
        // Transient failures (network, UAC, source errors) must not trigger
        // a full reinstall with its own UAC prompt and download.
        WingetOutcome::Failed(detail) => {
            tracing::warn!("winget upgrade output: {}", detail);
            Err(format!("winget upgrade failed: {}", detail))
        }
    }
}

/// Updates PawnIO Modules. Skips the download when the recorded local tag
/// already matches upstream latest; an unreachable API or missing marker
/// falls through to downloading.
pub fn update_pawnio_modules() -> Result<(), String> {
    let latest_tag = latest_modules_download_url()
        .as_deref()
        .and_then(tag_from_modules_url);
    let local = local_modules_version();
    if !modules_update_needed(local.as_deref(), latest_tag.as_deref()) {
        tracing::info!(
            "PawnIO Modules already at latest ({})",
            latest_tag.as_deref().unwrap_or_default()
        );
        return Ok(());
    }
    download_and_extract_modules()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthenticodeOutcome {
    Valid,
    Invalid,
    Unavailable,
}

/// Cached Authenticode verdict for the loaded DLL. Clearable on purpose:
/// `reset_dll_fns` drops it so an upgraded DLL is re-verified instead of
/// inheriting the previous file's result. Transient PowerShell failures are
/// not cached, so Retry can recover without restarting the app.
static AUTHENTICODE: parking_lot::Mutex<Option<(std::path::PathBuf, AuthenticodeOutcome)>> =
    parking_lot::Mutex::new(None);

/// Drops the cached Authenticode verdict. Called with the DLL pointers so an
/// upgraded DLL is re-verified on next load.
fn invalidate_authenticode() {
    *AUTHENTICODE.lock() = None;
}

fn cached_authenticode_status(p: &std::path::Path) -> Result<(), &'static str> {
    if let Some((cached_path, cached)) = AUTHENTICODE.lock().as_ref()
        && cached_path == p
    {
        return match cached {
            AuthenticodeOutcome::Valid => Ok(()),
            AuthenticodeOutcome::Invalid => Err("DLL authenticode not valid"),
            AuthenticodeOutcome::Unavailable => Err("authenticode check failed"),
        };
    }
    match check_authenticode(p) {
        AuthenticodeOutcome::Valid => {
            *AUTHENTICODE.lock() = Some((p.to_path_buf(), AuthenticodeOutcome::Valid));
            Ok(())
        }
        AuthenticodeOutcome::Invalid => {
            *AUTHENTICODE.lock() = Some((p.to_path_buf(), AuthenticodeOutcome::Invalid));
            Err("DLL authenticode not valid")
        }
        AuthenticodeOutcome::Unavailable => Err("authenticode check failed"),
    }
}

fn classify_authenticode_output(success: bool, stdout: &[u8]) -> AuthenticodeOutcome {
    if !success {
        return AuthenticodeOutcome::Unavailable;
    }
    let txt = String::from_utf8_lossy(stdout);
    let txt = txt.trim();
    if txt.eq_ignore_ascii_case("true") {
        AuthenticodeOutcome::Valid
    } else if txt.eq_ignore_ascii_case("false") {
        AuthenticodeOutcome::Invalid
    } else {
        AuthenticodeOutcome::Unavailable
    }
}

fn check_authenticode(p: &std::path::Path) -> AuthenticodeOutcome {
    let out = std::process::Command::new(crate::util::powershell_exe())
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-AuthenticodeSignature '{}').Status -eq 'Valid'",
                p.display().to_string().replace('\'', "''")
            ),
        ])
        .creation_flags(0x08000000)
        .output();
    match out {
        Ok(out) => classify_authenticode_output(out.status.success(), &out.stdout),
        Err(_) => AuthenticodeOutcome::Unavailable,
    }
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
    // Authenticode check via PowerShell Get-AuthenticodeSignature (cached until
    // the next DLL reset).
    // Fail-closed: PowerShell missing, command failure, or unexpected output
    // all refuse the DLL. Developers with a self-signed test DLL must set
    // FRAMEWORK_ALLOW_UNSIGNED_PAWNIO=1 explicitly.
    let authenticode = cached_authenticode_status(&p);
    match authenticode {
        Ok(()) => {}
        Err(e) => {
            // Unsigned bypass is debug-only: a release binary must never
            // let an env var disable signature enforcement, otherwise any
            // same-user attacker can set it and load an unsigned DLL.
            #[cfg(debug_assertions)]
            let bypass_allowed = std::env::var_os("FRAMEWORK_ALLOW_UNSIGNED_PAWNIO").is_some();
            #[cfg(not(debug_assertions))]
            let bypass_allowed = false;
            if !bypass_allowed {
                warn!(
                    "PawnIO DLL authenticode check failed ({}): {}",
                    e,
                    p.display()
                );
                return Err(e);
            }
            warn!(
                "FRAMEWORK_ALLOW_UNSIGNED_PAWNIO set (debug only); loading DLL despite authenticode failure ({}): {}",
                e,
                p.display()
            );
        }
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
    use windows_sys::Win32::Foundation::FreeLibrary;
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
        let addr = GetProcAddress(dll, c"pawnio_open".as_ptr() as *const u8).ok_or_else(|| {
            FreeLibrary(dll);
            "pawnio_open not found"
        })?;
        let open: PawnioOpen = std::mem::transmute(addr);
        let addr = GetProcAddress(dll, c"pawnio_load".as_ptr() as *const u8).ok_or_else(|| {
            FreeLibrary(dll);
            "pawnio_load not found"
        })?;
        let load: PawnioLoad = std::mem::transmute(addr);
        let addr =
            GetProcAddress(dll, c"pawnio_execute".as_ptr() as *const u8).ok_or_else(|| {
                FreeLibrary(dll);
                "pawnio_execute not found"
            })?;
        let exec: PawnioExecute = std::mem::transmute(addr);
        let addr = GetProcAddress(dll, c"pawnio_close".as_ptr() as *const u8).ok_or_else(|| {
            FreeLibrary(dll);
            "pawnio_close not found"
        })?;
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
///
/// Deliberately does NOT FreeLibrary the old image: outstanding
/// PawnioHandles hold copied function pointers that may still be in use on
/// another thread, and unloading under them is use-after-free. The stale
/// mapping leaks until process exit; upgrades are rare user-initiated
/// events, so one mapping per upgrade is negligible and always safe.
pub fn reset_dll_fns() {
    let _init = DLL_INIT_LOCK.lock();
    // Drop the module record without unloading: the image stays mapped for
    // any in-flight users. Re-init below overwrites it with the new image.
    *DLL_MODULE.lock() = None;
    *DLL_OPEN.lock() = None;
    *DLL_LOAD.lock() = None;
    *DLL_EXEC.lock() = None;
    *DLL_CLOSE.lock() = None;
    invalidate_pawnio_version();
    invalidate_authenticode();
}

/// Opens PawnIO handle and loads module blob.
pub(super) fn open_handle(blob: &[u8]) -> Result<PawnioHandle, &'static str> {
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
/// Why a PawnIO module call did not produce a value.
///
/// Kept as a type rather than a message string because the two cases call for
/// different advice: a rejected call usually means the module or driver is not
/// what the caller expected, while a short read means the module answered with
/// a different buffer size than requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoctlFailure {
    /// The module returned a non-zero HRESULT.
    Failed,
    /// The module succeeded but wrote fewer bytes than requested, so the output
    /// buffer is stale and must not be used.
    ShortRead,
}

/// Executes a PawnIO module function through the open handle.
pub(super) fn exec_ioctl(
    handle: &PawnioHandle,
    name: &str,
    inputs: &[u64],
    outputs: &mut [u64],
) -> Result<usize, IoctlFailure> {
    let name_c = CString::new(name).map_err(|_| IoctlFailure::Failed)?;
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
        return Err(IoctlFailure::Failed);
    }
    // Short write leaves buffer stale; must not be treated as valid.
    if return_size != outputs.len() {
        return Err(IoctlFailure::ShortRead);
    }
    Ok(return_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::ExitStatusExt;

    #[test]
    fn run_command_with_timeout_captures_output() {
        let mut command = std::process::Command::new(crate::util::system32_exe("cmd.exe"));
        command.args(["/c", "echo hello"]);
        let output =
            run_command_with_timeout(command, std::time::Duration::from_secs(5)).expect("run");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[test]
    fn winget_package_version_parses_numeric_components() {
        assert_eq!(
            winget_package_version("Microsoft.DesktopAppInstaller_1.27.430.0_x64__8wekyb3d8bbwe"),
            Some(vec![1, 27, 430, 0])
        );
        assert_eq!(
            winget_package_version("Microsoft.DesktopAppInstaller"),
            None
        );
        assert_eq!(
            winget_package_version("Microsoft.DesktopAppInstaller_x.y_z"),
            None
        );
    }

    #[test]
    fn invalidate_authenticode_clears_the_cache() {
        // White-box: the whole point of Mutex<Option<…>> over OnceLock is
        // clearability, so pin that property directly. No PowerShell runs.
        *AUTHENTICODE.lock() = Some((
            std::path::PathBuf::from("test.dll"),
            AuthenticodeOutcome::Valid,
        ));
        invalidate_authenticode();
        assert!(AUTHENTICODE.lock().is_none());
    }

    #[test]
    fn authenticode_output_classification_only_caches_definitive_results() {
        assert_eq!(
            classify_authenticode_output(true, b"True\r\n"),
            AuthenticodeOutcome::Valid
        );
        assert_eq!(
            classify_authenticode_output(true, b"False"),
            AuthenticodeOutcome::Invalid
        );
        assert_eq!(
            classify_authenticode_output(false, b"True"),
            AuthenticodeOutcome::Unavailable
        );
        assert_eq!(
            classify_authenticode_output(true, b""),
            AuthenticodeOutcome::Unavailable
        );
    }

    fn test_output(code: u32, stdout: &str, stderr: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn winget_success_wins_over_text() {
        // Exit status decides first: even confusing text cannot flip success.
        assert_eq!(
            classify_winget_output(&test_output(0, "No package found", "")),
            WingetOutcome::Success
        );
    }

    #[test]
    fn winget_up_to_date_signals() {
        assert_eq!(
            classify_winget_output(&test_output(1, "No available upgrade found", "")),
            WingetOutcome::UpToDate
        );
        assert_eq!(
            classify_winget_output(&test_output(1, "", "already installed")),
            WingetOutcome::UpToDate
        );
    }

    #[test]
    fn winget_missing_package_signals() {
        assert_eq!(
            classify_winget_output(&test_output(1, "No installed package found", "")),
            WingetOutcome::NotInstalled
        );
        assert_eq!(
            classify_winget_output(&test_output(1, "", "not recognized as an internal command")),
            WingetOutcome::NotInstalled
        );
    }

    #[test]
    fn winget_unknown_output_is_failure_with_detail() {
        // Localized or unexpected text must not be mistaken for success; the
        // raw detail is preserved for the error message.
        match classify_winget_output(&test_output(1, "既に最新です", "")) {
            WingetOutcome::Failed(detail) => assert!(detail.contains("既に最新です")),
            other => panic!("expected Failed, got {other:?}"),
        }
        match classify_winget_output(&test_output(1, "", "")) {
            WingetOutcome::Failed(detail) => assert!(detail.contains("exit code")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
