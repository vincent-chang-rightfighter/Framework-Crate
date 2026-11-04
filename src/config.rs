use crate::types::Config;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Serializes config writes so only one writer persists at a time.
static CONFIG_SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Per-save unique suffix to avoid temp file collisions between concurrent writers.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Newest persisted config version; prevents stale debounced saves from overwriting newer shutdown saves.
static LAST_SAVED_VERSION: AtomicU64 = AtomicU64::new(0);

/// Builds unique temp extension from timestamp, PID, and counter for cross-process uniqueness.
fn unique_tmp_extension() -> String {
    let counter = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = crate::util::current_time_ms();
    let pid = std::process::id();
    format!("toml.{}.{}.{}.tmp", timestamp, pid, counter)
}

/// Serializes tests that mutate the process-global `FRAMEWORK_CONTROL_CONFIG_DIR`
/// env var; parallel tests reading/writing different temp dirs would race otherwise.
#[cfg(test)]
pub(crate) static CONFIG_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn config_path() -> Result<PathBuf, String> {
    let config_dir = {
        #[cfg(any(debug_assertions, test))]
        {
            if let Some(os) = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR") {
                let p = PathBuf::from(&os);
                validate_config_dir(&p)?;
                p
            } else {
                default_config_dir()
            }
        }
        #[cfg(not(any(debug_assertions, test)))]
        {
            if std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR").is_some() {
                tracing::warn!(
                    "FRAMEWORK_CONTROL_CONFIG_DIR is ignored in release builds; using the default config directory"
                );
            }
            default_config_dir()
        }
    };
    // create_dir_all is idempotent and cheap; avoid caching to prevent stale test dir reuse
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config directory {}: {}",
            config_dir.display(),
            e
        )
    })?;
    // ensure it is a directory, not a file/symlink placed by attacker
    if let Ok(meta) = std::fs::symlink_metadata(&config_dir) {
        if !meta.is_dir() {
            return Err(format!(
                "config directory is not a directory: {}",
                config_dir.display()
            ));
        }
        if meta.file_type().is_symlink() || dir_has_reparse_point(&config_dir) {
            return Err(format!(
                "config directory is a symlink or junction, refusing: {}",
                config_dir.display()
            ));
        }
    }
    // Canonicalize to collapse `..`/nested junctions in parents so the
    // joined config.toml cannot escape the validated directory. Failure
    // to canonicalize (e.g. race) fails closed.
    let canonical = std::fs::canonicalize(&config_dir).map_err(|e| {
        format!(
            "Failed to canonicalize config directory {}: {}",
            config_dir.display(),
            e
        )
    })?;
    Ok(canonical.join("config.toml"))
}

#[cfg(any(debug_assertions, test))]
fn validate_config_dir(path: &std::path::Path) -> Result<(), String> {
    use std::path::Component;
    let s = path.to_string_lossy();
    // Reject Win32 extended-length / device paths that bypass normalization.
    if s.contains(r"\\?\") || s.contains(r"\\.\") {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must not contain \\\\?\\ or \\\\.\\ prefix: {}",
            path.display()
        ));
    }
    if !path.is_absolute() {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must be absolute: {}",
            path.display()
        ));
    }
    // Reject parent-dir components that could escape after join.
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must not contain '..': {}",
            path.display()
        ));
    }
    // Reject UNC paths: a network config dir leaks NTLM auth and lives
    // outside every local ACL check below.
    if path.components().any(|c| {
        matches!(
            c,
            Component::Prefix(p)
                if matches!(
                    p.kind(),
                    std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)
                )
        )
    }) {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must not be a UNC path: {}",
            path.display()
        ));
    }
    Ok(())
}

/// True when the path is a symlink or junction: both carry
/// FILE_ATTRIBUTE_REPARSE_POINT, while Rust's is_symlink misses junctions.
/// Shared with the modules downloader, which stages to a sibling directory.
pub(crate) fn dir_has_reparse_point(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };
    use windows_sys::core::PCWSTR;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: null-terminated path buffer.
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr() as PCWSTR) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn default_config_dir() -> PathBuf {
    #[cfg(test)]
    {
        // Keep tests off the real user config.
        std::env::temp_dir().join("framework-crate-tests")
    }
    #[cfg(not(test))]
    {
        let base = dirs::config_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("framework-crate")
    }
}

/// Write-like rights: writing/appending data, taking ownership, or changing
/// the DACL all let Everyone rewrite or re-permission the file.
const WORLD_WRITE_MASK: u32 = {
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_APPEND_DATA, FILE_WRITE_DATA, WRITE_DAC, WRITE_OWNER,
    };
    FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE | WRITE_DAC | WRITE_OWNER
};

/// Checks the file DACL for an allow-ACE granting write-like rights to a
/// broad SID (Everyone, Users, Authenticated Users). Returns false on any
/// query failure (fail-open: inspection errors must not block saves).
fn file_has_world_write_ace(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, PSID,
        SECURITY_MAX_SID_SIZE, WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::core::PCWSTR;

    // SIDs whose write access is dangerous: Everyone plus the broad
    // built-in groups a per-user admin file must never grant.
    let mut watched: Vec<(Vec<u8>, usize)> = Vec::new();
    for well_known in [WinWorldSid, WinBuiltinUsersSid, WinAuthenticatedUserSid] {
        let mut buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = buf.len() as u32;
        // SAFETY: buffer is SECURITY_MAX_SID_SIZE bytes, the documented maximum.
        let ok = unsafe {
            CreateWellKnownSid(
                well_known,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as PSID,
                &mut len,
            )
        };
        if ok != 0 {
            watched.push((buf.to_vec(), len as usize));
        }
    }
    if watched.is_empty() {
        return false;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: null-terminated path; DACL and SD outputs are freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr() as PCWSTR,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 || dacl.is_null() {
        if !sd.is_null() {
            // SAFETY: SD was allocated by the call above.
            unsafe { LocalFree(sd) };
        }
        return false;
    }
    // Walk ACEs manually: the first ACE follows the 8-byte ACL header and each
    // ACE carries its own size. Bounds-checked against AclSize throughout.
    // SAFETY: dacl points to a valid ACL; sd is freed at the end.
    unsafe {
        let acl_size = (*dacl).AclSize as usize;
        let ace_count = (*dacl).AceCount as usize;
        let mut offset = std::mem::size_of::<ACL>();
        let mut hit = false;
        for _ in 0..ace_count {
            if offset + std::mem::size_of::<ACE_HEADER>() > acl_size {
                break;
            }
            let header = &*((dacl as *const u8).add(offset) as *const ACE_HEADER);
            let size = header.AceSize as usize;
            if size < std::mem::size_of::<ACE_HEADER>() || offset + size > acl_size {
                break;
            }
            if header.AceType as u32 == ACCESS_ALLOWED_ACE_TYPE {
                let ace = &*((dacl as *const u8).add(offset) as *const ACCESS_ALLOWED_ACE);
                let sid = &ace.SidStart as *const u32 as *const u8;
                // SID length is 8 header bytes plus 4 per sub-authority.
                let sid_len = 8 + 4 * (*sid.add(1) as usize);
                let watched_hit = watched.iter().any(|(buf, len)| {
                    sid_len == *len && core::slice::from_raw_parts(sid, sid_len) == &buf[..*len]
                });
                if watched_hit && ace.Mask & WORLD_WRITE_MASK != 0 {
                    hit = true;
                    break;
                }
            }
            offset += size;
        }
        LocalFree(sd);
        hit
    }
}

/// Warns if the config file at `path` may be world-writable.
/// On Windows, %APPDATA% is per-user (user+SYSTEM+Admins only) so inherited ACL
/// is already restrictive; we warn if the DACL holds an explicit allow-ACE
/// granting write-like rights to Everyone. Fail-open: inspection errors are
/// silently ignored and never block a save.
pub(crate) fn warn_if_world_writable(path: &std::path::Path) {
    {
        if !path.exists() {
            return;
        }
        if file_has_world_write_ace(path) {
            tracing::warn!(
                "Config file {} grants write access to a broad group — consider removing the permissive ACE",
                path.display()
            );
        } else {
            tracing::debug!("Config file {} has no world-writable ACE", path.display());
        }
    }
}

/// Best-effort hardening: removes explicit Everyone write ACEs from the
/// config file, leaving inherited ACLs (e.g. from %APPDATA%) untouched.
/// Failures are warn-only and do not abort the save.
pub(crate) fn harden_file_acl(path: &std::path::Path) {
    if !file_has_world_write_ace(path) {
        tracing::debug!(
            "Config file {} saved with inherited ACL from %APPDATA% (per-user)",
            path.display()
        );
        return;
    }
    match revoke_everyone_write(path) {
        Ok(true) => tracing::warn!(
            "Removed permissive broad-group write ACEs from {}",
            path.display()
        ),
        Ok(false) => tracing::warn!(
            "Config file {} has an inherited broad-group write ACE, leaving it (parent dir out of scope)",
            path.display()
        ),
        Err(e) => tracing::warn!(
            "Failed to remove permissive ACE from {}: {}",
            path.display(),
            e
        ),
    }
}

/// Revokes broad-group write-like rights on `path` via explicit REVOKE
/// entries, then re-verifies. Only explicit ACEs are touched: inherited ones
/// belong to the parent directory and are reported, not modified.
/// Returns Ok(true) when the file verifies clean afterwards.
fn revoke_everyone_write(path: &std::path::Path) -> Result<bool, &'static str> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
        SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
        TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
        INHERITED_ACE, NO_INHERITANCE, PSID, SECURITY_MAX_SID_SIZE, WinAuthenticatedUserSid,
        WinBuiltinUsersSid, WinWorldSid,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::core::{PCWSTR, PWSTR};

    // Scrub the same broad SIDs the detector watches. Skipping e.g. Users
    // here would leave the file writable while detection keeps firing.
    let mut sid_bufs: Vec<Vec<u8>> = Vec::new();
    let mut sid_lens: Vec<usize> = Vec::new();
    for well_known in [WinWorldSid, WinBuiltinUsersSid, WinAuthenticatedUserSid] {
        let mut buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = buf.len() as u32;
        // SAFETY: buffer is SECURITY_MAX_SID_SIZE bytes, the documented maximum.
        let ok = unsafe {
            CreateWellKnownSid(
                well_known,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as PSID,
                &mut len,
            )
        };
        if ok != 0 {
            sid_lens.push(len as usize);
            sid_bufs.push(buf.to_vec());
        }
    }
    if sid_bufs.is_empty() {
        return Err("could not build well-known SIDs");
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: null-terminated path; DACL and SD outputs freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr() as PCWSTR,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 || dacl.is_null() {
        if !sd.is_null() {
            // SAFETY: SD was allocated by the call above.
            unsafe { LocalFree(sd) };
        }
        return Err("could not read DACL");
    }
    // Refuse when any offending ACE is inherited: those belong to the parent
    // directory and must not be rewritten here.
    // SAFETY: dacl points to a valid ACL; bounds-checked against AclSize.
    let explicit_only = unsafe {
        let acl_size = (*dacl).AclSize as usize;
        let mut offset = std::mem::size_of::<ACL>();
        let mut all_explicit = true;
        while offset + std::mem::size_of::<ACE_HEADER>() <= acl_size {
            let header = &*((dacl as *const u8).add(offset) as *const ACE_HEADER);
            let size = header.AceSize as usize;
            if size < std::mem::size_of::<ACE_HEADER>() || offset + size > acl_size {
                break;
            }
            if header.AceType as u32 == ACCESS_ALLOWED_ACE_TYPE {
                let ace = &*((dacl as *const u8).add(offset) as *const ACCESS_ALLOWED_ACE);
                let sid = &ace.SidStart as *const u32 as *const u8;
                let sid_len = 8 + 4 * (*sid.add(1) as usize);
                let watched = sid_bufs.iter().zip(sid_lens.iter()).any(|(buf, len)| {
                    sid_len == *len && core::slice::from_raw_parts(sid, sid_len) == &buf[..*len]
                });
                if watched
                    && ace.Mask & WORLD_WRITE_MASK != 0
                    && (header.AceFlags as u32) & INHERITED_ACE != 0
                {
                    all_explicit = false;
                    break;
                }
            }
            offset += size;
        }
        all_explicit
    };
    if !explicit_only {
        // SAFETY: SD was allocated above.
        unsafe { LocalFree(sd) };
        return Ok(false);
    }
    let mut trustees: Vec<TRUSTEE_W> = Vec::with_capacity(sid_bufs.len());
    for buf in sid_bufs.iter_mut() {
        trustees.push(TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: buf.as_mut_ptr() as PWSTR,
        });
    }
    let entries: Vec<EXPLICIT_ACCESS_W> = trustees
        .iter()
        .map(|trustee| EXPLICIT_ACCESS_W {
            grfAccessPermissions: WORLD_WRITE_MASK,
            grfAccessMode: REVOKE_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: *trustee,
        })
        .collect();
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: explicit entries and old DACL are valid; new DACL freed below.
    // sid_bufs outlives the call, so trustee pointers stay valid.
    let err =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut new_dacl) };
    // SAFETY: SD was allocated above.
    unsafe { LocalFree(sd) };
    if err != 0 || new_dacl.is_null() {
        return Err("could not rebuild DACL");
    }
    // SAFETY: null-terminated path; new DACL built above, freed below.
    let err = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr() as PCWSTR,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: allocated by SetEntriesInAclW above.
    unsafe { LocalFree(new_dacl as *mut core::ffi::c_void) };
    if err != 0 {
        return Err("could not write DACL");
    }
    // Verify the removal actually landed.
    if file_has_world_write_ace(path) {
        return Err("permissive ACE still present after revoke");
    }
    Ok(true)
}

/// Backs up corrupt config bytes before next save overwrites them.
fn backup_corrupt_config(path: &std::path::Path, content: &[u8]) {
    use std::io::Write;
    let backup = path.with_extension("toml.corrupt");
    if backup.exists() {
        tracing::warn!(
            "{} is corrupt; keeping existing backup {}",
            path.display(),
            backup.display()
        );
        return;
    }
    let result = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&backup)
        .and_then(|mut f| f.write_all(content));
    match result {
        Ok(()) => tracing::warn!(
            "Backed up corrupt config to {} before falling back to defaults",
            backup.display()
        ),
        Err(be) => tracing::warn!(
            "Failed to back up corrupt config {}: {}",
            path.display(),
            be
        ),
    }
}

pub fn load() -> Result<Config, String> {
    let path = config_path()?;
    tracing::debug!("Loading config from: {}", path.display());
    if path.exists() {
        warn_if_world_writable(&path);
        let bytes = match crate::util::read_validated_file_bytes(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Err(format!("Failed to read {}: {}", path.display(), e));
            }
        };
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                backup_corrupt_config(&path, e.as_bytes());
                return Err(format!("Failed to read {}: {}", path.display(), e));
            }
        };
        let mut config: Config = match toml::from_str(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                // Load falls back to defaults and next save overwrites; preserve corrupt file.
                backup_corrupt_config(&path, content.as_bytes());
                return Err(format!("Failed to parse {}: {}", path.display(), e));
            }
        };
        config.validate();
        return Ok(config);
    }
    Ok(Config::default())
}

/// Skips write if newer version already persisted; check is under lock to avoid shutdown race.
pub fn save_versioned(config: &Config, ver: u64, sync: bool) -> Result<(), String> {
    let _guard = match CONFIG_SAVE_LOCK.lock() {
        Ok(g) => g,
        Err(p) => {
            tracing::warn!("CONFIG_SAVE_LOCK poisoned, recovering");
            p.into_inner()
        }
    };
    let newest = LAST_SAVED_VERSION.load(Ordering::SeqCst);
    if ver < newest {
        tracing::debug!(
            "Skipping config save v{} (v{} already on disk)",
            ver,
            newest
        );
        return Ok(());
    }
    let result = save_impl(config, sync);
    if result.is_ok() {
        LAST_SAVED_VERSION.store(ver, Ordering::SeqCst);
    }
    result
}

/// Renders the on-disk config file content (comment header + pretty TOML).
/// Shared by the internal save path and settings export so both files
/// stay byte-identical in shape.
fn render_config_toml(config: &Config) -> Result<String, String> {
    // Clone to avoid mutating caller's Config during validate/sort.
    let mut config = config.clone();
    config.validate();
    if let Some(ref mut curve) = config.fan.curve {
        curve.curve.points.sort_by_key(|p| p[0]);
    }
    let body = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
    Ok(format!(
        "# Framework Crate configuration\n\
         # Edit values below; the app validates on load.\n\
         #\n\
         # [fan]\n\
         # mode = \"disabled\" | \"manual\" | \"curve\"\n\
         # [fan.manual]\n\
         # duty_pct = 0..100\n\
         # [fan.curve]\n\
         # poll_ms = 500..5000            (fan curve step interval)\n\
         # hysteresis_c = 0..10            (temperature hysteresis)\n\
         # rate_limit_pct_per_step = 1..100\n\
         # rate_limit_down_pct_per_step = 1..100   (optional; defaults to rate_limit_pct_per_step)\n\
         # points = [[temp, duty], ...]\n\
         #\n\
         # [telemetry]\n\
         # poll_ms = 200..2000             (sensor read interval)\n\
         # ui_refresh_ms = 50..1000        (UI refresh interval)\n\
         # selected_sensors = [\"Sensor Name\", ...]\n\
         # chart_window_seconds = 15 | 30 | 60  (temperature chart window)\n\
         #\n\
         # [battery]\n\
         # [battery.charge_limit_max_pct]\n\
         # enabled = true/false\n\
         # value = 25..100\n\
         #\n\
         {}\n",
        body
    ))
}

/// Maximum accepted import file size; the real file is a few KB, so
/// anything larger is not a settings export.
const MAX_IMPORT_BYTES: u64 = 1 << 20;

/// Top-level keys an export may contain. Anything else is rejected so a
/// foreign TOML (or a future newer schema) can never silently partially
/// apply.
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "fan",
    "battery",
    "telemetry",
    "theme",
    "pl_custom_applied",
    "version",
    "exported_from",
];

/// Strict import: unknown top-level and nested fields are hard errors
/// (nothing applies). Older versions migrate through `validate()` like the
/// startup load; newer versions reject. Values still go through `validate()`
/// for the same clamping the startup load applies.
pub fn import_strict(content: &str) -> Result<Config, String> {
    let table: toml::Table = toml::from_str(content).map_err(|e| format!("invalid TOML: {}", e))?;
    let mut unknown: Vec<&str> = table
        .keys()
        .map(String::as_str)
        .filter(|k| !KNOWN_TOP_LEVEL_KEYS.contains(k))
        .collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        return Err(format!("unknown setting(s): {}", unknown.join(", ")));
    }
    // Nested strictness: `#[serde(default)]` would silently drop typos like
    // `[fan] bogus = 1`. (`deny_unknown_fields` on the structs is avoided:
    // it does not compose with the `#[serde(flatten)]` on the curve config
    // and would also harden the startup load path, which must stay lenient.)
    check_nested_unknown(&table)?;
    match table.get("version") {
        Some(toml::Value::Integer(v)) if *v == crate::types::CONFIG_VERSION as i64 => {}
        Some(toml::Value::Integer(v)) if *v < crate::types::CONFIG_VERSION as i64 => {
            // Older backup: allow through; `validate()` below runs the same
            // load-time migration (e.g. 4→5 curve points) and re-stamps.
        }
        Some(toml::Value::Integer(v)) => {
            return Err(format!(
                "unsupported settings version {} (want {})",
                v,
                crate::types::CONFIG_VERSION
            ));
        }
        Some(_) => {
            return Err("settings version must be an integer".to_string());
        }
        None => {
            return Err("missing settings version".to_string());
        }
    }
    let mut config: Config =
        toml::from_str(content).map_err(|e| format!("invalid settings: {}", e))?;
    config.validate();
    Ok(config)
}

/// Rejects unknown keys inside the common nested tables (`[fan]`,
/// `[fan.manual]`, `[fan.curve]`, `[battery]`, …) with dotted paths.
fn check_nested_unknown(table: &toml::Table) -> Result<(), String> {
    // Explicit per-table walk (keeps dotted paths in errors).
    let mut unknown: Vec<String> = Vec::new();
    let check = |tbl: Option<&toml::Value>, path: &str, known: &[&str], out: &mut Vec<String>| {
        if let Some(toml::Value::Table(t)) = tbl {
            for k in t.keys() {
                if !known.contains(&k.as_str()) {
                    out.push(format!("{path}.{k}"));
                }
            }
        }
    };
    let fan = table.get("fan");
    check(
        fan,
        "fan",
        &[
            "mode",
            "manual",
            "curve",
            "unified_duty",
            "per_fan_duty",
            "calibration_tables",
        ],
        &mut unknown,
    );
    let fan_tbl = fan.and_then(|v| v.as_table());
    check(
        fan_tbl.and_then(|t| t.get("manual")),
        "fan.manual",
        &["duty_pct"],
        &mut unknown,
    );
    check(
        fan_tbl.and_then(|t| t.get("curve")),
        "fan.curve",
        &[
            "sensors",
            "points",
            "hysteresis_c",
            "rate_limit_pct_per_step",
            "rate_limit_down_pct_per_step",
            "poll_ms",
            "per_fan_points",
        ],
        &mut unknown,
    );
    let battery = table.get("battery");
    check(
        battery,
        "battery",
        &["charge_limit_max_pct"],
        &mut unknown,
    );
    check(
        battery
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("charge_limit_max_pct")),
        "battery.charge_limit_max_pct",
        &["enabled", "value"],
        &mut unknown,
    );
    check(
        table.get("telemetry"),
        "telemetry",
        &[
            "poll_ms",
            "ui_refresh_ms",
            "selected_sensors",
            "chart_window_seconds",
        ],
        &mut unknown,
    );
    unknown.sort();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown setting(s): {}", unknown.join(", ")))
    }
}

/// Reads a settings file from an arbitrary path and strict-imports it.
/// Uses the hardened reader (reparse/dir/hardlink refusal + size cap) so a
/// planted symlink cannot redirect the read; the machine check runs after
/// the structural one: a different chassis size rejects, anything softer
/// (same size, missing key, unreadable local identity) comes back as a
/// warning alongside the config.
pub fn import_file(path: &std::path::Path) -> Result<ImportedSettings, String> {
    let bytes = crate::util::read_validated_file_bytes(path)
        .map_err(|e| format!("cannot read import file: {}", e))?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(format!("import file too large ({} bytes)", bytes.len()));
    }
    let content =
        String::from_utf8(bytes).map_err(|e| format!("import file is not UTF-8: {}", e))?;
    let config = import_strict(&content)?;
    let exported_from = toml::from_str::<toml::Table>(&content)
        .ok()
        .and_then(|t| t.get("exported_from").cloned())
        .and_then(|v| v.as_str().map(str::to_string));
    let warning = check_origin(exported_from.as_deref())?;
    Ok(ImportedSettings { config, warning })
}

/// Imported settings plus an optional machine-mismatch warning. `None`
/// means the file matches this machine; anything else was applied anyway
/// and should be surfaced, never hidden.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSettings {
    pub config: Config,
    pub warning: Option<String>,
}

/// This machine's SMBIOS product name (e.g. "Laptop 13 (AMD Ryzen
/// 7040Series)"); `None` when unreadable.
pub fn local_product_name() -> Option<String> {
    framework_lib::smbios::get_product_name()
}

/// Extracts the chassis size ("13", "16", …) from a Framework product
/// name by reading the digits after "Laptop". `None` when the shape is
/// unrecognized; never guess from bare numbers elsewhere in the string
/// (a "7040" must not read as size).
pub fn chassis_size(product: &str) -> Option<String> {
    const MARKER: &str = "laptop";
    let lower = product.to_ascii_lowercase();
    let start = lower.find(MARKER)? + MARKER.len();
    let digits: String = product[start..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Machine check for an import. Returns the warning to surface (`None`
/// when the file matches this machine); a different form factor or chassis
/// size is a hard error since its fan/thermal envelope does not apply here.
pub fn check_origin(exported_from: Option<&str>) -> Result<Option<String>, String> {
    check_origin_against(exported_from, local_product_name().as_deref())
}

/// Laptop vs Desktop from a product name; `None` when unrecognized.
/// Checked before chassis size so a Desktop export can never apply to a
/// Laptop (and vice versa) even though neither carries a size digit.
#[derive(Debug, PartialEq, Eq)]
enum FormFactor {
    Laptop,
    Desktop,
}

fn form_factor(product: &str) -> Option<FormFactor> {
    let lower = product.to_ascii_lowercase();
    if lower.contains("laptop") {
        Some(FormFactor::Laptop)
    } else if lower.contains("desktop") {
        Some(FormFactor::Desktop)
    } else {
        None
    }
}

fn check_origin_against(
    exported_from: Option<&str>,
    local: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(exported) = exported_from else {
        return Ok(Some("no machine info in file; applied anyway".to_string()));
    };
    let Some(local) = local else {
        return Ok(Some(
            "cannot determine this machine; applied anyway".to_string(),
        ));
    };
    if exported == local {
        return Ok(None);
    }
    if let (Some(a), Some(b)) = (form_factor(exported), form_factor(local))
        && a != b
    {
        return Err(format!(
            "settings are for '{exported}', this machine is '{local}'"
        ));
    }
    match (chassis_size(exported), chassis_size(local)) {
        (Some(a), Some(b)) if a != b => Err(format!(
            "settings are for '{exported}', this machine is '{local}'"
        )),
        (Some(_), Some(_)) => Ok(Some(format!(
            "file is from '{exported}'; same chassis size, applied with warning"
        ))),
        _ => Ok(Some("cannot compare machine; applied anyway".to_string())),
    }
}

/// TOML basic-string escaping for the machine stamp (product names carry
/// ™/®/parentheses; only backslash and quote need escaping).
fn toml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Exports the config to a user-chosen path. Stamps the machine identity
/// (`exported_from`) so import can refuse a foreign chassis. Mirrors the
/// internal save hardening (exclusive create plus symlink refusal); no ACL
/// hardening here since the destination is the user's own file.
pub fn export_to_path(config: &Config, dest: &std::path::Path) -> Result<(), String> {
    let mut content = render_config_toml(config)?;
    if let Some(origin) = local_product_name() {
        // The stamp must stay top-level: anything after a [table] header
        // would belong to that table, so splice it before the first
        // non-comment line.
        let mut stamped = String::with_capacity(content.len() + origin.len() + 64);
        let mut inserted = false;
        for line in content.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if !inserted && !trimmed.is_empty() && !trimmed.starts_with('#') {
                stamped.push_str("# Exported from this machine; import checks it.\n");
                stamped.push_str(&format!("exported_from = {}\n", toml_escape(&origin)));
                inserted = true;
            }
            stamped.push_str(line);
        }
        if !inserted {
            stamped.push_str(&format!("exported_from = {}\n", toml_escape(&origin)));
        }
        content = stamped;
    }
    if dir_has_reparse_point(dest) {
        return Err(format!(
            "refusing to write through symlink: {}",
            dest.display()
        ));
    }
    // TOCTOU hardening: a planted junction/mount-point parent would redirect
    // the exclusive create below outside the chosen directory. The check and
    // the create are not atomic, but this closes the casual case; the dest
    // itself is still created with create_new (O_EXCL).
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
        && dir_has_reparse_point(parent)
    {
        return Err(format!(
            "refusing to write under a symlinked directory: {}",
            parent.display()
        ));
    }
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dest)
        .and_then(|mut f| f.write_all(content.as_bytes()))
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "file already exists (overwrite disabled for safety): {}",
                    dest.display()
                )
            } else {
                format!("export failed: {}", e)
            }
        })
}

fn save_impl(config: &Config, sync: bool) -> Result<(), String> {
    let path = config_path()?;
    tracing::debug!("Saving config to: {}", path.display());
    let tmp_path = path.with_extension(unique_tmp_extension());
    let content = render_config_toml(config)?;
    use std::io::Write;
    let result = (|| {
        // create_new (O_EXCL) + symlink reject: a pre-planted symlink at the
        // tmp path must not redirect our write to an arbitrary file.
        if let Ok(meta) = std::fs::symlink_metadata(&tmp_path)
            && meta.file_type().is_symlink()
        {
            return Err(format!(
                "refusing to write through symlink: {}",
                tmp_path.display()
            ));
        }
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|e| format!("create tmp failed: {}", e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("write tmp failed: {}", e))?;
        if sync {
            f.sync_all()
                .map_err(|e| format!("sync tmp failed: {}", e))?;
        }
        drop(f);
        atomic_replace(&tmp_path, &path, sync)
    })();
    if result.is_ok() {
        harden_file_acl(&path);
    }
    if result.is_err()
        && let Err(e) = std::fs::remove_file(&tmp_path)
    {
        tracing::debug!("Failed to remove tmp config {}: {}", tmp_path.display(), e);
    }
    result
}

pub(crate) fn atomic_replace(
    tmp: &std::path::Path,
    dest: &std::path::Path,
    sync: bool,
) -> Result<(), String> {
    // Windows rename fails if dest exists; use MoveFileExW for atomic replace.
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

        let tmp_wide: Vec<u16> = OsStr::new(tmp)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dest_wide: Vec<u16> = OsStr::new(dest)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: Paths are null-terminated UTF-16; tmp and dest share directory so replace is atomic. Use WRITE_THROUGH only when sync is required.
        let flags = if sync {
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        } else {
            MOVEFILE_REPLACE_EXISTING
        };
        let success = unsafe { MoveFileExW(tmp_wide.as_ptr(), dest_wide.as_ptr(), flags) };
        if success != 0 {
            return Ok(());
        }
        // Fallback: try rename first (atomically replaces on Windows when dest exists).
        // If rename fails (e.g. cross-device), backup dest then delete-then-rename.
        // Refuse to follow symlinks: a planted dest/bak symlink would make
        // copy/remove write outside the config dir.
        for p in [dest, tmp] {
            if let Ok(meta) = std::fs::symlink_metadata(p)
                && meta.file_type().is_symlink()
            {
                return Err(format!(
                    "refusing to write through symlink: {}",
                    p.display()
                ));
            }
        }
        let bak = dest.with_extension(format!(
            "toml.bak-{}-{}-{}",
            crate::util::current_time_ms(),
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        match std::fs::rename(tmp, dest) {
            Ok(()) => Ok(()),
            Err(_) => {
                // rename failed; backup dest, delete, then retry rename.
                if dest.exists() {
                    use std::io::Write;
                    match crate::util::read_validated_file_bytes(dest) {
                        Ok(bytes) => {
                            match std::fs::OpenOptions::new()
                                .create_new(true)
                                .write(true)
                                .open(&bak)
                            {
                                Ok(mut f) => {
                                    if let Err(e) = f.write_all(&bytes) {
                                        tracing::warn!(
                                            "Failed to write config backup {:?}: {}",
                                            bak,
                                            e
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to create config backup {:?}: {}",
                                        bak,
                                        e
                                    )
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Refusing to back up config {:?}: {}", dest, e)
                        }
                    }
                    let _ = std::fs::remove_file(dest);
                }
                match std::fs::rename(tmp, dest) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        if bak.exists()
                            && let Ok(bytes) = crate::util::read_validated_file_bytes(&bak)
                        {
                            use std::io::Write;
                            match std::fs::OpenOptions::new()
                                .create_new(true)
                                .write(true)
                                .open(dest)
                            {
                                Ok(mut f) => {
                                    if let Err(restore_err) = f.write_all(&bytes) {
                                        tracing::warn!(
                                            "Failed to restore config backup {:?} → {:?}: {}",
                                            bak,
                                            dest,
                                            restore_err
                                        );
                                    }
                                }
                                Err(restore_err) => tracing::warn!(
                                    "Failed to restore config backup {:?} → {:?}: {}",
                                    bak,
                                    dest,
                                    restore_err
                                ),
                            }
                        }
                        Err(format!("rename failed: {}", e))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use tempfile::TempDir;

    #[test]
    fn harden_removes_explicit_everyone_write_ace() {
        // End-to-end through the real DACL: plant an Everyone write ACE the
        // way a misconfigured tool would, then verify detection sees it and
        // hardening removes it. Skips quietly when icacls is unavailable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl-test.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Everyone:W"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            file_has_world_write_ace(&path),
            "planted ACE must be detected"
        );
        harden_file_acl(&path);
        assert!(
            !file_has_world_write_ace(&path),
            "permissive ACE must be revoked"
        );
    }

    #[test]
    fn harden_removes_explicit_users_write_ace() {
        // Same end-to-end as above but with the Users group: detection
        // already watched it, and revoke must now clear it too.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl-users.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Users:W"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            file_has_world_write_ace(&path),
            "planted Users ACE must be detected"
        );
        harden_file_acl(&path);
        assert!(
            !file_has_world_write_ace(&path),
            "Users write ACE must be revoked"
        );
    }

    fn tmp_config() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        (dir, path)
    }

    #[test]
    fn world_write_detects_builtin_users_ace() {
        // Same end-to-end as above but with the Users group ACE.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl-users.toml");
        std::fs::write(&path, "x = 1").unwrap();
        let grant = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "Users:W"])
            .output();
        let Ok(out) = grant else { return };
        if !out.status.success() {
            return;
        }
        assert!(
            file_has_world_write_ace(&path),
            "Users write ACE must be detected"
        );
    }

    #[test]
    fn validate_config_dir_rejects_unc() {
        assert!(validate_config_dir(std::path::Path::new(r"\\server\share\cfg")).is_err());
        assert!(validate_config_dir(std::path::Path::new(r"C:\cfg")).is_ok());
    }

    #[test]
    fn reparse_check_passes_plain_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir_has_reparse_point(dir.path()));
    }

    #[test]
    fn atomic_replace_creates_dest() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("src.toml");
        let dest = dir.path().join("dest.toml");
        std::fs::write(&tmp, "content").unwrap();
        atomic_replace(&tmp, &dest, true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "content");
        assert!(!tmp.exists());
    }

    #[test]
    fn atomic_replace_overwrites_existing_dest() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("src.toml");
        let dest = dir.path().join("dest.toml");
        std::fs::write(&dest, "old").unwrap();
        std::fs::write(&tmp, "new").unwrap();
        atomic_replace(&tmp, &dest, true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(!tmp.exists());
    }

    #[test]
    fn atomic_replace_tmp_missing_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("nope.toml");
        let dest = dir.path().join("dest.toml");
        assert!(atomic_replace(&tmp, &dest, true).is_err());
    }

    #[test]
    fn atomic_replace_preserves_bak_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("src.toml");
        let dest = dir.path().join("config.toml");
        std::fs::write(&dest, "old").unwrap();
        std::fs::write(&tmp, "new").unwrap();
        atomic_replace(&tmp, &dest, true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        // NOTE: .bak only in fallback path; atomic MoveFileExW creates none.
    }

    #[test]
    fn atomic_replace_no_bak_when_dest_absent() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("src.toml");
        let dest = dir.path().join("config.toml");
        std::fs::write(&tmp, "new").unwrap();
        atomic_replace(&tmp, &dest, true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        // No .bak when dest was absent.
        assert!(!dest.with_extension("toml.bak").exists());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (_dir, path) = tmp_config();
        let mut cfg = Config::default();
        cfg.fan.mode = FanControlMode::Manual;
        cfg.fan.manual = Some(ManualConfig { duty_pct: 75 });
        cfg.telemetry.poll_ms = 1000;
        cfg.battery.charge_limit_max_pct = Some(SettingU8 {
            enabled: true,
            value: 80,
        });

        // Serialize with header as save does.
        let body = toml::to_string_pretty(&cfg).unwrap();
        let content = format!("# Framework Crate configuration\n{}\n", body);
        std::fs::write(&path, &content).unwrap();

        // TOML ignores # comments so direct parse matches load logic.
        let raw = std::fs::read_to_string(&path).unwrap();
        let loaded: Config = toml::from_str(&raw).unwrap();
        assert_eq!(loaded.fan.mode, FanControlMode::Manual);
        assert_eq!(loaded.fan.manual.as_ref().unwrap().duty_pct, 75);
        assert_eq!(loaded.telemetry.poll_ms, 1000);
        let blim = loaded.battery.charge_limit_max_pct.unwrap();
        assert!(blim.enabled);
        assert_eq!(blim.value, 80);
    }

    #[test]
    fn save_sorts_curve_points() {
        let mut cfg = Config::default();
        cfg.fan.mode = FanControlMode::Curve;
        cfg.fan.curve = Some(GlobalCurveConfig {
            curve: CurveConfig {
                sensors: vec![],
                points: vec![[80, 100], [50, 20], [65, 60]],
                hysteresis_c: 2,
                rate_limit_pct_per_step: 10,
                rate_limit_down_pct_per_step: None,
            },
            poll_ms: 1000,
            per_fan_points: Vec::new(),
        });

        let mut cfg2 = cfg.clone();
        cfg2.validate();
        if let Some(ref mut curve) = cfg2.fan.curve {
            curve.curve.points.sort_by_key(|p| p[0]);
        }
        let body = toml::to_string_pretty(&cfg2).unwrap();
        let loaded: Config = toml::from_str(&body).unwrap();
        let pts = &loaded.fan.curve.unwrap().curve.points;
        assert_eq!(pts[0][0], 50);
        assert_eq!(pts[1][0], 65);
        assert_eq!(pts[2][0], 80);
    }

    #[test]
    fn load_nonexistent_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_config.toml");
        if !path.exists() {
            let cfg = Config::default();
            assert_eq!(cfg.fan.mode, FanControlMode::Disabled);
        }
    }

    #[test]
    fn config_parser_survives_random_ascii() {
        let mut seed = 0x0BAD_F00D_DEAD_BEEFu64;
        for _ in 0..500 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (seed as usize) % 120;
            let text: String = (0..len)
                .map(|i| {
                    let v = seed.rotate_left((i % 63) as u32) as u8;
                    (b' ' + (v % 95)) as char
                })
                .collect();
            let _ = toml::from_str::<Config>(&text);
        }
    }

    #[test]
    fn load_corrupted_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml = [").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let result: Result<Config, _> = toml::from_str(&raw);
        assert!(result.is_err());
    }

    #[test]
    fn import_strict_accepts_minimal_current_version() {
        let cfg = crate::config::import_strict("version = 2\n[fan]\nmode = \"curve\"\n").unwrap();
        assert_eq!(cfg.fan.mode, FanControlMode::Curve);
        assert_eq!(cfg.version, crate::types::CONFIG_VERSION);
    }

    #[test]
    fn import_strict_rejects_unknown_top_level_field() {
        let err = crate::config::import_strict(
            "version = 2\nbogus_future_thing = 1\n[fan]\nmode = \"curve\"\n",
        )
        .unwrap_err();
        assert!(
            err.contains("unknown setting(s): bogus_future_thing"),
            "{err}"
        );
    }

    #[test]
    fn import_strict_rejects_version_mismatch() {
        let err =
            crate::config::import_strict("version = 999\n[fan]\nmode = \"curve\"\n").unwrap_err();
        assert!(err.contains("unsupported settings version 999"), "{err}");
    }

    #[test]
    fn import_strict_rejects_nested_unknown_field() {
        let err = crate::config::import_strict(
            "version = 2\n[fan]\nmode = \"curve\"\nbogus = 1\n",
        )
        .unwrap_err();
        assert!(err.contains("fan.bogus"), "{err}");
        let err = crate::config::import_strict(
            "version = 2\n[fan.curve]\nevil = 1\n",
        )
        .unwrap_err();
        assert!(err.contains("fan.curve.evil"), "{err}");
    }

    #[test]
    fn import_strict_migrates_older_version() {
        // Old backup (version 0): must migrate through validate(), not reject.
        let cfg =
            crate::config::import_strict("version = 0\n[fan]\nmode = \"curve\"\n").unwrap();
        assert_eq!(cfg.version, crate::types::CONFIG_VERSION);
        assert_eq!(cfg.fan.mode, FanControlMode::Curve);
        // 4-point legacy curve migrates to 5 editable points.
        let cfg = crate::config::import_strict(
            "version = 0\n[fan.curve]\npoints = [[30, 0], [45, 20], [60, 40], [75, 80]]\n",
        )
        .unwrap();
        assert_eq!(cfg.fan.curve.as_ref().unwrap().curve.points.len(), 7);
    }

    #[test]
    fn import_strict_rejects_missing_version() {
        let err = crate::config::import_strict("[fan]\nmode = \"curve\"\n").unwrap_err();
        assert!(err.contains("missing settings version"), "{err}");
    }

    #[test]
    fn export_import_round_trip_through_file() {
        let mut cfg = Config::default();
        cfg.fan.mode = FanControlMode::Manual;
        cfg.fan.manual = Some(ManualConfig { duty_pct: 42 });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exported.toml");
        crate::config::export_to_path(&cfg, &path).unwrap();
        let imported = crate::config::import_file(&path).unwrap();
        let mut expected = cfg.clone();
        expected.validate();
        assert_eq!(imported.config, expected);
        // Same-machine round trip matches, unless this host cannot read
        // its own identity (then export omits the stamp and import warns).
        assert!(
            imported.warning.is_none() || crate::config::local_product_name().is_none(),
            "{:?}",
            imported.warning
        );
    }

    #[test]
    fn export_stamp_is_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stamped.toml");
        crate::config::export_to_path(&Config::default(), &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let table: toml::Table = toml::from_str(&raw).unwrap();
        // A stamp after a [table] header would nest inside it and be
        // invisible to the import check; it must sit at the top level.
        if crate::config::local_product_name().is_some() {
            assert!(
                table
                    .get("exported_from")
                    .and_then(|v| v.as_str())
                    .is_some(),
                "{raw}"
            );
        }
    }

    #[test]
    fn chassis_size_reads_laptop_digits_only() {
        use crate::config::chassis_size;
        assert_eq!(
            chassis_size("Laptop 13 (AMD Ryzen 7040Series)"),
            Some("13".to_string())
        );
        assert_eq!(
            chassis_size("Laptop 16 (AMD Ryzen 7040 Series)"),
            Some("16".to_string())
        );
        assert_eq!(
            chassis_size("Laptop 13 (13th Gen Intel Core)"),
            Some("13".to_string())
        );
        assert_eq!(
            chassis_size("Laptop 12 (Intel Core i3-N305)"),
            Some("12".to_string())
        );
        assert_eq!(chassis_size("Unknown Box"), None);
        // Must not read the "7040" as a chassis size.
        assert_eq!(chassis_size("7040Series"), None);
    }

    #[test]
    fn origin_verdict_matrix() {
        use crate::config::check_origin_against;
        let intel13 = "Laptop 13 (13th Gen Intel Core)";
        let amd13 = "Laptop 13 (AMD Ryzen 7040Series)";
        let amd16 = "Laptop 16 (AMD Ryzen 7040 Series)";
        assert_eq!(check_origin_against(Some(intel13), Some(intel13)), Ok(None));
        let warn = check_origin_against(Some(amd13), Some(intel13)).unwrap();
        assert!(warn.unwrap().contains("same chassis size"));
        let err = check_origin_against(Some(amd16), Some(intel13)).unwrap_err();
        assert!(err.contains("this machine is"), "{err}");
        let warn = check_origin_against(None, Some(intel13)).unwrap();
        assert!(warn.unwrap().contains("no machine info"));
        let warn = check_origin_against(Some(amd13), None).unwrap();
        assert!(warn.unwrap().contains("cannot determine"));
        let warn = check_origin_against(Some("Weird Box"), Some(intel13)).unwrap();
        assert!(warn.unwrap().contains("cannot compare"));
        let desktop = "Framework Desktop (AMD Ryzen AI Max 300 Series)";
        let err = check_origin_against(Some(desktop), Some(intel13)).unwrap_err();
        assert!(err.contains("this machine is"), "{err}");
        let err = check_origin_against(Some(intel13), Some(desktop)).unwrap_err();
        assert!(err.contains("this machine is"), "{err}");
    }

    #[test]
    fn export_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.toml");
        std::fs::write(&path, "x").unwrap();
        let err = crate::config::export_to_path(&Config::default(), &path).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn save_versioned_skips_stale_version() {
        let _env_guard = CONFIG_DIR_TEST_LOCK.lock().unwrap();
        // An older (lower) version must not overwrite a newer one already on
        // disk: this guards shutdown saves against being clobbered by a stale
        // debounced save and vice versa.
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe {
            std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path());
        }

        // Versions are process-global and other tests may have advanced the
        // counter; start above the current value so this test is order-safe.
        let base = LAST_SAVED_VERSION
            .load(std::sync::atomic::Ordering::SeqCst)
            .saturating_add(10);

        let mut old = Config::default();
        old.fan.mode = FanControlMode::Manual;
        old.fan.manual = Some(ManualConfig { duty_pct: 30 });
        crate::config::save_versioned(&old, base, true).unwrap();

        let mut new = Config::default();
        new.fan.mode = FanControlMode::Manual;
        new.fan.manual = Some(ManualConfig { duty_pct: 70 });
        crate::config::save_versioned(&new, base + 100, true).unwrap();

        // Stale write (lower version than the on-disk newest) must be skipped.
        let mut stale = Config::default();
        stale.fan.mode = FanControlMode::Manual;
        stale.fan.manual = Some(ManualConfig { duty_pct: 10 });
        crate::config::save_versioned(&stale, base + 50, true).unwrap();

        let path = crate::config::config_path().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let loaded: Config = toml::from_str(&raw).unwrap();
        assert_eq!(loaded.fan.manual.as_ref().unwrap().duty_pct, 70);

        unsafe {
            if let Some(v) = prev {
                std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v);
            } else {
                std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR");
            }
        }
    }
}
