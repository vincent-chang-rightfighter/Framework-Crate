//! MSR/MMIO power limit encoding, decoding and the write paths.

use tracing::debug;

use super::bios::BiosDefaults;
use super::modules::{load_intel_mchbar_blob, load_intel_msr_blob};
use super::{PawnioHandle, exec_ioctl, open_handle};

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
pub(super) fn write_msr_pl1_pl2(
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
pub(super) fn write_mmio_pl1_pl2(
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

#[cfg(test)]
mod tests {
    use super::*;

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
