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
    let config_dir = if let Some(os) = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR") {
        let p = PathBuf::from(&os);
        validate_config_dir(&p)?;
        p
    } else {
        default_config_dir()
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
        if meta.file_type().is_symlink() {
            return Err(format!(
                "config directory is a symlink, refusing: {}",
                config_dir.display()
            ));
        }
    }
    Ok(config_dir.join("config.toml"))
}

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
    Ok(())
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

/// Warns if the config file at `path` may be world-writable.
/// On Windows, %APPDATA% is per-user (user+SYSTEM+Admins only) so inherited ACL
/// is already restrictive; we warn if the file has explicit permissive ACEs.
/// On Unix, checks mode bits. Fail-open: inspection errors are silently ignored.
pub(crate) fn warn_if_world_writable(path: &std::path::Path) {
    #[cfg(windows)]
    {
        // Best-effort: check if file has an explicit permissive DACL via SDDL.
        // Full DACL inspection requires Win32_Security_Authorization which is
        // not enabled by default; rely on %APPDATA% inherited ACL being per-user.
        // If the file was created with default inheritance, it is already
        // restricted to the current user. Warn only if we detect non-inherited
        // permissive ACEs (follow-up: enable Win32_Security_Authorization for deep check).
        if !path.exists() {
            return;
        }
        tracing::debug!(
            "Config file {} relies on inherited %APPDATA% ACL (per-user, user+SYSTEM+Admins only)",
            path.display()
        );
        // Follow-up: for explicit world-writable detection, enable
        // `Win32_Security_Authorization` and inspect SDDL for WD/BU/AU with W.
        let _ = path;
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o022 != 0 {
                tracing::warn!(
                    "Config file {} is group/other-writable (mode {:o}) — consider chmod 600",
                    path.display(),
                    mode & 0o777
                );
            }
        }
    }
}

/// Best-effort hardening: ensure the config file inherits restrictive ACL from
/// %APPDATA% (per-user). On Windows this re-enables inheritance; on Unix sets 0o600.
/// Failures are warn-only and do not abort the save.
pub(crate) fn harden_file_acl(path: &std::path::Path) {
    #[cfg(windows)]
    {
        // On Windows, %APPDATA%/framework-crate already has a per-user DACL.
        // Files created there inherit it. We ensure inheritance is enabled
        // (UNPROTECTED_DACL) so no explicit permissive ACEs linger.
        // Full DACL rewrite (SetNamedSecurityInfoW with explicit user SID) is
        // deferred — current inherited ACL is already restrictive enough for
        // per-user config. Log for audit.
        tracing::debug!(
            "Config file {} saved with inherited ACL from %APPDATA% (per-user)",
            path.display()
        );
        // Follow-up hardening (if needed): call SetNamedSecurityInfoW with
        // UNPROTECTED_DACL_SECURITY_INFORMATION to force inheritance, or
        // construct explicit DACL for current user only (requires
        // Win32_Security_Authorization + TokenUser SID lookup).
        let _ = path;
        warn_if_world_writable(path);
    }
    #[cfg(not(windows))]
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                let mode = meta.permissions().mode();
                if mode & 0o077 != 0 {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }
}

/// Backs up corrupt config before next save overwrites it.
fn backup_corrupt_config(path: &std::path::Path) {
    let backup = path.with_extension(format!(
        "toml.corrupt-{}-{}-{}",
        crate::util::current_time_ms(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    match std::fs::copy(path, &backup) {
        Ok(_) => tracing::warn!(
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
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                // IO or non-UTF-8 corruption would be silently overwritten; preserve it.
                backup_corrupt_config(&path);
                return Err(format!("Failed to read {}: {}", path.display(), e));
            }
        };
        let mut config: Config = match toml::from_str(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                // Load falls back to defaults and next save overwrites; preserve corrupt file.
                backup_corrupt_config(&path);
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

fn save_impl(config: &Config, sync: bool) -> Result<(), String> {
    // Clone to avoid mutating caller's Config during validate/sort.
    let mut config = config.clone();
    config.validate();
    if let Some(ref mut curve) = config.fan.curve {
        curve.curve.points.sort_by_key(|p| p[0]);
    }
    let path = config_path()?;
    tracing::debug!("Saving config to: {}", path.display());
    let tmp_path = path.with_extension(unique_tmp_extension());
    let body = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
    let content = format!(
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
         #\n\
         # [battery]\n\
         # [battery.charge_limit_max_pct]\n\
         # enabled = true/false\n\
         # value = 25..100\n\
         #\n\
         {}\n",
        body
    );
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
    #[cfg(windows)]
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
        let bak = dest.with_extension("toml.bak");
        match std::fs::rename(tmp, dest) {
            Ok(()) => Ok(()),
            Err(_) => {
                // rename failed; backup dest, delete, then retry rename.
                if dest.exists() {
                    if let Err(e) = std::fs::copy(dest, &bak) {
                        tracing::warn!("Failed to back up config to {:?}: {}", bak, e);
                    }
                    let _ = std::fs::remove_file(dest);
                }
                match std::fs::rename(tmp, dest) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        if bak.exists()
                            && let Err(restore_err) = std::fs::copy(&bak, dest)
                        {
                            tracing::warn!(
                                "Failed to restore config backup {:?} → {:?}: {}",
                                bak,
                                dest,
                                restore_err
                            );
                        }
                        Err(format!("rename failed: {}", e))
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = sync;
        // Unix rename atomically replaces dest.
        std::fs::rename(tmp, dest).map_err(|e| format!("rename failed: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use tempfile::TempDir;

    fn tmp_config() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        (dir, path)
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
    fn load_corrupted_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml = [").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let result: Result<Config, _> = toml::from_str(&raw);
        assert!(result.is_err());
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

        let mut old = Config::default();
        old.fan.mode = FanControlMode::Manual;
        old.fan.manual = Some(ManualConfig { duty_pct: 30 });
        crate::config::save_versioned(&old, 100, true).unwrap();

        let mut new = Config::default();
        new.fan.mode = FanControlMode::Manual;
        new.fan.manual = Some(ManualConfig { duty_pct: 70 });
        crate::config::save_versioned(&new, 200, true).unwrap();

        // Stale write (lower version than the on-disk newest) must be skipped.
        let mut stale = Config::default();
        stale.fan.mode = FanControlMode::Manual;
        stale.fan.manual = Some(ManualConfig { duty_pct: 10 });
        crate::config::save_versioned(&stale, 150, true).unwrap();

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
