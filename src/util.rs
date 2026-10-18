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

/// Global serialized EC write mutex to prevent out-of-order hardware writes.
pub fn ec_write_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Acquires EC write lock with bounded wait: 500ms fast path, then up to
/// 5s total. Returns None on persistent contention so callers skip the
/// cycle instead of piling up waiters behind a stuck holder.
pub async fn acquire_ec_write() -> Option<tokio::sync::MutexGuard<'static, ()>> {
    use std::time::Duration;
    if let Ok(g) = tokio::time::timeout(Duration::from_millis(500), ec_write_mutex().lock()).await {
        return Some(g);
    }
    tracing::warn!("EC write contended >500ms, waiting up to 5s");
    tokio::time::timeout(Duration::from_secs(5), ec_write_mutex().lock())
        .await
        .ok()
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

/// Cap concurrent EC blocking I/O so a hung driver can't exhaust the
/// shared `max_blocking_threads=8` pool. Reads that miss a permit skip
/// the cycle instead of queueing behind orphans; the skip counter makes
/// overload observable in logs. Writes stay serialized by `ec_write_mutex`.
pub fn ec_io_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(3))
}

pub fn ec_io_skips() -> &'static std::sync::atomic::AtomicU64 {
    static SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    &SKIPS
}

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

/// EC read path with overload protection. Takes an owned semaphore permit
/// for the whole blocking call so a hung driver can't pile up more than
/// 3 concurrent EC reads behind orphans; excess cycles skip fast with a
/// skip counter instead of queueing into pool exhaustion.
pub async fn spawn_ec_read_with_timeout<F, T>(
    timeout: std::time::Duration,
    task: F,
) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let Ok(_permit) = ec_io_slots().try_acquire() else {
        let n = ec_io_skips().fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        // Log sparingly: every 10th skip to avoid log spam while hung.
        if n % 10 == 1 {
            tracing::warn!("EC I/O overloaded, skipping read (total skips={n})");
        }
        return Err("EC I/O overloaded, skipping read".to_string());
    };
    // Permit held across the blocking call; dropped on completion/timeout.
    let _permit = _permit;
    spawn_blocking_with_timeout(timeout, task).await
}

/// Monotonic EC write sequence for log correlation. Timeouts release the
/// lock while the orphan keeps running, so a late write can still land
/// out of order; the seq lets logs prove it (re-assert loops heal it).
pub fn next_ec_write_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Serialized EC write with timeout. Prefer over acquire + spawn pairs.
///
/// The mutex guard is held only until completion OR timeout, whichever
/// comes first. On timeout the mutex is released so later writes are not
/// stuck behind a hung driver; the orphaned thread keeps running without
/// the guard (drivers offer no cancellation) and its late result is
/// discarded. A timed-out write may therefore still land out of order —
/// the periodic re-assert loops (fan duties every 30s, PL sync every
/// 250ms) heal that — but it can never wedge the write path.
pub async fn spawn_ec_guarded<T>(
    guard: tokio::sync::MutexGuard<'static, ()>,
    timeout: std::time::Duration,
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let seq = next_ec_write_seq();
    tracing::trace!("EC write dispatch ec_seq={seq}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::task::spawn_blocking(task);
    // Forward completion; the receiver is gone after a timeout, in which
    // case the late result is dropped here instead of updating state.
    // Late arrivals are still traced so out-of-order landings are provable.
    tokio::spawn(async move {
        let res = handle.await;
        if tx.send(res).is_err() {
            tracing::trace!("EC write late arrival ec_seq={seq} (receiver gone)");
        }
    });
    let result = tokio::time::timeout(timeout, rx).await;
    drop(guard);
    match result {
        Ok(Ok(Ok(val))) => {
            tracing::trace!("EC write complete ec_seq={seq}");
            Ok(val)
        }
        Ok(Ok(Err(join_err))) => Err(format!("Task panicked: {}", join_err)),
        Ok(Err(_)) => Err("EC writer task vanished".to_string()),
        Err(_) => Err(format!(
            "Hardware I/O timed out after {:?} (ec_seq={seq})",
            timeout
        )),
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
    async fn ec_guarded_timeout_releases_mutex() {
        // The orphaned thread still sleeps 200ms, but the mutex must be
        // free again right after the 10ms timeout, not held hostage.
        let guard = ec_write_mutex().lock().await;
        let res = spawn_ec_guarded(guard, std::time::Duration::from_millis(10), || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            1u32
        })
        .await;
        assert!(res.is_err());
        let relock = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            ec_write_mutex().lock(),
        )
        .await
        .expect("mutex must be released on timeout while orphan still runs");
        drop(relock);
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
