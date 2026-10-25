//! BIOS power-limit defaults and the AC adapter snapshot.

use tracing::warn;

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

fn resolve_ac_present(snapshot: Option<bool>, has_battery: bool) -> Option<bool> {
    snapshot.or_else(|| (!has_battery).then_some(true))
}

/// Reads current AC-present state from shared snapshot.
pub fn read_ac_present() -> Option<bool> {
    resolve_ac_present(
        *BATTERY_AC_SNAPSHOT.read(),
        crate::cli::ec_wrapper::detect_platform().has_battery(),
    )
}

static BATTERY_AC_SNAPSHOT: parking_lot::RwLock<Option<bool>> = parking_lot::RwLock::new(None);

/// Publishes AC state for `read_ac_present`.
pub fn publish_ac_snapshot(ac_present: bool) {
    *BATTERY_AC_SNAPSHOT.write() = Some(ac_present);
}

/// Outcome of looking for the persisted BIOS factory snapshot.
///
/// The three "no defaults" outcomes lead to different decisions: only `Missing`
/// may be followed by capturing the live values. `Unusable` and `Unavailable`
/// both mean the factory limits must not be written over.
#[derive(Debug)]
pub(super) enum PersistedBios {
    /// Parsed successfully; the factory limits are here.
    Loaded(BiosDefaults),
    /// No file. First run, or the user deleted it to re-capture.
    Missing,
    /// Present but unparseable. Already copied aside, and must not be
    /// overwritten, or the factory limits are gone for good.
    Unusable,
    /// The config directory could not be resolved, so the file can be neither
    /// read nor written.
    Unavailable,
}

pub(super) fn bios_defaults_valid(d: &BiosDefaults) -> bool {
    // Restored values go straight into register encoding, so they get the
    // same finite/range guarantees as UI input. Zero watts is allowed for a
    // disabled limit, but an enabled limit must meet the 5W floor or the
    // CPU would hard-throttle; PL1 must not exceed PL2. Upper bounds match
    // the write boundary so a corrupt huge snapshot is quarantined.
    use super::limits::{
        LIMIT_MAX_TIME_S, LIMIT_MAX_WATTS, LIMIT_MIN_ENABLED_WATTS, platform_max_watts,
    };
    let watts = [d.pl1_watts, d.pl2_watts, d.pl1_mmio_watts, d.pl2_mmio_watts];
    let times = [
        d.pl1_time_s,
        d.pl2_time_s,
        d.pl1_mmio_time_s,
        d.pl2_mmio_time_s,
    ];
    let enabled = [
        d.pl1_enabled,
        d.pl2_enabled,
        d.pl1_mmio_enabled,
        d.pl2_mmio_enabled,
    ];
    let model_max = platform_max_watts();
    watts
        .iter()
        .all(|v| v.is_finite() && *v >= 0.0 && *v <= LIMIT_MAX_WATTS && *v <= model_max)
        && times
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0 && *v <= LIMIT_MAX_TIME_S)
        && super::limits::plausible_rapl_unit(d.power_unit)
        && super::limits::plausible_rapl_unit(d.time_unit)
        && watts
            .iter()
            .zip(enabled)
            .all(|(w, en)| !en || *w >= LIMIT_MIN_ENABLED_WATTS)
        && (!d.pl1_enabled || d.pl1_time_s > 0.0)
        && (!d.pl2_enabled || d.pl2_time_s > 0.0)
        && (!d.pl1_mmio_enabled || d.pl1_mmio_time_s > 0.0)
        && (!d.pl2_mmio_enabled || d.pl2_mmio_time_s > 0.0)
        && (!d.pl1_enabled || !d.pl2_enabled || d.pl1_watts <= d.pl2_watts)
        && (!d.pl1_mmio_enabled || !d.pl2_mmio_enabled || d.pl1_mmio_watts <= d.pl2_mmio_watts)
}

/// Backs up a corrupt or invalid snapshot aside and refuses to overwrite it.
fn quarantine_corrupt(path: &std::path::Path, reason: String) -> PersistedBios {
    let backup = path.with_extension("toml.corrupt");
    if backup.exists() {
        tracing::debug!(
            "bios_defaults.toml is still corrupt ({}); keeping existing backup {}",
            reason,
            backup.display()
        );
        return PersistedBios::Unusable;
    }
    let copied = (|| {
        let mut src = std::fs::File::open(path)?;
        let mut dst = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&backup)?;
        std::io::copy(&mut src, &mut dst)?;
        dst.sync_all()
    })();
    match copied {
        Ok(()) => warn!(
            "bios_defaults.toml is corrupt ({}); backed up to {} — NOT overwriting with live values. Delete the file to re-capture.",
            reason,
            backup.display()
        ),
        Err(be) => {
            let _ = std::fs::remove_file(&backup);
            warn!(
                "bios_defaults.toml is corrupt ({}) and backup failed: {}",
                reason, be
            );
        }
    }
    PersistedBios::Unusable
}

pub(super) fn load_persisted_bios_defaults() -> PersistedBios {
    let Ok(path) = bios_defaults_path() else {
        return PersistedBios::Unavailable;
    };
    if path.exists() {
        crate::config::warn_if_world_writable(&path);
    }
    let content = match crate::util::read_validated_file(&path) {
        Ok(c) => c,
        // Missing file: first run.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PersistedBios::Missing,
        Err(e) => {
            warn!(
                "bios_defaults.toml exists but cannot be read ({}); refusing to overwrite",
                e
            );
            return PersistedBios::Unusable;
        }
    };
    match toml::from_str::<BiosDefaults>(&content) {
        Ok(parsed) => {
            if bios_defaults_valid(&parsed) {
                PersistedBios::Loaded(parsed)
            } else {
                // Parses but cannot be encoded safely: same treatment as corrupt.
                quarantine_corrupt(&path, "values out of range".to_string())
            }
        }
        Err(e) => {
            // Corrupt file: back up and refuse to overwrite.
            quarantine_corrupt(&path, e.to_string())
        }
    }
}

pub(super) fn persist_bios_defaults(defaults: &BiosDefaults) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktops_report_ac_when_snapshot_is_unknown() {
        assert_eq!(resolve_ac_present(None, false), Some(true));
        assert_eq!(resolve_ac_present(None, true), None);
        assert_eq!(resolve_ac_present(Some(false), false), Some(false));
        assert_eq!(resolve_ac_present(Some(true), true), Some(true));
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
    fn missing_file_is_distinct_from_an_unusable_one() {
        // The two cases both mean "no defaults in hand" but only the first may
        // be followed by capturing live values. Collapsing them would let a
        // corrupt file be overwritten, destroying the factory limits it still
        // holds.
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };

        // Nothing written yet.
        assert!(
            matches!(load_persisted_bios_defaults(), PersistedBios::Missing),
            "an absent file must be Missing, not Unusable"
        );

        // A file that is not valid TOML for BiosDefaults.
        let path = dir.path().join("bios_defaults.toml");
        std::fs::write(&path, "this is not = valid = toml [[[").unwrap();
        assert!(
            matches!(load_persisted_bios_defaults(), PersistedBios::Unusable),
            "an unparseable file must be Unusable, not Missing"
        );
        // It must still be there: the corrupt copy is a backup, not a move.
        assert!(path.exists(), "the corrupt file must not be removed");

        if let Some(v) = prev {
            unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v) };
        } else {
            unsafe { std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR") };
        }
    }

    #[test]
    fn unreadable_file_is_not_treated_as_missing() {
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };

        let path = dir.path().join("bios_defaults.toml");
        std::fs::create_dir(&path).unwrap();
        assert!(
            matches!(load_persisted_bios_defaults(), PersistedBios::Unusable),
            "an existing but unreadable path must not be captured over"
        );
        assert!(path.is_dir(), "the unreadable path must be left in place");

        if let Some(v) = prev {
            unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v) };
        } else {
            unsafe { std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR") };
        }
    }

    #[test]
    fn invalid_values_are_quarantined_like_corrupt_files() {
        // Parses as TOML but must never reach the register encoder.
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };
        let path = dir.path().join("bios_defaults.toml");
        std::fs::write(
            &path,
            "pl1_watts = nan\npl1_enabled = true\npl1_clamped = false\npl1_time_s = 28.0\npl2_watts = 60.0\npl2_enabled = true\npl2_clamped = true\npl2_time_s = 2.5\npower_unit = 0.125\ntime_unit = 0.001\n",
        )
        .unwrap();
        assert!(
            matches!(load_persisted_bios_defaults(), PersistedBios::Unusable),
            "NaN watts must be Unusable, not Loaded"
        );
        assert!(path.exists(), "the invalid file must be kept, not removed");
        if let Some(v) = prev {
            unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v) };
        } else {
            unsafe { std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR") };
        }
    }

    #[test]
    fn zero_time_window_is_quarantined_when_enabled() {
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };
        let path = dir.path().join("bios_defaults.toml");
        std::fs::write(
            &path,
            "pl1_watts = 30.0\npl1_enabled = true\npl1_clamped = false\npl1_time_s = 0.0\npl2_watts = 60.0\npl2_enabled = true\npl2_clamped = true\npl2_time_s = 2.5\npower_unit = 0.125\ntime_unit = 0.001\n",
        )
        .unwrap();
        assert!(
            matches!(load_persisted_bios_defaults(), PersistedBios::Unusable),
            "enabled limit with a zero time window must be Unusable"
        );
        assert!(path.exists(), "the invalid file must be kept, not removed");
        if let Some(v) = prev {
            unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v) };
        } else {
            unsafe { std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR") };
        }
    }

    #[test]
    fn huge_values_are_quarantined_like_corrupt_files() {
        // Parses as TOML but exceeds the write-boundary caps; must never
        // reach the register encoder.
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };
        let path = dir.path().join("bios_defaults.toml");
        std::fs::write(
            &path,
            "pl1_watts = 99999.0\npl1_enabled = true\npl1_clamped = false\npl1_time_s = 28.0\npl2_watts = 60.0\npl2_enabled = true\npl2_clamped = true\npl2_time_s = 2.5\npower_unit = 0.125\ntime_unit = 0.001\n",
        )
        .unwrap();
        assert!(
            matches!(load_persisted_bios_defaults(), PersistedBios::Unusable),
            "over-cap watts must be Unusable, not Loaded"
        );
        assert!(path.exists(), "the invalid file must be kept, not removed");
        if let Some(v) = prev {
            unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v) };
        } else {
            unsafe { std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR") };
        }
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
        match load_persisted_bios_defaults() {
            PersistedBios::Loaded(loaded) => assert_eq!(defaults, loaded),
            other => panic!("expected Loaded, got {other:?}"),
        }
        // Second persist should overwrite (but init_bios_defaults would not call it if already loaded).
        // 35W passes per-model caps everywhere (min cap is 40W); the old
        // 99W fixture exceeds non-Desktop caps and is correctly quarantined.
        let defaults2 = BiosDefaults {
            pl1_watts: 35.0,
            ..defaults
        };
        persist_bios_defaults(&defaults2).unwrap();
        match load_persisted_bios_defaults() {
            PersistedBios::Loaded(loaded2) => assert_eq!(loaded2.pl1_watts, 35.0),
            other => panic!("expected Loaded, got {other:?}"),
        }
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v);
            } else {
                std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR");
            }
        }
    }
}

#[cfg(test)]
mod stable_backup_tests {
    use super::*;

    #[test]
    fn corrupt_backup_is_not_duplicated_on_reload() {
        let _env_guard = crate::config::CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR");
        unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", dir.path()) };

        let path = dir.path().join("bios_defaults.toml");
        std::fs::write(&path, "this is not = valid = toml [[[").unwrap();
        assert!(matches!(
            load_persisted_bios_defaults(),
            PersistedBios::Unusable
        ));
        assert!(matches!(
            load_persisted_bios_defaults(),
            PersistedBios::Unusable
        ));
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("bios_defaults.toml.corrupt")
            })
            .count();
        assert_eq!(backups, 1, "reloading must not create another backup");

        if let Some(v) = prev {
            unsafe { std::env::set_var("FRAMEWORK_CONTROL_CONFIG_DIR", v) };
        } else {
            unsafe { std::env::remove_var("FRAMEWORK_CONTROL_CONFIG_DIR") };
        }
    }
}
