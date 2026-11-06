//! Shared CPU power currency types.
//!
//! Split from `limits.rs` (F10): the register snapshot, limit params, and
//! write-boundary constants are used by both the read side (`read.rs`) and
//! the write side (`limits.rs`, `sync.rs`, `bios.rs`), so they live here and
//! both sides import from `super::types::`. Encoding, validation, and the
//! write paths stay in `limits.rs`.

use super::ffi::IoctlFailure;

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
    /// Turbo ratio telemetry (100MHz BCLK assumed). Best-effort reads that
    /// never affect availability or any write decision; `None` hides the row.
    pub turbo_limit_ghz: Option<f64>,
    pub turbo_now_ghz: Option<f64>,
    /// True when the MMIO mirror was read successfully. Distinguishes a
    /// genuine 0 W/disabled mirror from an unavailable IntelMCHBAR module.
    pub mmio_available: bool,
    /// Firmware-supported range from MSR 0x614, in seconds. `None` keeps the
    /// read-back time-window check advisory.
    pub min_time_s: Option<f64>,
    pub max_time_s: Option<f64>,
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
            max_time_s: self.max_time_s,
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
fn effective_limit(msr: f64, msr_en: bool, mmio: f64, mmio_en: bool) -> f64 {
    let msr_valid = msr_en && msr > 0.0 && msr.is_finite();
    let mmio_valid = mmio_en && mmio > 0.0 && mmio.is_finite();
    match (msr_valid, mmio_valid) {
        (true, true) => msr.min(mmio),
        (true, false) => msr,
        (false, true) => mmio,
        (false, false) => 0.0,
    }
}

/// The PL1/PL2 limits and the RAPL units they are expressed in.
///
/// Passed as one value rather than eleven positional arguments: the write
/// path and the sync thread both need all eleven, and a caller that has them
/// as a `CpuPowerInfo` should not have to unpack and repack them.
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
    /// Firmware maximum time window from MSR 0x614, if known. Used to turn
    /// read-back time differences into failures only inside the supported
    /// range; `None` keeps the old advisory behavior.
    pub max_time_s: Option<f64>,
    pub power_unit: f64,
    pub time_unit: f64,
}

/// Absolute ceiling for any model (Desktop); per-model caps are lower.
pub const LIMIT_MAX_WATTS: f64 = 120.0;
/// Upper bound for a PL time window.
pub const LIMIT_MAX_TIME_S: f64 = 120.0;
/// Minimum meaningful enabled limit: below this the CPU cannot make
/// progress and the machine appears hung.
pub const LIMIT_MIN_ENABLED_WATTS: f64 = 5.0;

#[cfg(test)]
mod tests {
    use super::*;

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
