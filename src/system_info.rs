//! Win32 system integration: window parking, tray primitives, hardware
//! inventory, and logon startup tasks.
//!
//! Split from a single 2,221-line module (P3); implementation lives in
//! submodules and this root re-exports the surface actually used across the
//! crate. Four former `pub` helpers (`is_elevated`,
//! `force_foreground_window`, `find_tray_window`, `post_message`) are now
//! submodule-internal — each is only called within its own submodule, and
//! this binary crate has no external consumers, so no
//! `crate::system_info::*` path referenced anywhere was broken by the split.
//! (`decode_console_bytes`/`build_startup_task` were already `pub(crate)`.)

mod startup_task;
mod system;
mod window;

/// UTF-16 NUL-terminated encoding shared by the registry, window, and
/// task-name Win32 calls.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Same as [`to_wide`] for paths: `OsStr::encode_wide` preserves non-UTF-8
/// paths that `to_string_lossy` would mangle. Single home for the three
/// Win32 path call sites (task dir create / ACL probe / hardened create).
pub(crate) fn to_wide_path(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub use startup_task::{SingleInstanceGuard, set_startup_launch, startup_launch_enabled};
pub use system::{
    cpu_name, display_refresh_rate, display_resolution, is_intel_cpu, os_version, total_memory_gb,
    work_area_size,
};
pub use window::{
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, WM_POWERBROADCAST, find_main_window_by_pid,
    hide_window_to_tray, is_iconic, is_window, load_icon_from_bytes, request_show_running_instance,
    restore_window_from_tray, shell_notify_add, shell_notify_delete, shell_notify_modify_tip,
    show_request_message_id, show_tray_menu, taskbar_created_msg,
};
