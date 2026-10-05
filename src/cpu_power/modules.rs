//! PawnIO Modules acquisition: download, hash verification, safe ZIP extraction,
//! the local version marker, and the blob caches used by the limit paths.

use std::os::windows::process::CommandExt;
use std::sync::Arc;

use tracing::{debug, warn};

const MODULES_DIR_NAME: &str = "modules";
/// Marker file recording which upstream tag the local bins came from.
const MODULES_VERSION_FILE: &str = ".version";
/// Last-known-good PawnIO.Modules upstream release tag, used ONLY as a
/// fallback download URL when the GitHub API is unreachable. The primary
/// path always queries the latest release. Hashes are enforced by default;
/// set FRAMEWORK_ALLOW_UNKNOWN_MODULE_HASH=1 only after manual verification.
const LAST_KNOWN_MODULES_VERSION: &str = "0.2.11";
const INTEL_MSR_SHA256: &str = "d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f";
const INTEL_MCHBAR_SHA256: &str =
    "3f82b832d99b4aac37d2a20fdb7c9baa2a3bc0488612c9019c9484eb0e8a6eae";

/// Module blob file names. Named next to the hashes they belong to: a typo in
/// a file name compiles either way and only shows up as a missing blob at
/// runtime, next to a hash that is correct.
const INTEL_MSR_FILE: &str = "IntelMSR.bin";
const INTEL_MCHBAR_FILE: &str = "IntelMCHBAR.bin";

/// Upper bound for the downloaded Modules ZIP. Releases are a few hundred KB;
/// anything larger is either the wrong file or a decompression-bomb attempt.
const MAX_MODULES_ZIP_BYTES: u64 = 32 * 1024 * 1024;
/// Upper bound for total extracted staging bytes and member count.
const MAX_STAGING_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_MEMBERS: usize = 1024;

/// Reads the locally installed modules tag recorded at download time.
pub(super) fn local_modules_version() -> Option<String> {
    let dir = modules_dir();
    let raw = std::fs::read_to_string(dir.join(MODULES_VERSION_FILE)).ok()?;
    let ver = raw.trim().trim_start_matches('v').to_string();
    if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        return None;
    }
    Some(ver)
}

/// Extracts the release tag from a `.../download/{tag}/...` asset URL.
pub(super) fn tag_from_modules_url(url: &str) -> Option<String> {
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

/// Whether a Modules update download is needed: only when the recorded
/// local tag differs from the upstream latest. Unknown on either side means
/// download, so a missing marker or an unreachable API never blocks an update.
pub(super) fn modules_update_needed(local: Option<&str>, latest_tag: Option<&str>) -> bool {
    match (local, latest_tag) {
        (Some(l), Some(t)) => l != t,
        _ => true,
    }
}

/// Creates a file exclusively: fails if it exists, never follows a planted
/// symlink at the temp path.
fn create_new_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
}

/// Escapes a string for embedding in a PowerShell single-quoted string.
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Manual fallback instructions appended to download/extraction failures.
fn manual_download_hint() -> &'static str {
    "check internet or download latest release_*.zip manually from https://github.com/namazso/PawnIO.Modules/releases/latest and place IntelMSR.bin / IntelMCHBAR.bin into the modules folder (use Open Modules Folder)"
}

/// One-line failure detail from child output: stderr, else stdout, else exit
/// code. Shared so every subprocess failure reads the same way.
pub(super) fn proc_failure_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        format!("exit code {}", output.status.code().unwrap_or(-1))
    }
}

/// Records which upstream tag the local bins came from.
///
/// Only the cached version display string is dropped. The blobs themselves
/// are untouched, because this is also called from the self-heal path inside
/// `pawnio_modules_version`, and forcing a re-read plus SHA-256 re-verification
/// of both files from a getter would be a needless cost.
fn persist_local_modules_version(dir: &std::path::Path, tag: &str) {
    let path = dir.join(MODULES_VERSION_FILE);
    let _ = std::fs::remove_file(&path);
    if let Ok(mut f) = create_new_file(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{}", tag);
    }
    invalidate_modules_version();
}

/// Drops the cached Modules version display string.
fn invalidate_modules_version() {
    *MODULES_VERSION.write() = None;
}

/// Tries to fetch latest release asset download URL directly via GitHub API.
/// Returns Some(url) if API succeeds, else None to use version-constructed URL.
pub(super) fn latest_modules_download_url() -> Option<String> {
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

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    // sha2 0.11 returns a hybrid-array without LowerHex; encode manually so
    // the format does not depend on the digest crate's output type.
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Explicit opt-in for loading module blobs whose hashes are not pinned.
/// Default is fail-closed; only set this after manually verifying a newer
/// upstream release.
fn module_hash_override_enabled() -> bool {
    std::env::var_os("FRAMEWORK_ALLOW_UNKNOWN_MODULE_HASH").is_some()
}

fn check_module_hash(
    path: &std::path::Path,
    expected: &str,
    actual: &str,
    allow_unknown: bool,
) -> Result<(), &'static str> {
    if actual == expected {
        return Ok(());
    }
    if allow_unknown {
        warn!(
            "PawnIO module hash mismatch but continuing due to FRAMEWORK_ALLOW_UNKNOWN_MODULE_HASH: {} expected {} got {}",
            path.display(),
            expected,
            actual
        );
        return Ok(());
    }
    warn!(
        "PawnIO module hash mismatch, refusing to load: {} expected {} got {}",
        path.display(),
        expected,
        actual
    );
    Err("module hash mismatch")
}

fn verify_module_hash(path: &std::path::Path, expected: &str) -> Result<(), &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "module blob missing")?;
    let actual = sha256_hex(&bytes);
    check_module_hash(path, expected, &actual, module_hash_override_enabled())
}

/// Reads module blob and verifies hash on same bytes to prevent TOCTOU.
fn read_verified_module(path: &std::path::Path, expected: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "module blob missing")?;
    let actual = sha256_hex(&bytes);
    check_module_hash(path, expected, &actual, module_hash_override_enabled())?;
    Ok(bytes)
}

static MODULES_CACHE: parking_lot::RwLock<Option<bool>> = parking_lot::RwLock::new(None);

/// Checks if usable module blobs are present (cached). Presence alone is not
/// enough: hashes are enforced, so a mismatched manual copy counts as missing.
pub fn modules_downloaded() -> bool {
    {
        let guard = MODULES_CACHE.read();
        if let Some(cached) = *guard {
            return cached;
        }
    }
    let dir = modules_dir();
    let present = dir.join(INTEL_MSR_FILE).is_file() && dir.join(INTEL_MCHBAR_FILE).is_file();
    let ready = present
        && verify_module_hash(&dir.join(INTEL_MSR_FILE), INTEL_MSR_SHA256).is_ok()
        && verify_module_hash(&dir.join(INTEL_MCHBAR_FILE), INTEL_MCHBAR_SHA256).is_ok();
    *MODULES_CACHE.write() = Some(ready);
    ready
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

/// Rejects symlinks/junctions inside staging (no symlink following) and caps
/// total extracted bytes, so a hostile archive cannot fill the disk.
fn validate_staging(staging: &std::path::Path) -> Result<(), String> {
    let mut stack = vec![staging.to_path_buf()];
    let mut total: u64 = 0;
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
            } else if ft.is_file() {
                total = total.saturating_add(
                    e.metadata()
                        .map(|m| m.len())
                        .map_err(|e| format!("staging metadata failed: {}", e))?,
                );
                if total > MAX_STAGING_BYTES {
                    return Err(format!(
                        "staging exceeds {} bytes, refusing to promote",
                        MAX_STAGING_BYTES
                    ));
                }
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
    let mut member_count = 0usize;
    for member in String::from_utf8_lossy(&list_out.stdout).lines() {
        let m = member.trim().trim_end_matches('/');
        if m.is_empty() {
            continue;
        }
        member_count += 1;
        if member_count > MAX_ARCHIVE_MEMBERS {
            return Err(format!(
                "archive has more than {} members, refusing to extract",
                MAX_ARCHIVE_MEMBERS
            ));
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
        let ps = format!(
            "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
            ps_escape(&zip_tmp.display().to_string()),
            ps_escape(&staging.display().to_string())
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
    // 3. Reject symlinks planted inside staging and cap extracted bytes.
    if let Err(e) = validate_staging(&staging) {
        cleanup(&staging, zip_tmp);
        return Err(e);
    }
    // 4. Require the two expected bins as regular files, then enforce hashes.
    for name in [INTEL_MSR_FILE, INTEL_MCHBAR_FILE] {
        let src = staging.join(name);
        let meta =
            std::fs::symlink_metadata(&src).map_err(|_| format!("archive missing {}", name))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            cleanup(&staging, zip_tmp);
            return Err(format!("archive missing {}", name));
        }
    }
    for (name, expected) in [
        (INTEL_MSR_FILE, INTEL_MSR_SHA256),
        (INTEL_MCHBAR_FILE, INTEL_MCHBAR_SHA256),
    ] {
        if let Err(e) = verify_module_hash(&staging.join(name), expected) {
            cleanup(&staging, zip_tmp);
            return Err(format!(
                "{} {}; upstream may be newer than this app — update the app, or set FRAMEWORK_ALLOW_UNKNOWN_MODULE_HASH=1 only after manual verification",
                name, e
            ));
        }
    }
    // 5. Promote verified bins into modules dir; old bins stay until replaced.
    for name in [INTEL_MSR_FILE, INTEL_MCHBAR_FILE] {
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
    Ok(())
}

/// Size sanity check before extraction: rejects truncated downloads below and
/// wrong-file/decompression-bomb candidates above.
fn check_zip_size(zip_tmp: &std::path::Path) -> Result<(), String> {
    let meta = std::fs::metadata(zip_tmp).map_err(|e| format!("stat zip failed: {}", e))?;
    if meta.len() < 1000 {
        let _ = std::fs::remove_file(zip_tmp);
        return Err("downloaded zip too small".to_string());
    }
    if meta.len() > MAX_MODULES_ZIP_BYTES {
        let _ = std::fs::remove_file(zip_tmp);
        return Err(format!(
            "downloaded zip exceeds {} bytes",
            MAX_MODULES_ZIP_BYTES
        ));
    }
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
        let _guard =
            create_new_file(&zip_tmp).map_err(|e| format!("failed to create temp zip: {}", e))?;
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
    let ps_command = format!(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
         Invoke-WebRequest -Uri '{url}' -OutFile '{zip}' -UseBasicParsing -ErrorAction Stop",
        url = ps_escape(&url),
        zip = ps_escape(&zip_tmp.display().to_string()),
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
        let detail = proc_failure_detail(&output);
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
            "download/extraction failed: {} — {}",
            detail,
            manual_download_hint()
        ));
    }

    // Size sanity check before extraction (too small or too large).
    check_zip_size(&zip_tmp)?;
    if let Err(e) = extract_staged_zip(&zip_tmp, &dir) {
        warn!("Staged extraction failed: {}, trying curl fallback", e);
        if try_curl_download(&url, &dir).is_ok() {
            debug!(
                "PawnIO modules extracted via curl fallback after staged failure to {}",
                dir.display()
            );
            return Ok(());
        }
        return Err(format!("{} — {}", e, manual_download_hint()));
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
        let _guard = create_new_file(&zip_tmp)
            .map_err(|e| format!("failed to create temp zip (possible race): {}", e))?;
    }
    // Refuse oversized responses up front via Content-Length.
    let max_size = MAX_MODULES_ZIP_BYTES.to_string();
    let curl_output = std::process::Command::new("curl.exe")
        .args([
            "-L",
            "--fail",
            "--proto",
            "=https",
            "--max-time",
            "30",
            "--max-filesize",
            &max_size,
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
    // Size sanity check before extraction (too small or too large).
    check_zip_size(&zip_tmp)?;
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

/// Cached Modules version display string. Lives next to the blob caches so
/// that invalidating one invalidates all of them.
static MODULES_VERSION: parking_lot::RwLock<Option<String>> = parking_lot::RwLock::new(None);

/// Drops every cache derived from the modules directory.
///
/// The version string is derived from the same files as the blobs, so it is
/// cleared here too. Callers used to have to remember a second call, and a
/// fourth one would have silently missed it.
fn invalidate_blob_cache() {
    *MSR_BLOB_CACHE.lock() = None;
    *MCHBAR_BLOB_CACHE.lock() = None;
    invalidate_modules_version();
    invalidate_modules_cache();
}

/// Returns the locally installed PawnIO Modules version for display:
/// recorded tag, hash-inferred tag for unmarked bins, `unknown` for
/// unrecognized manual installs, `not installed` when absent.
///
/// Self-heals: when the blobs match a known release hash but carry no version
/// marker (a manual copy), the inferred tag is recorded so the number shows
/// up next time. That is a write to the modules directory from a getter, which
/// is why it happens at most once per process.
pub fn pawnio_modules_version() -> String {
    if let Some(v) = MODULES_VERSION.read().clone() {
        return v;
    }
    let dir = modules_dir();
    let v = if let Some(local) = local_modules_version() {
        local
    } else if dir.join(INTEL_MSR_FILE).is_file() && dir.join(INTEL_MCHBAR_FILE).is_file() {
        let msr = std::fs::read(dir.join(INTEL_MSR_FILE))
            .map(|b| sha256_hex(&b))
            .unwrap_or_default();
        let mchbar = std::fs::read(dir.join(INTEL_MCHBAR_FILE))
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
    modules_downloaded()
}

/// Loads a module blob through the cache, verifying hash on first load.
/// Shared by the MSR/MCHBAR loaders so the two cannot drift apart.
fn load_blob(
    cache: &parking_lot::Mutex<Option<Arc<Vec<u8>>>>,
    name: &str,
    expected: &str,
) -> Result<Vec<u8>, &'static str> {
    {
        let guard = cache.lock();
        if let Some(cached) = guard.as_ref() {
            return Ok((**cached).clone());
        }
    }
    let blob = read_verified_module(&modules_dir().join(name), expected)?;
    let arc = Arc::new(blob.clone());
    *cache.lock() = Some(arc);
    Ok(blob)
}

/// Loads IntelMSR blob (cached after first verified load).
pub(super) fn load_intel_msr_blob() -> Result<Vec<u8>, &'static str> {
    load_blob(&MSR_BLOB_CACHE, INTEL_MSR_FILE, INTEL_MSR_SHA256)
}

/// Loads IntelMCHBAR blob (cached after first verified load).
pub(super) fn load_intel_mchbar_blob() -> Result<Vec<u8>, &'static str> {
    load_blob(&MCHBAR_BLOB_CACHE, INTEL_MCHBAR_FILE, INTEL_MCHBAR_SHA256)
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
    fn modules_update_needed_only_on_tag_mismatch() {
        assert!(!modules_update_needed(Some("0.2.11"), Some("0.2.11")));
        assert!(modules_update_needed(Some("0.2.10"), Some("0.2.11")));
        // Unknown on either side must not block an update.
        assert!(modules_update_needed(None, Some("0.2.11")));
        assert!(modules_update_needed(Some("0.2.11"), None));
        assert!(modules_update_needed(None, None));
    }

    #[test]
    fn verify_module_hash_rejects_mismatch() {
        let dir = std::env::temp_dir().join("framework-crate-hash-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.bin");
        std::fs::write(&path, b"not-a-module").unwrap();
        let actual = sha256_hex(b"not-a-module");
        // Fail-closed by default; explicit override is the only bypass.
        assert!(check_module_hash(&path, INTEL_MSR_SHA256, &actual, false).is_err());
        assert!(check_module_hash(&path, INTEL_MSR_SHA256, &actual, true).is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
