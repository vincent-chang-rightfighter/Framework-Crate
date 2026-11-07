//! MSR/MMIO power limit decoding, validation and write paths.
//!
//! Split by concern: register encoding/read-back checks (`decode`), range
//! validation (`validate`), and the MSR write path (`write`).

mod decode;
mod validate;
mod write;

pub use decode::pl_limits_differ_from_bios;
pub(super) use decode::{decode_power_limit, decode_time_window, decode_time_window_bounds};
pub(super) use validate::{platform_max_watts, plausible_rapl_unit};
pub use validate::{validate_limit_params, validate_limit_values};
pub(super) use write::write_msr_pl1_pl2;
pub use write::{write_bios_defaults, write_msr_pl1_pl2_public};
