/// Get current time in milliseconds since UNIX epoch.
///
/// Returns 0 if system time is before UNIX epoch (should not happen in practice).
pub fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Get current time in milliseconds since UNIX epoch as i64.
///
/// Useful for temp_chart::TempSample which uses i64 timestamps.
pub fn current_time_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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
    fn current_time_ms_i64_returns_positive_value() {
        let ts = current_time_ms_i64();
        assert!(ts > 0);
    }

    #[test]
    fn current_time_ms_and_i64_are_consistent() {
        let ts_u64 = current_time_ms();
        let ts_i64 = current_time_ms_i64();
        // They might differ by a few ms due to timing, so just check same order of magnitude
        assert!((ts_u64 as i64 - ts_i64).abs() < 100);
    }
}
