use parking_lot::RwLock;
use std::sync::Arc;

/// Clones inner Arc under read lock.
pub fn read_lock<T>(lock: &Arc<RwLock<Arc<T>>>) -> Arc<T> {
    Arc::clone(&lock.read())
}

/// Executes closure under write lock on Arc<RwLock<Arc<T>>>.
pub fn with_write_lock<T, R>(lock: &Arc<RwLock<Arc<T>>>, f: impl FnOnce(&mut Arc<T>) -> R) -> R {
    f(&mut lock.write())
}

/// Current wall-clock time in ms since UNIX epoch.
pub fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Monotonic ms since process start; immune to wall-clock adjustments.
pub fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    start.elapsed().as_millis() as u64
}

/// Absolute path to an exe under %SystemRoot%\System32.
///
/// An elevated process must never resolve system tools from PATH or from
/// attacker-controlled env: any user-writable directory earlier in PATH
/// would win the lookup, and a spoofed SystemRoot would redirect every
/// tool. Prefer the OS-known System32 directory; env is only a fallback
/// and the result is validated to end in System32\<name>.
/// (winget is intentionally excluded: it is a store-managed execution
/// alias with a versioned path, not a System32 binary.)
pub fn system32_exe(name: &str) -> std::path::PathBuf {
    // Prefer the OS API over env: GetSystemDirectoryW cannot be spoofed
    // via SystemRoot like getenv can.
    if let Some(dir) = windows_system32_dir() {
        return dir.join(name);
    }
    tracing::warn!("GetSystemDirectoryW failed, falling back to SystemRoot env (spoofable)");
    let root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    root.join("System32").join(name)
}

fn windows_system32_dir() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    // MAX_PATH-sized buffer is enough for the system directory.
    let mut buf = vec![0u16; 260];
    // SAFETY: GetSystemDirectoryW writes up to the buffer length and
    // returns the length excluding nul; no other global state is touched.
    let len = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if len == 0 || (len as usize) >= buf.len() {
        return None;
    }
    let os = std::ffi::OsString::from_wide(&buf[..len as usize]);
    let dir = std::path::PathBuf::from(os);
    // Sanity: must end in System32 (case-insensitive) or refuse.
    if dir
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("System32"))
    {
        Some(dir)
    } else {
        None
    }
}

/// Absolute path to Windows PowerShell 5.1. Note it does NOT live in the
/// System32 root but in WindowsPowerShell\v1.0: pinning a bare
/// "powershell.exe" under System32 breaks the call, and fail-closed
/// checks (e.g. Authenticode) then refuse to run at all.
pub fn powershell_exe() -> std::path::PathBuf {
    system32_exe(r"WindowsPowerShell\v1.0\powershell.exe")
}

/// Serialized CPU power operation mutex to prevent concurrent Apply/Reset/Sync races.
pub fn cpu_power_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub const EC_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
pub const PAWNIO_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);
/// PawnIO install/upgrade and Modules download may take 30s+ (winget + UAC + network)
pub const PAWNIO_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Runs blocking task with timeout; maps JoinError and timeout to String.
/// Note: on timeout the blocking thread continues until completion (tokio pool reuses threads,
/// bounded by max_blocking_threads=8). Caller should treat timeout as failure and reset EC if repeated.
pub async fn spawn_blocking_with_timeout<F, T>(
    timeout: std::time::Duration,
    task: F,
) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(task)).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(join_err)) => Err(format!("Task panicked: {}", join_err)),
        Err(_) => Err(format!("Hardware I/O timed out after {:?}", timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_time_ms_returns_positive_value() {
        let ts = current_time_ms();
        assert!(ts > 0);
    }

    #[test]
    fn pinned_system_tools_exist() {
        // Guards the System32 pinning: a wrong subpath fails at runtime
        // (powershell.exe is NOT in the System32 root, which once broke
        // the Authenticode check and fail-closed refused the PawnIO DLL).
        for name in ["schtasks.exe", "curl.exe", "tar.exe"] {
            assert!(system32_exe(name).exists(), "{name} missing");
        }
        assert!(powershell_exe().exists(), "powershell missing");
    }

    #[test]
    fn monotonic_ms_is_monotonic() {
        let a = monotonic_ms();
        let b = monotonic_ms();
        assert!(b >= a);
    }

    #[tokio::test]
    async fn spawn_blocking_with_timeout_success() {
        let res =
            spawn_blocking_with_timeout(std::time::Duration::from_millis(100), || 42u32).await;
        assert_eq!(res.unwrap(), 42);
    }

    #[tokio::test]
    async fn spawn_blocking_with_timeout_times_out() {
        let res = spawn_blocking_with_timeout(std::time::Duration::from_millis(10), || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            1u32
        })
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("timed out"));
    }

    #[tokio::test]
    async fn spawn_blocking_with_timeout_panic() {
        let res = spawn_blocking_with_timeout(std::time::Duration::from_millis(100), || {
            panic!("test panic");
        })
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("panicked"));
    }
}
