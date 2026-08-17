use std::sync::Arc;
use parking_lot::RwLock;

/// Read-lock an `Arc<RwLock<Arc<T>>>` and clone the inner `Arc`.
///
/// This is the idiomatic pattern used throughout the codebase for shared
/// state: each logical field is an `Arc<RwLock<Arc<T>>>`, and readers call
/// `read_lock` to get a cheap `Arc` snapshot without holding the lock.
pub fn read_lock<T>(lock: &Arc<RwLock<Arc<T>>>) -> Arc<T> {
    Arc::clone(&lock.read())
}

/// Execute a closure under a write-lock on an `Arc<RwLock<Arc<T>>>`.
///
/// The closure receives `&mut Arc<T>` and can replace the inner value via
/// `Arc::make_mut` or direct assignment. The lock is released when the
/// closure returns.
pub fn with_write_lock<T, R>(
    lock: &Arc<RwLock<Arc<T>>>,
    f: impl FnOnce(&mut Arc<T>) -> R,
) -> R {
    f(&mut lock.write())
}

/// Get current time in milliseconds since UNIX epoch.
///
/// Returns 0 if system time is before UNIX epoch (should not happen in practice).
pub fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Monotonic millisecond ticker since process start.
///
/// Use for every interval / idle / re-assert computation. Wall-clock time
/// (current_time_ms) is fine for absolute timestamps, but system clock
/// adjustments (NTP sync, manual changes, resume drift) stretch or compress
/// wall-clock deltas — a backward jump can defer a 60 s fan re-baseline, a
/// resume without the PBT event can miss the re-assert, and a forward jump
/// can prune the whole temp history. Instant is immune to all of that.
pub fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    start.elapsed().as_millis() as u64
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
}
