//! Native Windows file dialogs for settings export/import.
//!
//! Uses comdlg32 `GetSaveFileNameW`/`GetOpenFileNameW` directly through
//! windows-sys so no extra picker dependency is needed. Both entry points
//! block on a modal dialog and must run off the UI thread (callers use
//! `spawn_blocking_with_timeout`).

use std::path::PathBuf;

/// Long-path-aware dialog buffer; the legacy 260 cap no longer applies
/// when the buffer can hold the full return.
const FILE_BUFFER_LEN: usize = 32_768;

/// Shows the Save dialog for exporting settings. Returns `Ok(None)` when
/// the user cancels, `Ok(Some(path))` on confirm.
pub fn pick_export_dest() -> Result<Option<PathBuf>, String> {
    show_dialog(true)
}

/// Shows the Open dialog for importing settings. Same cancel contract.
pub fn pick_import_source() -> Result<Option<PathBuf>, String> {
    show_dialog(false)
}

fn show_dialog(save: bool) -> Result<Option<PathBuf>, String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, GetSaveFileNameW, OFN_DONTADDTORECENT,
        OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };

    let filter = toml_filter();
    let mut file_buf = vec![0u16; FILE_BUFFER_LEN];
    if save {
        // Seed the file name box; the user may still change it.
        let seed: Vec<u16> = "framework-crate-config.toml"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        file_buf[..seed.len()].copy_from_slice(&seed);
    }
    let def_ext: Vec<u16> = "toml\0".encode_utf16().collect();
    let title: Vec<u16> = (if save {
        "Export settings"
    } else {
        "Import settings"
    })
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();

    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    // NOCHANGEDIR: the config-dir fallback resolves "." against the
    // process CWD, so the dialog must never move it behind our back.
    // Note: no OVERWRITEPROMPT by design. Export uses exclusive create
    // (create_new) and refuses to truncate an existing file for safety, so
    // the dialog must not promise an overwrite it cannot deliver. Users
    // pick a new name; the AlreadyExists error tells them so.
    ofn.Flags = if save {
        OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT
    } else {
        OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST
    };
    if save {
        ofn.lpstrDefExt = def_ext.as_ptr();
    }
    // SAFETY: all pointers borrow live locals for the call; the struct was
    // zeroed so reserved/hook fields are null. The dialog is modal and
    // returns before any borrow ends.
    let ok = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    };
    if ok == 0 {
        // SAFETY: no arguments, always safe to call.
        let err = unsafe { CommDlgExtendedError() };
        if err == 0 {
            // User cancelled; not an error.
            return Ok(None);
        }
        return Err(format!("file dialog failed ({:#x})", err));
    }
    let len = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    Ok(Some(PathBuf::from(String::from_utf16_lossy(
        &file_buf[..len],
    ))))
}

/// Double-NUL-terminated filter list: TOML first, all-files fallback.
fn toml_filter() -> Vec<u16> {
    let mut v = Vec::new();
    for part in [
        "TOML Files (*.toml)",
        "*.toml",
        "All Files (*.*)",
        "*.*",
        "",
    ] {
        v.extend(part.encode_utf16());
        v.push(0);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_filter_is_double_nul_terminated() {
        let f = toml_filter();
        assert!(f.len() >= 2);
        assert_eq!(&f[f.len() - 2..], &[0, 0]);
        let text = String::from_utf16_lossy(&f);
        assert!(text.contains("*.toml"));
    }
}
