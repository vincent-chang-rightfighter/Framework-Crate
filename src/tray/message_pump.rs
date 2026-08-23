use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::system_info;
use super::event::{TrayCommand, TrayEvent, ID_SHOW, ID_QUIT};

use crate::system_info::{WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND};

const WM_APP: u32 = 0x8000;
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_COMMAND_READY: u32 = WM_APP + 2;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
// Explorer restart notification is a registered message, not a fixed
// constant. Must be queried via `RegisterWindowMessageW("TaskbarCreated")`.

// SAFETY: Thread-locals accessed only on pump thread that created the window.
thread_local! {
    static EVENT_TX: std::cell::RefCell<Option<mpsc::Sender<TrayEvent>>> = const { std::cell::RefCell::new(None) };
    static TRAY_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// Main window HWND for immediate restore without waiting for 2s UI tick.
    static APP_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// Cached HICON for re-adding icon on WM_TASKBARCREATED.
    static TRAY_HICON: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

static TRAY_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Wakes tray thread to drain command queue; caller must retry on failure.
pub fn notify_tray_thread() -> bool {
    let tid = TRAY_THREAD_ID.load(Ordering::Acquire);
    if tid == 0 {
        return false;
    }
    let ok = unsafe { PostThreadMessageW(tid, WM_COMMAND_READY, 0, 0) != 0 };
    if !ok {
        // NOTE: Polled at 2 Hz while queue missing; debug to avoid log spam.
        tracing::debug!("PostThreadMessageW to tray thread {} failed (queue not ready?)", tid);
    }
    ok
}

#[repr(C)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
struct MSG {
    hwnd: *mut core::ffi::c_void,
    message: u32,
    wParam: usize,
    lParam: isize,
    time: u32,
    pt: POINT,
}

#[repr(C)]
#[allow(clippy::upper_case_acronyms)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
struct WNDCLASSW {
    style: u32,
    lpfnWndProc: *const core::ffi::c_void,
    cbClsExtra: i32,
    cbWndExtra: i32,
    hInstance: *mut core::ffi::c_void,
    hIcon: *mut core::ffi::c_void,
    hCursor: *mut core::ffi::c_void,
    hbrBackground: *mut core::ffi::c_void,
    lpszMenuName: *const u16,
    lpszClassName: *const u16,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetMessageW(lpMsg: *mut MSG, hWnd: *mut core::ffi::c_void, wMsgFilterMin: u32, wMsgFilterMax: u32) -> i32;
    fn TranslateMessage(lpMsg: *const MSG) -> i32;
    fn DispatchMessageW(lpMsg: *const MSG) -> i32;
    fn RegisterClassW(lpWndClass: *const WNDCLASSW) -> u16;
    fn UnregisterClassW(lpClassName: *const u16, hInstance: *mut core::ffi::c_void) -> i32;
    fn CreateWindowExW(
        dwExStyle: u32, lpClassName: *const u16, lpWindowName: *const u16,
        dwStyle: u32, x: i32, y: i32, nWidth: i32, nHeight: i32,
        hWndParent: *mut core::ffi::c_void, hMenu: *mut core::ffi::c_void,
        hInstance: *mut core::ffi::c_void, lpParam: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
    fn DestroyWindow(hWnd: *mut core::ffi::c_void) -> i32;
    fn DestroyIcon(hIcon: *mut core::ffi::c_void) -> i32;
    fn GetModuleHandleW(lpModuleName: *const u16) -> *mut core::ffi::c_void;
    fn GetCursorPos(lpPoint: *mut POINT) -> i32;
    fn SetForegroundWindow(hWnd: *mut core::ffi::c_void) -> i32;
    fn DefWindowProcW(hWnd: *mut core::ffi::c_void, msg: u32, wParam: usize, lParam: isize) -> isize;
    fn PostThreadMessageW(idThread: u32, msg: u32, wParam: usize, lParam: isize) -> i32;
    fn ShowWindow(hWnd: *mut core::ffi::c_void, nCmdShow: i32) -> i32;
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: *mut core::ffi::c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if msg == WM_TRAYICON {
        let lparam_u32 = lparam as u32;
        if lparam_u32 == WM_LBUTTONUP {
            APP_HWND.with(|hwnd| {
                let app_hwnd = hwnd.get();
                if app_hwnd != 0 {
                    system_info::restore_window_from_tray(app_hwnd);
                }
            });
            EVENT_TX.with(|tx| {
                if let Some(sender) = tx.borrow().as_ref() {
                    let _ = sender.send(TrayEvent::Show);
                    let _ = sender.send(TrayEvent::Restored);
                }
            });
        } else if lparam_u32 == WM_RBUTTONUP {
            EVENT_TX.with(|tx| {
                if let Some(sender) = tx.borrow().as_ref() {
                    handle_tray_right_click(sender);
                }
            });
            // TrackPopupMenu runs a modal loop that consumes the thread's
            // WM_COMMAND_READY wake. Re-post after the menu so the outer
            // GetMessageW drains the buffered Shutdown/Reinit command.
            let _ = unsafe {
                PostThreadMessageW(TRAY_THREAD_ID.load(Ordering::Acquire), WM_COMMAND_READY, 0, 0)
            };
        }
        return 0;
    }
    if msg == system_info::taskbar_created_msg() {
        let hwnd = TRAY_HWND.with(|h| h.get());
        let icon = TRAY_HICON.with(|h| h.get());
        if hwnd != 0 && icon != 0 {
            let ok = system_info::shell_notify_add(hwnd, icon, "Framework Crate", WM_TRAYICON);
            tracing::info!("[TASKBAR] Explorer restarted, tray icon re-added: {}", ok);
        }
        return 0;
    }
    if msg == WM_POWERBROADCAST {
        let wparam_u32 = wparam as u32;
        if wparam_u32 == PBT_APMRESUMEAUTOMATIC || wparam_u32 == PBT_APMRESUMESUSPEND {
            tracing::info!("[POWER] System resumed from sleep/hibernate (wParam={:#x})", wparam_u32);
            EVENT_TX.with(|tx| {
                if let Some(sender) = tx.borrow().as_ref() {
                    let _ = sender.send(TrayEvent::PowerResumed);
                }
            });
            return 0;
        }
    }
    if msg == system_info::show_request_message_id() {
        // Second instance requested restore via the tray window. The running
        // instance owns SAVED_PLACEMENT, so it restores here and notifies App
        // to sync visible state.
        APP_HWND.with(|hwnd| {
            let app_hwnd = hwnd.get();
            if app_hwnd != 0 {
                system_info::restore_window_from_tray(app_hwnd);
            }
        });
        EVENT_TX.with(|tx| {
            if let Some(sender) = tx.borrow().as_ref() {
                let _ = sender.send(TrayEvent::Show);
                let _ = sender.send(TrayEvent::Restored);
            }
        });
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub fn spawn_message_pump(
    event_tx: mpsc::Sender<TrayEvent>,
    command_rx: mpsc::Receiver<TrayCommand>,
    icon_ready_tx: mpsc::SyncSender<bool>,
    thread_ready_tx: mpsc::SyncSender<()>,
    app_hwnd: isize,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            message_pump_loop(event_tx, command_rx, icon_ready_tx, thread_ready_tx, app_hwnd);
        })) {
            tracing::error!("Tray message pump panicked: {:?}", e);
        }
    })
}

fn cleanup_and_exit(tray_icon_loaded: bool, tray_hwnd: *mut core::ffi::c_void, hicon: Option<isize>) {
    TRAY_THREAD_ID.store(0, Ordering::Release);
    if tray_icon_loaded {
        system_info::shell_notify_delete(tray_hwnd as isize);
    }
    if let Some(icon) = hicon {
        unsafe {
            // SAFETY: HICON from CreateIconFromResourceEx must be freed to avoid GDI leak.
            DestroyIcon(icon as *mut core::ffi::c_void);
        }
    }
    if !tray_hwnd.is_null() {
        unsafe {
            DestroyWindow(tray_hwnd);
            let class_name: Vec<u16> = "FrameworkControlTray\0".encode_utf16().collect();
            let h_instance = GetModuleHandleW(std::ptr::null());
            UnregisterClassW(class_name.as_ptr(), h_instance);
        }
    }
}

fn message_pump_loop(
    event_tx: mpsc::Sender<TrayEvent>,
    command_rx: mpsc::Receiver<TrayCommand>,
    icon_ready_tx: mpsc::SyncSender<bool>,
    thread_ready_tx: mpsc::SyncSender<()>,
    app_hwnd: isize,
) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    TRAY_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);

    EVENT_TX.with(|tx| {
        *tx.borrow_mut() = Some(event_tx.clone());
    });
    APP_HWND.with(|hwnd| {
        hwnd.set(app_hwnd);
    });

    let tray_hwnd = create_hidden_window();
    if tray_hwnd.is_null() {
        tracing::error!("Failed to create tray message window");
        cleanup_and_exit(false, tray_hwnd, None);
        return;
    }
    TRAY_HWND.with(|hwnd| {
        hwnd.set(tray_hwnd as isize);
    });

    let mut tray_icon_loaded = false;
    let mut hicon: Option<isize> = None;

    let icon_data = include_bytes!("../../assets/app.ico");
    match system_info::load_icon_from_bytes(icon_data) {
        Some(icon) => {
            hicon = Some(icon);
            TRAY_HICON.with(|h| h.set(icon));
            tracing::info!("Tray icon loaded");
        }
        None => {
            tracing::warn!("Failed to load tray icon");
        }
    }

    // NOTE: Primes queue so PostThreadMessageW can succeed.
    {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let result = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if result == 0 || result == -1 {
            tracing::info!("Tray message pump exiting during priming (result={})", result);
            cleanup_and_exit(tray_icon_loaded, tray_hwnd, hicon);
            return;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let _ = thread_ready_tx.send(());

    loop {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let result = unsafe {
            GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0)
        };

        if result == 0 || result == -1 {
            tracing::info!("Tray message pump exiting (result={})", result);
            cleanup_and_exit(tray_icon_loaded, tray_hwnd, hicon);
            return;
        }

        if msg.message == WM_COMMAND_READY {
            // NOTE: Drains all commands per wake to prevent Shutdown starvation.
            loop {
                match command_rx.try_recv() {
                    Ok(TrayCommand::Shutdown) => {
                        tracing::info!("Tray shutdown");
                        cleanup_and_exit(tray_icon_loaded, tray_hwnd, hicon);
                        return;
                    }
                    Ok(TrayCommand::CreateIcon) => {
                        if !tray_icon_loaded {
                            if let Some(icon) = hicon {
                                let ok = system_info::shell_notify_add(
                                    tray_hwnd as isize, icon, "Framework Crate", WM_TRAYICON,
                                );
                                tray_icon_loaded = ok;
                                // NOTE: try_send avoids blocking GetMessageW on sync(1) channel.
                                let _ = icon_ready_tx.try_send(ok);
                                if ok {
                                    tracing::info!("Tray icon created");
                                } else {
                                    tracing::warn!("Shell_NotifyIconW failed");
                                }
                            } else {
                                let _ = icon_ready_tx.try_send(false);
                            }
                        } else {
                            let _ = icon_ready_tx.try_send(true);
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        cleanup_and_exit(tray_icon_loaded, tray_hwnd, hicon);
                        return;
                    }
                }
            }
            continue;
        }

        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn create_hidden_window() -> *mut core::ffi::c_void {
    unsafe {
        let class_name: Vec<u16> = "FrameworkControlTray\0".encode_utf16().collect();
        let h_instance = GetModuleHandleW(std::ptr::null());

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: tray_wnd_proc as *const core::ffi::c_void,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: h_instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };

        let mut atom = RegisterClassW(&wc);
        if atom == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(1410) {
                tracing::info!("Window class already registered, unregistering and retrying");
                UnregisterClassW(class_name.as_ptr(), h_instance);
                atom = RegisterClassW(&wc);
            }
        }
        if atom == 0 {
            tracing::error!("RegisterClassW failed: {}", std::io::Error::last_os_error());
            return std::ptr::null_mut();
        }

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            std::ptr::null(),
            0,
            0, 0, 1, 1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            h_instance,
            std::ptr::null_mut(),
        );

        if !hwnd.is_null() {
            ShowWindow(hwnd, 0);
        }

        hwnd
    }
}

fn handle_tray_right_click(event_tx: &mpsc::Sender<TrayEvent>) {
    let mut point = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut point); }

    let tray_hwnd = TRAY_HWND.with(|hwnd| hwnd.get());
    unsafe { SetForegroundWindow(tray_hwnd as *mut core::ffi::c_void); }

    if let Some(cmd) = system_info::show_tray_menu(tray_hwnd, point.x, point.y) {
        match cmd {
            ID_SHOW => {
                APP_HWND.with(|hwnd| {
                    let app_hwnd = hwnd.get();
                    if app_hwnd != 0 {
                        system_info::restore_window_from_tray(app_hwnd);
                    }
                });
                let _ = event_tx.send(TrayEvent::MenuShow);
                let _ = event_tx.send(TrayEvent::Restored);
            }
            ID_QUIT => { let _ = event_tx.send(TrayEvent::MenuQuit); }
            _ => {}
        }
    }
}
