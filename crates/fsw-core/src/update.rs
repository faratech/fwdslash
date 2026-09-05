//! Update check and atomic self-update for the GitHub-distributed flavor,
//! plus the flavor-independent state both flavors' updaters persist.
//!
//! The GitHub download/register path below is gated to
//! `packaged && !is_store_flavor()`: the Microsoft Store build updates through
//! the Store (`fwdslash update`, in the CLI, which owns every route) and must
//! never fetch a bundle from GitHub, and unpackaged dev builds should not ping
//! GitHub either. The *switch* is no longer flavor-gated —
//! [`update_check_allowed`] takes `packaged` and `auto_update` only, and the
//! flavor now only decides the switch's **default** ([`default_auto_update`]).
//!
//! Two-phase flow: `run_update_check` consults the GitHub latest-release API
//! (via the System32 `curl.exe`, a real child whose files land in the real
//! file system), and when a newer release exists it downloads the signed
//! `.msixbundle` and registers it with `Add-AppxPackage
//! -DeferRegistrationWhenPackagesAreInUse`. MSIX deployment is transactional:
//! the running version keeps running and the next launch is the new one.
//!
//! The download directory holds at most one bundle: a new download prunes any
//! other `*.msixbundle` first, and `sweep_update_directory` (called by
//! `fwdslash uninstall`) removes the directory outright. A registered bundle
//! is deliberately KEPT — deferred registration only applies at the next
//! launch, and [`pending_bundle_path`] is the apply-now path for it: the CLI's
//! `fwdslash update install` hands the bundle to an identity-less helper that
//! registers it with `-ForceApplicationShutdown` (the broker is resident, so a
//! deferred registration would never land) behind a watchdog task that brings
//! the product back afterwards. The bundle is deleted by the first check that
//! finds the running version current, which is the proof it applied.
//!
//! Registry values `AutoUpdate`/`LastUpdateCheck`/`AvailableUpdate` live under
//! the settings key, so — like every other value there — they are written
//! through `crate::settings_write`, never with the in-process registry API
//! (issue #52). Only that module knows a packaged write has to reach both the
//! real hive and the package's private one.

/// GitHub API endpoint for the latest release.
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/faratech/fwdslash/releases/latest";
/// One update check per day, at most.
pub const CHECK_CADENCE_SECS: u64 = 24 * 60 * 60;
pub const AUTO_UPDATE_VALUE: &str = "AutoUpdate";
pub const LAST_UPDATE_CHECK_VALUE: &str = "LastUpdateCheck";
pub const AVAILABLE_UPDATE_VALUE: &str = "AvailableUpdate";
/// Optional `REG_SZ` override for the install ladder, read by the CLI:
/// `auto` (the default when absent), `appinstall`, `store`, `winget`, `notify`.
/// The one-line escape hatch if a route ever has to be switched off in the
/// field without shipping a build.
pub const UPDATE_ROUTE_VALUE: &str = "UpdateRoute";
/// The file the identity-less update helper reports through, in
/// [`update_directory_path`]. The helper must never write HKCU — its writes
/// would land in the real hive while the packaged app reads the virtualized
/// one — so it writes one of `completed`, `paused` or `error:0x…` here and the
/// next packaged `update check`/`update status` folds that into the registry.
pub const UPDATE_RESULT_FILE: &str = "last-result.txt";

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

/// Normalizes the running version to the three-part shape [`parse_version`]
/// accepts, by dropping a four-part version's trailing group.
///
/// `package_version()` reports the MSIX version, which is always four parts
/// (`0.0.2.0`), while release tags are three (`v0.0.3`). Comparing them
/// directly made [`is_newer_version`] answer `false` for every packaged
/// install, so the GitHub flavor could never see a release. A three-part
/// input is returned unchanged.
#[must_use]
pub fn normalize_running_version(version: &str) -> String {
    let mut groups = version.split('.');
    match (
        groups.next(),
        groups.next(),
        groups.next(),
        groups.next(),
        groups.next(),
    ) {
        (Some(major), Some(minor), Some(patch), Some(_), None) => {
            format!("{major}.{minor}.{patch}")
        }
        _ => version.to_string(),
    }
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

/// The name suffix of the Microsoft Store submission artifact. Every release
/// carries two bundles: the Trusted Signing-signed GitHub flavor, which is the
/// one to install, and this unsigned Partner Center-identity bundle, which
/// `Add-AppxPackage` would reject outright. Skip it.
const STORE_BUNDLE_SUFFIX: &str = "-store-unsigned.msixbundle";

/// The first `browser_download_url` whose value ends in `.msixbundle` but is
/// not the unsigned Store submission artifact.
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
        if url.ends_with(".msixbundle") && !url.ends_with(STORE_BUNDLE_SUFFIX) {
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

/// The gate for an **automatic** check: a packaged build whose Automatic
/// updates switch is on. Both flavors now qualify — the Store flavor checks
/// through `StoreContext` and the GitHub flavor through the releases API — so
/// the flavor is no longer part of the question. It survives only in
/// [`default_auto_update`], which decides what the switch means when the user
/// has never touched it.
///
/// Unpackaged builds never check: there is nothing they could install.
#[must_use]
pub fn update_check_allowed(packaged: bool, auto_update: bool) -> bool {
    packaged && auto_update
}

/// What the Automatic updates switch means when nothing is stored: on for the
/// GitHub flavor (its only update route is this one), off for the Store flavor
/// (the Store already updates the app on its own schedule, and driving the
/// Store from inside the app is opt-in by decision).
#[must_use]
pub fn default_auto_update(store_flavor: bool) -> bool {
    !store_flavor
}

/// The Automatic updates switch, from the raw stored DWORD.
///
/// The stored value is the **inverted** flag — `1` means auto-update off — and
/// that is deliberately unchanged, so an explicit "off" recorded by an older
/// build still reads as off. Only the absent case consults the flavor.
#[must_use]
pub fn auto_update_from_value(stored: Option<u32>, store_flavor: bool) -> bool {
    match stored {
        Some(value) => value == 0,
        None => default_auto_update(store_flavor),
    }
}

/// "never" / "just now" / "N minutes ago" / "N hours ago" / "N days ago" — the
/// one-line last-check line the settings About card and `update status` print.
///
/// A `last` in the future (a clock that moved backwards) reads as "just now"
/// rather than as a negative age.
#[must_use]
pub fn format_last_check(now: u64, last: Option<u64>) -> String {
    let Some(last) = last else {
        return "never".to_string();
    };
    let elapsed = now.saturating_sub(last);
    let plural = |count: u64, unit: &str| -> String {
        if count == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{count} {unit}s ago")
        }
    };
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 60 * 60 {
        plural(elapsed / 60, "minute")
    } else if elapsed < 24 * 60 * 60 {
        plural(elapsed / (60 * 60), "hour")
    } else {
        plural(elapsed / (24 * 60 * 60), "day")
    }
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
        extract_bundle_url, extract_tag_name, is_newer_version, update_check_allowed, UpdateOutcome,
        AUTO_UPDATE_VALUE, AVAILABLE_UPDATE_VALUE, LAST_UPDATE_CHECK_VALUE, RELEASES_LATEST_URL,
    };
    use crate::{FSW_SETTINGS_KEY, is_store_flavor, package_version};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_registry::CURRENT_USER;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    /// Reads the Automatic updates switch. Absent or unreadable falls back to
    /// the flavor default (on for GitHub, off for the Store); an explicitly
    /// stored value always wins, and the stored encoding is unchanged, so
    /// nobody's recorded "off" flips.
    pub fn read_auto_update_enabled() -> bool {
        let stored = CURRENT_USER
            .open(FSW_SETTINGS_KEY)
            .ok()
            .and_then(|key| key.get_u32(AUTO_UPDATE_VALUE).ok());
        super::auto_update_from_value(stored, is_store_flavor())
    }

    pub fn set_auto_update_enabled(enabled: bool) -> Result<(), u32> {
        // Stored as the disabled flag (1 = auto-update off).
        crate::set_setting_u32(AUTO_UPDATE_VALUE, u32::from(!enabled))
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
        crate::delete_setting(AVAILABLE_UPDATE_VALUE)
    }

    pub fn clear_cached_update_tag() -> Result<(), u32> {
        crate::delete_setting(AVAILABLE_UPDATE_VALUE)
    }

    /// Records that a check attempt happened, whatever the outcome: an
    /// offline or rate-limited launch must not retry per launch.
    pub fn note_check_attempt() -> Result<(), u32> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        crate::set_setting_u64(LAST_UPDATE_CHECK_VALUE, now)
    }

    /// The Unix time of the last check attempt, or `None` when none has run.
    /// Public because both the CLI's `update status` and the settings About
    /// card render it through [`super::format_last_check`].
    #[must_use]
    pub fn last_update_check() -> Option<u64> {
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

    /// `%LOCALAPPDATA%\ForwardSlashWindows\update`: the only place a
    /// downloaded bundle, the staged update helper and the helper's
    /// [`super::UPDATE_RESULT_FILE`] are ever written. Public because the CLI
    /// stages the helper into it and reads the result file back out.
    #[must_use]
    pub fn update_directory_path() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|dir| dir.join("ForwardSlashWindows").join("update"))
    }

    fn bundle_name(tag: &str) -> String {
        format!("fwdslash-{tag}.msixbundle")
    }

    /// Deletes every `*.msixbundle` in the update directory except `keep`.
    /// One release's bundle is ~10 MB and nothing else prunes them.
    /// `keep = None` deletes all of them.
    fn prune_bundles(directory: &Path, keep: Option<&std::ffi::OsStr>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_bundle = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("msixbundle"));
            if is_bundle && path.file_name() != keep {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Deletes every downloaded bundle, keeping the (empty) directory.
    /// Called once the running version has caught up: whatever was downloaded
    /// has been applied, so no bundle is worth its ~10 MB any more.
    fn discard_downloaded_bundles() {
        if let Some(directory) = update_directory_path() {
            prune_bundles(&directory, None);
        }
    }

    /// Removes the whole update directory. Called by `fwdslash uninstall`, so
    /// an uninstall leaves no downloaded bundle behind. An absent directory
    /// is success.
    pub fn sweep_update_directory() -> Result<(), u32> {
        let Some(directory) = update_directory_path() else {
            return Ok(());
        };
        match std::fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.raw_os_error().unwrap_or(-1).cast_unsigned()),
        }
    }

    /// The downloaded bundle for the cached update tag, when it is still on
    /// disk. The settings app offers "Restart to update" only for this.
    #[must_use]
    pub fn pending_bundle_path() -> Option<PathBuf> {
        let tag = cached_update_tag()?;
        let bundle = update_directory_path()?.join(bundle_name(&tag));
        bundle.is_file().then_some(bundle)
    }

    fn download_bundle(url: &str, tag: &str) -> Option<PathBuf> {
        let update_dir = update_directory_path()?;
        std::fs::create_dir_all(&update_dir).ok()?;
        let destination = update_dir.join(bundle_name(tag));
        // Whatever an earlier release left here is dead weight.
        prune_bundles(&update_dir, destination.file_name());
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

    fn register_bundle(path: &Path) -> bool {
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
    /// `force` is the user pressing "Check now": it bypasses the daily
    /// cadence and the Automatic updates switch, but never the two facts —
    /// packaged, and not the Store flavor — that decide whether this route
    /// exists at all.
    pub fn run_update_check(force: bool) -> UpdateOutcome {
        let packaged = crate::has_package_identity();
        let auto_update = read_auto_update_enabled();
        // The gate no longer knows about flavors, so the flavor test the
        // GitHub route needs is spelled out here: this function downloads a
        // bundle from GitHub, which the Store flavor must never do.
        if is_store_flavor() || !(packaged && (force || update_check_allowed(packaged, auto_update)))
        {
            return UpdateOutcome::NotDue;
        }
        // Throttle first: even an offline or rate-limited attempt counts.
        let last = last_update_check();
        if !force && !super::check_is_due(last, now_unix()) {
            return UpdateOutcome::NotDue;
        }
        let _ = note_check_attempt();

        let Some(release_json) = fetch_release_json() else {
            return UpdateOutcome::Unavailable;
        };
        let Some(tag) = extract_tag_name(&release_json) else {
            return UpdateOutcome::Unavailable;
        };
        // `package_version()` is the four-part MSIX version; release tags are
        // three-part, and `parse_version` rejects four groups.
        let running_version = super::normalize_running_version(
            &package_version().unwrap_or_else(|| crate::FSW_VERSION.to_string()),
        );
        if !is_newer_version(&running_version, tag) {
            // A stale notice from an older check is no longer relevant, and a
            // bundle still on disk has either been applied (this process IS
            // the version it delivered) or names a release we are already past
            // — either way nothing can register it again.
            let _ = clear_cached_update_tag();
            discard_downloaded_bundles();
            return UpdateOutcome::UpToDate;
        }

        if auto_update
            && let Some(url) = extract_bundle_url(&release_json)
            && let Some(bundle) = download_bundle(url, tag)
            && register_bundle(&bundle)
        {
            // The bundle stays on disk: registration was deferred (this
            // process is part of the package it updates), so `restart_to_update`
            // still needs the file to force it through. The next check that
            // finds the running version current deletes it.
            let _ = set_cached_update_tag(tag);
            return UpdateOutcome::Ready(tag.to_string());
        }
        let _ = set_cached_update_tag(tag);
        UpdateOutcome::Ready(tag.to_string())
    }

    /// Records the tag (GitHub) or version (Store) an update would move to.
    /// Public because the CLI's Store routes persist it from outside this
    /// module — and, like every other Settings write, it goes through
    /// `settings_write` so a packaged write reaches both hives (issue #52).
    pub fn set_cached_update_tag(tag: &str) -> Result<(), u32> {
        crate::set_setting_string(AVAILABLE_UPDATE_VALUE, tag)
    }

    use std::os::windows::process::CommandExt;
}

#[cfg(windows)]
pub use windows_impl::{
    cached_update_tag, clear_cached_update_tag, dismiss_update, last_update_check,
    note_check_attempt, pending_bundle_path, read_auto_update_enabled, run_update_check,
    set_auto_update_enabled, set_cached_update_tag, sweep_update_directory, update_directory_path,
};

// Non-Windows stand-ins for the three entry points other crates call
// unconditionally, so `fwdslash uninstall` and the settings app's update card
// compile on every host. There is no update pipeline off Windows.

/// No update directory exists off Windows; sweeping one is a no-op success.
#[cfg(not(windows))]
pub fn sweep_update_directory() -> Result<(), u32> {
    Ok(())
}

/// Never a pending bundle off Windows.
#[cfg(not(windows))]
#[must_use]
pub fn pending_bundle_path() -> Option<std::path::PathBuf> {
    None
}

/// No update directory off Windows.
#[cfg(not(windows))]
#[must_use]
pub fn update_directory_path() -> Option<std::path::PathBuf> {
    None
}

/// No check has ever run off Windows.
#[cfg(not(windows))]
#[must_use]
pub fn last_update_check() -> Option<u64> {
    None
}
