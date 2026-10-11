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
/// An elevated process must never resolve system tools from PATH: any
/// user-writable directory earlier in PATH would win the lookup.
/// (winget is intentionally excluded: it is a store-managed execution
/// alias with a versioned path, not a System32 binary.)
pub fn system32_exe(name: &str) -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    root.join("System32").join(name)
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

/// Serialized EC write with timeout. Prefer over acquire + spawn pairs.
///
/// The write-mutex guard moves into a detached task and is held until the
/// blocking thread FINISHES, so a timed-out call can never be overtaken by
/// its own retry: EC writes stay FIFO even under timeouts. The caller still
/// gets its timeout promptly; the orphaned thread completes in the
/// background while still holding its place in line.
pub async fn spawn_ec_guarded<T>(
    guard: tokio::sync::MutexGuard<'static, ()>,
    timeout: std::time::Duration,
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _held = guard;
        let res = tokio::task::spawn_blocking(task).await;
        let _ = tx.send(res);
    });
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(Ok(val))) => Ok(val),
        Ok(Ok(Err(join_err))) => Err(format!("Task panicked: {}", join_err)),
        Ok(Err(_)) => Err("EC writer task vanished".to_string()),
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
