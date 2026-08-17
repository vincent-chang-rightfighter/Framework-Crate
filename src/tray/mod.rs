pub mod event;
pub mod message_pump;

use std::sync::mpsc;
use std::thread::JoinHandle;

pub use event::{TrayCommand, TrayEvent};
pub use message_pump::spawn_message_pump;
use message_pump::notify_tray_thread;

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
    pub(crate) just_restored_at: Option<std::time::Instant>,
}

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
            just_restored_at: None,
        }
    }

    pub fn init(&mut self, hwnd: isize) -> bool {
        if self.initialized {
            return true;
        }

        self.hwnd = hwnd;

        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let (icon_ready_tx, icon_ready_rx) = mpsc::sync_channel(1);
        let (thread_ready_tx, thread_ready_rx) = mpsc::sync_channel(1);

        let handle = spawn_message_pump(event_tx, command_rx, icon_ready_tx, thread_ready_tx);

        // Do not block on the thread-ready signal: poll it from the UI tick
        // via show_icon_async(). This keeps init() off the UI thread's
        // critical path (a hung tray thread must not freeze the window).
        self.command_tx = Some(command_tx);
        self.event_rx = Some(event_rx);
        self.icon_ready_rx = Some(icon_ready_rx);
        self.thread_ready_rx = Some(thread_ready_rx);
        self.thread_handle = Some(handle);
        self.initialized = true;
        self.thread_ready = false;
        self.icon_requested = false;
        self.init_started_at = Some(std::time::Instant::now());

        tracing::info!("Tray manager initialized with HWND: {}", hwnd);
        true
    }

    /// Send CreateIcon once the tray thread has signaled it is inside
    /// GetMessageW (PostThreadMessageW needs the queue to exist first).
    /// Non-blocking: returns false until the thread is ready, then true.
    /// Falls back after 3s so a stuck thread never wedges the UI.
    pub fn show_icon_async(&mut self) -> bool {
        if self.icon_requested {
            return true;
        }
        if !self.thread_ready {
            match self.thread_ready_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                Some(()) => self.thread_ready = true,
                None => {
                    let timed_out = self
                        .init_started_at
                        .is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(3));
                    if !timed_out {
                        return false;
                    }
                    // Best-effort fallback: the command stays queued, but the
                    // notify may be lost if the thread has no queue yet.
                    tracing::warn!("Tray thread ready signal timeout, proceeding anyway");
                    self.thread_ready = true;
                }
            }
        }
        self.icon_requested = true;
        if let Some(rx) = &self.icon_ready_rx {
            while rx.try_recv().is_ok() {}
        }
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(TrayCommand::CreateIcon);
        }
        notify_tray_thread();
        true
    }

    /// Check if the async icon creation has completed, updating `icon_loaded`.
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

    pub fn hide_icon(&self) {
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(TrayCommand::RemoveIcon);
        }
        notify_tray_thread();
    }

    /// Restore the parked window back on screen at its saved position.
    /// The swapchain stays valid while parked, so no blank frame appears.
    pub fn restore_window(&self) {
        crate::system_info::restore_window_from_tray(self.hwnd);
    }

    /// Park the window off-screen (keeps it WS_VISIBLE so WM_PAINT keeps
    /// arriving and the swapchain stays valid) instead of SW_HIDE, which
    /// would make the first frames after restore blank/white.
    pub fn hide_window(&self) {
        crate::system_info::hide_window_to_tray(self.hwnd);
    }

    pub fn shutdown(&mut self) {
        if !self.initialized {
            return;
        }
        if let Some(tx) = self.command_tx.take() {
            let _ = tx.send(TrayCommand::Shutdown);
        }
        notify_tray_thread();
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        self.initialized = false;
        tracing::info!("Tray shutdown complete");
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

    /// True while the message pump thread is still running.
    pub fn is_alive(&self) -> bool {
        self.thread_handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Drop all thread state so the next `init()` spawns a fresh pump.
    pub fn reset(&mut self) {
        self.command_tx = None;
        self.event_rx = None;
        self.icon_ready_rx = None;
        self.thread_ready_rx = None;
        self.thread_handle = None;
        self.initialized = false;
        self.thread_ready = false;
        self.icon_requested = false;
        self.init_started_at = None;
        self.icon_loaded = false;
        tracing::warn!("TrayManager state reset");
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

    pub fn reinit(&mut self, hwnd: isize) {
        tracing::info!("TrayManager reinit with new HWND: {}", hwnd);

        if let Some(tx) = self.command_tx.take() {
            let _ = tx.send(TrayCommand::Shutdown);
        }
        notify_tray_thread();
        self.event_rx = None;
        self.icon_ready_rx = None;
        self.thread_ready_rx = None;

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }

        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let (icon_ready_tx, icon_ready_rx) = mpsc::sync_channel(1);
        let (thread_ready_tx, thread_ready_rx) = mpsc::sync_channel(1);

        let handle = spawn_message_pump(event_tx, command_rx, icon_ready_tx, thread_ready_tx);

        self.hwnd = hwnd;
        self.command_tx = Some(command_tx);
        self.event_rx = Some(event_rx);
        self.icon_ready_rx = Some(icon_ready_rx);
        self.thread_ready_rx = Some(thread_ready_rx);
        self.thread_handle = Some(handle);
        self.initialized = true;
        self.thread_ready = false;
        self.icon_requested = false;
        self.init_started_at = Some(std::time::Instant::now());
        self.icon_loaded = false;

        tracing::info!("TrayManager reinit complete, new HWND: {}", hwnd);
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
