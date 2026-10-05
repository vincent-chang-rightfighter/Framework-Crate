#![cfg_attr(not(test), windows_subsystem = "windows")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "windows"))]
compile_error!("Framework Crate is Windows-only");

mod app;
mod background_task;
mod cli;
mod config;
mod config_save_task;
mod cpu_power;
mod curve_canvas;
mod fan_control;
mod probe;
mod style;
mod sub_state;
mod system_info;
mod temp_chart;
mod tray;
mod types;
mod update_check;
mod util;
mod views;

pub use app::{App, AppState, Message, SystemInfo};
pub use style::*;
pub use util::{read_lock, with_write_lock};

include!(concat!(env!("OUT_DIR"), "/icon_rgba.rs"));

/// Tokio executor with 2 workers for tray app; EC calls use blocking pool, workers cover async loop and iced tasks pinned to LP-E core.
struct SmallTokioExecutor {
    rt: tokio::runtime::Runtime,
}

impl iced::Executor for SmallTokioExecutor {
    fn new() -> Result<Self, iced::futures::io::Error> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(8)
            .enable_all()
            .build()
            .map_err(iced::futures::io::Error::other)
            .map(|rt| Self { rt })
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        drop(self.rt.spawn(future));
    }

    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(future)
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.rt.enter();
        f()
    }
}

/// Rotate once the log passes this size, keeping `app.log.1` .. `app.log.3`.
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const LOG_KEEP: usize = 3;

/// Log writes to wait before the first reopen retry, and the base for the
/// doubling backoff after that.
const REOPEN_RETRY_BASE: u32 = 16;

/// Writes to wait before retrying a reopen that has already failed
/// `reopen_failures` times.
///
/// Doubling keeps the cost bounded: a log that can never be reopened costs
/// O(log n) filesystem calls over the process lifetime rather than three per
/// log line.
fn reopen_retry_threshold(reopen_failures: u32) -> u64 {
    // Doubling with saturating_mul rather than a shift: `checked_shl` only
    // rejects a shift of 64 or more, so `16u64.checked_shl(60)` still returns
    // Some(0) because 2^64 truncates. That would turn a long outage into a
    // busy retry loop, which is the exact failure this backoff exists to avoid.
    let mut wait = REOPEN_RETRY_BASE as u64;
    for _ in 0..reopen_failures.min(64) {
        wait = wait.saturating_mul(2);
    }
    wait
}

/// Append-only log file that rotates itself once it grows past `MAX_LOG_BYTES`.
///
/// The handle is reopened on rotation because Windows cannot rename a file that
/// is still open, so a rename-based rotation would silently fail for as long as
/// tracing holds the file. Dropping the handle first means a failure in between
/// leaves the sink without one, so `push` retries the reopen with backoff
/// instead of staying silent until the process exits.
struct LogSink {
    path: std::path::PathBuf,
    state: std::sync::Mutex<LogState>,
}

struct LogState {
    /// `None` while the handle is closed for rotation, and after a reopen that
    /// failed.
    file: Option<std::fs::File>,
    written: u64,
    /// Consecutive failed reopen attempts; drives the retry backoff.
    reopen_failures: u32,
    /// Log writes seen since the last failed reopen.
    writes_since_failure: u32,
}

impl LogSink {
    fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let sink = Self {
            path,
            state: std::sync::Mutex::new(LogState {
                file: None,
                written: 0,
                reopen_failures: 0,
                writes_since_failure: 0,
            }),
        };
        // Scoped so the guard is released before `sink` is moved.
        {
            let mut state = sink.lock()?;
            sink.reopen(&mut state)?;
        }
        Ok(sink)
    }

    fn lock(&self) -> std::io::Result<std::sync::MutexGuard<'_, LogState>> {
        self.state
            .lock()
            .map_err(|_| std::io::Error::other("log mutex poisoned"))
    }

    /// Reopens the log, rotating first when it has grown past the cap.
    /// Must be called with the state lock held.
    fn reopen(&self, state: &mut LogState) -> std::io::Result<()> {
        // Drop the handle first so the rename below is not blocked on Windows.
        state.file = None;
        if state.written > MAX_LOG_BYTES {
            rotate_files(&self.path);
            // The previous contents are now in app.log.1. Clearing the counter
            // stops a reopen that keeps failing from rotating the same file
            // again on every retry.
            state.written = 0;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        state.written = file.metadata().map(|m| m.len()).unwrap_or(0);
        state.file = Some(file);
        Ok(())
    }

    /// The real write path, shared by both `Write` impls.
    fn push(&self, buf: &[u8]) -> std::io::Result<usize> {
        use std::io::Write;
        let mut state = self.lock()?;
        if state.file.is_none() {
            // The handle is gone: either mid-rotation, or a previous reopen
            // failed because the disk filled up or something held the file. A
            // logon-scheduled run is exactly when that is worth recovering,
            // because stderr is discarded and there is nowhere else to look.
            state.writes_since_failure = state.writes_since_failure.saturating_add(1);
            if u64::from(state.writes_since_failure) < reopen_retry_threshold(state.reopen_failures)
            {
                return Err(std::io::Error::other("log file unavailable"));
            }
            state.writes_since_failure = 0;
            if let Err(e) = self.reopen(&mut state) {
                state.reopen_failures = state.reopen_failures.saturating_add(1);
                return Err(e);
            }
            state.reopen_failures = 0;
        }
        let file = state
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("log file unavailable"))?;
        let n = file.write(buf)?;
        state.written += n as u64;
        if state.written > MAX_LOG_BYTES {
            self.reopen(&mut state)?;
        }
        Ok(n)
    }

    fn flush_inner(&self) -> std::io::Result<()> {
        use std::io::Write;
        if let Ok(mut state) = self.lock()
            && let Some(file) = state.file.as_mut()
        {
            file.flush()?;
        }
        Ok(())
    }
}

/// Shifts `app.log` to `app.log.1` and so on. The oldest backup is deleted first
/// because `fs::rename` cannot overwrite an existing file.
fn rotate_files(path: &std::path::Path) {
    let numbered = |i: usize| {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{i}"));
        std::path::PathBuf::from(name)
    };
    let _ = std::fs::remove_file(numbered(LOG_KEEP));
    for i in (1..LOG_KEEP).rev() {
        let _ = std::fs::rename(numbered(i), numbered(i + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.push(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_inner()
    }
}

/// `Arc<LogSink>` only implements `MakeWriter` when `&LogSink: Write`, since the
/// sink is shared by every tracing thread.
impl std::io::Write for &LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (**self).push(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (**self).flush_inner()
    }
}

static LOG: std::sync::OnceLock<Option<std::sync::Arc<LogSink>>> = std::sync::OnceLock::new();

/// Shared log sink, opened on first use. `None` when the file cannot be opened,
/// in which case logging falls back to stderr only.
fn log_sink() -> Option<&'static std::sync::Arc<LogSink>> {
    LOG.get_or_init(|| {
        let base = dirs::config_dir().or_else(dirs::data_local_dir)?;
        let path = base.join("framework-crate").join("app.log");
        match LogSink::open(path) {
            Ok(sink) => Some(std::sync::Arc::new(sink)),
            Err(e) => {
                eprintln!("Failed to open log file: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Lifecycle log for events outside the tracing subscriber, and the file sink
/// for tracing itself. Task Scheduler discards stderr, so anything printed there
/// is lost exactly when a logon launch misbehaves; this is the only record.
fn fallback_log(msg: &str) {
    // windows_subsystem hides stderr when double-clicked; also write to file
    eprintln!("{}", msg);
    use std::io::Write;
    if let Some(sink) = log_sink()
        && let Err(e) = writeln!(&**sink, "[{}] {}", chrono_like_timestamp(), msg)
    {
        eprintln!("Failed to write log: {e}");
    }
}

fn chrono_like_timestamp() -> String {
    // lightweight ISO 8601 timestamp without chrono dep
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    // days since 1970-01-01 to Y-M-D (simplified leap year calc)
    let mut y = 1970u32;
    let mut remaining = days;
    loop {
        let days_in_year =
            if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u32;
    for &d in &month_days {
        if remaining < d as u64 {
            break;
        }
        remaining -= d as u64;
        mo += 1;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        mo,
        remaining + 1,
        h,
        m,
        s
    )
}

/// Single-instance guard; prevents parallel EC I/O and tray/config races from second process.
fn acquire_single_instance(minimized: bool) -> Option<system_info::SingleInstanceGuard> {
    match system_info::SingleInstanceGuard::acquire("FrameworkCrateSingleInstance") {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            fallback_log(&format!(
                "startup: another instance holds the single-instance mutex, exiting (pid {})",
                std::process::id()
            ));
            // Only foreground existing window on manual launch; schtasks fires on unlock so stay silent.
            if !minimized {
                // Signal running instance to restore; second process cannot restore parked window itself.
                system_info::request_show_running_instance();
            }
            std::process::exit(0);
        }
        Err(e) => {
            // The mutex could not be created (e.g. handle exhaustion). Two
            // unguarded instances would race EC writes, so refuse to start
            // rather than run without the guard. Logged because a logon
            // launch has no console, and exit code 1 (not silent 0) so Task
            // Scheduler reports the failure instead of a clean run.
            fallback_log(&format!(
                "startup: single-instance mutex unavailable ({e}); refusing to start without the guard"
            ));
            std::process::exit(1);
        }
    }
}

fn main() {
    let minimized = std::env::args().any(|a| a == "--minimized");
    // Log the launch itself: this is the first thing a user checks when a
    // Task Scheduler start seems to have done nothing.
    fallback_log(&format!(
        "startup: pid={} minimized={} cwd={:?}",
        std::process::id(),
        minimized,
        std::env::current_dir().ok()
    ));
    // Hold guard for process lifetime.
    let _single_instance = acquire_single_instance(minimized);

    #[cfg(not(test))]
    {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        // Write to the same file as fallback_log. Task Scheduler gives a
        // scheduled process no console, so stderr-only logging would drop
        // every warning and error exactly when a logon launch needs diagnosis.
        match log_sink() {
            Some(sink) => {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_ansi(false)
                    .with_writer(sink.clone())
                    .try_init();
            }
            None => {
                let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
            }
        }
    }

    let window_icon =
        match iced::window::icon::from_rgba(ICON_RGBA.to_vec(), ICON_WIDTH, ICON_HEIGHT) {
            Ok(icon) => Some(icon),
            Err(e) => {
                // Run without icon instead of panicking before UI shows.
                tracing::error!("Failed to load window icon: {}", e);
                fallback_log(&format!("startup: failed to load window icon: {e}"));
                None
            }
        };

    fn app_title(_app: &App) -> String {
        "Framework Crate".to_string()
    }

    fn app_theme(_app: &App) -> iced::Theme {
        iced::Theme::Dark
    }

    let run_result = iced::application(move || App::new(minimized), App::update, App::view)
        .title(app_title)
        .subscription(App::subscription)
        .theme(app_theme)
        .executor::<SmallTokioExecutor>()
        .antialiasing(false)
        .window(iced::window::Settings {
            // NOTE: .window overrides earlier size calls; set size here.
            size: iced::Size::new(900.0, 613.0),
            resizable: false,
            icon: window_icon,
            exit_on_close_request: false,
            ..iced::window::Settings::default()
        })
        .run();

    match run_result {
        Ok(()) => fallback_log("shutdown: event loop returned normally"),
        Err(e) => {
            fallback_log(&format!("shutdown: event loop failed: {e}"));
            eprintln!("Failed to start application: {}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FanControlMode, sorted_sensor_list};
    use std::sync::atomic::Ordering;

    #[test]
    fn log_sink_rotates_oversized_file_and_stays_writable() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.log");
        let sink = LogSink::open(path.clone()).expect("open sink");

        let line = "x".repeat(1000);
        let mut sink_ref = &sink;
        for _ in 0..(MAX_LOG_BYTES / line.len() as u64 + 4) {
            writeln!(sink_ref, "{line}").expect("write");
        }

        // Rotation must actually happen even though the sink holds the file
        // open; a rename-based rotation would silently fail on Windows.
        assert!(
            dir.path().join("app.log.1").exists(),
            "oversized log should have rotated"
        );
        assert!(
            std::fs::metadata(&path).expect("stat").len() <= MAX_LOG_BYTES,
            "fresh log should be under the cap"
        );

        writeln!(sink_ref, "after rotation").expect("write after rotation");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("after rotation"),
            "sink must keep writing to the reopened file"
        );
    }

    #[test]
    fn reopen_retry_threshold_doubles_per_failure() {
        let base = REOPEN_RETRY_BASE as u64;
        assert_eq!(reopen_retry_threshold(0), base);
        assert_eq!(reopen_retry_threshold(1), base * 2);
        assert_eq!(reopen_retry_threshold(2), base * 4);
        assert_eq!(reopen_retry_threshold(5), base * 32);
    }

    #[test]
    fn reopen_retry_threshold_never_shrinks_or_wraps() {
        // A shift that would overflow must saturate rather than wrap to a small
        // value, which would turn a long outage into a busy retry loop.
        assert!(reopen_retry_threshold(31) > reopen_retry_threshold(30));
        assert!(reopen_retry_threshold(32) > reopen_retry_threshold(31));
        assert_eq!(reopen_retry_threshold(u32::MAX), u64::MAX);
        for n in 0..64 {
            assert!(
                reopen_retry_threshold(n) >= REOPEN_RETRY_BASE as u64,
                "threshold dropped below the base at {n}"
            );
        }
    }

    #[test]
    fn sink_recovers_after_a_failed_reopen() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.log");
        let sink = LogSink::open(path.clone()).expect("open sink");

        writeln!(&sink, "before").expect("write");
        // Simulate the reopen that rotation depends on failing: the handle is
        // dropped and the reopen cannot open the file back up.
        {
            let mut state = sink.lock().expect("lock");
            state.file = None;
            state.written = 0;
            state.reopen_failures = 0;
            state.writes_since_failure = 0;
        }

        // Writes 1..=REOPEN_RETRY_BASE-1 report the sink unavailable, and the
        // REOPEN_RETRY_BASE'th one reopens, because the retry fires when the
        // counter reaches the threshold rather than exceeds it.
        let mut sink_ref = &sink;
        for i in 0..REOPEN_RETRY_BASE - 1 {
            let err = write!(sink_ref, "lost {i}").expect_err("should be unavailable");
            assert_eq!(err.to_string(), "log file unavailable");
        }
        writeln!(sink_ref, "recovered").expect("write after backoff");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("recovered"),
            "sink must recover once the reopen is retried"
        );
        // A successful reopen clears the failure history, so the next outage
        // starts from the base window again rather than an ever-growing one.
        assert_eq!(
            sink.lock().expect("lock").reopen_failures,
            0,
            "success must reset the backoff"
        );
    }

    #[test]
    fn sink_backs_off_instead_of_retrying_every_write() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = LogSink::open(dir.path().join("app.log")).expect("open sink");
        {
            let mut state = sink.lock().expect("lock");
            state.file = None;
            // Pretend several reopen attempts have already failed.
            state.reopen_failures = 3;
            state.writes_since_failure = 0;
        }
        let threshold = reopen_retry_threshold(3);
        let mut sink_ref = &sink;
        for _ in 0..threshold - 1 {
            write!(sink_ref, "x").expect_err("should still be backing off");
        }
        writeln!(sink_ref, "done").expect("retried on the threshold write");
    }

    #[test]
    fn sink_backoff_grows_after_repeated_reopen_failures() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.log");
        let sink = LogSink::open(path.clone()).expect("open sink");

        // Drop the handle first: Windows opens with FILE_SHARE_DELETE, so
        // removing the file would otherwise succeed while the sink keeps
        // writing to the unlinked handle.
        {
            let mut state = sink.lock().expect("lock");
            state.file = None;
        }
        // Replace the log with a directory: reopening the path now fails
        // because the name is taken by something that is not a file.
        std::fs::remove_file(&path).expect("remove log");
        std::fs::create_dir(&path).expect("create dir in its place");

        let mut sink_ref = &sink;
        // First window: writes 1..=REOPEN_RETRY_BASE-1 are dropped, the
        // REOPEN_RETRY_BASE'th attempts the reopen and fails.
        for _ in 0..REOPEN_RETRY_BASE - 1 {
            write!(sink_ref, "x").expect_err("backing off");
        }
        write!(sink_ref, "x").expect_err("first reopen attempt fails");
        // The failure has to be counted, otherwise the next window would still
        // be REOPEN_RETRY_BASE long. The error alone cannot show this: a
        // retry that fires too early also returns Err.
        assert_eq!(
            sink.lock().expect("lock").reopen_failures,
            1,
            "a failed reopen must be counted"
        );

        // The next window must be twice as long, which only happens if the
        // failure was counted. Otherwise the sink retries on every 16th write.
        let second = reopen_retry_threshold(1);
        assert_eq!(second, (REOPEN_RETRY_BASE as u64) * 2);
        for _ in 0..second - 1 {
            write!(sink_ref, "x").expect_err("still backing off after the first failure");
        }
        write!(sink_ref, "x").expect_err("second reopen attempt fails");
        assert_eq!(
            sink.lock().expect("lock").reopen_failures,
            2,
            "each failed reopen must be counted"
        );

        // And the window doubles again rather than resetting.
        assert_eq!(reopen_retry_threshold(2), (REOPEN_RETRY_BASE as u64) * 4);
    }

    #[test]
    fn reopen_clears_the_size_counter_after_rotating() {
        // Point the sink at a log whose parent directory is then removed. Both
        // the rotation and the reopen fail from there on, deterministically:
        // creating a file needs its parent, and renaming needs the entry to go.
        // reopen must still clear `written`, or every later retry would think
        // the log is still oversized and shift the backups again.
        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().join("gone");
        std::fs::create_dir(&parent).expect("make parent");
        let sink = LogSink::open(parent.join("app.log")).expect("open sink");
        // Release the handle before removing the tree, otherwise Windows keeps
        // the directory alive.
        {
            let mut state = sink.lock().expect("lock");
            state.file = None;
        }
        std::fs::remove_dir_all(&parent).expect("remove parent");

        let mut state = sink.lock().expect("lock");
        state.written = MAX_LOG_BYTES + 1;

        let err = sink.reopen(&mut state);
        assert!(err.is_err(), "reopen must fail once the parent is gone");
        assert_eq!(
            state.written, 0,
            "a rotated log must not look oversized again"
        );
        assert!(state.file.is_none(), "a failed reopen leaves no handle");
    }

    #[test]
    fn sink_resets_the_backoff_after_a_reopen_eventually_succeeds() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.log");
        let sink = LogSink::open(path.clone()).expect("open sink");
        {
            let mut state = sink.lock().expect("lock");
            state.file = None;
        }
        // Occupy the name with a directory so the reopen cannot open it.
        std::fs::remove_file(&path).expect("remove log");
        std::fs::create_dir(&path).expect("create dir in its place");

        let mut sink_ref = &sink;
        for _ in 0..REOPEN_RETRY_BASE - 1 {
            write!(sink_ref, "x").expect_err("backing off");
        }
        write!(sink_ref, "x").expect_err("reopen fails while blocked");
        assert_eq!(sink.lock().expect("lock").reopen_failures, 1);

        // Free the name. The next window is 32 writes because the failure was
        // counted; the last of those retries reopens successfully and must
        // clear the history so a later outage starts from the base window.
        std::fs::remove_dir(&path).expect("remove dir");
        for _ in 0..reopen_retry_threshold(1) - 1 {
            write!(sink_ref, "x").expect_err("still backing off");
        }
        writeln!(sink_ref, "back").expect("reopen now succeeds");
        assert_eq!(
            sink.lock().expect("lock").reopen_failures,
            0,
            "a successful reopen must clear the failure history"
        );
    }

    #[test]
    fn sorted_sensor_list_empty_selected_uses_all_keys() {
        let selected: Vec<String> = vec![];
        let keys = vec!["C".into(), "A".into(), "B".into()];
        let result = sorted_sensor_list(&selected, &keys);
        assert_eq!(result, vec!["C", "A", "B"]);
    }

    #[test]
    fn sorted_sensor_list_filters_stale_names() {
        let selected = vec!["A".into(), "Gone".into(), "C".into()];
        let keys = vec!["A".into(), "B".into(), "C".into()];
        let result = sorted_sensor_list(&selected, &keys);
        assert_eq!(result, vec!["A", "C"]);
    }

    #[test]
    fn sorted_sensor_list_preserves_key_order() {
        let selected = vec!["C".into(), "A".into(), "B".into()];
        let keys = vec!["A".into(), "B".into(), "C".into()];
        let result = sorted_sensor_list(&selected, &keys);
        assert_eq!(result, vec!["A", "B", "C"]);
    }

    #[test]
    fn config_round_trip_toml() {
        let config = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Manual,
                manual: Some(types::ManualConfig { duty_pct: 60 }),
                curve: None,
                ..Default::default()
            },
            battery: types::BatteryConfig::default(),
            telemetry: types::TelemetryConfig::default(),
        };
        let serialized = toml::to_string(&config).expect("serialize");
        let deserialized: types::Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn config_round_trip_with_curve() {
        let config = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Curve,
                manual: None,
                curve: Some(types::GlobalCurveConfig {
                    poll_ms: 1000,
                    curve: types::CurveConfig {
                        sensors: vec![],
                        points: vec![[30, 10], [50, 50], [70, 90]],
                        hysteresis_c: 2,
                        rate_limit_pct_per_step: 5,
                        rate_limit_down_pct_per_step: None,
                    },
                }),
                ..Default::default()
            },
            battery: types::BatteryConfig::default(),
            telemetry: types::TelemetryConfig::default(),
        };
        let serialized = toml::to_string(&config).expect("serialize");
        let deserialized: types::Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(config, deserialized);
    }

    #[test]
    fn config_save_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("test_config.toml");

        let original = types::Config {
            fan: types::FanControlConfig {
                mode: FanControlMode::Manual,
                manual: Some(types::ManualConfig { duty_pct: 45 }),
                curve: None,
                ..Default::default()
            },
            battery: types::BatteryConfig {
                charge_limit_max_pct: Some(types::SettingU8 {
                    enabled: true,
                    value: 80,
                }),
            },
            telemetry: types::TelemetryConfig {
                poll_ms: 1000,
                ui_refresh_ms: 200,
                selected_sensors: vec!["CPU".into(), "Battery".into()],
            },
        };

        let serialized = toml::to_string(&original).expect("serialize");
        std::fs::write(&config_path, &serialized).expect("write");
        let content = std::fs::read_to_string(&config_path).expect("read");
        let deserialized: types::Config = toml::from_str(&content).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    // Serialize tests touching config to avoid cross-test pollution.
    static APP_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn app_config_lock() -> std::sync::MutexGuard<'static, ()> {
        APP_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn settings_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert!(!app.show_settings);
        let _ = app.update(Message::SettingsToggled);
        assert!(app.show_settings);
        let _ = app.update(Message::SettingsToggled);
        assert!(!app.show_settings);
    }

    #[tokio::test]
    async fn sensor_settings_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert!(!app.show_sensor_settings);
        let _ = app.update(Message::ToggleSensorSettings);
        assert!(app.show_sensor_settings);
        let _ = app.update(Message::ToggleSensorSettings);
        assert!(!app.show_sensor_settings);
    }

    #[tokio::test]
    async fn chart_window_changed_validates_option() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert_eq!(app.chart_window_seconds, 30, "default window is 30s");
        let _ = app.update(Message::ChartWindowChanged(60));
        assert_eq!(app.chart_window_seconds, 60);
        let _ = app.update(Message::ChartWindowChanged(45));
        assert_eq!(
            app.chart_window_seconds, 60,
            "invalid windows must be rejected"
        );
    }

    #[tokio::test]
    async fn battery_details_toggle_flips_flag() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        assert!(!app.show_battery_details);
        let _ = app.update(Message::ToggleBatteryDetails);
        assert!(app.show_battery_details);
        let _ = app.update(Message::ToggleBatteryDetails);
        assert!(!app.show_battery_details);
    }

    #[tokio::test]
    async fn fan_mode_curve_creates_default_config() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanModeChanged(FanControlMode::Curve));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.mode, FanControlMode::Curve);
        assert!(cfg.fan.curve.is_some());
    }

    #[tokio::test]
    async fn fan_mode_disabled_sets_disabled() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanModeChanged(FanControlMode::Curve));
        let _ = app.update(Message::FanModeChanged(FanControlMode::Disabled));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.mode, FanControlMode::Disabled);
    }

    #[tokio::test]
    async fn fan_manual_duty_is_clamped() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanDutyChanged(5));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(5));
    }

    #[tokio::test]
    async fn fan_manual_duty_above_max_clamps() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanDutyChanged(200));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(100));
    }

    #[tokio::test]
    async fn fan_manual_duty_in_range_unchanged() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanDutyChanged(60));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.fan.manual.as_ref().map(|m| m.duty_pct), Some(60));
    }

    #[tokio::test]
    async fn charge_limit_toggle_creates_default() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::ChargeLimitToggled(true));
        let cfg = read_lock(&app.state.lifecycle.config);
        let limit = cfg.battery.charge_limit_max_pct;
        assert!(limit.is_some());
        assert!(limit.unwrap().enabled);
    }

    #[tokio::test]
    async fn charge_limit_changes_value() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::ChargeLimitChanged(80));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.battery.charge_limit_max_pct.map(|l| l.value), Some(80));
    }

    #[tokio::test]
    async fn poll_rate_changes_config_and_atomic() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::PollRateChanged(1000));
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.telemetry.poll_ms, 1000);
        assert_eq!(app.state.lifecycle.poll_ms.load(Ordering::Relaxed), 1000);
    }

    #[tokio::test]
    async fn ui_refresh_rate_changes_interval() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::UiRefreshRateChanged(200));
        assert_eq!(app.tick_interval_ms, 200);
        let cfg = read_lock(&app.state.lifecycle.config);
        assert_eq!(cfg.telemetry.ui_refresh_ms, 200);
    }

    #[test]
    fn validate_battery_charge_limit_clamps_high() {
        let mut cfg = types::Config::default();
        cfg.battery.charge_limit_max_pct = Some(types::SettingU8 {
            enabled: true,
            value: 150,
        });
        cfg.validate();
        assert_eq!(cfg.battery.charge_limit_max_pct.unwrap().value, 100);
    }

    #[test]
    fn validate_battery_charge_limit_clamps_low() {
        let mut cfg = types::Config::default();
        cfg.battery.charge_limit_max_pct = Some(types::SettingU8 {
            enabled: true,
            value: 10,
        });
        cfg.validate();
        assert_eq!(cfg.battery.charge_limit_max_pct.unwrap().value, 25);
    }

    #[tokio::test]
    async fn close_request_in_non_manual_does_not_show_warning() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::FanModeChanged(FanControlMode::Disabled));
        let id = iced::window::Id::unique();
        let _task = app.update(Message::CloseRequested(id));
        assert!(!app.show_quit_warning);
    }

    #[tokio::test]
    async fn quit_cancel_hides_warning() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::QuitCanceled);
        assert!(!app.show_quit_warning);
    }

    #[tokio::test]
    async fn quit_duty_changes_clamped() {
        let _guard = app_config_lock();
        let (mut app, _) = App::new(false);
        let _ = app.update(Message::QuitDutyChanged(5));
        assert_eq!(app.quit_duty_value, 5);
        let _ = app.update(Message::QuitDutyChanged(150));
        assert_eq!(app.quit_duty_value, 100);
        let _ = app.update(Message::QuitDutyChanged(50));
        assert_eq!(app.quit_duty_value, 50);
    }
}
