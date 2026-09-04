//! Update check and atomic self-update for the GitHub-distributed flavor.
//!
//! Gated to `packaged && !is_store_flavor() && AutoUpdate`: the Microsoft
//! Store build updates through the Store and must never perform this check
//! (Store policy), and unpackaged dev builds should not ping GitHub either.
//!
//! Two-phase flow: `run_update_check` consults the GitHub latest-release API
//! (via the System32 `curl.exe`, a real child whose files land in the real
//! file system), and when a newer release exists it downloads the signed
//! `.msixbundle` and registers it with `Add-AppxPackage
//! -DeferRegistrationWhenPackagesAreInUse`. MSIX deployment is transactional:
//! the running version keeps running and the next launch is the new one.
//!
//! Registry values `AutoUpdate`/`LastUpdateCheck`/`AvailableUpdate` live under
//! the settings key and are read back only by the same packaged process, so
//! MSIX virtualization of these writes is self-consistent — unlike
//! `persist_disabled`, which must reach the real hive.

use crate::FSW_SETTINGS_KEY;

/// GitHub API endpoint for the latest release.
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/faratech/fwdslash/releases/latest";
/// One update check per day, at most.
pub const CHECK_CADENCE_SECS: u64 = 24 * 60 * 60;
pub const AUTO_UPDATE_VALUE: &str = "AutoUpdate";
pub const LAST_UPDATE_CHECK_VALUE: &str = "LastUpdateCheck";
pub const AVAILABLE_UPDATE_VALUE: &str = "AvailableUpdate";

/// A semantic `(major, minor, patch)` triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// Parses `vMAJOR.MINOR.PATCH` (leading `v`/`V` optional). Anything with a
/// fourth group, a `-`/`+` suffix, non-numeric groups, or fewer than three
/// groups is rejected — pre-release and build metadata never trigger updates.
#[must_use]
pub fn parse_version(text: &str) -> Option<Version> {
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    let mut groups = text.split('.');
    let major = groups.next()?;
    let minor = groups.next()?;
    let patch = groups.next()?;
    if groups.next().is_some() {
        return None;
    }
    let parse_group = |group: &str| -> Option<u64> {
        if group.is_empty() || !group.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        group.parse().ok()
    };
    Some(Version {
        major: parse_group(major)?,
        minor: parse_group(minor)?,
        patch: parse_group(patch)?,
    })
}

/// Strictly-greater comparison; equal or older is never an update.
#[must_use]
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => false,
    }
}

/// The `tag_name` string from a GitHub release JSON — the first occurrence,
/// read to the closing quote. `None` when absent.
#[must_use]
pub fn extract_tag_name(release_json: &str) -> Option<&str> {
    extract_json_string_field(release_json, "\"tag_name\":\"")
        .or_else(|| extract_json_string_field(release_json, "\"tag_name\": \""))
}

/// The first `browser_download_url` whose value ends in `.msixbundle`.
#[must_use]
pub fn extract_bundle_url(release_json: &str) -> Option<&str> {
    let compact = "\"browser_download_url\":\"";
    let spaced = "\"browser_download_url\": \"";
    let mut search_from = 0;
    while search_from < release_json.len() {
        // Whichever key spelling appears first from here on.
        let compact_at = release_json[search_from..].find(compact).map(|offset| search_from + offset);
        let spaced_at = release_json[search_from..].find(spaced).map(|offset| search_from + offset);
        let (key_len, start) = match (compact_at, spaced_at) {
            (Some(a), Some(b)) => if a <= b { (compact.len(), a) } else { (spaced.len(), b) },
            (Some(a), None) => (compact.len(), a),
            (None, Some(b)) => (spaced.len(), b),
            (None, None) => return None,
        };
        let value_start = start + key_len;
        let Some(value_end) = release_json[value_start..].find('"') else {
            return None;
        };
        let value_end = value_start + value_end;
        let url = &release_json[value_start..value_end];
        if url.ends_with(".msixbundle") {
            return Some(url);
        }
        search_from = value_end;
    }
    None
}

fn extract_json_string_field<'a>(json: &'a str, field_key: &str) -> Option<&'a str> {
    let start = json.find(field_key)? + field_key.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Whether an update check should run now. `None` for the last-check time
/// means a check has never run.
#[must_use]
pub fn check_is_due(last_check: Option<u64>, now: u64) -> bool {
    match last_check {
        Some(last) => now.saturating_sub(last) >= CHECK_CADENCE_SECS,
        None => true,
    }
}

/// The gate: only the packaged GitHub flavor with auto-update enabled checks.
pub fn update_check_allowed(packaged: bool, store_flavor: bool, auto_update: bool) -> bool {
    packaged && !store_flavor && auto_update
}

/// The outcome of one update-check attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Not due yet, disabled, or the gate says no: silently do nothing.
    NotDue,
    /// GitHub could not be reached or returned nothing usable: silently skip.
    Unavailable,
    /// The running version is current.
    UpToDate,
    /// A newer release exists; the tag names it. With auto-update on the
    /// bundle was already downloaded and registered.
    Ready(String),
}

#[cfg(windows)]
pub mod windows_impl {
    use super::{
        check_is_due, extract_bundle_url, extract_tag_name, is_newer_version, update_check_allowed,
        UpdateOutcome, AUTO_UPDATE_VALUE, AVAILABLE_UPDATE_VALUE, LAST_UPDATE_CHECK_VALUE,
        RELEASES_LATEST_URL,
    };
    use crate::{FSW_SETTINGS_KEY, is_store_flavor, package_version};
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_registry::CURRENT_USER;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    /// Reads the auto-update switch. Absent or unreadable means on: the
    /// default for the GitHub flavor.
    pub fn read_auto_update_enabled() -> bool {
        match CURRENT_USER.open(FSW_SETTINGS_KEY) {
            Ok(key) => match key.get_u32(AUTO_UPDATE_VALUE) {
                // The value stores the DISABLED flag inverted (1 = off).
                Ok(value) => value == 0,
                Err(_) => true,
            },
            Err(_) => true,
        }
    }

    pub fn set_auto_update_enabled(enabled: bool) -> Result<(), u32> {
        let key = CURRENT_USER
            .create(FSW_SETTINGS_KEY)
            .map_err(|e| e.code().0 as u32)?;
        // Stored as the disabled flag (1 = auto-update off).
        key.set_u32(AUTO_UPDATE_VALUE, u32::from(!enabled))
            .map_err(|e| e.code().0 as u32)
    }

    /// The persisted newer-release tag, if any.
    pub fn cached_update_tag() -> Option<String> {
        let key = CURRENT_USER.open(FSW_SETTINGS_KEY).ok()?;
        let tag = key.get_string(AVAILABLE_UPDATE_VALUE).ok()?;
        if tag.is_empty() { None } else { Some(tag) }
    }

    /// Clears the persisted update notice (user dismissed it, or a newer
    /// check found the running version current).
    pub fn dismiss_update() -> Result<(), u32> {
        let key = CURRENT_USER
            .open(FSW_SETTINGS_KEY)
            .map_err(|e| e.code().0 as u32)?;
        key.remove_value(AVAILABLE_UPDATE_VALUE)
            .map_err(|e| e.code().0 as u32)
    }

    pub fn clear_cached_update_tag() -> Result<(), u32> {
        let key = CURRENT_USER
            .open(FSW_SETTINGS_KEY)
            .map_err(|e| e.code().0 as u32)?;
        key.remove_value(AVAILABLE_UPDATE_VALUE)
            .map_err(|e| e.code().0 as u32)
    }

    /// Records that a check attempt happened, whatever the outcome: an
    /// offline or rate-limited launch must not retry per launch.
    pub fn note_check_attempt() -> Result<(), u32> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let key = CURRENT_USER
            .create(FSW_SETTINGS_KEY)
            .map_err(|e| e.code().0 as u32)?;
        key.set_u64(LAST_UPDATE_CHECK_VALUE, now)
            .map_err(|e| e.code().0 as u32)
    }

    fn last_check_time() -> Option<u64> {
        let key = CURRENT_USER.open(FSW_SETTINGS_KEY).ok()?;
        key.get_u64(LAST_UPDATE_CHECK_VALUE).ok()
    }

    fn fetch_release_json() -> Option<String> {
        let output = Command::new("curl.exe")
            .args(["-fsSL", "--connect-timeout", "5", "--max-time", "10", RELEASES_LATEST_URL])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn download_bundle(url: &str, tag: &str) -> Option<std::path::PathBuf> {
        let update_dir = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .map(|dir| dir.join("ForwardSlashWindows").join("update"))?;
        std::fs::create_dir_all(&update_dir).ok()?;
        let destination = update_dir.join(format!("fwdslash-{tag}.msixbundle"));
        let status = Command::new("curl.exe")
            .args(["-fsSL", "-o"])
            .arg(&destination)
            .arg(url)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if status.status.success() && destination.is_file() {
            Some(destination)
        } else {
            None
        }
    }

    fn register_bundle(path: &std::path::Path) -> bool {
        // -DeferRegistrationWhenPackagesAreInUse: this process is part of the
        // package being updated; registration defers to the next launch.
        let command = format!(
            "Add-AppxPackage -Path '{}' -DeferRegistrationWhenPackagesAreInUse",
            path.display()
        );
        Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &command])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Runs one update-check attempt. Never blocks longer than the curl
    /// timeouts; never surfaces an error to the caller — failures are
    /// `Unavailable`.
    pub fn run_update_check() -> UpdateOutcome {
        let packaged = crate::has_package_identity();
        let store_flavor = is_store_flavor();
        let auto_update = read_auto_update_enabled();
        if !update_check_allowed(packaged, store_flavor, auto_update) {
            return UpdateOutcome::NotDue;
        }
        // Throttle first: even an offline or rate-limited attempt counts.
        let last = last_check_time();
        if !super::check_is_due(last, now_unix()) {
            return UpdateOutcome::NotDue;
        }
        let _ = note_check_attempt();

        let Some(release_json) = fetch_release_json() else {
            return UpdateOutcome::Unavailable;
        };
        let Some(tag) = extract_tag_name(&release_json) else {
            return UpdateOutcome::Unavailable;
        };
        let running_version = package_version().unwrap_or_else(|| crate::FSW_VERSION.to_string());
        if !is_newer_version(&running_version, tag) {
            // A stale notice from an older check is no longer relevant.
            let _ = clear_cached_update_tag();
            return UpdateOutcome::UpToDate;
        }

        if auto_update {
            if let Some(url) = extract_bundle_url(&release_json) {
                if download_bundle(url, tag).is_some() && register_bundle(std::path::Path::new(&download_path(tag))) {
                    let _ = set_cached_update_tag(tag);
                    return UpdateOutcome::Ready(tag.to_string());
                }
            }
        }
        let _ = set_cached_update_tag(tag);
        UpdateOutcome::Ready(tag.to_string())
    }

    fn set_cached_update_tag(tag: &str) -> Result<(), u32> {
        let key = CURRENT_USER
            .create(FSW_SETTINGS_KEY)
            .map_err(|e| e.code().0 as u32)?;
        key.set_string(AVAILABLE_UPDATE_VALUE, tag)
            .map_err(|e| e.code().0 as u32)
    }

    fn download_path(tag: &str) -> String {
        format!(
            "{}\\ForwardSlashWindows\\update\\fwdslash-{tag}.msixbundle",
            std::env::var("LOCALAPPDATA").unwrap_or_default()
        )
    }

    use std::os::windows::process::CommandExt;
}

#[cfg(windows)]
pub use windows_impl::{
    cached_update_tag, dismiss_update, read_auto_update_enabled, run_update_check,
    set_auto_update_enabled,
};
