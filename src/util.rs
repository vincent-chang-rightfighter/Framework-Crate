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

/// Hard cap for config/BIOS snapshots; both are small TOML documents. The
/// elevated app must not be made to allocate an arbitrary user-sized file.
const MAX_VALIDATED_FILE_BYTES: u64 = 1024 * 1024;

/// Reads a regular file through one validated handle. Rejects reparse points,
/// directories and files with multiple hard links before reading any data.
pub(crate) fn read_validated_file_bytes(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to read a reparse point",
        ));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to read a directory",
        ));
    }
    if info.nNumberOfLinks > 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to read a file with multiple hard links",
        ));
    }
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    if size > MAX_VALIDATED_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "refusing to read an oversized file",
        ));
    }
    let mut content = Vec::with_capacity(size.min(64 * 1024) as usize);
    file.take(MAX_VALIDATED_FILE_BYTES + 1)
        .read_to_end(&mut content)?;
    if content.len() as u64 > MAX_VALIDATED_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file grew beyond the size limit while reading",
        ));
    }
    Ok(content)
}

pub(crate) fn read_validated_file(path: &std::path::Path) -> std::io::Result<String> {
    let bytes = read_validated_file_bytes(path)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Serialized CPU power operation mutex to prevent concurrent Apply/Reset/Sync races.
pub fn cpu_power_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub const EC_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
pub const PAWNIO_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);
/// Sync-changing operations do stop-sync + write/refresh + start-sync, so
/// they get more headroom than a single PawnIO read.
pub const PAWNIO_SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
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

    #[test]
    fn read_validated_file_reads_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "value = 1").unwrap();
        assert_eq!(read_validated_file(&path).unwrap(), "value = 1");
    }

    #[test]
    fn read_validated_file_rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.toml");
        std::fs::write(&path, vec![b'x'; MAX_VALIDATED_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(
            read_validated_file(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn read_validated_file_rejects_hardlinks_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.toml");
        std::fs::write(&original, "value = 1").unwrap();
        let link = dir.path().join("link.toml");
        std::fs::hard_link(&original, &link).unwrap();
        assert_eq!(
            read_validated_file(&link).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            read_validated_file(dir.path()).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn read_validated_file_rejects_symlinks_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.toml");
        std::fs::write(&original, "value = 1").unwrap();
        let link = dir.path().join("link.toml");
        if std::os::windows::fs::symlink_file(&original, &link).is_err() {
            return;
        }
        assert_eq!(
            read_validated_file(&link).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
