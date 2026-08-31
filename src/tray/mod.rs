pub mod event;
pub mod message_pump;

use std::sync::mpsc;
use std::thread::JoinHandle;

pub use event::{TrayCommand, TrayEvent};
use message_pump::notify_tray_thread;
pub use message_pump::spawn_message_pump;

pub struct TrayManager {
    hwnd: isize,
    command_tx: Option<mpsc::Sender<TrayCommand>>,
    event_rx: Option<mpsc::Receiver<TrayEvent>>,
    icon_ready_rx: Option<mpsc::Receiver<bool>>,
    thread_ready_rx: Option<mpsc::Receiver<()>>,
    initialized: bool,
    thread_ready: bool,
    icon_requested: bool,
    init_started_at: Option<std::time::Instant>,
    icon_loaded: bool,
    thread_handle: Option<JoinHandle<()>>,
    /// Pending HWND for two-phase reinit after old pump exits.
    pending_reinit_hwnd: Option<isize>,
    /// Last WM_COMMAND_READY wake time for retry until icon creation.
    last_notify_at: Option<std::time::Instant>,
    pub(crate) just_restored_at: Option<std::time::Instant>,
}

/// Retry interval for WM_COMMAND_READY wake-ups while icon creation is pending.
const NOTIFY_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

impl Default for TrayManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TrayManager {
    pub fn new() -> Self {
        Self {
            hwnd: 0,
            command_tx: None,
            event_rx: None,
            icon_ready_rx: None,
            thread_ready_rx: None,
            initialized: false,
            thread_ready: false,
            icon_requested: false,
            init_started_at: None,
            icon_loaded: false,
            thread_handle: None,
            pending_reinit_hwnd: None,
            last_notify_at: None,
            just_restored_at: None,
        }
    }

    pub fn init(&mut self, hwnd: isize) -> bool {
        if self.initialized {
            return true;
        }
        // NOTE: Pending reinit takes precedence; caller must poll poll_reinit().
        if self.pending_reinit_hwnd.is_some() {
            return false;
        }

        self.spawn_pump(hwnd);

        tracing::info!("Tray manager initialized with HWND: {}", hwnd);
        true
    }

    fn spawn_pump(&mut self, hwnd: isize) {
        self.hwnd = hwnd;

        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let (icon_ready_tx, icon_ready_rx) = mpsc::sync_channel(1);
        let (thread_ready_tx, thread_ready_rx) = mpsc::sync_channel(1);

        let handle = spawn_message_pump(event_tx, command_rx, icon_ready_tx, thread_ready_tx, hwnd);

        self.command_tx = Some(command_tx);
        self.event_rx = Some(event_rx);
        self.icon_ready_rx = Some(icon_ready_rx);
        self.thread_ready_rx = Some(thread_ready_rx);
        self.thread_handle = Some(handle);
        self.initialized = true;
        self.thread_ready = false;
        self.icon_requested = false;
        self.last_notify_at = None;
        self.init_started_at = Some(std::time::Instant::now());
        self.icon_loaded = false;
    }

    /// Posts CreateIcon once pump signals readiness; re-posts every NOTIFY_RETRY_INTERVAL to recover lost wake-ups.
    pub fn show_icon_async(&mut self) -> bool {
        if self.icon_requested {
            if let Some(rx) = &self.icon_ready_rx
                && let Ok(ready) = rx.try_recv()
            {
                self.icon_loaded = ready;
            }
            if !self.icon_loaded && self.is_alive() {
                let due = self
                    .last_notify_at
                    .is_none_or(|t| t.elapsed() >= NOTIFY_RETRY_INTERVAL);
                if due {
                    self.last_notify_at = Some(std::time::Instant::now());
                    // NOTE: Duplicate CreateIcon is idempotent; re-post is lost-wakeup recovery.
                    if let Some(tx) = &self.command_tx {
                        let _ = tx.send(TrayCommand::CreateIcon);
                    }
                    notify_tray_thread();
                }
            }
            return true;
        }
        if !self.thread_ready {
            match self
                .thread_ready_rx
                .as_ref()
                .and_then(|rx| rx.try_recv().ok())
            {
                Some(()) => self.thread_ready = true,
                None => {
                    let timed_out = self
                        .init_started_at
                        .is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(3));
                    if !timed_out {
                        return false;
                    }
                    tracing::warn!("Tray thread ready signal timeout, proceeding anyway");
                    self.thread_ready = true;
                }
            }
        }
        if self.command_tx.is_none() {
            return false;
        }
        self.icon_requested = true;
        if let Some(rx) = &self.icon_ready_rx {
            while rx.try_recv().is_ok() {}
        }
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(TrayCommand::CreateIcon);
        }
        self.last_notify_at = Some(std::time::Instant::now());
        notify_tray_thread();
        true
    }

    pub fn check_icon_ready(&mut self) -> bool {
        if let Some(rx) = &self.icon_ready_rx
            && let Ok(ready) = rx.try_recv()
        {
            self.icon_loaded = ready;
            tracing::info!("IconReady from channel: {}", ready);
            return ready;
        }
        self.icon_loaded
    }

    pub fn restore_window(&self) {
        crate::system_info::restore_window_from_tray(self.hwnd);
    }

    /// Parks window off-screen with WS_VISIBLE to keep swapchain valid.
    pub fn hide_window(&self) {
        crate::system_info::hide_window_to_tray(self.hwnd);
    }

    /// Requests tray thread shutdown without blocking; thread exits with process.
    pub fn shutdown(&mut self) {
        if !self.initialized {
            return;
        }
        if let Some(tx) = self.command_tx.take() {
            let _ = tx.send(TrayCommand::Shutdown);
        }
        notify_tray_thread();
        self.event_rx = None;
        self.icon_ready_rx = None;
        self.thread_ready_rx = None;
        self.thread_handle = None;
        self.pending_reinit_hwnd = None;
        self.initialized = false;
        self.thread_ready = false;
        self.icon_requested = false;
        tracing::info!("Tray shutdown requested (thread detached)");
    }

    pub fn receive_event(&self) -> Option<TrayEvent> {
        self.event_rx.as_ref().and_then(|rx| rx.try_recv().ok())
    }

    pub fn icon_loaded(&self) -> bool {
        self.icon_loaded
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn is_alive(&self) -> bool {
        self.thread_handle
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    pub fn reset(&mut self) {
        // do not detach thread_handle immediately; keep handle so cleanup_and_exit
        // can DestroyIcon even after we drop channels. Detaching (None) would leak HICON until process exit.
        // We disconnect channels to signal the pump to exit, but keep handle for is_alive/poll.
        self.command_tx = None;
        // NOTE: Wakes live pump to observe disconnect and unregister window class.
        notify_tray_thread();
        self.event_rx = None;
        self.icon_ready_rx = None;
        self.thread_ready_rx = None;
        // Keep thread_handle to allow poll_reinit/is_alive to join; will be cleared on next init or poll
        if self.thread_handle.as_ref().is_some_and(|h| h.is_finished()) {
            self.thread_handle = None;
        }
        self.pending_reinit_hwnd = None;
        self.initialized = false;
        self.thread_ready = false;
        self.icon_requested = false;
        self.init_started_at = None;
        self.icon_loaded = false;
        self.last_notify_at = None;
        self.just_restored_at = None;
        tracing::warn!("TrayManager state reset (thread_handle kept for join)");
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn is_recently_restored(&self) -> bool {
        self.just_restored_at
            .map(|t| t.elapsed() < std::time::Duration::from_millis(2000))
            .unwrap_or(false)
    }

    pub fn mark_restored(&mut self) {
        self.just_restored_at = Some(std::time::Instant::now());
    }

    /// Phase 1 of two-phase reinit: requests old pump shutdown without blocking.
    pub fn request_reinit(&mut self, hwnd: isize) {
        tracing::info!("TrayManager reinit requested with new HWND: {}", hwnd);

        if let Some(tx) = self.command_tx.take() {
            let _ = tx.send(TrayCommand::Shutdown);
        }
        notify_tray_thread();
        self.event_rx = None;
        self.icon_ready_rx = None;
        self.thread_ready_rx = None;
        self.command_tx = None;
        self.initialized = false;
        self.thread_ready = false;
        self.icon_requested = false;
        self.pending_reinit_hwnd = Some(hwnd);
    }

    /// Phase 2 of two-phase reinit: spawns fresh pump once old thread exits.
    pub fn poll_reinit(&mut self) -> bool {
        let Some(hwnd) = self.pending_reinit_hwnd else {
            return false;
        };
        let finished = self.thread_handle.as_ref().is_none_or(|h| h.is_finished());
        if !finished {
            return false;
        }
        self.pending_reinit_hwnd = None;
        self.thread_handle = None;
        self.spawn_pump(hwnd);
        tracing::info!("TrayManager reinit complete, new HWND: {}", hwnd);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_manager_new_defaults() {
        let tm = TrayManager::new();
        assert_eq!(tm.hwnd(), 0);
        assert!(!tm.is_initialized());
        assert!(!tm.icon_loaded());
        assert!(!tm.is_recently_restored());
    }

    #[test]
    fn tray_manager_default_matches_new() {
        let tm1 = TrayManager::default();
        let tm2 = TrayManager::new();
        assert_eq!(tm1.hwnd(), tm2.hwnd());
        assert_eq!(tm1.is_initialized(), tm2.is_initialized());
        assert_eq!(tm1.icon_loaded(), tm2.icon_loaded());
    }

    #[test]
    fn tray_manager_mark_restored() {
        let mut tm = TrayManager::new();
        assert!(!tm.is_recently_restored());
        tm.mark_restored();
        assert!(tm.is_recently_restored());
    }

    #[test]
    fn tray_manager_receive_event_none_when_not_init() {
        let tm = TrayManager::new();
        assert_eq!(tm.receive_event(), None);
    }

    #[test]
    fn tray_manager_shutdown_no_panic_when_not_init() {
        let mut tm = TrayManager::new();
        tm.shutdown();
        assert!(!tm.is_initialized());
    }
}
