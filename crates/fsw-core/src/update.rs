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
//! Two-phase flow: `run_update_check` queries the GitHub release API through
//! `WinHTTP`, binds the response to one tag-derived asset and its SHA-256 digest,
//! and stages that verified bundle. Deployment is deliberately owned by the
//! CLI helper, which uses the native package deployment API.
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

/// A security-relevant update verification failure.  The text is deliberately
/// stable and contains no URL, filesystem path, certificate subject, or raw
/// HRESULT that an untrusted endpoint could turn into user-facing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateVerificationError {
    /// The staged update files or their protected location could not be read.
    IoFailure,
    UntrustedOrigin,
    AssetNameMismatch,
    SignatureFailure,
    PublisherMismatch,
    IdentityMismatch,
    ArchitectureMismatch,
    VersionMismatch,
    DigestMismatch,
    /// Package deployment did not complete within the bounded helper lifetime.
    TimedOut,
}

impl UpdateVerificationError {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::IoFailure => "The staged update files could not be read safely.",
            Self::UntrustedOrigin => "The update did not come from the trusted release origin.",
            Self::AssetNameMismatch => "The release asset name does not match the release tag.",
            Self::SignatureFailure => "The update package signature could not be verified.",
            Self::PublisherMismatch => {
                "The update package publisher does not match Forward Slash Windows."
            }
            Self::IdentityMismatch => {
                "The update package identity does not match Forward Slash Windows."
            }
            Self::ArchitectureMismatch => {
                "The update package architecture is not supported by this device."
            }
            Self::VersionMismatch => "The update package version does not match the release tag.",
            Self::DigestMismatch => "The downloaded update does not match the release digest.",
            Self::TimedOut => "The update package installation timed out.",
        }
    }

    /// Stable, non-sensitive machine code for helper result reporting.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::IoFailure => "io-failure",
            Self::UntrustedOrigin => "untrusted-origin",
            Self::AssetNameMismatch => "asset-name-mismatch",
            Self::SignatureFailure => "signature-failure",
            Self::PublisherMismatch => "publisher-mismatch",
            Self::IdentityMismatch => "identity-mismatch",
            Self::ArchitectureMismatch => "architecture-mismatch",
            Self::VersionMismatch => "version-mismatch",
            Self::DigestMismatch => "digest-mismatch",
            Self::TimedOut => "timed-out",
        }
    }
}

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

/// Parses the one canonical release identifier accepted at trust boundaries.
/// Store/GitHub inputs must use lowercase `vMAJOR.MINOR.PATCH` exactly.
#[must_use]
pub fn parse_release_tag(text: &str) -> Option<Version> {
    let version = text.strip_prefix('v')?;
    if version.starts_with(['v', 'V']) {
        return None;
    }
    parse_version(version)
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

fn parse_package_version(text: &str) -> Option<[u16; 4]> {
    let mut groups = text.split('.');
    let mut parsed = [0_u16; 4];
    for value in &mut parsed {
        let group = groups.next()?;
        if group.is_empty() || !group.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *value = group.parse().ok()?;
    }
    if groups.next().is_some() {
        return None;
    }
    Some(parsed)
}

/// Compares the exact four-part versions returned by Windows package APIs.
#[must_use]
pub fn is_newer_package_version(current: &str, candidate: &str) -> bool {
    match (
        parse_package_version(current),
        parse_package_version(candidate),
    ) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => false,
    }
}

/// Validates a cached Store four-part version or GitHub release tag against
/// the currently installed four-part package version.
#[must_use]
pub fn is_newer_available_version(current: &str, candidate: &str) -> bool {
    if candidate.starts_with('v') {
        return is_newer_github_release(current, candidate);
    }
    is_newer_package_version(current, candidate)
}

/// Whether a cached GitHub release can still be applied over `current`.
///
/// A staged GitHub bundle is named from its release tag, so accepting a
/// Store-style package version here would let a foreign cache entry reach the
/// bundle helper. Equal and older tags are deliberately rejected: an update
/// that has already landed must never be scheduled again from its cache.
#[must_use]
pub fn is_newer_github_release(current: &str, tag: &str) -> bool {
    let Some(candidate) = parse_release_tag(tag) else {
        return false;
    };
    parse_version(&normalize_running_version(current)).is_some_and(|current| candidate > current)
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
        let compact_at = release_json[search_from..]
            .find(compact)
            .map(|offset| search_from + offset);
        let spaced_at = release_json[search_from..]
            .find(spaced)
            .map(|offset| search_from + offset);
        let (key_len, start) = match (compact_at, spaced_at) {
            (Some(a), Some(b)) => {
                if a <= b {
                    (compact.len(), a)
                } else {
                    (spaced.len(), b)
                }
            }
            (Some(a), None) => (compact.len(), a),
            (None, Some(b)) => (spaced.len(), b),
            (None, None) => return None,
        };
        let value_start = start + key_len;
        let value_end = release_json[value_start..].find('"')?;
        let value_end = value_start + value_end;
        let url = &release_json[value_start..value_end];
        if url.ends_with(".msixbundle") && !url.ends_with(STORE_BUNDLE_SUFFIX) {
            return Some(url);
        }
        search_from = value_end;
    }
    None
}

/// The only GitHub asset an update is permitted to download.  Never select an
/// arbitrary `.msixbundle` from release JSON: maintainers may attach other
/// bundles and an attacker must not be able to steer selection by ordering.
/// The release pipeline names bundles after the four-part MSIX version
/// (`tools/Package-Msix.ps1`), not the three-part tag.
#[must_use]
pub fn expected_bundle_url(tag: &str) -> Option<String> {
    let version = expected_bundle_version(tag)?;
    Some(format!(
        "https://github.com/faratech/fwdslash/releases/download/{tag}/fwdslash-{version}.msixbundle"
    ))
}

/// The exact four-part MSIX version carried by the GitHub bundle for `tag`.
/// Release tags deliberately have three parts while MSIX reserves the fourth.
#[must_use]
pub fn expected_bundle_version(tag: &str) -> Option<String> {
    let version = parse_release_tag(tag)?;
    Some(format!(
        "{}.{}.{}.0",
        version.major, version.minor, version.patch
    ))
}

/// The SHA-256 value associated with the exact selected asset.  GitHub returns
/// an asset object for the URL, so keep the search within that object rather
/// than accepting an unrelated `digest` field in release notes. GitHub orders
/// `digest` *before* `browser_download_url` inside the object, so the window
/// must be bounded backward to the enclosing `{` as well as forward to the `}`.
#[must_use]
pub fn extract_bundle_digest<'a>(release_json: &'a str, asset_url: &str) -> Option<&'a str> {
    let url_at = release_json.find(asset_url)?;
    let object_end = release_json[url_at..].find('}')? + url_at;
    let object_start = release_json[..url_at].rfind('{')? + 1;
    let asset = &release_json[object_start..object_end];
    let digest_at = asset
        .find("\"digest\":")
        .or_else(|| asset.find("\"digest\": "))?;
    // Tolerate both `"digest":"sha256:…"` and `"digest": "sha256:…"`: the
    // key is fixed, but the whitespace between the colon and the value is
    // whatever the API emitted.
    let value = asset[digest_at + "\"digest\"".len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let value = value.strip_prefix('"')?;
    let value = value.strip_prefix("sha256:")?;
    let end = value.find('"')?;
    let digest = &value[..end];
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digest)
}

fn extract_json_string_field<'a>(json: &'a str, field_key: &str) -> Option<&'a str> {
    let start = json.find(field_key)? + field_key.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Whether an update check should run now. `None` for the last-check time
/// means a check has never run. A stored time in the future (wrong RTC,
/// travel, manual correction) counts as due: `saturating_sub` would
/// otherwise suppress every automatic check until real time catches up.
#[must_use]
pub fn check_is_due(last_check: Option<u64>, now: u64) -> bool {
    match last_check {
        Some(last) => last > now || now.saturating_sub(last) >= CHECK_CADENCE_SECS,
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
    /// A release was reached, but a required trust check failed.  This is not
    /// retried as an ordinary network outage and no package is registered.
    VerificationFailed(UpdateVerificationError),
    /// The running version is current.
    UpToDate,
    /// A newer release exists; the tag names it. With auto-update on the
    /// bundle was already downloaded and registered.
    Ready(String),
}

#[cfg(windows)]
pub mod windows_impl {
    use super::{
        AUTO_UPDATE_VALUE, AVAILABLE_UPDATE_VALUE, LAST_UPDATE_CHECK_VALUE, UpdateOutcome,
        UpdateVerificationError, expected_bundle_url, extract_bundle_digest, extract_tag_name,
        is_newer_version, update_check_allowed,
    };
    use crate::{FSW_SETTINGS_KEY, is_store_flavor, package_version};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_registry::CURRENT_USER;

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs())
    }

    /// Reads the Automatic updates switch. Absent or unreadable falls back to
    /// the flavor default (on for GitHub, off for the Store); an explicitly
    /// stored value always wins, and the stored encoding is unchanged, so
    /// nobody's recorded "off" flips.
    #[must_use]
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
    #[must_use]
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
            .map_or(0, |duration| duration.as_secs());
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

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// SHA-256 for the release asset digest.  Keeping this small implementation
    /// local avoids adding a second cryptography dependency to the core crate
    /// solely for an update check.
    // The fixed SHA-256 schedule has statically proven index bounds: the
    // expansion runs from 16..64 and the round loop from 0..64.
    #[allow(
        clippy::chunks_exact_to_as_chunks,
        clippy::indexing_slicing,
        clippy::items_after_statements,
        clippy::many_single_char_names,
        clippy::too_many_lines
    )]
    fn sha256_hex(input: &[u8]) -> Option<String> {
        let bit_length = (input.len() as u64).checked_mul(8)?;
        let mut bytes = input.to_vec();
        bytes.push(0x80);
        while bytes.len() % 64 != 56 {
            bytes.push(0);
        }
        bytes.extend_from_slice(&bit_length.to_be_bytes());

        let mut state = [
            0x6a09_e667_u32,
            0xbb67_ae85,
            0x3c6e_f372,
            0xa54f_f53a,
            0x510e_527f,
            0x9b05_688c,
            0x1f83_d9ab,
            0x5be0_cd19,
        ];
        const K: [u32; 64] = [
            0x428a_2f98,
            0x7137_4491,
            0xb5c0_fbcf,
            0xe9b5_dba5,
            0x3956_c25b,
            0x59f1_11f1,
            0x923f_82a4,
            0xab1c_5ed5,
            0xd807_aa98,
            0x1283_5b01,
            0x2431_85be,
            0x550c_7dc3,
            0x72be_5d74,
            0x80de_b1fe,
            0x9bdc_06a7,
            0xc19b_f174,
            0xe49b_69c1,
            0xefbe_4786,
            0x0fc1_9dc6,
            0x240c_a1cc,
            0x2de9_2c6f,
            0x4a74_84aa,
            0x5cb0_a9dc,
            0x76f9_88da,
            0x983e_5152,
            0xa831_c66d,
            0xb003_27c8,
            0xbf59_7fc7,
            0xc6e0_0bf3,
            0xd5a7_9147,
            0x06ca_6351,
            0x1429_2967,
            0x27b7_0a85,
            0x2e1b_2138,
            0x4d2c_6dfc,
            0x5338_0d13,
            0x650a_7354,
            0x766a_0abb,
            0x81c2_c92e,
            0x9272_2c85,
            0xa2bf_e8a1,
            0xa81a_664b,
            0xc24b_8b70,
            0xc76c_51a3,
            0xd192_e819,
            0xd699_0624,
            0xf40e_3585,
            0x106a_a070,
            0x19a4_c116,
            0x1e37_6c08,
            0x2748_774c,
            0x34b0_bcb5,
            0x391c_0cb3,
            0x4ed8_aa4a,
            0x5b9c_ca4f,
            0x682e_6ff3,
            0x748f_82ee,
            0x78a5_636f,
            0x84c8_7814,
            0x8cc7_0208,
            0x90be_fffa,
            0xa450_6ceb,
            0xbef9_a3f7,
            0xc671_78f2,
        ];
        for block in bytes.chunks_exact(64) {
            let mut words = [0_u32; 64];
            for (index, word) in words[..16].iter_mut().enumerate() {
                *word = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().ok()?);
            }
            for index in 16..64 {
                let s0 = words[index - 15].rotate_right(7)
                    ^ words[index - 15].rotate_right(18)
                    ^ (words[index - 15] >> 3);
                let s1 = words[index - 2].rotate_right(17)
                    ^ words[index - 2].rotate_right(19)
                    ^ (words[index - 2] >> 10);
                words[index] = words[index - 16]
                    .wrapping_add(s0)
                    .wrapping_add(words[index - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
            for index in 0..64 {
                let choose = (e & f) ^ ((!e) & g);
                let majority = (a & b) ^ (a & c) ^ (b & c);
                let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let t1 = h
                    .wrapping_add(sum1)
                    .wrapping_add(choose)
                    .wrapping_add(K[index])
                    .wrapping_add(words[index]);
                let t2 = sum0.wrapping_add(majority);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
            state[4] = state[4].wrapping_add(e);
            state[5] = state[5].wrapping_add(f);
            state[6] = state[6].wrapping_add(g);
            state[7] = state[7].wrapping_add(h);
        }
        let mut output = String::with_capacity(64);
        use std::fmt::Write;
        for word in state {
            write!(&mut output, "{word:08x}").ok()?;
        }
        Some(output)
    }

    /// A bounded HTTPS GET with `WinHTTP`. Hosts and paths are supplied by this
    /// module only; callers never provide a URL that can change proxy, scheme,
    /// executable, or command-line behavior.
    fn https_get(host: &str, path: &str) -> Option<Vec<u8>> {
        use std::ffi::c_void;
        use windows_sys::Win32::Networking::WinHttp::{
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
            WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
            WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData,
            WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
        };

        let agent = wide("ForwardSlashWindows/1");
        let host = wide(host);
        let path = wide(path);
        // SAFETY: every string is NUL-terminated; all handles are closed on
        // every path after construction; read buffers are valid for their sizes.
        unsafe {
            let session = WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            if session.is_null() {
                return None;
            }
            let _ = WinHttpSetTimeouts(session, 5_000, 5_000, 5_000, 10_000);
            let connection = WinHttpConnect(session, host.as_ptr(), 443, 0);
            if connection.is_null() {
                WinHttpCloseHandle(session);
                return None;
            }
            let request = WinHttpOpenRequest(
                connection,
                wide("GET").as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            );
            if request.is_null() {
                WinHttpCloseHandle(connection);
                WinHttpCloseHandle(session);
                return None;
            }
            let sent =
                WinHttpSendRequest(request, std::ptr::null(), 0, std::ptr::null_mut(), 0, 0, 0)
                    != 0;
            let received = sent && WinHttpReceiveResponse(request, std::ptr::null_mut()) != 0;
            // A 404/5xx body is not a release manifest or a bundle: without a
            // status check it would be misclassified downstream as a trust
            // failure instead of an unavailable endpoint.
            let status_ok = received && {
                let mut status = 0_u32;
                let mut status_len = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
                WinHttpQueryHeaders(
                    request,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    std::ptr::null(),
                    (&raw mut status).cast::<c_void>(),
                    &raw mut status_len,
                    std::ptr::null_mut(),
                ) != 0
                    && status == 200
            };
            let mut body = Vec::new();
            if status_ok {
                loop {
                    let mut available = 0_u32;
                    if WinHttpQueryDataAvailable(request, &raw mut available) == 0 || available == 0
                    {
                        break;
                    }
                    let mut chunk = vec![0_u8; available as usize];
                    let mut read = 0_u32;
                    if WinHttpReadData(
                        request,
                        chunk.as_mut_ptr().cast::<c_void>(),
                        available,
                        &raw mut read,
                    ) == 0
                    {
                        body.clear();
                        break;
                    }
                    chunk.truncate(read as usize);
                    body.extend_from_slice(&chunk);
                    if body.len() > 128 * 1024 * 1024 {
                        body.clear();
                        break;
                    }
                }
            }
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connection);
            WinHttpCloseHandle(session);
            status_ok.then_some(body)
        }
    }

    fn fetch_release_json() -> Option<String> {
        String::from_utf8(https_get(
            "api.github.com",
            "/repos/faratech/fwdslash/releases/latest",
        )?)
        .ok()
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

    /// The local download name for a tag's bundle: the same four-part form the
    /// release pipeline attaches and `expected_bundle_url` fetches.
    fn bundle_name(tag: &str) -> String {
        match super::expected_bundle_version(tag) {
            Some(version) => format!("fwdslash-{version}.msixbundle"),
            None => format!("fwdslash-{tag}.msixbundle"),
        }
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
            let extension = path.extension();
            let is_bundle =
                extension.is_some_and(|extension| extension.eq_ignore_ascii_case("msixbundle"));
            // The digest sidecar `download_bundle` writes next to each bundle.
            // Its stem is the bundle's own file name, so it goes with the
            // bundle it belongs to rather than outliving it.
            let is_sidecar =
                extension.is_some_and(|extension| extension.eq_ignore_ascii_case("sha256"));
            let owner = if is_sidecar {
                path.file_stem()
            } else {
                path.file_name()
            };
            if (is_bundle || is_sidecar) && owner != keep {
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
        let current = package_version().unwrap_or_else(|| crate::FSW_VERSION.to_string());
        if !super::is_newer_github_release(&current, &tag) {
            // A prior version may have left a bundle behind after its package
            // was already applied. Remove both the stale notice and every
            // staged bundle before a caller can create the detached helper.
            let _ = clear_cached_update_tag();
            discard_downloaded_bundles();
            return None;
        }
        let bundle = update_directory_path()?.join(bundle_name(&tag));
        bundle.is_file().then_some(bundle)
    }

    fn download_bundle(
        url: &str,
        tag: &str,
        digest: &str,
    ) -> Result<PathBuf, UpdateVerificationError> {
        const GITHUB_PREFIX: &str = "https://github.com";

        let expected_url =
            expected_bundle_url(tag).ok_or(UpdateVerificationError::VersionMismatch)?;
        if url != expected_url {
            return Err(UpdateVerificationError::UntrustedOrigin);
        }
        let update_dir = update_directory_path().ok_or(UpdateVerificationError::DigestMismatch)?;
        std::fs::create_dir_all(&update_dir)
            .map_err(|_| UpdateVerificationError::DigestMismatch)?;
        let destination = update_dir.join(bundle_name(tag));
        // One source for the URL: the same allow-listed address the release
        // JSON had to match, split into the host https_get dials and the path
        // it requests. Re-deriving it here would only compare it with itself.
        let path = expected_url
            .strip_prefix(GITHUB_PREFIX)
            .ok_or(UpdateVerificationError::UntrustedOrigin)?;
        let body = https_get("github.com", path).ok_or(UpdateVerificationError::UntrustedOrigin)?;
        if sha256_hex(&body).as_deref() != Some(digest) {
            return Err(UpdateVerificationError::DigestMismatch);
        }
        // Do not discard the last known-good deferred bundle until the new
        // body has been fully received and digest-verified.
        let temporary =
            update_dir.join(format!(".{}.{}.part", bundle_name(tag), std::process::id()));
        std::fs::write(&temporary, body).map_err(|_| UpdateVerificationError::DigestMismatch)?;
        if destination.exists() {
            std::fs::remove_file(&destination)
                .map_err(|_| UpdateVerificationError::DigestMismatch)?;
        }
        std::fs::rename(&temporary, &destination)
            .map_err(|_| UpdateVerificationError::DigestMismatch)?;
        prune_bundles(&update_dir, destination.file_name());
        std::fs::write(destination.with_extension("msixbundle.sha256"), digest)
            .map_err(|_| UpdateVerificationError::DigestMismatch)?;
        Ok(destination)
    }

    /// Runs one update-check attempt. Never blocks longer than the curl
    /// timeouts; never surfaces an error to the caller — failures are
    /// `Unavailable`.
    /// `force` is the user pressing "Check now": it bypasses the daily
    /// cadence and the Automatic updates switch, but never the two facts —
    /// packaged, and not the Store flavor — that decide whether this route
    /// exists at all.
    #[must_use]
    pub fn run_update_check(force: bool) -> UpdateOutcome {
        let packaged = crate::has_package_identity();
        let auto_update = read_auto_update_enabled();
        // The gate no longer knows about flavors, so the flavor test the
        // GitHub route needs is spelled out here: this function downloads a
        // bundle from GitHub, which the Store flavor must never do.
        if is_store_flavor()
            || !(packaged && (force || update_check_allowed(packaged, auto_update)))
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
        let Some(tag) =
            extract_tag_name(&release_json).filter(|tag| super::parse_release_tag(tag).is_some())
        else {
            return UpdateOutcome::VerificationFailed(UpdateVerificationError::VersionMismatch);
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

        let Some(url) = expected_bundle_url(tag).filter(|url| release_json.contains(url.as_str()))
        else {
            return UpdateOutcome::VerificationFailed(UpdateVerificationError::AssetNameMismatch);
        };
        let Some(digest) = extract_bundle_digest(&release_json, &url) else {
            return UpdateOutcome::VerificationFailed(UpdateVerificationError::DigestMismatch);
        };
        if auto_update {
            return match download_bundle(&url, tag, digest) {
                Ok(_) => {
                    // Registration is deferred, so retain the verified bundle
                    // and its exact release tag until the helper applies it.
                    let _ = set_cached_update_tag(tag);
                    UpdateOutcome::Ready(tag.to_string())
                }
                Err(error) => UpdateOutcome::VerificationFailed(error),
            };
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
