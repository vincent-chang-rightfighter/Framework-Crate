// Strict settings import/export with machine checks.
//
// Split from config.rs: unknown-field rejection, old-version migration,
// machine-stamp checks, and TOML rendering for export live here.
use crate::types::Config;

use super::io::render_config_toml;
use super::paths::dir_has_reparse_point;

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
    check(battery, "battery", &["charge_limit_max_pct"], &mut unknown);
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

#[cfg(test)]
mod tests {
    use crate::types::*;

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
        let err = crate::config::import_strict("version = 2\n[fan]\nmode = \"curve\"\nbogus = 1\n")
            .unwrap_err();
        assert!(err.contains("fan.bogus"), "{err}");
        let err = crate::config::import_strict("version = 2\n[fan.curve]\nevil = 1\n").unwrap_err();
        assert!(err.contains("fan.curve.evil"), "{err}");
    }

    #[test]
    fn import_strict_migrates_older_version() {
        // Old backup (version 0): must migrate through validate(), not reject.
        let cfg = crate::config::import_strict("version = 0\n[fan]\nmode = \"curve\"\n").unwrap();
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
        use super::check_origin_against;
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
}
