// Config directory resolution and validation.
//
// Split from config.rs: path lookup, the debug/test directory override,
// and reparse-point detection live here.
use std::path::PathBuf;

/// Serializes tests that mutate the process-global `FRAMEWORK_CONTROL_CONFIG_DIR`
/// env var; parallel tests reading/writing different temp dirs would race otherwise.
#[cfg(test)]
pub(crate) static CONFIG_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn config_path() -> Result<PathBuf, String> {
    let config_dir = {
        #[cfg(any(debug_assertions, test))]
        {
            if let Some(os) = std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR") {
                let p = PathBuf::from(&os);
                validate_config_dir(&p)?;
                p
            } else {
                default_config_dir()
            }
        }
        #[cfg(not(any(debug_assertions, test)))]
        {
            if std::env::var_os("FRAMEWORK_CONTROL_CONFIG_DIR").is_some() {
                tracing::warn!(
                    "FRAMEWORK_CONTROL_CONFIG_DIR is ignored in release builds; using the default config directory"
                );
            }
            default_config_dir()
        }
    };
    // create_dir_all is idempotent and cheap; avoid caching to prevent stale test dir reuse
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config directory {}: {}",
            config_dir.display(),
            e
        )
    })?;
    // ensure it is a directory, not a file/symlink placed by attacker
    if let Ok(meta) = std::fs::symlink_metadata(&config_dir) {
        if !meta.is_dir() {
            return Err(format!(
                "config directory is not a directory: {}",
                config_dir.display()
            ));
        }
        if meta.file_type().is_symlink() || dir_has_reparse_point(&config_dir) {
            return Err(format!(
                "config directory is a symlink or junction, refusing: {}",
                config_dir.display()
            ));
        }
    }
    // Canonicalize to collapse `..`/nested junctions in parents so the
    // joined config.toml cannot escape the validated directory. Failure
    // to canonicalize (e.g. race) fails closed.
    let canonical = std::fs::canonicalize(&config_dir).map_err(|e| {
        format!(
            "Failed to canonicalize config directory {}: {}",
            config_dir.display(),
            e
        )
    })?;
    Ok(canonical.join("config.toml"))
}

#[cfg(any(debug_assertions, test))]
fn validate_config_dir(path: &std::path::Path) -> Result<(), String> {
    use std::path::Component;
    let s = path.to_string_lossy();
    // Reject Win32 extended-length / device paths that bypass normalization.
    if s.contains(r"\\?\") || s.contains(r"\\.\") {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must not contain \\\\?\\ or \\\\.\\ prefix: {}",
            path.display()
        ));
    }
    if !path.is_absolute() {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must be absolute: {}",
            path.display()
        ));
    }
    // Reject parent-dir components that could escape after join.
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must not contain '..': {}",
            path.display()
        ));
    }
    // Reject UNC paths: a network config dir leaks NTLM auth and lives
    // outside every local ACL check below.
    if path.components().any(|c| {
        matches!(
            c,
            Component::Prefix(p)
                if matches!(
                    p.kind(),
                    std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)
                )
        )
    }) {
        return Err(format!(
            "FRAMEWORK_CONTROL_CONFIG_DIR must not be a UNC path: {}",
            path.display()
        ));
    }
    Ok(())
}

/// True when the path is a symlink or junction: both carry
/// FILE_ATTRIBUTE_REPARSE_POINT, while Rust's is_symlink misses junctions.
/// Shared with the modules downloader, which stages to a sibling directory.
pub(crate) fn dir_has_reparse_point(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };
    use windows_sys::core::PCWSTR;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: null-terminated path buffer.
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr() as PCWSTR) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// True only for symlinks and junctions, i.e. reparse tags that redirect the
/// path elsewhere. Cloud placeholders (OneDrive and similar) carry the
/// reparse attribute but are ordinary files for the caller's purpose, so the
/// broader [`dir_has_reparse_point`] would wrongly block exporting settings
/// into a synced folder.
pub(crate) fn is_symlink_or_junction(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileAttributeTagInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::SystemServices::{
        IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: null-terminated path; the handle is closed before returning.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    // SAFETY: handle came from CreateFileW above.
    unsafe { CloseHandle(handle) };
    ok != 0
        && (info.ReparseTag == IO_REPARSE_TAG_SYMLINK
            || info.ReparseTag == IO_REPARSE_TAG_MOUNT_POINT)
}

fn default_config_dir() -> PathBuf {
    #[cfg(test)]
    {
        // Keep tests off the real user config.
        std::env::temp_dir().join("framework-crate-tests")
    }
    #[cfg(not(test))]
    {
        let base = dirs::config_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("framework-crate")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_config_dir_rejects_unc() {
        assert!(validate_config_dir(std::path::Path::new(r"\\server\share\cfg")).is_err());
        assert!(validate_config_dir(std::path::Path::new(r"C:\cfg")).is_ok());
    }

    #[test]
    fn reparse_check_passes_plain_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir_has_reparse_point(dir.path()));
    }

    #[test]
    fn symlink_or_junction_ignores_plain_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.toml");
        std::fs::write(&file, "x = 1").unwrap();
        assert!(!is_symlink_or_junction(dir.path()));
        assert!(!is_symlink_or_junction(&file));
        assert!(!is_symlink_or_junction(&dir.path().join("missing")));
    }

    #[test]
    fn symlink_or_junction_detects_symlinks_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.toml");
        std::fs::write(&original, "x = 1").unwrap();
        let link = dir.path().join("link.toml");
        if std::os::windows::fs::symlink_file(&original, &link).is_err() {
            return; // needs Developer Mode or elevation
        }
        assert!(is_symlink_or_junction(&link));
    }
}
