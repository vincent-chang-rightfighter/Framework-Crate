//! MSR power-limit write path and read-back verification.

use std::sync::atomic::{AtomicBool, Ordering};
use tracing::debug;

use super::super::bios::BiosDefaults;
use super::super::ffi::{PawnioHandle, exec_ioctl, open_handle};
use super::super::modules::load_intel_msr_blob;
use super::super::types::PowerLimitParams;
use super::decode::{
    Readback, classify_readback, encode_power_limit, encode_time_window, encoded_watts,
    readback_time_clamp_mismatch, readback_time_mismatch,
};
use super::validate::validate_limit_params;

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

static TIME_CLAMP_WARNED: AtomicBool = AtomicBool::new(false);

/// Encodes PL1/PL2, writes them to `target`, then verifies the write landed.
fn write_power_limits(target: &LimitTarget<'_>, params: &PowerLimitParams) -> Result<(), String> {
    validate_limit_params(params)?;
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

    if readback_time_mismatch(rb_out[0], pl1_enc, pl2_enc, params) {
        return Err(format!(
            "{} time window not reflected in read-back — firmware may be clamping it",
            target.label
        ));
    }

    if readback_time_clamp_mismatch(rb_out[0], pl1_enc, pl2_enc, params)
        && !TIME_CLAMP_WARNED.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(
            "{} time/clamp fields differ from the request; firmware may clamp the time window",
            target.label
        );
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
pub(in crate::cpu_power) fn write_msr_pl1_pl2(
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

/// Writes MSR 0x610 (opens IntelMSR handle internally).
pub fn write_msr_pl1_pl2_public(params: PowerLimitParams) -> Result<(), String> {
    let msr_blob = load_intel_msr_blob()?;
    let msr_handle = open_handle(&msr_blob)?;
    write_msr_pl1_pl2(&msr_handle, &params)
}

/// Restores the MSR halves of the persisted BIOS snapshot.
///
/// The official IntelMCHBAR module exposes only read ioctls, so the MMIO
/// mirror (MCHBAR+0x59A0) cannot be written without a custom module. Reset
/// therefore restores MSR only; the UI states this explicitly.
pub fn write_bios_defaults(bios: &BiosDefaults) -> Result<(), String> {
    let (power_unit, time_unit) = super::super::read::read_rapl_units()?;
    write_msr_pl1_pl2_public(PowerLimitParams {
        pl1_watts: bios.pl1_watts,
        pl1_enabled: bios.pl1_enabled,
        pl1_clamped: bios.pl1_clamped,
        pl1_time_s: bios.pl1_time_s,
        pl2_watts: bios.pl2_watts,
        pl2_enabled: bios.pl2_enabled,
        pl2_clamped: bios.pl2_clamped,
        pl2_time_s: bios.pl2_time_s,
        max_time_s: None,
        power_unit,
        time_unit,
    })
}
