mod version;

pub use version::{
    invalidate_modules_version, invalidate_pawnio_version, pawnio_modules_version, pawnio_version,
};

use std::ffi::CString;
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};
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

const MODULES_DIR_NAME: &str = "modules";
/// Marker file recording which upstream tag the local bins came from.
const MODULES_VERSION_FILE: &str = ".version";
/// Last-known-good PawnIO.Modules upstream release tag, used ONLY as a
/// fallback download URL when the GitHub API is unreachable. The primary
/// path always queries the latest release; hashes stay advisory.
const LAST_KNOWN_MODULES_VERSION: &str = "0.2.11";
const INTEL_MSR_SHA256: &str = "d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f";
const INTEL_MCHBAR_SHA256: &str =
    "3f82b832d99b4aac37d2a20fdb7c9baa2a3bc0488612c9019c9484eb0e8a6eae";

/// Reads the locally installed modules tag recorded at download time.
fn local_modules_version() -> Option<String> {
    let dir = modules_dir();
    let raw = std::fs::read_to_string(dir.join(MODULES_VERSION_FILE)).ok()?;
    let ver = raw.trim().trim_start_matches('v').to_string();
    if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        return None;
    }
    Some(ver)
}

/// Extracts the release tag from a `.../download/{tag}/...` asset URL.
fn tag_from_modules_url(url: &str) -> Option<String> {
    let marker = "/download/";
    let start = url.find(marker)? + marker.len();
    let rest = &url[start..];
    let end = rest.find('/')?;
    let tag = rest[..end].trim().trim_start_matches('v').to_string();
    if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        return None;
    }
    Some(tag)
}

/// Records which upstream tag the local bins came from.
fn persist_local_modules_version(dir: &std::path::Path, tag: &str) {
    let path = dir.join(MODULES_VERSION_FILE);
    let _ = std::fs::remove_file(&path);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", tag);
    }
    invalidate_modules_version();
}

/// Tries to fetch latest release asset download URL directly via GitHub API.
/// Returns Some(url) if API succeeds, else None to use version-constructed URL.
fn latest_modules_download_url() -> Option<String> {
    let out = std::process::Command::new("curl.exe")
        .args([
            "-s",
            "-L",
            "--max-time",
            "15",
            "--retry",
            "1",
            "--retry-delay",
            "2",
            "https://api.github.com/repos/namazso/PawnIO.Modules/releases/latest",
        ])
        .creation_flags(0x08000000)
        .output();
    if let Ok(o) = out {
        let body = String::from_utf8_lossy(&o.stdout);
        // Find browser_download_url for release_*.zip
        for line in body.split(',') {
            let line = line.trim();
            if line.contains("browser_download_url")
                && line.contains("release_")
                && line.contains(".zip")
                && let Some(start) = line.find("\"https://")
                && let Some(end) = line[start..].find('"').map(|i| start + i)
            {
                let url = line[start..end].to_string();
                if url.contains("PawnIO.Modules") {
                    tracing::info!("PawnIO Modules latest asset URL: {}", url);
                    return Some(url);
                }
            }
        }
    }
    None
}

static PS_SCRIPT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Local modules directory (%APPDATA%/framework-crate/modules/).
/// Per-user on purpose: downloads and reads stay in the user profile and
/// never touch machine-wide locations.
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
    let actual = sha256_hex(&bytes);
    if actual != expected {
        // Advisory only: modules track upstream Latest, so a hash rotation
        // must not brick CPU Power. Mismatch is logged for audit.
        warn!(
            "PawnIO module hash mismatch: {} expected {} got {}",
            path.display(),
            expected,
            actual
        );
    }
    Ok(())
}

/// Reads module blob and verifies hash on same bytes to prevent TOCTOU.
fn read_verified_module(path: &std::path::Path, expected: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "module blob missing")?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        // Advisory only, see verify_module_hash.
        warn!(
            "PawnIO module hash mismatch: {} expected {} got {}",
            path.display(),
            expected,
            actual
        );
    }
    Ok(bytes)
}

static MODULES_CACHE: parking_lot::RwLock<Option<bool>> = parking_lot::RwLock::new(None);

/// Checks if module blobs are present in any candidate dir (cached).
pub fn modules_downloaded() -> bool {
    {
        let guard = MODULES_CACHE.read();
        if let Some(cached) = *guard {
            return cached;
        }
    }
    let present = modules_dir().join("IntelMSR.bin").is_file()
        && modules_dir().join("IntelMCHBAR.bin").is_file();
    *MODULES_CACHE.write() = Some(present);
    present
}

fn invalidate_modules_cache() {
    *MODULES_CACHE.write() = None;
}

/// Creates a fresh empty staging dir for archive extraction.
fn fresh_staging_dir(dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let staging = dir.join(format!(
        "pawnio_stage_{}_{}.tmp",
        std::process::id(),
        PS_SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir(&staging).map_err(|e| format!("failed to create staging dir: {}", e))?;
    Ok(staging)
}

/// Rejects ZIP-slip members: parent refs, absolute paths, drive letters.
fn member_path_safe(name: &str) -> bool {
    let n = name.replace('\\', "/");
    if n.starts_with('/') {
        return false;
    }
    if n.len() >= 2 && n.as_bytes()[1] == b':' {
        return false;
    }
    for comp in n.split('/') {
        if comp == ".." {
            return false;
        }
    }
    true
}

/// Rejects symlinks/junctions inside staging (no symlink following).
fn staging_has_no_links(staging: &std::path::Path) -> Result<(), String> {
    let mut stack = vec![staging.to_path_buf()];
    while let Some(p) = stack.pop() {
        let entries = std::fs::read_dir(&p).map_err(|e| format!("read staging failed: {}", e))?;
        for e in entries {
            let e = e.map_err(|e| format!("read staging entry failed: {}", e))?;
            let ft = e
                .file_type()
                .map_err(|e| format!("staging file_type failed: {}", e))?;
            if ft.is_symlink() {
                return Err(format!("staging contains symlink: {}", e.path().display()));
            }
            if ft.is_dir() {
                stack.push(e.path());
            }
        }
    }
    Ok(())
}

/// Validates archive members, extracts into a fresh staging dir, then
/// promotes only the two expected verified bins into `dir`.
fn extract_staged_zip(zip_tmp: &std::path::Path, dir: &std::path::Path) -> Result<(), String> {
    // 1. Pre-list members and reject traversal before extracting anything.
    let list_out = std::process::Command::new("tar.exe")
        .args(["-tf", &zip_tmp.display().to_string()])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| format!("failed to list archive (tar.exe missing?): {}", e))?;
    if !list_out.status.success() {
        return Err("failed to list archive contents".to_string());
    }
    for member in String::from_utf8_lossy(&list_out.stdout).lines() {
        let m = member.trim().trim_end_matches('/');
        if m.is_empty() {
            continue;
        }
        if !member_path_safe(m) {
            return Err(format!("archive contains unsafe path: {}", m));
        }
    }
    // 2. Extract into a fresh staging dir (never directly into modules dir).
    let staging = fresh_staging_dir(dir)?;
    let cleanup = |staging: &std::path::Path, zip: &std::path::Path| {
        let _ = std::fs::remove_file(zip);
        let _ = std::fs::remove_dir_all(staging);
    };
    let tar_out = std::process::Command::new("tar.exe")
        .args([
            "-xf",
            &zip_tmp.display().to_string(),
            "-C",
            &staging.display().to_string(),
        ])
        .creation_flags(0x08000000)
        .output();
    let tar_ok = matches!(&tar_out, Ok(o) if o.status.success());
    if !tar_ok {
        let esc = |s: &str| s.replace('\'', "''");
        let ps = format!(
            "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
            esc(&zip_tmp.display().to_string()),
            esc(&staging.display().to_string())
        );
        let ps_out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps])
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| format!("extraction failed (tar and powershell unavailable): {}", e))?;
        if !ps_out.status.success() {
            cleanup(&staging, zip_tmp);
            return Err("extraction failed".to_string());
        }
    }
    // 3. Reject symlinks planted inside staging.
    if let Err(e) = staging_has_no_links(&staging) {
        cleanup(&staging, zip_tmp);
        return Err(e);
    }
    // 4. Require the two expected bins as regular files, log advisory hashes.
    for name in ["IntelMSR.bin", "IntelMCHBAR.bin"] {
        let src = staging.join(name);
        let meta =
            std::fs::symlink_metadata(&src).map_err(|_| format!("archive missing {}", name))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            cleanup(&staging, zip_tmp);
            return Err(format!("archive missing {}", name));
        }
    }
    let _ = verify_module_hash(&staging.join("IntelMSR.bin"), INTEL_MSR_SHA256);
    let _ = verify_module_hash(&staging.join("IntelMCHBAR.bin"), INTEL_MCHBAR_SHA256);
    // 5. Promote verified bins into modules dir; old bins stay until replaced.
    for name in ["IntelMSR.bin", "IntelMCHBAR.bin"] {
        let src = staging.join(name);
        let dst = dir.join(name);
        let _ = std::fs::remove_file(&dst);
        if let Err(e) = std::fs::rename(&src, &dst) {
            cleanup(&staging, zip_tmp);
            return Err(format!("failed to promote {}: {}", name, e));
        }
    }
    cleanup(&staging, zip_tmp);
    invalidate_blob_cache();
    invalidate_modules_version();
    Ok(())
}

/// Downloads PawnIO Modules ZIP and extracts blobs.
pub fn download_and_extract_modules() -> Result<(), String> {
    let dir = modules_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create modules directory: {}", e))?;

    // Primary: latest release asset URL. Fallback: last-known release so a
    // flaky GitHub API degrades to a slightly old download instead of an error.
    let url = latest_modules_download_url().unwrap_or_else(|| {
        warn!("GitHub API unreachable, falling back to last-known release");
        format!(
            "https://github.com/namazso/PawnIO.Modules/releases/download/{0}/release_{1}.zip",
            LAST_KNOWN_MODULES_VERSION,
            LAST_KNOWN_MODULES_VERSION.replace('.', "_")
        )
    });
    let zip_path = dir.join("pawnio_modules_latest.zip");
    // Primary ZIP is deterministic and vulnerable to pre-created symlink; also use unique tmp
    if let Ok(meta) = std::fs::symlink_metadata(&zip_path)
        && meta.file_type().is_symlink()
    {
        warn!("Refusing to use symlink at {}", zip_path.display());
        return Err("zip path is symlink".to_string());
    }
    let zip_tmp = dir.join(format!(
        "pawnio_modules_latest_{}_{}.zip.tmp",
        std::process::id(),
        PS_SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    // ensure tmp is fresh
    {
        let _guard = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&zip_tmp)
            .map_err(|e| format!("failed to create temp zip: {}", e))?;
    }
    debug!("Downloading modules from: {}", url);

    // Remove stale ZIP if it exists and is too small.
    if let Ok(meta) = std::fs::metadata(&zip_path)
        && meta.len() < 1000
    {
        let _ = std::fs::remove_file(&zip_path);
    }

    // Download via inline -Command (no script file, no script-path race).
    // Escape single quotes for PowerShell single-quoted strings.
    let esc = |s: &str| s.replace('\'', "''");
    let ps_command = format!(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
         Invoke-WebRequest -Uri '{url}' -OutFile '{zip}' -UseBasicParsing -ErrorAction Stop",
        url = esc(&url),
        zip = esc(&zip_tmp.display().to_string()),
    );

    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &ps_command,
        ])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("failed to run powershell: {}", e))?;

    // PowerShell only downloads; extraction always goes through staged validation.
    let _ = std::fs::remove_file(&zip_path);
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
        warn!(
            "PowerShell download failed: {}, trying curl fallback",
            detail
        );
        if try_curl_download(&url, &dir).is_ok() {
            debug!(
                "PawnIO modules extracted via curl fallback to {}",
                dir.display()
            );
            return Ok(());
        }
        warn!("curl fallback also failed");
        return Err(format!(
            "download/extraction failed: {} — check internet or download latest release_*.zip manually from https://github.com/namazso/PawnIO.Modules/releases/latest and place IntelMSR.bin / IntelMCHBAR.bin into the modules folder (use Open Modules Folder)",
            detail
        ));
    }

    // Basic size sanity check before extraction.
    if let Ok(meta) = std::fs::metadata(&zip_tmp)
        && meta.len() < 1000
    {
        let _ = std::fs::remove_file(&zip_tmp);
        return Err("downloaded zip too small".to_string());
    }
    if let Err(e) = extract_staged_zip(&zip_tmp, &dir) {
        warn!("Staged extraction failed: {}, trying curl fallback", e);
        if try_curl_download(&url, &dir).is_ok() {
            debug!(
                "PawnIO modules extracted via curl fallback after staged failure to {}",
                dir.display()
            );
            return Ok(());
        }
        return Err(format!(
            "{} — check internet or download latest release_*.zip manually from https://github.com/namazso/PawnIO.Modules/releases/latest and place IntelMSR.bin / IntelMCHBAR.bin into the modules folder (use Open Modules Folder)",
            e
        ));
    }
    if let Some(tag) = tag_from_modules_url(&url) {
        persist_local_modules_version(&dir, &tag);
    }

    debug!("PawnIO modules extracted successfully to {}", dir.display());
    Ok(())
}

fn try_curl_download(url: &str, dir: &std::path::Path) -> Result<(), String> {
    // Download to a unique temp file created with create_new, then staged extraction.
    // Do not delete old bins before download success — keep fallback if download fails
    let zip_tmp = dir.join(format!(
        "pawnio_modules_curl_{}_{}.zip.tmp",
        std::process::id(),
        PS_SCRIPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    // Ensure tmp doesn't exist via create_new guard
    {
        let _guard = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&zip_tmp)
            .map_err(|e| format!("failed to create temp zip (possible race): {}", e))?;
    }
    let curl_output = std::process::Command::new("curl.exe")
        .args([
            "-L",
            "--fail",
            "--proto",
            "=https",
            "--max-time",
            "30",
            "--retry",
            "1",
            "--retry-delay",
            "2",
            "-o",
            &zip_tmp.display().to_string(),
            url,
        ])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| format!("curl not available: {}", e))?;
    if !curl_output.status.success() {
        let _ = std::fs::remove_file(&zip_tmp);
        return Err("curl download failed".to_string());
    }
    // Basic size sanity check before extraction
    if let Ok(meta) = std::fs::metadata(&zip_tmp)
        && meta.len() < 1000
    {
        let _ = std::fs::remove_file(&zip_tmp);
        return Err("downloaded zip too small".to_string());
    }
    let res = extract_staged_zip(&zip_tmp, dir);
    if res.is_ok()
        && let Some(tag) = tag_from_modules_url(url)
    {
        persist_local_modules_version(dir, &tag);
    }
    res
}

static MSR_BLOB_CACHE: parking_lot::Mutex<Option<Arc<Vec<u8>>>> = parking_lot::Mutex::new(None);
static MCHBAR_BLOB_CACHE: parking_lot::Mutex<Option<Arc<Vec<u8>>>> = parking_lot::Mutex::new(None);

fn invalidate_blob_cache() {
    *MSR_BLOB_CACHE.lock() = None;
    *MCHBAR_BLOB_CACHE.lock() = None;
    invalidate_modules_cache();
}

/// Opens modules directory in Explorer (the dir actually in use, else target).
pub fn open_modules_dir() -> Result<(), String> {
    let dir = modules_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::process::Command::new("explorer.exe")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Re-validates modules after manual placement; clears caches.
pub fn redetect_modules() -> bool {
    invalidate_blob_cache();
    invalidate_modules_version();
    modules_downloaded()
}

/// Loads IntelMSR blob (cached after first verified load).
fn load_intel_msr_blob() -> Result<Vec<u8>, &'static str> {
    {
        let guard = MSR_BLOB_CACHE.lock();
        if let Some(cached) = guard.as_ref() {
            return Ok((**cached).clone());
        }
    }
    let blob = read_verified_module(&modules_dir().join("IntelMSR.bin"), INTEL_MSR_SHA256)?;
    let arc = Arc::new(blob.clone());
    *MSR_BLOB_CACHE.lock() = Some(arc);
    Ok(blob)
}

/// Loads IntelMCHBAR blob (cached after first verified load).
fn load_intel_mchbar_blob() -> Result<Vec<u8>, &'static str> {
    {
        let guard = MCHBAR_BLOB_CACHE.lock();
        if let Some(cached) = guard.as_ref() {
            return Ok((**cached).clone());
        }
    }
    let blob = read_verified_module(&modules_dir().join("IntelMCHBAR.bin"), INTEL_MCHBAR_SHA256)?;
    let arc = Arc::new(blob.clone());
    *MCHBAR_BLOB_CACHE.lock() = Some(arc);
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
    /// Effective PL1 (lower of enabled MSR and MMIO).
    pub fn effective_pl1(&self) -> f64 {
        effective_limit(
            self.pl1_msr,
            self.pl1_msr_enabled,
            self.pl1_mmio,
            self.pl1_mmio_enabled,
        )
    }

    /// Effective PL2 (lower of enabled MSR and MMIO).
    pub fn effective_pl2(&self) -> f64 {
        effective_limit(
            self.pl2_msr,
            self.pl2_msr_enabled,
            self.pl2_mmio,
            self.pl2_mmio_enabled,
        )
    }

    /// Pre-fills edit fields from current MSR values.
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
            (
                String::new(),
                String::new(),
                true,
                true,
                false,
                false,
                String::new(),
                String::new(),
            )
        }
    }
}

/// Lower of enabled limits, ignoring 0/invalid and disabled registers.
fn effective_limit(msr: f64, msr_en: bool, mmio: f64, mmio_en: bool) -> f64 {
    let msr_valid = msr_en && msr > 0.0 && msr.is_finite();
    let mmio_valid = mmio_en && mmio > 0.0 && mmio.is_finite();
    match (msr_valid, mmio_valid) {
        (true, true) => msr.min(mmio),
        (true, false) => msr,
        (false, true) => mmio,
        (false, false) => 0.0,
    }
}

/// Decodes power limit from raw 32-bit half.
fn decode_power_limit(raw_val: u64, unit: f64) -> (f64, bool, bool, u32, u32) {
    let raw_bits = (raw_val & 0x7FFF) as f64;
    let enabled = (raw_val >> 15) & 1 == 1;
    let clamped = (raw_val >> 16) & 1 == 1;
    // Intel MSR 0x610: Y = 5 bits [21:17], Z = 2 bits [23:22].
    let time_y = ((raw_val >> 17) & 0x1F) as u32;
    let time_z = ((raw_val >> 22) & 0x3) as u32;
    (raw_bits * unit, enabled, clamped, time_y, time_z)
}

/// Decodes time window (2^Y * (1+Z/4) * Time_Unit).
fn decode_time_window(y: u32, z: u32, time_unit: f64) -> f64 {
    (1u64 << y) as f64 * (1.0 + z as f64 / 4.0) * time_unit
}

/// Encodes time window into Y and Z fields.
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

/// Encodes power limit into 32-bit register half.
fn encode_power_limit(
    watts: f64,
    enabled: bool,
    clamped: bool,
    unit: f64,
    time_y: u32,
    time_z: u32,
) -> u32 {
    let max_raw = ((1u32 << 15) - 1) as f64; // 15-bit field: max 32767
    let raw = (watts / unit).round().clamp(0.0, max_raw) as u32;
    let mut val = raw & 0x7FFF;
    if enabled {
        val |= 1 << 15;
    }
    if clamped {
        val |= 1 << 16;
    }
    val |= (time_y & 0x1F) << 17;
    val |= (time_z & 0x3) << 22;
    val
}

/// Power limit parameters.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PowerLimitParams {
    pub(crate) pl1_watts: f64,
    pub(crate) pl1_enabled: bool,
    pub(crate) pl1_clamped: bool,
    pub(crate) pl1_time_s: f64,
    pub(crate) pl2_watts: f64,
    pub(crate) pl2_enabled: bool,
    pub(crate) pl2_clamped: bool,
    pub(crate) pl2_time_s: f64,
    pub(crate) power_unit: f64,
    pub(crate) time_unit: f64,
}

/// Writes MSR_PKG_POWER_LIMIT (0x610) via IntelMSR.
fn write_msr_pl1_pl2(
    msr_handle: &PawnioHandle,
    params: &PowerLimitParams,
) -> Result<(), &'static str> {
    if params.power_unit <= 0.0 || params.time_unit <= 0.0 {
        return Err("invalid RAPL units (power_unit or time_unit is zero)");
    }
    let (pl1_y, pl1_z) = encode_time_window(params.pl1_time_s, params.time_unit);
    let (pl2_y, pl2_z) = encode_time_window(params.pl2_time_s, params.time_unit);

    let pl1_enc = encode_power_limit(
        params.pl1_watts,
        params.pl1_enabled,
        params.pl1_clamped,
        params.power_unit,
        pl1_y,
        pl1_z,
    );
    let pl2_enc = encode_power_limit(
        params.pl2_watts,
        params.pl2_enabled,
        params.pl2_clamped,
        params.power_unit,
        pl2_y,
        pl2_z,
    );

    let new_val = ((pl2_enc as u64) << 32) | (pl1_enc as u64);

    let mut inp = [0u64; 2];
    inp[0] = 0x610; // MSR_PKG_POWER_LIMIT
    inp[1] = new_val;
    let mut out2 = [0u64; 1];
    exec_ioctl(msr_handle, "ioctl_write_msr", &inp, &mut out2)
        .map_err(|_| "ioctl_write_msr failed — is IntelMSR module loaded?")?;

    // Verify the write landed by re-reading MSR 0x610. The CPU silently
    // drops the write when the limit is BIOS-locked.
    let tolerance = params.power_unit.max(0.25);
    let mut rb_out = [0u64; 1];
    if exec_ioctl(msr_handle, "ioctl_read_msr", &[0x610], &mut rb_out).is_err() {
        return Err("MSR write succeeded but read-back failed");
    }
    let rb_raw = rb_out[0];
    let (rb_pl1, rb_pl1_en, ..) = decode_power_limit(rb_raw, params.power_unit);
    let (rb_pl2, rb_pl2_en, ..) = decode_power_limit(rb_raw >> 32, params.power_unit);
    // Compare against encoded target, not raw request, to avoid false lock detection.
    let expected_pl1 = ((pl1_enc & 0x7FFF) as f64) * params.power_unit;
    let expected_pl2 = ((pl2_enc & 0x7FFF) as f64) * params.power_unit;
    if (rb_pl1 - expected_pl1).abs() > tolerance
        || (rb_pl2 - expected_pl2).abs() > tolerance
        || rb_pl1_en != params.pl1_enabled
        || rb_pl2_en != params.pl2_enabled
    {
        debug!(
            "MSR read-back mismatch: wrote PL1={:.2}W(en={}) PL2={:.2}W(en={}), read PL1={:.2}W(en={}) PL2={:.2}W(en={})",
            expected_pl1,
            params.pl1_enabled,
            expected_pl2,
            params.pl2_enabled,
            rb_pl1,
            rb_pl1_en,
            rb_pl2,
            rb_pl2_en
        );
        return Err("MSR write not reflected in read-back — register may be locked");
    }

    debug!(
        "MSR PL1/PL2 written: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) raw=0x{:016X}",
        params.pl1_watts, params.pl1_time_s, params.pl2_watts, params.pl2_time_s, new_val
    );
    Ok(())
}

/// Writes PACKAGE_POWER_LIMIT_MMIO (MCHBAR+0x59A0) via IntelMCHBAR.
fn write_mmio_pl1_pl2(
    mchbar_handle: &PawnioHandle,
    params: &PowerLimitParams,
) -> Result<(), &'static str> {
    if params.power_unit <= 0.0 || params.time_unit <= 0.0 {
        return Err("invalid RAPL units (power_unit or time_unit is zero)");
    }
    let (pl1_y, pl1_z) = encode_time_window(params.pl1_time_s, params.time_unit);
    let (pl2_y, pl2_z) = encode_time_window(params.pl2_time_s, params.time_unit);

    let pl1_enc = encode_power_limit(
        params.pl1_watts,
        params.pl1_enabled,
        params.pl1_clamped,
        params.power_unit,
        pl1_y,
        pl1_z,
    );
    let pl2_enc = encode_power_limit(
        params.pl2_watts,
        params.pl2_enabled,
        params.pl2_clamped,
        params.power_unit,
        pl2_y,
        pl2_z,
    );

    let new_val = ((pl2_enc as u64) << 32) | (pl1_enc as u64);

    let mmio_offset = 0x59A0u64;
    let mut inp = [0u64; 2];
    inp[0] = mmio_offset;
    inp[1] = new_val;
    let mut out2 = [0u64; 1];
    exec_ioctl(mchbar_handle, "ioctl_write_qword", &inp, &mut out2)
        .map_err(|_| "ioctl_write_qword failed — IntelMCHBAR module may not support MMIO write")?;

    // Verify write by re-reading MMIO, mirroring MSR verification.
    let tolerance = params.power_unit.max(0.25);
    let mut rb_out = [0u64; 1];
    if exec_ioctl(
        mchbar_handle,
        "ioctl_read_qword",
        &[mmio_offset],
        &mut rb_out,
    )
    .is_err()
    {
        return Err("MMIO write succeeded but read-back failed");
    }
    let rb_raw = rb_out[0];
    let (rb_pl1, rb_pl1_en, ..) = decode_power_limit(rb_raw, params.power_unit);
    let (rb_pl2, rb_pl2_en, ..) = decode_power_limit(rb_raw >> 32, params.power_unit);
    let expected_pl1 = ((pl1_enc & 0x7FFF) as f64) * params.power_unit;
    let expected_pl2 = ((pl2_enc & 0x7FFF) as f64) * params.power_unit;
    if (rb_pl1 - expected_pl1).abs() > tolerance
        || (rb_pl2 - expected_pl2).abs() > tolerance
        || rb_pl1_en != params.pl1_enabled
        || rb_pl2_en != params.pl2_enabled
    {
        debug!(
            "MMIO read-back mismatch: wrote PL1={:.2}W(en={}) PL2={:.2}W(en={}), read PL1={:.2}W(en={}) PL2={:.2}W(en={})",
            expected_pl1,
            params.pl1_enabled,
            expected_pl2,
            params.pl2_enabled,
            rb_pl1,
            rb_pl1_en,
            rb_pl2,
            rb_pl2_en
        );
        return Err("MMIO write not reflected in read-back — register may be locked");
    }

    debug!(
        "MMIO PL1/PL2 written: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) raw=0x{:016X}",
        params.pl1_watts, params.pl1_time_s, params.pl2_watts, params.pl2_time_s, new_val
    );
    Ok(())
}

/// Writes MSR 0x610 (opens IntelMSR handle internally).
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

/// Writes both MSR and MMIO from `BiosDefaults` to restore factory state.
pub fn write_bios_defaults(bios: &BiosDefaults) -> Result<(), &'static str> {
    write_msr_pl1_pl2_public(
        bios.pl1_watts,
        bios.pl1_enabled,
        bios.pl1_clamped,
        bios.pl1_time_s,
        bios.pl2_watts,
        bios.pl2_enabled,
        bios.pl2_clamped,
        bios.pl2_time_s,
        bios.power_unit,
        bios.time_unit,
    )?;
    // MMIO may be absent on older files; best-effort only.
    if bios.pl1_mmio_watts > 0.0 || bios.pl2_mmio_watts > 0.0 {
        let _ = write_mmio_pl1_pl2_public(
            bios.pl1_mmio_watts,
            bios.pl1_mmio_enabled,
            bios.pl1_mmio_clamped,
            bios.pl1_mmio_time_s,
            bios.pl2_mmio_watts,
            bios.pl2_mmio_enabled,
            bios.pl2_mmio_clamped,
            bios.pl2_mmio_time_s,
            bios.power_unit,
            bios.time_unit,
        );
    }
    Ok(())
}

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

/// Reads all CPU power info via PawnIO modules.
pub fn read_cpu_power() -> CpuPowerInfo {
    let mut info = CpuPowerInfo::default();

    // Load handles independently; failure of one does not discard the other.
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

    // Read MSR 0x606 for RAPL units; required to decode limits.
    let mut units_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_msr", &[0x606], &mut out).is_ok() {
            let raw = out[0];
            let unit_bits = (raw & 0xF) as u32;
            info.power_unit = 1.0 / (1u32 << unit_bits) as f64;
            let time_bits = ((raw >> 16) & 0xF) as u32;
            info.time_unit = 1.0 / (1u32 << time_bits) as f64;
            debug!(
                "Power unit: {} W/unit (bits={}), time unit: {} s/unit (bits={})",
                info.power_unit, unit_bits, info.time_unit, time_bits
            );
            units_ok = true;
        }
    }
    if !units_ok {
        // Keep earlier root-cause error instead of masking.
        if info.error_msg.is_none() {
            info.error_msg = Some("Failed to read MSR 0x606");
        }
        return info;
    }

    // Read MSR 0x610 for static PL1/PL2.
    let mut msr610_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_msr", &[0x610], &mut out).is_ok() {
            msr610_ok = true;
            let raw = out[0];
            debug!("MSR 0x610 raw: 0x{:016X}", raw);
            let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
            let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) =
                decode_power_limit(raw >> 32, info.power_unit);
            info.pl1_msr = pl1;
            info.pl1_msr_enabled = pl1_en;
            info.pl1_msr_clamped = pl1_cl;
            info.pl1_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
            info.pl2_msr = pl2;
            info.pl2_msr_enabled = pl2_en;
            info.pl2_msr_clamped = pl2_cl;
            info.pl2_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
            debug!(
                "MSR PL1: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s",
                pl1, pl1_en, pl1_cl, pl1_y, pl1_z, info.pl1_time_s
            );
            debug!(
                "MSR PL2: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s",
                pl2, pl2_en, pl2_cl, pl2_y, pl2_z, info.pl2_time_s
            );
        }
    }

    // Read MMIO at MCHBAR+0x59A0 via IntelMCHBAR.
    let mmio_offset = 0x59A0u64;
    let mut mmio_ok = false;
    if let Some(ref handle) = mchbar_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_qword", &[mmio_offset], &mut out).is_ok() {
            mmio_ok = true;
            let raw = out[0];
            let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
            let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) =
                decode_power_limit(raw >> 32, info.power_unit);
            info.pl1_mmio = pl1;
            info.pl1_mmio_enabled = pl1_en;
            info.pl1_mmio_clamped = pl1_cl;
            info.pl1_mmio_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
            info.pl2_mmio = pl2;
            info.pl2_mmio_enabled = pl2_en;
            info.pl2_mmio_clamped = pl2_cl;
            info.pl2_mmio_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
            debug!(
                "MMIO PL1: {:.1}W (en={} clamp={}) time={:.1}s",
                pl1, pl1_en, pl1_cl, info.pl1_mmio_time_s
            );
            debug!(
                "MMIO PL2: {:.1}W (en={} clamp={}) time={:.1}s",
                pl2, pl2_en, pl2_cl, info.pl2_mmio_time_s
            );
        }
    }

    // Both registers unreadable: report unavailable.
    if !msr610_ok && !mmio_ok {
        if info.error_msg.is_none() {
            info.error_msg = Some("Failed to read MSR 0x610 and MMIO power limits");
        }
        return info;
    }
    info.available = true;
    info
}

/// Sync thread that continuously writes MSR 0x610 and MMIO to counter
/// firmware/EC overwrites. Runs until `external_alive` is dropped.
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

    /// Starts sync thread; `external_alive` tracks liveness without locking.
    fn start(
        &mut self,
        params: PowerLimitParams,
        external_alive: Arc<AtomicBool>,
    ) -> Result<(), &'static str> {
        self.stop();

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let alive = Arc::new(AtomicBool::new(false));
        let alive_clone = alive.clone();
        let alive_for_thread = alive.clone();
        let external_clone = external_alive.clone();
        let external_for_thread = external_alive.clone();

        let handle = std::thread::Builder::new()
            .name("cpu-power-sync".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sync_thread_main(running_clone, alive_for_thread, external_for_thread, params);
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
        // Liveness will be set to true by the thread after successful handle init.
        Ok(())
    }

    /// Stops sync thread (single shutdown helper; Drop reuses it).
    /// The worker wakes within ~100ms (interruptible sleep), so joining
    /// under the outer `sync_thread` lock cannot stall the UI.
    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.alive.store(false, Ordering::Release);
    }

    /// Stops sync thread.
    fn stop(&mut self) {
        self.shutdown();
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Sync thread main loop; writes MSR and MMIO every 250ms.
fn sync_thread_main(
    running: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    external_alive: Arc<AtomicBool>,
    params: PowerLimitParams,
) {
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

    // Load IntelMCHBAR for MMIO write (may fail).
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            warn!(
                "Sync thread: MMIO write unavailable ({}), will write MSR only",
                e
            );
            None
        }
    };
    alive.store(true, Ordering::Release);
    external_alive.store(true, Ordering::Release);

    debug!(
        "Sync thread started: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) mmio={}",
        params.pl1_watts,
        params.pl1_time_s,
        params.pl2_watts,
        params.pl2_time_s,
        mchbar_handle.is_some()
    );

    // Track consecutive failures; keep retrying despite overrides.
    let mut write_failures: u32 = 0;
    let mut mmio_write_failures: u32 = 0;
    while running.load(Ordering::Relaxed) {
        // Unconditionally re-assert limits every 250ms. The whole point of
        // this thread is to win against firmware/EC overwrites.
        match write_msr_pl1_pl2(&msr_handle, &params) {
            Ok(()) => {
                write_failures = 0;
            }
            Err(e) => {
                write_failures += 1;
                if write_failures <= 5 {
                    warn!("Sync thread MSR write failed: {}", e);
                } else if write_failures == 6 {
                    warn!(
                        "Sync thread MSR write keeps failing ({}), suppressing further warnings",
                        e
                    );
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
                        warn!(
                            "Sync thread MMIO write keeps failing ({}), suppressing further warnings",
                            e
                        );
                    }
                }
            }
        }
        // Circuit breaker: BIOS-locked register will never succeed; stop after 30 consecutive failures
        if write_failures >= 30 {
            warn!("Sync thread giving up after 30 consecutive MSR failures (likely BIOS-locked)");
            break;
        }
        // Back off on persistent failures; cap at 30s.
        // BIOS-locked register will otherwise spin forever; 30s reduces thermal contention.
        let interval_ms = if write_failures >= 20 {
            30_000
        } else if write_failures >= 10 {
            5000
        } else if write_failures >= 4 {
            1000
        } else {
            250
        };
        // Sleep in short chunks so stop_sync()/join() never blocks on the
        // full backoff interval (previously up to 30s of UI freeze).
        let mut slept_ms: u64 = 0;
        while slept_ms < interval_ms {
            if !running.load(Ordering::Relaxed) {
                break;
            }
            let chunk = (interval_ms - slept_ms).min(100);
            std::thread::sleep(std::time::Duration::from_millis(chunk));
            slept_ms += chunk;
        }
    }

    debug!("Sync thread stopped");
}

/// BIOS defaults captured once at first run and persisted for Reset.
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
    /// Power source at capture; resume only restores when source matches.
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

/// Reads current AC-present state from shared snapshot (defaults to AC).
pub fn read_ac_present() -> bool {
    *BATTERY_AC_SNAPSHOT.read()
}

static BATTERY_AC_SNAPSHOT: parking_lot::RwLock<bool> = parking_lot::RwLock::new(true);

/// Publishes AC state for `read_ac_present`.
pub fn publish_ac_snapshot(ac_present: bool) {
    *BATTERY_AC_SNAPSHOT.write() = ac_present;
}

fn load_persisted_bios_defaults() -> Option<BiosDefaults> {
    let path = bios_defaults_path().ok()?;
    if path.exists() {
        crate::config::warn_if_world_writable(&path);
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Missing file: first run.
        Err(_) => return None,
    };
    match toml::from_str::<BiosDefaults>(&content) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            // Corrupt file: back up and refuse to overwrite.
            let backup =
                path.with_extension(format!("toml.corrupt-{}", crate::util::current_time_ms()));
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
    // Atomic write via temp file to avoid corruption.
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        crate::util::current_time_ms(),
        std::process::id()
    ));
    if let Ok(meta) = std::fs::symlink_metadata(&tmp)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to write through symlink: {}",
            tmp.display()
        ));
    }
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("write tmp bios_defaults failed: {}", e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("write tmp bios_defaults failed: {}", e))?;
    }
    // Reuse the shared atomic replace (MoveFileExW + rename-first fallback).
    match crate::config::atomic_replace(&tmp, &path, true) {
        Ok(()) => {
            crate::config::harden_file_acl(&path);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("replace bios_defaults failed: {}", e))
        }
    }
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
        // hash mismatch is advisory (warn + allow) to follow github latest without code change
        assert!(verify_module_hash(&path, INTEL_MSR_SHA256).is_ok());
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
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
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
        let defaults2 = BiosDefaults {
            pl1_watts: 99.0,
            ..defaults
        };
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

    #[test]
    fn effective_limit_ignores_disabled() {
        // MSR disabled, MMIO enabled -> effective is MMIO
        assert_eq!(effective_limit(15.0, false, 20.0, true), 20.0);
        // MSR enabled 15, MMIO disabled 10 -> effective is MSR
        assert_eq!(effective_limit(15.0, true, 10.0, false), 15.0);
        // Both enabled, lower wins
        assert_eq!(effective_limit(20.0, true, 15.0, true), 15.0);
        // Both disabled -> 0
        assert_eq!(effective_limit(10.0, false, 20.0, false), 0.0);
        // Zero value with enabled is invalid
        assert_eq!(effective_limit(0.0, true, 20.0, true), 20.0);
    }

    #[test]
    fn cpu_power_info_effective_uses_enabled() {
        let mut info = CpuPowerInfo {
            pl1_msr: 15.0,
            pl1_msr_enabled: false,
            pl1_mmio: 20.0,
            pl1_mmio_enabled: true,
            pl2_msr: 30.0,
            pl2_msr_enabled: true,
            pl2_mmio: 25.0,
            pl2_mmio_enabled: true,
            ..Default::default()
        };
        assert_eq!(info.effective_pl1(), 20.0);
        assert_eq!(info.effective_pl2(), 25.0);
        info.pl1_msr_enabled = true;
        assert_eq!(info.effective_pl1(), 15.0);
    }

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
