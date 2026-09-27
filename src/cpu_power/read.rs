//! Reading the current PL1/PL2 limits through the PawnIO modules.

use tracing::debug;

use super::ffi::{exec_ioctl, open_handle};
use super::limits::{CpuPowerInfo, decode_power_limit, decode_time_window};
use super::modules::{load_intel_mchbar_blob, load_intel_msr_blob};

/// Reads all CPU power info via PawnIO modules.
pub(super) fn read_cpu_power() -> CpuPowerInfo {
    let mut info = CpuPowerInfo::default();

    // Load handles independently; failure of one does not discard the other.
    let msr_handle = match load_intel_msr_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            info.error_msg = Some(e);
            None
        }
    };
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            if info.error_msg.is_none() {
                info.error_msg = Some(e);
            }
            None
        }
    };

    // Read MSR 0x606 for RAPL units; required to decode limits.
    let mut units_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_msr", &[0x606], &mut out).is_ok() {
            let raw = out[0];
            let unit_bits = (raw & 0xF) as u32;
            info.power_unit = 1.0 / (1u32 << unit_bits) as f64;
            let time_bits = ((raw >> 16) & 0xF) as u32;
            info.time_unit = 1.0 / (1u32 << time_bits) as f64;
            debug!(
                "Power unit: {} W/unit (bits={}), time unit: {} s/unit (bits={})",
                info.power_unit, unit_bits, info.time_unit, time_bits
            );
            units_ok = true;
        }
    }
    if !units_ok {
        // Keep earlier root-cause error instead of masking.
        if info.error_msg.is_none() {
            info.error_msg = Some("Failed to read MSR 0x606");
        }
        return info;
    }

    // Read MSR 0x610 for static PL1/PL2.
    let mut msr610_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_msr", &[0x610], &mut out).is_ok() {
            msr610_ok = true;
            let raw = out[0];
            debug!("MSR 0x610 raw: 0x{:016X}", raw);
            let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
            let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) =
                decode_power_limit(raw >> 32, info.power_unit);
            info.pl1_msr = pl1;
            info.pl1_msr_enabled = pl1_en;
            info.pl1_msr_clamped = pl1_cl;
            info.pl1_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
            info.pl2_msr = pl2;
            info.pl2_msr_enabled = pl2_en;
            info.pl2_msr_clamped = pl2_cl;
            info.pl2_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
            debug!(
                "MSR PL1: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s",
                pl1, pl1_en, pl1_cl, pl1_y, pl1_z, info.pl1_time_s
            );
            debug!(
                "MSR PL2: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s",
                pl2, pl2_en, pl2_cl, pl2_y, pl2_z, info.pl2_time_s
            );
        }
    }

    // Read MMIO at MCHBAR+0x59A0 via IntelMCHBAR.
    let mmio_offset = 0x59A0u64;
    let mut mmio_ok = false;
    if let Some(ref handle) = mchbar_handle {
        let mut out = [0u64; 1];
        if exec_ioctl(handle, "ioctl_read_qword", &[mmio_offset], &mut out).is_ok() {
            mmio_ok = true;
            let raw = out[0];
            let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
            let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) =
                decode_power_limit(raw >> 32, info.power_unit);
            info.pl1_mmio = pl1;
            info.pl1_mmio_enabled = pl1_en;
            info.pl1_mmio_clamped = pl1_cl;
            info.pl1_mmio_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
            info.pl2_mmio = pl2;
            info.pl2_mmio_enabled = pl2_en;
            info.pl2_mmio_clamped = pl2_cl;
            info.pl2_mmio_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
            debug!(
                "MMIO PL1: {:.1}W (en={} clamp={}) time={:.1}s",
                pl1, pl1_en, pl1_cl, info.pl1_mmio_time_s
            );
            debug!(
                "MMIO PL2: {:.1}W (en={} clamp={}) time={:.1}s",
                pl2, pl2_en, pl2_cl, info.pl2_mmio_time_s
            );
        }
    }

    // Both registers unreadable: report unavailable.
    if !msr610_ok && !mmio_ok {
        if info.error_msg.is_none() {
            info.error_msg = Some("Failed to read MSR 0x610 and MMIO power limits");
        }
        return info;
    }
    info.available = true;
    info
}
