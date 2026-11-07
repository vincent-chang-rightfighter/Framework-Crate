//! Register encoding/decoding and read-back verification.

use super::super::bios::BiosDefaults;
use super::super::types::{CpuPowerInfo, PowerLimitParams};

/// Whether a live readback differs from the persisted BIOS snapshot.
///
/// Used at startup to recover `pl_custom_applied` after an app restart: the
/// flag lives only in memory, but a custom MSR value can survive until the
/// next reboot. Tolerance follows the register quantization, not the user
/// request, so normal readback rounding is not mistaken for a custom write.
pub fn pl_limits_differ_from_bios(info: &CpuPowerInfo, bios: &BiosDefaults) -> bool {
    if !info.available {
        return false;
    }
    fn nearly(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }
    let tol_w = info.power_unit.max(0.25);
    let tol_t = info.time_unit.max(0.001) * 4.0;
    // The official IntelMCHBAR module is read-only, so the app never writes
    // the MMIO mirror. A mirror difference is firmware/EC behavior, not a
    // user custom write; only the MSR half is compared.
    !nearly(info.pl1_msr, bios.pl1_watts, tol_w)
        || info.pl1_msr_enabled != bios.pl1_enabled
        || info.pl1_msr_clamped != bios.pl1_clamped
        || !nearly(info.pl1_time_s, bios.pl1_time_s, tol_t)
        || !nearly(info.pl2_msr, bios.pl2_watts, tol_w)
        || info.pl2_msr_enabled != bios.pl2_enabled
        || info.pl2_msr_clamped != bios.pl2_clamped
        || !nearly(info.pl2_time_s, bios.pl2_time_s, tol_t)
}

/// Decodes power limit from raw 32-bit half.
pub(in crate::cpu_power) fn decode_power_limit(
    raw_val: u64,
    unit: f64,
) -> (f64, bool, bool, u32, u32) {
    let raw_bits = (raw_val & 0x7FFF) as f64;
    let enabled = (raw_val >> 15) & 1 == 1;
    let clamped = (raw_val >> 16) & 1 == 1;
    // Intel MSR 0x610: Y = 5 bits [21:17], Z = 2 bits [23:22].
    let time_y = ((raw_val >> 17) & 0x1F) as u32;
    let time_z = ((raw_val >> 22) & 0x3) as u32;
    (raw_bits * unit, enabled, clamped, time_y, time_z)
}

/// Decodes time window (2^Y * (1+Z/4) * Time_Unit).
pub(in crate::cpu_power) fn decode_time_window(y: u32, z: u32, time_unit: f64) -> f64 {
    (1u64 << y) as f64 * (1.0 + z as f64 / 4.0) * time_unit
}

/// Decodes MSR_PKG_POWER_INFO (0x614) time-window bounds in seconds.
/// Bits 54:48 and 62:56 are the max and min window; zero means unspecified.
pub(in crate::cpu_power) fn decode_time_window_bounds(raw: u64) -> (Option<f64>, Option<f64>) {
    let max = ((raw >> 48) & 0x7F) as f64;
    let min = ((raw >> 56) & 0x7F) as f64;
    ((max > 0.0).then_some(max), (min > 0.0).then_some(min))
}

/// Encodes time window into Y and Z fields.
pub(super) fn encode_time_window(time_s: f64, time_unit: f64) -> (u32, u32) {
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
pub(super) fn encode_power_limit(
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

/// Scales an encoded limit half back to watts.
///
/// The read-back is compared against this rather than against the raw request,
/// because the register can only hold a whole number of `power_unit` steps. If
/// the request were compared directly, every limit that is not an exact
/// multiple of the unit would look like a locked register.
pub(super) fn encoded_watts(encoded: u32, power_unit: f64) -> f64 {
    ((encoded & 0x7FFF) as f64) * power_unit
}

/// How far the read-back may differ from the request, in watts.
///
/// Floors at 0.25W so that a very small `power_unit` cannot turn ordinary
/// quantisation into a false lock detection.
pub(super) fn readback_tolerance(power_unit: f64) -> f64 {
    power_unit.max(0.25)
}

/// Whether a register read-back reflects the limits that were just written.
#[derive(Debug, PartialEq)]
pub(super) enum Readback {
    Matches,
    /// The write did not land. A BIOS-locked register keeps its previous
    /// value, and the enabled bit is cleared when a limit is disabled.
    Mismatch {
        pl1_watts: f64,
        pl1_enabled: bool,
        pl2_watts: f64,
        pl2_enabled: bool,
    },
}

/// Decides whether `rb_raw` reflects `pl1_enc` / `pl2_enc`.
///
/// This is the BIOS-lock detection: it is the only thing standing between a
/// silently ignored write and a UI that reports success, so it is kept pure
/// and tested directly rather than only exercised through a real handle.
pub(super) fn classify_readback(
    rb_raw: u64,
    pl1_enc: u32,
    pl2_enc: u32,
    params: &PowerLimitParams,
) -> Readback {
    let expected_pl1 = encoded_watts(pl1_enc, params.power_unit);
    let expected_pl2 = encoded_watts(pl2_enc, params.power_unit);
    let (rb_pl1, rb_pl1_en, ..) = decode_power_limit(rb_raw, params.power_unit);
    let (rb_pl2, rb_pl2_en, ..) = decode_power_limit(rb_raw >> 32, params.power_unit);
    let tolerance = readback_tolerance(params.power_unit);
    if (rb_pl1 - expected_pl1).abs() > tolerance
        || (rb_pl2 - expected_pl2).abs() > tolerance
        || rb_pl1_en != params.pl1_enabled
        || rb_pl2_en != params.pl2_enabled
    {
        Readback::Mismatch {
            pl1_watts: rb_pl1,
            pl1_enabled: rb_pl1_en,
            pl2_watts: rb_pl2,
            pl2_enabled: rb_pl2_en,
        }
    } else {
        Readback::Matches
    }
}

pub(super) fn readback_time_clamp_mismatch(
    rb_raw: u64,
    pl1_enc: u32,
    pl2_enc: u32,
    params: &PowerLimitParams,
) -> bool {
    let (_, _, rb1_cl, rb1_y, rb1_z) = decode_power_limit(rb_raw, params.power_unit);
    let (_, _, rb2_cl, rb2_y, rb2_z) = decode_power_limit(rb_raw >> 32, params.power_unit);
    let exp1_y = (pl1_enc >> 17) & 0x1F;
    let exp1_z = (pl1_enc >> 22) & 0x3;
    let exp2_y = (pl2_enc >> 17) & 0x1F;
    let exp2_z = (pl2_enc >> 22) & 0x3;
    rb1_cl != params.pl1_clamped
        || rb2_cl != params.pl2_clamped
        || rb1_y != exp1_y
        || rb1_z != exp1_z
        || rb2_y != exp2_y
        || rb2_z != exp2_z
}

/// A time-window mismatch that is safe to treat as a failed write: the request
/// is inside the firmware-declared maximum, so the firmware had no legitimate
/// reason to clamp it. Without 0x614 bounds, stay advisory.
pub(super) fn readback_time_mismatch(
    rb_raw: u64,
    pl1_enc: u32,
    pl2_enc: u32,
    params: &PowerLimitParams,
) -> bool {
    let Some(max_time) = params.max_time_s else {
        return false;
    };
    let (_, _, _, rb1_y, rb1_z) = decode_power_limit(rb_raw, params.power_unit);
    let (_, _, _, rb2_y, rb2_z) = decode_power_limit(rb_raw >> 32, params.power_unit);
    let exp1_y = (pl1_enc >> 17) & 0x1F;
    let exp1_z = (pl1_enc >> 22) & 0x3;
    let exp2_y = (pl2_enc >> 17) & 0x1F;
    let exp2_z = (pl2_enc >> 22) & 0x3;
    let exp1 = decode_time_window(exp1_y, exp1_z, params.time_unit);
    let exp2 = decode_time_window(exp2_y, exp2_z, params.time_unit);
    let rb1 = decode_time_window(rb1_y, rb1_z, params.time_unit);
    let rb2 = decode_time_window(rb2_y, rb2_z, params.time_unit);
    let tolerance = |expected: f64| (expected.abs() * 0.01).max(params.time_unit * 4.0);
    let in_range = |requested: f64| requested <= max_time + tolerance(requested);
    (params.pl1_enabled && in_range(params.pl1_time_s) && (rb1 - exp1).abs() > tolerance(exp1))
        || (params.pl2_enabled
            && in_range(params.pl2_time_s)
            && (rb2 - exp2).abs() > tolerance(exp2))
}

#[cfg(test)]
mod tests {
    use super::super::super::bios::BiosDefaults;
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

    /// Encodes a PL1/PL2 pair the way the write path does.
    fn encode_pair(p: &PowerLimitParams) -> (u32, u32) {
        let (pl1_y, pl1_z) = encode_time_window(p.pl1_time_s, p.time_unit);
        let (pl2_y, pl2_z) = encode_time_window(p.pl2_time_s, p.time_unit);
        (
            encode_power_limit(
                p.pl1_watts,
                p.pl1_enabled,
                p.pl1_clamped,
                p.power_unit,
                pl1_y,
                pl1_z,
            ),
            encode_power_limit(
                p.pl2_watts,
                p.pl2_enabled,
                p.pl2_clamped,
                p.power_unit,
                pl2_y,
                pl2_z,
            ),
        )
    }

    /// Packs an encoded pair into a raw 64-bit register the way the CPU would.
    fn pack_register(pl1_enc: u32, pl2_enc: u32) -> u64 {
        ((pl2_enc as u64) << 32) | (pl1_enc as u64)
    }

    #[test]
    fn readback_matches_when_the_register_reflects_the_write() {
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        let rb = pack_register(pl1_enc, pl2_enc);
        assert_eq!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Matches
        );
    }

    #[test]
    fn readback_time_clamp_check_catches_field_changes() {
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        assert!(!readback_time_clamp_mismatch(
            pack_register(pl1_enc, pl2_enc),
            pl1_enc,
            pl2_enc,
            &p
        ));

        let clamp_changed = pack_register(pl1_enc | (1 << 16), pl2_enc);
        assert!(readback_time_clamp_mismatch(
            clamp_changed,
            pl1_enc,
            pl2_enc,
            &p
        ));

        let y = (pl1_enc >> 17) & 0x1F;
        let z = (pl1_enc >> 22) & 0x3;
        let time_changed =
            encode_power_limit(p.pl1_watts, true, false, p.power_unit, y, (z + 1) & 0x3);
        let rb = pack_register(time_changed, pl2_enc);
        assert!(readback_time_clamp_mismatch(rb, pl1_enc, pl2_enc, &p));
    }

    #[test]
    fn decode_time_window_bounds_reads_614_fields() {
        let raw = (28u64 << 48) | (1u64 << 56);
        assert_eq!(decode_time_window_bounds(raw), (Some(28.0), Some(1.0)));
        assert_eq!(decode_time_window_bounds(0), (None, None));
        assert_eq!(decode_time_window_bounds(28u64 << 48), (Some(28.0), None));
    }

    #[test]
    fn strict_time_readback_only_fails_inside_the_firmware_range() {
        let mut p = params(28.0, 35.0);
        p.max_time_s = Some(30.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        assert!(!readback_time_mismatch(
            pack_register(pl1_enc, pl2_enc),
            pl1_enc,
            pl2_enc,
            &p
        ));

        let y = (pl1_enc >> 17) & 0x1F;
        let z = (pl1_enc >> 22) & 0x3;
        let time_changed =
            encode_power_limit(p.pl1_watts, true, false, p.power_unit, y, (z + 1) & 0x3);
        let rb = pack_register(time_changed, pl2_enc);
        assert!(readback_time_mismatch(rb, pl1_enc, pl2_enc, &p));

        let mut unknown_max = p;
        unknown_max.max_time_s = None;
        assert!(!readback_time_mismatch(rb, pl1_enc, pl2_enc, &unknown_max));

        let mut too_low_max = p;
        too_low_max.max_time_s = Some(10.0);
        assert!(!readback_time_mismatch(rb, pl1_enc, pl2_enc, &too_low_max));

        let mut disabled = p;
        disabled.pl1_enabled = false;
        assert!(!readback_time_mismatch(rb, pl1_enc, pl2_enc, &disabled));
    }

    #[test]
    fn readback_reports_a_locked_register_that_kept_its_old_value() {
        // What a BIOS-locked register does: the write is dropped and the
        // previous, different limits stay in place.
        let requested = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&requested);
        let (prev_pl1, prev_pl2) = encode_pair(&params(15.0, 20.0));
        match classify_readback(
            pack_register(prev_pl1, prev_pl2),
            pl1_enc,
            pl2_enc,
            &requested,
        ) {
            Readback::Mismatch {
                pl1_watts,
                pl2_watts,
                ..
            } => {
                assert!((pl1_watts - 15.0).abs() < 1e-9, "got {pl1_watts}");
                assert!((pl2_watts - 20.0).abs() < 1e-9, "got {pl2_watts}");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn readback_reports_a_locked_register_that_kept_the_value_but_flipped_enabled() {
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        // Clear PL1's enabled bit, which is what a register does when the
        // limit is refused while the wattage field is still accepted.
        let rb = pack_register(pl1_enc & !(1 << 15), pl2_enc);
        assert!(matches!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Mismatch { .. }
        ));
    }

    #[test]
    fn readback_reports_a_locked_register_with_only_the_pl2_enabled_bit_cleared() {
        // PL2 sits in the high half, so its enabled bit is bit 47 of the
        // register. PL1 is left untouched, so this only passes if the PL2
        // enabled comparison is actually performed.
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        let rb = pack_register(pl1_enc, pl2_enc & !(1 << 15));
        match classify_readback(rb, pl1_enc, pl2_enc, &p) {
            Readback::Mismatch {
                pl1_watts,
                pl2_enabled,
                ..
            } => {
                assert!(
                    (pl1_watts - 28.0).abs() < 1e-9,
                    "PL1 should still be the requested value, got {pl1_watts}"
                );
                assert!(!pl2_enabled, "PL2 enabled bit should read as cleared");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn readback_compares_against_the_encoded_value_not_the_request() {
        // The magnitude field is 15 bits, so the register cannot hold more
        // than 32767 steps; encode_power_limit clamps above that and the
        // register reads back the clamped value. Comparing the read-back
        // against the raw request would report a lock that does not exist.
        // Both halves are clamped so either comparison being wrong is caught.
        let mut p = params(28.0, 35.0);
        p.power_unit = 1.0; // 1W per step, so the ceiling is 32767W
        p.pl1_watts = 100_000.0;
        p.pl2_watts = 50_000.0;
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        assert_eq!(encoded_watts(pl1_enc, p.power_unit), 32767.0);
        assert_eq!(encoded_watts(pl2_enc, p.power_unit), 32767.0);
        let rb = pack_register(pl1_enc, pl2_enc);
        assert_eq!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Matches
        );
    }

    #[test]
    fn readback_tolerates_a_difference_exactly_at_the_tolerance() {
        // power_unit 0.125 floors the tolerance at 0.25W. A read-back exactly
        // 0.25W low is 222 steps, i.e. 27.75W, and must still be accepted:
        // the comparison is strictly greater-than, not greater-or-equal.
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        assert_eq!(readback_tolerance(p.power_unit), 0.25);
        let at_boundary = encode_power_limit(27.75, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(at_boundary, pl2_enc);
        assert_eq!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Matches
        );
        // One step further out (27.625W, 0.375W low) is a real difference.
        let past_boundary = encode_power_limit(27.625, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(past_boundary, pl2_enc);
        assert!(matches!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Mismatch { .. }
        ));
        // The same boundary applies to PL2, which is the high half: 34.75W is
        // exactly 0.25W below the 35.0W request and must still be accepted.
        let pl2_at_boundary = encode_power_limit(34.75, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(pl1_enc, pl2_at_boundary);
        assert_eq!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Matches
        );
        let pl2_past_boundary = encode_power_limit(34.625, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(pl1_enc, pl2_past_boundary);
        assert!(matches!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Mismatch { .. }
        ));
    }

    #[test]
    fn readback_tolerates_a_difference_inside_the_tolerance() {
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        // One unit (0.125W) below the requested PL1, well inside the 0.25W
        // floor.
        let nudged = encode_power_limit(27.875, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(nudged, pl2_enc);
        assert_eq!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Matches
        );
    }

    #[test]
    fn readback_tolerance_floors_at_a_quarter_watt() {
        // A very small power_unit must not make ordinary quantisation look
        // like a locked register.
        assert_eq!(readback_tolerance(0.125), 0.25);
        assert_eq!(readback_tolerance(0.0625), 0.25);
        assert_eq!(readback_tolerance(0.0), 0.25);
        // Above the floor the unit size is used as-is.
        assert_eq!(readback_tolerance(1.0), 1.0);
    }

    #[test]
    fn readback_flags_a_difference_outside_the_tolerance() {
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        // 27.0W is 1W below the request, far outside the 0.25W tolerance.
        let far_off = encode_power_limit(27.0, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(far_off, pl2_enc);
        assert!(matches!(
            classify_readback(rb, pl1_enc, pl2_enc, &p),
            Readback::Mismatch { .. }
        ));
    }

    #[test]
    fn readback_flags_a_pl2_difference_outside_the_tolerance() {
        // PL1 is exactly as requested, so this only passes if the PL2 wattage
        // comparison is actually performed.
        let p = params(28.0, 35.0);
        let (pl1_enc, pl2_enc) = encode_pair(&p);
        let far_off = encode_power_limit(20.0, true, false, POWER_UNIT, 0, 0);
        let rb = pack_register(pl1_enc, far_off);
        match classify_readback(rb, pl1_enc, pl2_enc, &p) {
            Readback::Mismatch {
                pl1_watts,
                pl2_watts,
                ..
            } => {
                assert!(
                    (pl1_watts - 28.0).abs() < 1e-9,
                    "PL1 should still match, got {pl1_watts}"
                );
                assert!((pl2_watts - 20.0).abs() < 1e-9, "got {pl2_watts}");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn encoded_watts_ignores_the_flag_bits() {
        // Only the 15-bit magnitude field carries watts; the enabled, clamped
        // and time-window bits must not leak into the value.
        let enc = encode_power_limit(28.0, true, true, POWER_UNIT, 5, 2);
        assert!((encoded_watts(enc, POWER_UNIT) - 28.0).abs() < 1e-9);
    }

    fn matching_bios() -> BiosDefaults {
        BiosDefaults {
            pl1_watts: 30.0,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 35.0,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            pl1_mmio_watts: 30.0,
            pl1_mmio_enabled: true,
            pl1_mmio_clamped: false,
            pl1_mmio_time_s: 28.0,
            pl2_mmio_watts: 35.0,
            pl2_mmio_enabled: true,
            pl2_mmio_clamped: false,
            pl2_mmio_time_s: 28.0,
            power_unit: 0.125,
            time_unit: 0.0009765625,
            captured_on_ac: true,
        }
    }

    fn matching_info(bios: &BiosDefaults) -> CpuPowerInfo {
        CpuPowerInfo {
            available: true,
            power_unit: bios.power_unit,
            time_unit: bios.time_unit,
            pl1_msr: bios.pl1_watts,
            pl1_msr_enabled: bios.pl1_enabled,
            pl1_msr_clamped: bios.pl1_clamped,
            pl1_time_s: bios.pl1_time_s,
            pl2_msr: bios.pl2_watts,
            pl2_msr_enabled: bios.pl2_enabled,
            pl2_msr_clamped: bios.pl2_clamped,
            pl2_time_s: bios.pl2_time_s,
            pl1_mmio: bios.pl1_mmio_watts,
            pl1_mmio_enabled: bios.pl1_mmio_enabled,
            pl1_mmio_clamped: bios.pl1_mmio_clamped,
            pl1_mmio_time_s: bios.pl1_mmio_time_s,
            pl2_mmio: bios.pl2_mmio_watts,
            pl2_mmio_enabled: bios.pl2_mmio_enabled,
            pl2_mmio_clamped: bios.pl2_mmio_clamped,
            pl2_mmio_time_s: bios.pl2_mmio_time_s,
            mmio_available: true,
            ..Default::default()
        }
    }

    #[test]
    fn live_limits_equal_bios_snapshot() {
        let bios = matching_bios();
        assert!(!pl_limits_differ_from_bios(&matching_info(&bios), &bios));
    }

    #[test]
    fn live_msr_change_is_detected_as_custom() {
        let bios = matching_bios();
        let mut info = matching_info(&bios);
        info.pl1_msr += 5.0;
        assert!(pl_limits_differ_from_bios(&info, &bios));
        let mut info = matching_info(&bios);
        info.pl1_time_s += 1.0;
        assert!(pl_limits_differ_from_bios(&info, &bios));
        let mut info = matching_info(&bios);
        info.pl2_msr_enabled = false;
        assert!(pl_limits_differ_from_bios(&info, &bios));
    }

    #[test]
    fn unavailable_info_is_not_treated_as_custom() {
        let bios = matching_bios();
        assert!(!pl_limits_differ_from_bios(
            &CpuPowerInfo {
                available: false,
                ..matching_info(&bios)
            },
            &bios
        ));
    }
}
