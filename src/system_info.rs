//! Win32 system integration: window parking, tray primitives, hardware
//! inventory, and logon startup tasks.
//!
//! Split from a single 2,221-line module (P3); implementation lives in
//! submodules and this root only re-exports the public surface, so
//! `crate::system_info::*` paths keep working.

mod startup_task;
mod system;
mod window;

/// UTF-16 NUL-terminated encoding shared by the registry, window, and
/// task-name Win32 calls.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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
