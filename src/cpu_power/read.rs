//! Reading the current PL1/PL2 limits through the PawnIO modules.

use tracing::debug;

use super::ffi::{exec_ioctl, open_handle};
use super::limits::{
    CpuPowerInfo, CpuPowerUnavailable, PowerRegister, decode_power_limit, decode_time_window,
};
use super::modules::{load_intel_mchbar_blob, load_intel_msr_blob};

/// Records the first cause of failure, so a later, more generic one cannot
/// mask the root cause.
fn set_first_failure(info: &mut CpuPowerInfo, cause: CpuPowerUnavailable) {
    if info.unavailable.is_none() {
        info.unavailable = Some(cause);
    }
}

/// Reads all CPU power info via PawnIO modules.
pub(super) fn read_cpu_power() -> CpuPowerInfo {
    let mut info = CpuPowerInfo::default();

    // Load handles independently; failure of one does not discard the other.
    let msr_handle = match load_intel_msr_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            set_first_failure(&mut info, CpuPowerUnavailable::Setup(e));
            None
        }
    };
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            set_first_failure(&mut info, CpuPowerUnavailable::Setup(e));
            None
        }
    };

    // Read MSR 0x606 for RAPL units; required to decode limits.
    let mut units_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x606], &mut out) {
            Ok(_) => {
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
            Err(reason) => set_first_failure(
                &mut info,
                CpuPowerUnavailable::Read {
                    register: PowerRegister::Units,
                    reason,
                },
            ),
        }
    }
    if !units_ok {
        return info;
    }

    // Read MSR 0x610 for static PL1/PL2.
    let mut msr610_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x610], &mut out) {
            Ok(_) => {
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
            Err(reason) => set_first_failure(
                &mut info,
                CpuPowerUnavailable::Read {
                    register: PowerRegister::Limits,
                    reason,
                },
            ),
        }
    }

    // Read MMIO at MCHBAR+0x59A0 via IntelMCHBAR.
    let mmio_offset = 0x59A0u64;
    let mut mmio_ok = false;
    if let Some(ref handle) = mchbar_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_qword", &[mmio_offset], &mut out) {
            Ok(_) => {
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
            Err(reason) => set_first_failure(
                &mut info,
                CpuPowerUnavailable::Read {
                    register: PowerRegister::LimitsMirror,
                    reason,
                },
            ),
        }
    }

    // Nothing was readable, so the card has no live values. The cause recorded
    // above already says which register refused and why; do not overwrite it
    // with a summary that loses both.
    if !msr610_ok && !mmio_ok {
        return info;
    }
    info.available = true;
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_power::ffi::IoctlFailure;

    #[test]
    fn set_first_failure_keeps_the_earliest_cause() {
        // The order of the calls in read_cpu_power is the point: the first
        // failure is the root cause and the later, more generic ones must not
        // mask it. Both setup and read failures can arrive, and whichever
        // comes first wins.
        let mut info = CpuPowerInfo::default();
        set_first_failure(
            &mut info,
            CpuPowerUnavailable::Setup("IntelMSR blob missing"),
        );
        set_first_failure(
            &mut info,
            CpuPowerUnavailable::Read {
                register: PowerRegister::Limits,
                reason: IoctlFailure::Failed,
            },
        );
        assert_eq!(
            info.unavailable,
            Some(CpuPowerUnavailable::Setup("IntelMSR blob missing"))
        );
    }

    #[test]
    fn set_first_failure_records_when_nothing_failed_before() {
        let mut info = CpuPowerInfo::default();
        assert!(info.unavailable.is_none());
        set_first_failure(
            &mut info,
            CpuPowerUnavailable::Read {
                register: PowerRegister::Units,
                reason: IoctlFailure::ShortRead,
            },
        );
        assert_eq!(
            info.unavailable,
            Some(CpuPowerUnavailable::Read {
                register: PowerRegister::Units,
                reason: IoctlFailure::ShortRead,
            })
        );
    }

    #[test]
    fn an_available_result_carries_no_reason() {
        // Nothing in read_cpu_power records a failure on the success path, so a
        // value that reports itself available must not also explain an outage.
        let info = CpuPowerInfo {
            available: true,
            ..Default::default()
        };
        assert!(info.unavailable.is_none());
    }
}
