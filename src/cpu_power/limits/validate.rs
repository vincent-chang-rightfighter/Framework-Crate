//! RAPL unit and power-limit range validation.

use super::super::types::{
    LIMIT_MAX_TIME_S, LIMIT_MAX_WATTS, LIMIT_MIN_ENABLED_WATTS, PowerLimitParams,
};

/// RAPL units are 1/2^n with n in 0..=15, so the plausible range is narrow.
pub(in crate::cpu_power) fn plausible_rapl_unit(unit: f64) -> bool {
    unit.is_finite() && (1.0 / 32768.0..=1.0).contains(&unit)
}

/// Per-model PL ceiling (hard safety cap). 120W is the absolute ceiling
/// for Desktop; smaller chassis get lower caps so a typo can't cook the
/// VRM for minutes. Unknown models fall back to a safe 65W.
pub fn max_watts_for_family(family: crate::cli::ec_wrapper::PlatformFamily) -> f64 {
    match family {
        crate::cli::ec_wrapper::PlatformFamily::Laptop12 => 40.0,
        crate::cli::ec_wrapper::PlatformFamily::Laptop13 => 70.0,
        crate::cli::ec_wrapper::PlatformFamily::Laptop16 => 100.0,
        crate::cli::ec_wrapper::PlatformFamily::Desktop => 120.0,
        crate::cli::ec_wrapper::PlatformFamily::Unknown => 65.0,
    }
}

pub(in crate::cpu_power) fn platform_max_watts() -> f64 {
    // Detect once per validation (infrequent: UI apply, sync start,
    // BIOS load). No plumbing needed through every caller.
    max_watts_for_family(crate::cli::ec_wrapper::detect_platform())
}

/// Rejects non-finite, negative, or out-of-range limit values before encoding.
///
/// `NaN as u32` is 0, so without this gate a NaN would encode as a 0W
/// enabled limit and hard-throttle the CPU. Upper bound is a Framework-safe
/// cap (120W covers all current boards with headroom; 1024W would let a
/// typo cook the VRM): beyond the 15-bit RAPL field the encoder silently
/// clamps, which then surfaces as a confusing read-back mismatch. Zero
/// stays allowed for a disabled limit, but an enabled 0W limit is rejected
/// (it would hard-throttle to a hang). PL1 must not exceed PL2 (RAPL
/// behavior is undefined otherwise). Kept pure and unit-tested; this is
/// the last line of defense at the write boundary for UI, BIOS-restore,
/// and sync paths.
pub fn validate_limit_params(params: &PowerLimitParams) -> Result<(), String> {
    for (name, v) in [
        ("power_unit", params.power_unit),
        ("time_unit", params.time_unit),
        ("PL1 watts", params.pl1_watts),
        ("PL2 watts", params.pl2_watts),
        ("PL1 time", params.pl1_time_s),
        ("PL2 time", params.pl2_time_s),
    ] {
        if !v.is_finite() {
            return Err(format!("invalid {name}: not finite ({v})"));
        }
        if v < 0.0 {
            return Err(format!("invalid {name}: negative ({v})"));
        }
    }
    for (name, v) in [
        ("power_unit", params.power_unit),
        ("time_unit", params.time_unit),
    ] {
        if !plausible_rapl_unit(v) {
            return Err(format!("invalid {name}: implausible RAPL unit ({v})"));
        }
    }
    validate_limit_values(params)
}

/// Range and order rules for PL watts/time, shared by the write path and the
/// UI pre-check so the two verdicts cannot drift apart.
///
/// Units are intentionally excluded: they come from the live snapshot, and
/// the UI must stay usable before the first successful read (unit checks
/// stay in `validate_limit_params`, which always runs before encoding).
pub fn validate_limit_values(params: &PowerLimitParams) -> Result<(), String> {
    // Self-sufficient like the write path: the UI feeds snapshot-derived
    // PL2 time/units that may be garbage before the first read, so every
    // field this helper judges is finite/negative-checked here (idempotent
    // under `validate_limit_params`, which already checked them above).
    for (name, v) in [
        ("PL1 watts", params.pl1_watts),
        ("PL2 watts", params.pl2_watts),
        ("PL1 time", params.pl1_time_s),
        ("PL2 time", params.pl2_time_s),
    ] {
        if !v.is_finite() {
            return Err(format!("invalid {name}: not finite ({v})"));
        }
        if v < 0.0 {
            return Err(format!("invalid {name}: negative ({v})"));
        }
    }
    for (name, v) in [
        ("PL1 watts", params.pl1_watts),
        ("PL2 watts", params.pl2_watts),
    ] {
        if v > LIMIT_MAX_WATTS {
            return Err(format!("invalid {name}: exceeds {LIMIT_MAX_WATTS}W ({v})"));
        }
    }
    // Per-model ceiling: a 13" typo at 120W would cook the VRM long before
    // PROCHOT saves it. The absolute cap above is the Desktop ceiling.
    {
        let model_max = platform_max_watts();
        for (name, v) in [
            ("PL1 watts", params.pl1_watts),
            ("PL2 watts", params.pl2_watts),
        ] {
            if v > model_max {
                return Err(format!(
                    "invalid {name}: exceeds this model ({v}W > {model_max}W)"
                ));
            }
        }
    }
    for (name, v, enabled) in [
        ("PL1 time", params.pl1_time_s, params.pl1_enabled),
        ("PL2 time", params.pl2_time_s, params.pl2_enabled),
    ] {
        if v > LIMIT_MAX_TIME_S {
            return Err(format!("invalid {name}: exceeds {LIMIT_MAX_TIME_S}s ({v})"));
        }
        if enabled && v <= 0.0 {
            return Err(format!(
                "invalid {name}: enabled limit requires a positive time window ({v})"
            ));
        }
    }
    // An enabled 0W (or near-zero) limit hard-throttles the CPU into an
    // apparent hang; require a floor for enabled limits.
    if params.pl1_enabled && params.pl1_watts < LIMIT_MIN_ENABLED_WATTS {
        return Err(format!(
            "invalid PL1 watts: enabled limit must be >= {LIMIT_MIN_ENABLED_WATTS}W ({}W)",
            params.pl1_watts
        ));
    }
    if params.pl2_enabled && params.pl2_watts < LIMIT_MIN_ENABLED_WATTS {
        return Err(format!(
            "invalid PL2 watts: enabled limit must be >= {LIMIT_MIN_ENABLED_WATTS}W ({}W)",
            params.pl2_watts
        ));
    }
    if params.pl1_enabled && params.pl2_enabled && params.pl1_watts > params.pl2_watts {
        return Err(format!(
            "invalid PL1/PL2: PL1 ({}W) must not exceed PL2 ({}W)",
            params.pl1_watts, params.pl2_watts
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.125W per RAPL power unit step, the value the About panel reads back
    /// from the CPU. 0.9765625us per time unit step.
    const POWER_UNIT: f64 = 0.125;
    const TIME_UNIT: f64 = 0.0009765625;

    fn params(pl1_watts: f64, pl2_watts: f64) -> PowerLimitParams {
        PowerLimitParams {
            pl1_watts,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            max_time_s: None,
            power_unit: POWER_UNIT,
            time_unit: TIME_UNIT,
        }
    }

    #[test]
    fn laptop12_core3_keeps_the_laptop12_watt_cap() {
        use crate::cli::ec_wrapper::PlatformFamily;
        assert_eq!(max_watts_for_family(PlatformFamily::Laptop12), 40.0);
        assert_eq!(max_watts_for_family(PlatformFamily::Laptop13), 70.0);
        assert_eq!(max_watts_for_family(PlatformFamily::Laptop16), 100.0);
        assert_eq!(max_watts_for_family(PlatformFamily::Desktop), 120.0);
        assert_eq!(max_watts_for_family(PlatformFamily::Unknown), 65.0);
    }

    #[test]
    fn validate_limit_params_rejects_non_finite_and_negative() {
        // Baseline 30/35W passes on every model (min per-model cap is 40W).
        assert!(validate_limit_params(&params(30.0, 35.0)).is_ok());
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut p = params(30.0, 35.0);
            p.pl1_watts = bad;
            assert!(validate_limit_params(&p).is_err(), "pl1={bad}");
            let mut p = params(30.0, 35.0);
            p.pl2_time_s = bad;
            assert!(validate_limit_params(&p).is_err(), "pl2_time={bad}");
        }
        let mut p = params(30.0, 35.0);
        p.power_unit = f64::NAN;
        assert!(validate_limit_params(&p).is_err());
    }

    #[test]
    fn validate_limit_params_rejects_over_ui_caps() {
        let mut p = params(30.0, 35.0);
        p.pl1_watts = LIMIT_MAX_WATTS + 1.0;
        assert!(validate_limit_params(&p).is_err());
        let mut p = params(30.0, 35.0);
        p.pl2_watts = LIMIT_MAX_WATTS + 1.0;
        assert!(validate_limit_params(&p).is_err());
        let mut p = params(30.0, 35.0);
        p.pl1_time_s = LIMIT_MAX_TIME_S + 1.0;
        assert!(validate_limit_params(&p).is_err());
        let mut p = params(30.0, 35.0);
        p.pl2_time_s = LIMIT_MAX_TIME_S + 1.0;
        assert!(validate_limit_params(&p).is_err());
        // Zero stays allowed for a disabled limit.
        let mut p = params(30.0, 35.0);
        p.pl1_watts = 0.0;
        p.pl1_enabled = false;
        assert!(validate_limit_params(&p).is_ok());
    }

    #[test]
    fn limit_values_ignores_units_but_params_does_not() {
        // Same watts/time verdict with zeroed units: the UI must stay usable
        // before the first successful RAPL read, while the write path still
        // refuses to encode without plausible units.
        let mut p = params(30.0, 35.0);
        p.power_unit = 0.0;
        p.time_unit = 0.0;
        assert!(validate_limit_values(&p).is_ok());
        assert!(validate_limit_params(&p).is_err());
    }

    #[test]
    fn limit_values_matches_params_verdict_when_units_valid() {
        // With sane units the two verdicts coincide, so tightening one side
        // cannot silently reopen the other.
        let watts = [
            0.0,
            -1.0,
            1.0,
            4.9,
            5.0,
            30.0,
            119.0,
            120.0,
            121.0,
            1000.0,
            f64::NAN,
            f64::INFINITY,
        ];
        let times = [0.0, -1.0, 28.0, 120.0, 121.0, f64::NAN];
        for &pl1 in &watts {
            for &pl2 in &watts {
                for &t1 in &times {
                    for enabled in [false, true] {
                        let p = PowerLimitParams {
                            pl1_watts: pl1,
                            pl1_enabled: enabled,
                            pl1_clamped: false,
                            pl1_time_s: t1,
                            pl2_watts: pl2,
                            pl2_enabled: enabled,
                            pl2_clamped: false,
                            pl2_time_s: 28.0,
                            max_time_s: None,
                            power_unit: POWER_UNIT,
                            time_unit: TIME_UNIT,
                        };
                        assert_eq!(
                            validate_limit_values(&p).is_ok(),
                            validate_limit_params(&p).is_ok(),
                            "diverged for {p:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn validate_limit_params_allows_time_above_reported_firmware_max() {
        // Some firmware reports a smaller 0x614 max than the BIOS itself uses
        // (18s reported while the factory PL1 window is 28s), so the field is
        // not a hard write bound. It only gates strict read-back checks.
        let mut p = params(30.0, 35.0);
        p.max_time_s = Some(18.0);
        assert!(validate_limit_params(&p).is_ok());
    }

    #[test]
    fn enabled_limits_require_a_positive_time_window() {
        let mut p = params(30.0, 35.0);
        p.pl1_time_s = 0.0;
        assert!(validate_limit_params(&p).is_err());
        let mut p = params(30.0, 35.0);
        p.pl2_time_s = 0.0;
        assert!(validate_limit_params(&p).is_err());
        let mut p = params(30.0, 35.0);
        p.pl1_time_s = 0.0;
        p.pl1_enabled = false;
        assert!(validate_limit_params(&p).is_ok());
        let mut p = params(30.0, 35.0);
        p.pl2_time_s = 0.0;
        p.pl2_enabled = false;
        assert!(validate_limit_params(&p).is_ok());
    }

    #[test]
    fn implausible_rapl_units_are_rejected() {
        for bad in [0.0, 1e-9, 1.5, 2.0] {
            let mut p = params(30.0, 35.0);
            p.power_unit = bad;
            assert!(validate_limit_params(&p).is_err(), "power_unit={bad}");
            let mut p = params(30.0, 35.0);
            p.time_unit = bad;
            assert!(validate_limit_params(&p).is_err(), "time_unit={bad}");
        }
    }
}
