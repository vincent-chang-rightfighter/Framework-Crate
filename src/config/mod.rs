//! Persisted settings: load/save, hardening, and strict import/export.
//!
//! Split from a single `config.rs` (F10-style); implementation lives in
//! submodules and this root re-exports the surface used across the crate.
//! Helpers that only tests reach (`chassis_size`, `import_strict`,
//! `local_product_name`) are exported under `cfg(test)` so release builds
//! do not carry unused public items; `check_origin_against` stays internal
//! to `strict` and is reached through `super` from its tests.
mod acl;
mod io;
mod paths;
mod strict;

pub use io::{load, save_versioned};
pub use paths::config_path;
pub use strict::{ImportedSettings, export_to_path, import_file};

#[cfg(test)]
pub use strict::{chassis_size, import_strict, local_product_name};

pub(crate) use acl::{harden_file_acl, warn_if_world_writable};
pub(crate) use io::atomic_replace;
#[cfg(test)]
pub(crate) use paths::CONFIG_DIR_TEST_LOCK;
pub(crate) use paths::dir_has_reparse_point;
