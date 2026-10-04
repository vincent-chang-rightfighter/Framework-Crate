//! Self update check: compares the running version against the latest
//! GitHub release tag. Notification only — nothing is downloaded or installed.

use std::os::windows::process::CommandExt;

/// API endpoint for this repository's latest release.
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/vincent-chang-rightfighter/Framework-Crate/releases/latest";

/// Project pages opened from the About page.
pub const PROJECT_URL: &str = "https://github.com/vincent-chang-rightfighter/Framework-Crate";
pub const RELEASES_URL: &str =
    "https://github.com/vincent-chang-rightfighter/Framework-Crate/releases";

/// Fetches the latest release tag (e.g. "v0.6.2") via curl.exe, like the
/// Modules download path. Returns None when unreachable.
pub fn fetch_latest_tag() -> Option<String> {
    let out = std::process::Command::new("curl.exe")
        .args([
            "-s",
            "-L",
            "--max-time",
            "15",
            "--retry",
            "1",
            "--retry-delay",
            "2",
            LATEST_RELEASE_API,
        ])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    for part in body.split(',') {
        let part = part.trim();
        let Some(key) = part.find("\"tag_name\"") else {
            continue;
        };
        let rest = &part[key + "\"tag_name\"".len()..];
        let Some(q1) = rest.find('"') else {
            continue;
        };
        let val = &rest[q1 + 1..];
        let Some(q2) = val.find('"') else {
            continue;
        };
        let tag = val[..q2].trim().to_string();
        if !tag.is_empty() {
            return Some(tag);
        }
    }
    None
}

/// Whether `latest_tag` (e.g. "v0.6.2") is newer than `current` (e.g.
/// "0.6.1"). Compares dot-separated numeric components; unknown formats
/// conservatively report not-newer rather than nagging on garbage.
pub fn is_newer(current: &str, latest_tag: &str) -> bool {
    fn parts(s: &str) -> Option<Vec<u64>> {
        let s = s.trim().trim_start_matches('v');
        if s.is_empty() {
            return None;
        }
        s.split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    }
    match (parts(current), parts(latest_tag)) {
        (Some(c), Some(l)) => {
            let len = c.len().max(l.len());
            for i in 0..len {
                let a = l.get(i).copied().unwrap_or(0);
                let b = c.get(i).copied().unwrap_or(0);
                if a != b {
                    return a > b;
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_is_newer() {
        assert!(is_newer("0.6.1", "v0.6.2"));
    }

    #[test]
    fn same_version_is_not_newer() {
        assert!(!is_newer("0.6.1", "v0.6.1"));
        assert!(!is_newer("0.6.1", "0.6.1"));
    }

    #[test]
    fn older_upstream_is_not_newer() {
        assert!(!is_newer("0.6.2", "v0.6.1"));
    }

    #[test]
    fn newer_minor_is_newer() {
        assert!(is_newer("0.6.1", "v0.7.0"));
    }

    #[test]
    fn garbage_never_nags() {
        assert!(!is_newer("0.6.1", "latest"));
        assert!(!is_newer("0.6.1", ""));
        assert!(!is_newer("", "v0.6.2"));
        assert!(!is_newer("0.6.1", "v0.6.x"));
    }
}
