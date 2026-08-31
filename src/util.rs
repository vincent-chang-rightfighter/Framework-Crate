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

/// Global serialized EC write mutex to prevent out-of-order hardware writes.
pub fn ec_write_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Acquires EC write lock with contention logging; avoids silent 1.5s stall.
/// Tries 500ms, logs if contended, then waits.
pub async fn acquire_ec_write() -> tokio::sync::MutexGuard<'static, ()> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        ec_write_mutex().lock(),
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            tracing::warn!("EC write contended >500ms, waiting");
            ec_write_mutex().lock().await
        }
    }
}

/// Tries to acquire EC write lock with timeout; avoids holding 1.5s blocking across all writers.
/// Returns `None` if contended, caller should retry after short delay.
#[allow(dead_code)]
pub async fn try_acquire_ec_write_timeout(
    timeout: std::time::Duration,
) -> Option<tokio::sync::MutexGuard<'static, ()>> {
    tokio::time::timeout(timeout, ec_write_mutex().lock())
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
