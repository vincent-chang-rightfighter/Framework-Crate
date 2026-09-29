//! MSR/MMIO power limit encoding, decoding and the write paths.

use tracing::debug;

use super::bios::BiosDefaults;
use super::ffi::{IoctlFailure, PawnioHandle, exec_ioctl, open_handle};
use super::modules::{load_intel_mchbar_blob, load_intel_msr_blob};

/// A register that CPU power limits are read from or written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerRegister {
    /// MSR 0x606, the RAPL power and time unit registers.
    Units,
    /// MSR 0x610, the static PL1/PL2 limits.
    Limits,
    /// MCHBAR+0x59A0, the package limit mirror.
    LimitsMirror,
}

/// Why the CPU Power card has no live values.
///
/// Split by cause because the two need different follow-up: a setup failure
/// means PawnIO, its DLL or a module blob is missing, while a read failure
/// means everything is installed but the register would not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuPowerUnavailable {
    /// PawnIO could not be brought up. The payload is the underlying reason.
    Setup(&'static str),
    /// A register read did not produce a usable value.
    Read {
        register: PowerRegister,
        reason: IoctlFailure,
    },
}

impl CpuPowerUnavailable {
    /// One line for the card, naming the register and why it would not answer.
    ///
    /// This deliberately does not tell the user what to do: the card already
    /// offers Install PawnIO or Download Modules based on the installation
    /// state, so the line only has to say what went wrong.
    pub fn message(self) -> &'static str {
        match self {
            Self::Setup(setup) => setup,
            Self::Read { register, reason } => match (register, reason) {
                (PowerRegister::Units, IoctlFailure::Failed) => {
                    "PawnIO rejected the read of MSR 0x606 (RAPL units)"
                }
                (PowerRegister::Units, IoctlFailure::ShortRead) => {
                    "MSR 0x606 read returned the wrong length, the IntelMSR module may be out of date"
                }
                (PowerRegister::Limits, IoctlFailure::Failed) => {
                    "PawnIO rejected the read of MSR 0x610 (PL1/PL2)"
                }
                (PowerRegister::Limits, IoctlFailure::ShortRead) => {
                    "MSR 0x610 read returned the wrong length, the IntelMSR module may be out of date"
                }
                (PowerRegister::LimitsMirror, IoctlFailure::Failed) => {
                    "PawnIO rejected the read of MCHBAR+0x59A0 (PL1/PL2)"
                }
                (PowerRegister::LimitsMirror, IoctlFailure::ShortRead) => {
                    "MCHBAR+0x59A0 read returned the wrong length, the IntelMCHBAR module may be out of date"
                }
            },
        }
    }
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
    /// Set whenever `available` is false; the UI renders `message()` from it.
    pub unavailable: Option<CpuPowerUnavailable>,
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

    /// The MSR limits as write parameters, ready to re-assert.
    ///
    /// MSR is the primary path: it is the register the sync thread re-asserts,
    /// so this is what callers pass back when they want to keep enforcing the
    /// values that are currently in effect.
    pub fn msr_limit_params(&self) -> PowerLimitParams {
        PowerLimitParams {
            pl1_watts: self.pl1_msr,
            pl1_enabled: self.pl1_msr_enabled,
            pl1_clamped: self.pl1_msr_clamped,
            pl1_time_s: self.pl1_time_s,
            pl2_watts: self.pl2_msr,
            pl2_enabled: self.pl2_msr_enabled,
            pl2_clamped: self.pl2_msr_clamped,
            pl2_time_s: self.pl2_time_s,
            power_unit: self.power_unit,
            time_unit: self.time_unit,
        }
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
pub(super) fn effective_limit(msr: f64, msr_en: bool, mmio: f64, mmio_en: bool) -> f64 {
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
pub(super) fn decode_power_limit(raw_val: u64, unit: f64) -> (f64, bool, bool, u32, u32) {
    let raw_bits = (raw_val & 0x7FFF) as f64;
    let enabled = (raw_val >> 15) & 1 == 1;
    let clamped = (raw_val >> 16) & 1 == 1;
    // Intel MSR 0x610: Y = 5 bits [21:17], Z = 2 bits [23:22].
    let time_y = ((raw_val >> 17) & 0x1F) as u32;
    let time_z = ((raw_val >> 22) & 0x3) as u32;
    (raw_bits * unit, enabled, clamped, time_y, time_z)
}

/// Decodes time window (2^Y * (1+Z/4) * Time_Unit).
pub(super) fn decode_time_window(y: u32, z: u32, time_unit: f64) -> f64 {
    (1u64 << y) as f64 * (1.0 + z as f64 / 4.0) * time_unit
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

/// The PL1/PL2 limits and the RAPL units they are expressed in.
///
/// Passed as one value rather than ten positional arguments: the write path
/// and the sync thread both need all ten, and a caller that has them as a
/// `CpuPowerInfo` should not have to unpack and repack them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerLimitParams {
    pub pl1_watts: f64,
    pub pl1_enabled: bool,
    pub pl1_clamped: bool,
    pub pl1_time_s: f64,
    pub pl2_watts: f64,
    pub pl2_enabled: bool,
    pub pl2_clamped: bool,
    pub pl2_time_s: f64,
    pub power_unit: f64,
    pub time_unit: f64,
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

/// A register that PL1/PL2 can be written to.
///
/// MSR 0x610 and MCHBAR+0x59A0 differ only in these values, so the encoding,
/// the read-back verification and the lock detection live in one place
/// instead of being maintained twice.
struct LimitTarget<'a> {
    handle: &'a PawnioHandle,
    address: u64,
    write_ioctl: &'static str,
    read_ioctl: &'static str,
    /// "MSR" or "MMIO". Prefixes the result messages so a failure names the
    /// register that was targeted.
    label: &'static str,
    /// Why the write ioctl failed. The two modules fail differently: the MSR
    /// blob is usually missing outright, while the MCHBAR blob may load fine
    /// and still not implement the qword write.
    write_failed: &'static str,
}

/// Encodes PL1/PL2, writes them to `target`, then verifies the write landed.
fn write_power_limits(target: &LimitTarget<'_>, params: &PowerLimitParams) -> Result<(), String> {
    if params.power_unit <= 0.0 || params.time_unit <= 0.0 {
        return Err("invalid RAPL units (power_unit or time_unit is zero)".to_string());
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
    inp[0] = target.address;
    inp[1] = new_val;
    let mut out2 = [0u64; 1];
    exec_ioctl(target.handle, target.write_ioctl, &inp, &mut out2)
        .map_err(|_| target.write_failed.to_string())?;

    // Verify the write landed by re-reading the register. The CPU silently
    // drops the write when the limit is BIOS-locked.
    let mut rb_out = [0u64; 1];
    exec_ioctl(
        target.handle,
        target.read_ioctl,
        &[target.address],
        &mut rb_out,
    )
    .map_err(|_| format!("{} write succeeded but read-back failed", target.label))?;

    if let Readback::Mismatch {
        pl1_watts,
        pl1_enabled,
        pl2_watts,
        pl2_enabled,
    } = classify_readback(rb_out[0], pl1_enc, pl2_enc, params)
    {
        debug!(
            "{} read-back mismatch: wrote PL1={:.2}W(en={}) PL2={:.2}W(en={}), read PL1={:.2}W(en={}) PL2={:.2}W(en={})",
            target.label,
            encoded_watts(pl1_enc, params.power_unit),
            params.pl1_enabled,
            encoded_watts(pl2_enc, params.power_unit),
            params.pl2_enabled,
            pl1_watts,
            pl1_enabled,
            pl2_watts,
            pl2_enabled
        );
        return Err(format!(
            "{} write not reflected in read-back — register may be locked",
            target.label
        ));
    }

    debug!(
        "{} PL1/PL2 written: PL1={:.1}W({:.1}s) PL2={:.1}W({:.1}s) raw=0x{:016X}",
        target.label,
        params.pl1_watts,
        params.pl1_time_s,
        params.pl2_watts,
        params.pl2_time_s,
        new_val
    );
    Ok(())
}

/// Writes MSR_PKG_POWER_LIMIT (0x610) via IntelMSR.
pub(super) fn write_msr_pl1_pl2(
    msr_handle: &PawnioHandle,
    params: &PowerLimitParams,
) -> Result<(), String> {
    write_power_limits(
        &LimitTarget {
            handle: msr_handle,
            address: 0x610, // MSR_PKG_POWER_LIMIT
            write_ioctl: "ioctl_write_msr",
            read_ioctl: "ioctl_read_msr",
            label: "MSR",
            write_failed: "ioctl_write_msr failed — is IntelMSR module loaded?",
        },
        params,
    )
}

/// Writes PACKAGE_POWER_LIMIT_MMIO (MCHBAR+0x59A0) via IntelMCHBAR.
pub(super) fn write_mmio_pl1_pl2(
    mchbar_handle: &PawnioHandle,
    params: &PowerLimitParams,
) -> Result<(), String> {
    write_power_limits(
        &LimitTarget {
            handle: mchbar_handle,
            address: 0x59A0,
            write_ioctl: "ioctl_write_qword",
            read_ioctl: "ioctl_read_qword",
            label: "MMIO",
            write_failed: "ioctl_write_qword failed — IntelMCHBAR module may not support MMIO write",
        },
        params,
    )
}

/// Writes MSR 0x610 (opens IntelMSR handle internally).
pub fn write_msr_pl1_pl2_public(params: PowerLimitParams) -> Result<(), String> {
    let msr_blob = load_intel_msr_blob()?;
    let msr_handle = open_handle(&msr_blob)?;
    write_msr_pl1_pl2(&msr_handle, &params)
}

fn write_mmio_pl1_pl2_public(params: PowerLimitParams) -> Result<(), String> {
    let mchbar_blob = load_intel_mchbar_blob()?;
    let mchbar_handle = open_handle(&mchbar_blob)?;
    write_mmio_pl1_pl2(&mchbar_handle, &params)
}

/// Outcome of restoring factory limits.
///
/// MMIO may be absent on older snapshots and some MCHBAR blobs lack qword
/// write support, so a failed MMIO restore must not fail the whole reset —
/// but it also must not be reported as full success.
#[derive(Debug, Clone, PartialEq)]
pub enum BiosRestore {
    /// MSR restored, and MMIO restored or absent from the snapshot.
    Full,
    /// MSR restored; the MMIO mirror may still hold the custom limit.
    Partial(String),
}

/// Writes both MSR and MMIO from `BiosDefaults` to restore factory state.
pub fn write_bios_defaults(bios: &BiosDefaults) -> Result<BiosRestore, String> {
    write_msr_pl1_pl2_public(PowerLimitParams {
        pl1_watts: bios.pl1_watts,
        pl1_enabled: bios.pl1_enabled,
        pl1_clamped: bios.pl1_clamped,
        pl1_time_s: bios.pl1_time_s,
        pl2_watts: bios.pl2_watts,
        pl2_enabled: bios.pl2_enabled,
        pl2_clamped: bios.pl2_clamped,
        pl2_time_s: bios.pl2_time_s,
        power_unit: bios.power_unit,
        time_unit: bios.time_unit,
    })?;
    // MMIO may be absent on older files; best-effort only, but reported.
    if (bios.pl1_mmio_watts > 0.0 || bios.pl2_mmio_watts > 0.0)
        && let Err(e) = write_mmio_pl1_pl2_public(PowerLimitParams {
            pl1_watts: bios.pl1_mmio_watts,
            pl1_enabled: bios.pl1_mmio_enabled,
            pl1_clamped: bios.pl1_mmio_clamped,
            pl1_time_s: bios.pl1_mmio_time_s,
            pl2_watts: bios.pl2_mmio_watts,
            pl2_enabled: bios.pl2_mmio_enabled,
            pl2_clamped: bios.pl2_mmio_clamped,
            pl2_time_s: bios.pl2_mmio_time_s,
            power_unit: bios.power_unit,
            time_unit: bios.time_unit,
        })
    {
        return Ok(BiosRestore::Partial(e));
    }
    Ok(BiosRestore::Full)
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

    #[test]
    fn unavailable_message_names_the_register_and_the_reason() {
        use super::{IoctlFailure, PowerRegister};
        for register in [
            PowerRegister::Units,
            PowerRegister::Limits,
            PowerRegister::LimitsMirror,
        ] {
            let failed = CpuPowerUnavailable::Read {
                register,
                reason: IoctlFailure::Failed,
            };
            let short = CpuPowerUnavailable::Read {
                register,
                reason: IoctlFailure::ShortRead,
            };
            assert_ne!(
                failed.message(),
                short.message(),
                "{register:?} must not report both failures the same way"
            );
            // A short read is a module-version problem, so the message has to
            // say so rather than blaming the driver.
            assert!(
                short.message().contains("module"),
                "short read for {register:?} should point at the module: {}",
                short.message()
            );
            assert!(
                !short.message().contains("rejected"),
                "short read for {register:?} is not a rejection: {}",
                short.message()
            );
        }
    }

    #[test]
    fn unavailable_message_passes_a_setup_reason_through() {
        assert_eq!(
            CpuPowerUnavailable::Setup("IntelMSR module blob missing").message(),
            "IntelMSR module blob missing"
        );
    }

    #[test]
    fn unavailable_messages_are_distinct() {
        use super::{IoctlFailure, PowerRegister};
        let all = [
            CpuPowerUnavailable::Setup("a"),
            CpuPowerUnavailable::Read {
                register: PowerRegister::Units,
                reason: IoctlFailure::Failed,
            },
            CpuPowerUnavailable::Read {
                register: PowerRegister::Units,
                reason: IoctlFailure::ShortRead,
            },
            CpuPowerUnavailable::Read {
                register: PowerRegister::Limits,
                reason: IoctlFailure::Failed,
            },
            CpuPowerUnavailable::Read {
                register: PowerRegister::Limits,
                reason: IoctlFailure::ShortRead,
            },
            CpuPowerUnavailable::Read {
                register: PowerRegister::LimitsMirror,
                reason: IoctlFailure::Failed,
            },
            CpuPowerUnavailable::Read {
                register: PowerRegister::LimitsMirror,
                reason: IoctlFailure::ShortRead,
            },
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a.message(), b.message(), "cases {i} and {j} collide");
                }
            }
        }
    }

    #[test]
    fn unavailable_always_carries_a_reason() {
        // The card falls back to its own wording when this is None, so an
        // unavailable result with no reason is a state the UI cannot explain.
        // On a machine without PawnIO this is the path that runs.
        let info = crate::cpu_power::read_cpu_power();
        if !info.available {
            assert!(
                info.unavailable.is_some(),
                "unavailable CPU power must carry a reason"
            );
        }
    }

    #[test]
    fn msr_limit_params_reads_the_msr_half_not_the_mmio_mirror() {
        // The two halves carry different values in the field. Reading the wrong
        // one writes limits the user never asked for, and the write path
        // verifies them, so nothing downstream would complain.
        let info = CpuPowerInfo {
            pl1_msr: 15.0,
            pl1_msr_enabled: true,
            pl1_msr_clamped: true,
            pl1_time_s: 28.0,
            pl2_msr: 35.0,
            pl2_msr_enabled: false,
            pl2_msr_clamped: true,
            pl2_time_s: 14.0,
            pl1_mmio: 99.0,
            pl1_mmio_enabled: true,
            pl1_mmio_clamped: false,
            pl1_mmio_time_s: 56.0,
            pl2_mmio: 77.0,
            pl2_mmio_enabled: true,
            pl2_mmio_clamped: false,
            pl2_mmio_time_s: 112.0,
            power_unit: 0.125,
            time_unit: 0.0009765625,
            ..Default::default()
        };
        let params = info.msr_limit_params();
        assert_eq!(params.pl1_watts, 15.0);
        assert!(params.pl1_enabled);
        assert!(params.pl1_clamped);
        assert_eq!(params.pl1_time_s, 28.0);
        assert_eq!(params.pl2_watts, 35.0);
        assert!(!params.pl2_enabled, "PL2 disabled must survive");
        assert!(params.pl2_clamped);
        assert_eq!(params.pl2_time_s, 14.0);
        assert_eq!(params.power_unit, 0.125);
        assert_eq!(params.time_unit, 0.0009765625);
    }

    #[test]
    fn msr_limit_params_of_an_unavailable_info_is_all_zero() {
        // A card that never read anything yields zeroed params rather than
        // stale ones; the write path rejects the zero unit, so this cannot
        // silently write nonsense.
        let info = CpuPowerInfo::default();
        let params = info.msr_limit_params();
        assert_eq!(params.pl1_watts, 0.0);
        assert_eq!(params.power_unit, 0.0);
        assert!(params.power_unit <= 0.0, "unit must not be faked");
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
}
