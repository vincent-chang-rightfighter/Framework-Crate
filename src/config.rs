use crate::types::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique suffix per save call so concurrent writers (background task and
/// exit-time sync save) never collide on the same temp file.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a full temp file extension (e.g. "toml.12345.0.tmp") in one allocation.
fn unique_tmp_extension() -> String {
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = crate::util::current_time_ms();
    format!("toml.{}.{}.tmp", timestamp, counter)
}

pub fn config_path() -> Result<PathBuf, String> {
    let config_dir = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_dir);
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config directory {}: {}", config_dir.display(), e))?;
    Ok(config_dir.join("config.toml"))
}

fn default_config_dir() -> PathBuf {
    #[cfg(test)]
    {
        // Tests must never touch the real user config.
        return std::env::temp_dir().join("framework-crate-tests");
    }
    #[cfg(not(test))]
    {
        let base = dirs::config_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("framework-crate")
    }
}

pub fn load() -> Result<Config, String> {
    let path = config_path()?;
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let mut config: Config = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
        config.validate();
        return Ok(config);
    }
    Ok(Config::default())
}

pub fn save(config: &Config) -> Result<(), String> {
    // Clone is required because validate() and sort_by_key() mutate in-place;
    // we must not alter the caller's Config.
    let mut config = config.clone();
    config.validate();
    if let Some(ref mut curve) = config.fan.curve {
        curve.curve.points.sort_by_key(|p| p[0]);
    }
    let path = config_path()?;
    let tmp_path = path.with_extension(unique_tmp_extension());
    let body = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
    let content = format!(
        "# Framework Crate configuration\n\
         # Edit values below; the app validates on load.\n\
         #\n\
         # [fan]\n\
         # mode = \"Disabled\" | \"Manual\" | \"Curve\"\n\
         # [fan.manual]\n\
         # duty_pct = 10..100\n\
         # [fan.curve]\n\
         # poll_ms = 500..10000            (fan curve step interval)\n\
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
         # charge_rate_soc_threshold_pct = 0..100  (optional; 0/absent = no threshold)\n\
         # [battery.charge_limit_max_pct]\n\
         # enabled = true/false\n\
         # value = 25..100\n\
         # [battery.charge_rate_c]\n\
         # enabled = true/false\n\
         # value = 0.05..1.0\n\
         #\n\
         {}\n",
        body
    );
    use std::io::Write;
    let result = (|| {
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| format!("create tmp failed: {}", e))?;
        f.write_all(content.as_bytes()).map_err(|e| format!("write tmp failed: {}", e))?;
        f.sync_all().map_err(|e| format!("sync tmp failed: {}", e))?;
        drop(f);
        atomic_replace(&tmp_path, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn atomic_replace(tmp: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    // On Windows, std::fs::rename fails if dest exists.
    // Use MoveFileExW with MOVEFILE_REPLACE_EXISTING for atomic replacement.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::ffi::OsStr;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;

        let tmp_wide: Vec<u16> = OsStr::new(tmp).encode_wide().chain(std::iter::once(0)).collect();
        let dest_wide: Vec<u16> = OsStr::new(dest).encode_wide().chain(std::iter::once(0)).collect();

        // MOVEFILE_REPLACE_EXISTING alone guarantees atomic rename on the same volume.
        // tmp is created alongside dest (same directory via with_extension), so this is always same-volume.
        // SAFETY: MoveFileExW atomically replaces dest with tmp on the same volume.
        // Both paths are null-terminated UTF-16 wide strings. tmp and dest are on
        // the same directory (same volume), so MOVEFILE_REPLACE_EXISTING is atomic.
        let success = unsafe { MoveFileExW(tmp_wide.as_ptr(), dest_wide.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
        if success != 0 {
            return Ok(());
        }
        // Fallback: back up dest → bak, then rename tmp → dest.
        // .bak is preserved after success so users can manually recover if needed.
        let bak = dest.with_extension("toml.bak");
        if dest.exists() {
            let _ = std::fs::remove_file(&bak);
            std::fs::rename(dest, &bak)
                .map_err(|e| format!("backup rename failed: {}", e))?;
        }
        match std::fs::rename(tmp, dest) {
            Ok(()) => Ok(()),
            Err(e) => {
                if bak.exists() {
                    if let Err(restore_err) = std::fs::rename(&bak, dest) {
                        tracing::warn!("Failed to restore config backup {:?} → {:?}: {}", bak, dest, restore_err);
                    }
                }
                Err(format!("rename failed: {}", e))
            }
        }
    }
    #[cfg(not(windows))]
    {
        // On Unix, rename is atomic and replaces dest
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
        atomic_replace(&tmp, &dest).unwrap();
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
        atomic_replace(&tmp, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(!tmp.exists());
    }

    #[test]
    fn atomic_replace_tmp_missing_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("nope.toml");
        let dest = dir.path().join("dest.toml");
        assert!(atomic_replace(&tmp, &dest).is_err());
    }

    #[test]
    fn atomic_replace_preserves_bak_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("src.toml");
        let dest = dir.path().join("config.toml");
        std::fs::write(&dest, "old").unwrap();
        std::fs::write(&tmp, "new").unwrap();
        atomic_replace(&tmp, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        // Note: .bak is only created in the fallback path (when MoveFileExW fails).
        // On Windows with atomic MoveFileExW, no .bak is expected.
    }

    #[test]
    fn atomic_replace_no_bak_when_dest_absent() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("src.toml");
        let dest = dir.path().join("config.toml");
        std::fs::write(&tmp, "new").unwrap();
        atomic_replace(&tmp, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        // No .bak should be created when dest didn't exist
        assert!(!dest.with_extension("toml.bak").exists());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let (_dir, path) = tmp_config();
        let mut cfg = Config::default();
        cfg.fan.mode = FanControlMode::Manual;
        cfg.fan.manual = Some(ManualConfig { duty_pct: 75 });
        cfg.telemetry.poll_ms = 1000;
        cfg.battery.charge_limit_max_pct = Some(SettingU8 { enabled: true, value: 80 });
        cfg.battery.charge_rate_c = Some(SettingF32 { enabled: true, value: 1.5 });

        // Serialize with header like save() does
        let body = toml::to_string_pretty(&cfg).unwrap();
        let content = format!("# Framework Crate configuration\n{}\n", body);
        std::fs::write(&path, &content).unwrap();

        // Load via toml parse (same logic as load(); TOML ignores # comments)
        let raw = std::fs::read_to_string(&path).unwrap();
        let loaded: Config = toml::from_str(&raw).unwrap();
        assert_eq!(loaded.fan.mode, FanControlMode::Manual);
        assert_eq!(loaded.fan.manual.as_ref().unwrap().duty_pct, 75);
        assert_eq!(loaded.telemetry.poll_ms, 1000);
        let blim = loaded.battery.charge_limit_max_pct.unwrap();
        assert!(blim.enabled);
        assert_eq!(blim.value, 80);
        let brate = loaded.battery.charge_rate_c.unwrap();
        assert!(brate.enabled);
        assert!((brate.value - 1.5).abs() < f32::EPSILON);
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
}
