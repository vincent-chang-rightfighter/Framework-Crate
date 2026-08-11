use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::system_info;
use super::event::{TrayCommand, TrayEvent, ID_SHOW, ID_QUIT};

const WM_APP: u32 = 0x8000;
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_COMMAND_READY: u32 = WM_APP + 2;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;

// SAFETY: These thread-locals are only accessed from the tray message pump thread.
// tray_wnd_proc is a Windows callback that runs on the same thread that created
// the window (via create_hidden_window), so all accesses are single-threaded.
// No locking needed since thread-local storage is inherently thread-safe.
thread_local! {
    static EVENT_TX: std::cell::RefCell<Option<mpsc::Sender<TrayEvent>>> = const { std::cell::RefCell::new(None) };
    static ICED_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static TRAY_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

static TRAY_THREAD_ID: AtomicU32 = AtomicU32::new(0);

pub fn notify_tray_thread() {
    let tid = TRAY_THREAD_ID.load(Ordering::Acquire);
    if tid != 0 {
        unsafe {
            PostThreadMessageW(tid, WM_COMMAND_READY, 0, 0);
        }
    }
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
extern "system" {
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
            EVENT_TX.with(|tx| {
                if let Some(sender) = tx.borrow().as_ref() {
                    let _ = sender.send(TrayEvent::Show);
                }
            });
        } else if lparam_u32 == WM_RBUTTONUP {
            EVENT_TX.with(|tx| {
                if let Some(sender) = tx.borrow().as_ref() {
                    handle_tray_right_click(sender);
                }
            });
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub fn spawn_message_pump(
    iced_hwnd: isize,
    event_tx: mpsc::Sender<TrayEvent>,
    command_rx: mpsc::Receiver<TrayCommand>,
    icon_ready_tx: mpsc::SyncSender<bool>,
    thread_ready_tx: mpsc::SyncSender<()>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        message_pump_loop(iced_hwnd, event_tx, command_rx, icon_ready_tx, thread_ready_tx);
    })
}

fn cleanup_and_exit(tray_icon_loaded: bool, tray_hwnd: *mut core::ffi::c_void) {
    // Clear the thread id so notify_tray_thread() stops posting to a dead thread.
    TRAY_THREAD_ID.store(0, Ordering::Release);
    if tray_icon_loaded {
        system_info::shell_notify_delete(tray_hwnd as isize);
    }
    unsafe {
        DestroyWindow(tray_hwnd);
    }
}

fn message_pump_loop(
    iced_hwnd: isize,
    event_tx: mpsc::Sender<TrayEvent>,
    command_rx: mpsc::Receiver<TrayCommand>,
    icon_ready_tx: mpsc::SyncSender<bool>,
    thread_ready_tx: mpsc::SyncSender<()>,
) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    TRAY_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);

    EVENT_TX.with(|tx| {
        *tx.borrow_mut() = Some(event_tx.clone());
    });
    ICED_HWND.with(|hwnd| {
        hwnd.set(iced_hwnd);
    });

    let tray_hwnd = create_hidden_window();
    if tray_hwnd.is_null() {
        tracing::error!("Failed to create tray message window");
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
            tracing::info!("Tray icon loaded");
        }
        None => {
            tracing::warn!("Failed to load tray icon");
        }
    }

    // Prime the message queue with one GetMessageW call. This creates the
    // thread's message queue (required before PostThreadMessageW can succeed)
    // and confirms the thread is ready to receive WM_COMMAND_READY messages.
    {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let result = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if result == 0 || result == -1 {
            tracing::info!("Tray message pump exiting during priming (result={})", result);
            cleanup_and_exit(tray_icon_loaded, tray_hwnd);
            return;
        }
        // Process the primed message normally
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Signal the main thread that we are in the message loop and ready
    // to process WM_COMMAND_READY notifications.
    let _ = thread_ready_tx.send(());

    loop {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let result = unsafe {
            GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0)
        };

        if result == 0 || result == -1 {
            tracing::info!("Tray message pump exiting (result={})", result);
            cleanup_and_exit(tray_icon_loaded, tray_hwnd);
            return;
        }

        if msg.message == WM_COMMAND_READY {
            match command_rx.try_recv() {
                Ok(TrayCommand::Shutdown) => {
                    tracing::info!("Tray shutdown");
                    cleanup_and_exit(tray_icon_loaded, tray_hwnd);
                    return;
                }
                Ok(TrayCommand::CreateIcon) => {
                    if !tray_icon_loaded {
                        if let Some(icon) = hicon {
                            let ok = system_info::shell_notify_add(
                                tray_hwnd as isize, icon, "Framework Crate", WM_TRAYICON,
                            );
                            tray_icon_loaded = ok;
                            let _ = icon_ready_tx.send(ok);
                            if ok {
                                tracing::info!("Tray icon created");
                            } else {
                                tracing::warn!("Shell_NotifyIconW failed");
                            }
                        } else {
                            let _ = icon_ready_tx.send(false);
                        }
                    } else {
                        let _ = icon_ready_tx.send(true);
                    }
                }
                Ok(TrayCommand::RemoveIcon) => {
                    if tray_icon_loaded {
                        system_info::shell_notify_delete(tray_hwnd as isize);
                        tray_icon_loaded = false;
                        tracing::info!("Tray icon removed");
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    cleanup_and_exit(tray_icon_loaded, tray_hwnd);
                    return;
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
            // ERROR_CLASS_ALREADY_EXISTS = 1410. If the class is already registered
            // from a previous reinit(), unregister it and try again.
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
            ShowWindow(hwnd, 0); // SW_HIDE — initialize show state
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
            ID_SHOW => { let _ = event_tx.send(TrayEvent::MenuShow); }
            ID_QUIT => { let _ = event_tx.send(TrayEvent::MenuQuit); }
            _ => {}
        }
    }
}
