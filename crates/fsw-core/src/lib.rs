#![cfg_attr(docsrs, feature(doc_auto_cfg))]

// The two validators are only reachable under `cfg(windows)`; on Linux the
// registry paths compile out and the import would be flagged unused.
#[cfg_attr(not(windows), allow(unused_imports))]
use fsw_path::{
    BareSlashMode, Context, RenderBuf, ResolveError, Resolved, eq_ignore_case,
    is_valid_distribution_name, is_valid_windows_root, resolve, resolve_under_root,
};
pub mod settings_write;
pub mod update;

pub use settings_write::{
    WritePlan, delete_setting, set_setting_string, set_setting_u32, set_setting_u64,
    sync_settings_to_real_hive, write_plan,
};

use std::fmt;
use std::path::PathBuf;

pub const FSW_BROKER_WINDOW_CLASS: &str = "ForwardSlashWindows.Broker";
pub const FSW_WM_QUERY_STATE: u32 = 0x8000 + 10; // WM_APP + 10
pub const FSW_WM_SET_PAUSED: u32 = 0x8000 + 11; // WM_APP + 11
pub const FSW_WM_SHOW_SETTINGS: u32 = 0x8000 + 12; // WM_APP + 12

/// The name every component registers with `RegisterWindowMessageW` to hear
/// that some *other* component changed state (issue #55).
///
/// Unlike the three `FSW_WM_*` messages above this one is not addressed to a
/// window: it is posted to `HWND_BROADCAST` by the writer and picked up by
/// whoever happens to be running — a settings window that would otherwise keep
/// rendering the state it read at launch, and the broker, whose tray tooltip
/// and menus would otherwise wait for the next health tick.
///
/// The string is the contract, not the number: `RegisterWindowMessageW` hands
/// every process the same id for the same string, and that id is only stable
/// for the life of the session. Never hardcode a value for it, and never
/// change this string without changing every component together.
pub const FSW_STATE_CHANGED_MESSAGE: &str = "ForwardSlashWindows.StateChanged";

/// The settings key, and every value name under it.
///
/// **Writes to this key go through [`settings_write`] and nowhere else** —
/// `set_setting_u32` / `set_setting_u64` / `set_setting_string` /
/// `delete_setting`. No other code may call `windows_registry`'s
/// `set_*`/`remove_value` on it: a packaged process's own write is virtualized
/// into the package hive, where the unpackaged shell adapters can never read
/// it, which is what issue #52 was. `settings_write` is the one place that
/// knows to write both hives.
pub const FSW_SETTINGS_KEY: &str = r"Software\ForwardSlashWindows\Settings";
pub const FSW_DISABLED_VALUE: &str = "Disabled";
pub const FSW_BARE_SLASH_MODE_VALUE: &str = "BareSlashMode";
pub const FSW_BARE_SLASH_DISTRIBUTION_VALUE: &str = "BareSlashDistribution";
/// Custom bare-slash root: an absolute Windows path (`C:\code`, a UNC) that
/// `/` opens and everything non-distro resolves under. Deliberately a separate
/// value, not a third `BareSlashMode`: both resolvers read any nonzero
/// BareSlashMode DWORD as "default distribution" (docs/divergences.md,
/// resolver 6), so a stale C++ build ignores this value and falls back to
/// today's behavior instead of disagreeing about what `/` means.
pub const FSW_BARE_SLASH_ROOT_VALUE: &str = "BareSlashRoot";

/// The three values below are hand copies of `include/fsw_filter_protocol.h` —
/// the only contract the broker shares with the minifilter, which Rust cannot
/// `#include`. The driver validates all of them (`fswfilter.c` message
/// dispatch): a wrong port, version, size or a non-zero Reserved silently
/// fails the publish, so if you touch the header, touch these.
pub const FSW_FILTER_PORT_NAME: &str = "\\FswFilterPort";
pub const FSW_FILTER_PROTOCOL_VERSION: u32 = 2;
pub const FSW_FILTER_MAX_DISTRIBUTIONS: usize = 32;

pub const LXSS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "ForwardSlashWindows";
pub const PROTOCOL_KEY: &str = r"Software\Classes\fwdslash";
pub const CMD_ADAPTER_KEY: &str = r"Software\ForwardSlashWindows\CmdAdapter";
pub const POWERSHELL_ADAPTER_ROOT: &str = r"Software\ForwardSlashWindows\PowerShellAdapter\";

/// The running crate version, embedded at compile time.
pub const FSW_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Dual-track identity values (docs/store-submission.md). Both packages share
/// the Identity Name; the Store flavor is discriminated by its assigned
/// GUID publisher at runtime — see `is_store_flavor`.
pub const STORE_IDENTITY_NAME: &str = "32827MikeFara.fwdslash";
pub const STORE_PUBLISHER: &str = "CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4";
pub const STORE_PACKAGE_FAMILY: &str = "32827MikeFara.fwdslash_t6j5qexy2jpp2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(isize)]
pub enum BrokerState {
    Unavailable = 0,
    Active = 1,
    Paused = 2,
}

impl From<isize> for BrokerState {
    fn from(val: isize) -> Self {
        match val {
            1 => Self::Active,
            2 => Self::Paused,
            _ => Self::Unavailable,
        }
    }
}

/// A point-in-time snapshot of the WSL environment and user preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub distributions: Vec<String>,
    pub default_distribution: Option<String>,
    pub bare_slash_mode: BareSlashMode,
    pub bare_slash_pinned: Option<String>,
    /// The custom bare-slash root, when one is configured and well-formed.
    pub bare_slash_root: Option<String>,
    pub disabled: bool,
}

/// Every value under `FSW_SETTINGS_KEY` the resolver needs, read through one
/// key handle.
///
/// The single-value getters below remain for callers that want exactly one
/// setting, but anything reading two or more (the broker's per-Enter snapshot,
/// the settings window's refresh) should take this: opening the key is the
/// expensive part, and `Snapshot::current` used to do it four times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsValues {
    pub disabled: bool,
    pub bare_slash_mode: BareSlashMode,
    /// The pinned distribution, or `None` when unset or empty.
    pub bare_slash_pinned: Option<String>,
    /// The custom bare-slash root, only when it is well-formed.
    pub bare_slash_root: Option<String>,
}

impl Default for SettingsValues {
    /// What an absent settings key means: nothing paused, nothing pinned.
    fn default() -> Self {
        Self {
            disabled: false,
            bare_slash_mode: BareSlashMode::DistributionList,
            bare_slash_pinned: None,
            bare_slash_root: None,
        }
    }
}

impl SettingsValues {
    /// Reads all four values from one open key. An absent or unreadable key
    /// yields [`Self::default`], and a malformed root is no root — the same
    /// semantics as the single-value getters, which now delegate here.
    #[must_use]
    pub fn read() -> Self {
        #[cfg(windows)]
        {
            use windows_registry::CURRENT_USER;

            let Ok(key) = CURRENT_USER.open(FSW_SETTINGS_KEY) else {
                return Self::default();
            };
            Self {
                disabled: key.get_u32(FSW_DISABLED_VALUE).is_ok_and(|value| value != 0),
                bare_slash_mode: match key.get_u32(FSW_BARE_SLASH_MODE_VALUE) {
                    Ok(value) if value != 0 => BareSlashMode::DefaultDistribution,
                    _ => BareSlashMode::DistributionList,
                },
                bare_slash_pinned: key
                    .get_string(FSW_BARE_SLASH_DISTRIBUTION_VALUE)
                    .ok()
                    .filter(|value| !value.is_empty()),
                bare_slash_root: key
                    .get_string(FSW_BARE_SLASH_ROOT_VALUE)
                    .ok()
                    .filter(|value| is_valid_windows_root(value)),
            }
        }
        #[cfg(not(windows))]
        {
            Self::default()
        }
    }
}

impl Snapshot {
    /// One pass over the registry: one Lxss handle for the distribution list
    /// and the default, one settings handle for the four preferences.
    #[must_use]
    pub fn current() -> Self {
        let (distributions, default_distribution) = read_lxss();
        let settings = SettingsValues::read();

        Self {
            distributions,
            default_distribution,
            bare_slash_mode: settings.bare_slash_mode,
            bare_slash_pinned: settings.bare_slash_pinned,
            bare_slash_root: settings.bare_slash_root,
            disabled: settings.disabled,
        }
    }

    pub fn context<'a>(&'a self, registry_refs: &'a [&'a str]) -> Context<'a, [&'a str]> {
        Context {
            registry: registry_refs,
            mode: self.bare_slash_mode,
            preferred: self.bare_slash_pinned.as_deref(),
            wsl_default: self.default_distribution.as_deref(),
        }
    }
}

/// The current package full name
/// (`Name_Version_Arch[_ResourceId]__PublisherHash`), or `None` without
/// package identity.
pub fn package_full_name() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
        use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

        static FULL_NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        FULL_NAME
            .get_or_init(|| unsafe {
                let mut length = 0u32;
                let first = GetCurrentPackageFullName(&mut length, std::ptr::null_mut());
                if first != ERROR_INSUFFICIENT_BUFFER || length == 0 {
                    return None;
                }
                let mut buffer = vec![0u16; length as usize];
                let second = GetCurrentPackageFullName(&mut length, buffer.as_mut_ptr());
                if second != 0 {
                    return None;
                }
                // length includes the terminating NUL.
                Some(String::from_utf16_lossy(&buffer[..length as usize - 1]))
            })
            .clone()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn is_four_part_version(text: &str) -> bool {
    let mut groups = text.split('.');
    groups.clone().count() == 4
        && groups.all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_digit()))
}

/// The package family from a full name. Full names are
/// `Name_Version_Arch[_ResourceId]__PublisherHash`; our manifest declares no
/// ResourceId, so the shipped form contains an empty group (a double
/// underscore): `32827MikeFara.fwdslash_0.0.2.0_x64__hash`. Parse from the
/// right, skipping empty groups — the identity name itself may contain
/// underscores.
pub fn package_family_from_full_name(full: &str) -> Option<String> {
    let (name, hash) = split_full_name_tail(full)?;
    Some(format!("{name}_{hash}"))
}

/// The four-part package version from a full name.
pub fn package_version_from_full_name(full: &str) -> Option<String> {
    let fields: Vec<&str> = full.split('_').filter(|field| !field.is_empty()).collect();
    if fields.len() < 4 {
        return None;
    }
    let version = fields[fields.len() - 3];
    is_four_part_version(version).then(|| version.to_string())
}

/// Splits a full name into `(identity name, publisher hash)`. From the right:
/// hash, arch, version, then the name — which may itself contain underscores.
fn split_full_name_tail(full: &str) -> Option<(String, String)> {
    let fields: Vec<&str> = full.split('_').filter(|field| !field.is_empty()).collect();
    if fields.len() < 4 {
        return None;
    }
    let version = fields[fields.len() - 3];
    if !is_four_part_version(version) {
        return None;
    }
    let hash = fields[fields.len() - 1];
    let name = fields[..fields.len() - 3].join("_");
    Some((name, hash.to_string()))
}

/// The package family (`Name_PublisherHash`) of the running process.
pub fn package_family() -> Option<String> {
    static FAMILY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FAMILY
        .get_or_init(|| package_family_from_full_name(&package_full_name()?))
        .clone()
}

/// The package architecture (`x64`, `arm64`, …) of the running process.
pub fn package_architecture() -> Option<String> {
    static ARCH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ARCH.get_or_init(|| {
        let full_name = package_full_name()?;
        let fields: Vec<&str> = full_name
            .split('_')
            .filter(|field| !field.is_empty())
            .collect();
        fields.get(fields.len() - 2).map(|arch| (*arch).to_string())
    })
    .clone()
}

/// The four-part package version of the running process.
pub fn package_version() -> Option<String> {
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| package_version_from_full_name(&package_full_name()?))
        .clone()
}

/// True when this packaged build came from the Microsoft Store track: the
/// package family matches the Partner Center identity. The GitHub-distributed
/// build carries the Trusted Signing publisher, whose hash differs.
pub fn is_store_flavor() -> bool {
    package_family().as_deref() == Some(STORE_PACKAGE_FAMILY)
}

/// Checks whether the running process has MSIX package identity.
pub fn has_package_identity() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE;
        use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

        static PACKAGED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *PACKAGED.get_or_init(|| {
            let mut length: u32 = 0;
            unsafe {
                GetCurrentPackageFullName(&mut length, std::ptr::null_mut())
                    != APPMODEL_ERROR_NO_PACKAGE as u32
            }
        })
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Enumerate registered WSL distribution names under HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss.
pub fn list_registered_distributions() -> Vec<String> {
    #[cfg(windows)]
    {
        let Ok(lxss) = windows_registry::CURRENT_USER.open(LXSS_KEY) else {
            return Vec::new();
        };
        distributions_from_lxss(&lxss)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The distribution list and the default, from a single Lxss key handle.
///
/// The two used to be separate public calls, each opening the key; the
/// broker's per-Enter snapshot pays for both. The answers are identical to
/// `list_registered_distributions()` + `get_default_distribution(&list)`.
fn read_lxss() -> (Vec<String>, Option<String>) {
    #[cfg(windows)]
    {
        let Ok(lxss) = windows_registry::CURRENT_USER.open(LXSS_KEY) else {
            return (Vec::new(), None);
        };
        let distributions = distributions_from_lxss(&lxss);
        let default = default_distribution_from_lxss(&lxss, &distributions)
            .or_else(|| single_registered_distribution(&distributions));
        (distributions, default)
    }
    #[cfg(not(windows))]
    {
        (Vec::new(), None)
    }
}

#[cfg(windows)]
fn distributions_from_lxss(lxss: &windows_registry::Key) -> Vec<String> {
    let mut distros = Vec::new();
    let Ok(keys) = lxss.keys() else {
        return distros;
    };
    for subkey_name in keys {
        if let Ok(subkey) = lxss.open(&subkey_name) {
            if let Ok(name) = subkey.get_string("DistributionName") {
                if is_valid_distribution_name(&name) {
                    distros.push(name);
                }
            }
        }
    }
    distros
}

/// The `DefaultDistribution` GUID resolved to a name, if it is registered.
#[cfg(windows)]
fn default_distribution_from_lxss(
    lxss: &windows_registry::Key,
    registered: &[String],
) -> Option<String> {
    let default_guid = lxss.get_string("DefaultDistribution").ok()?;
    let name = lxss.open(&default_guid).ok()?.get_string("DistributionName").ok()?;
    registered
        .iter()
        .any(|distro| eq_ignore_case(distro, &name))
        .then_some(name)
}

/// With exactly one distribution registered, WSL's own default is that one
/// whether or not the registry says so.
fn single_registered_distribution(registered: &[String]) -> Option<String> {
    match registered {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// Checks if a distribution name is registered (case-insensitive ordinal).
pub fn is_registered_distribution(candidate: &str) -> bool {
    list_registered_distributions()
        .iter()
        .any(|distro| eq_ignore_case(distro, candidate))
}

/// Determines the default WSL distribution.
#[must_use]
pub fn get_default_distribution(registered: &[String]) -> Option<String> {
    #[cfg(windows)]
    {
        if let Ok(lxss) = windows_registry::CURRENT_USER.open(LXSS_KEY) {
            if let Some(name) = default_distribution_from_lxss(&lxss, registered) {
                return Some(name);
            }
        }
    }

    single_registered_distribution(registered)
}

/// Reads the user's bare-slash mode from HKCU.
///
/// Reading more than one setting? Take [`SettingsValues::read`] instead — each
/// of these getters opens the key on its own.
#[must_use]
pub fn get_bare_slash_mode() -> BareSlashMode {
    SettingsValues::read().bare_slash_mode
}

/// Reads any pinned bare-slash distribution name from HKCU. Empty when unset.
#[must_use]
pub fn get_bare_slash_override() -> String {
    SettingsValues::read().bare_slash_pinned.unwrap_or_default()
}

/// Reads the custom bare-slash root. An absent, empty, or malformed value is
/// no root at all — a bad stored value must degrade to today's behavior, not
/// poison every resolve.
#[must_use]
pub fn get_bare_slash_root() -> Option<String> {
    SettingsValues::read().bare_slash_root
}

/// Returns whether forward-slash path resolution is globally paused/disabled.
#[must_use]
pub fn is_disabled() -> bool {
    SettingsValues::read().disabled
}

/// Sets the disabled state in HKCU\Software\ForwardSlashWindows\Settings.
/// Persists the global pause flag.
///
/// Like every other settings write it goes through [`settings_write`], which
/// reaches the real hive with `reg.exe` when this process is packaged: the
/// PowerShell module reads this flag from an unpackaged shell, where a
/// virtualized write is invisible (verified 2026-09-04; see
/// docs/compatibility.md). Issue #52 is the same fault for the BareSlash*
/// values, which used to be written in-process only.
pub fn persist_disabled(disabled: bool) -> Result<(), u32> {
    set_setting_u32(FSW_DISABLED_VALUE, u32::from(disabled))
}

/// Updates bare slash settings in HKCU.
///
/// One write per value through [`settings_write`], so a packaged settings app
/// and an unpackaged shell adapter agree about what `/` means (issue #52).
/// Every value is attempted even after a failure — a half-applied mode with a
/// stale pin is worse than a reported error.
pub fn write_bare_slash_settings(
    default_mode: bool,
    pinned_distribution: &str,
    root: Option<&str>,
) -> Result<(), u32> {
    let mode = set_setting_u32(FSW_BARE_SLASH_MODE_VALUE, u32::from(default_mode));

    let pinned = if default_mode && !pinned_distribution.is_empty() {
        set_setting_string(FSW_BARE_SLASH_DISTRIBUTION_VALUE, pinned_distribution)
    } else {
        delete_setting(FSW_BARE_SLASH_DISTRIBUTION_VALUE)
    };

    let configured_root = match root {
        Some(path) if !path.is_empty() => set_setting_string(FSW_BARE_SLASH_ROOT_VALUE, path),
        _ => delete_setting(FSW_BARE_SLASH_ROOT_VALUE),
    };

    mode.and(pinned).and(configured_root)
}

/// Resolves a forward-slash path against the live user registry configuration.
///
/// When a custom bare-slash root is configured (`BareSlashRoot`) and the
/// input's first segment is not a registered distribution, the root owns the
/// input entirely: a bare `/` opens the root and `/foo` resolves to
/// root\foo — in either bare-slash mode. Only registered-distribution inputs
/// keep WSL semantics, which is the escape hatch to `\\wsl.localhost` the
/// README advertises. A root that is absent or malformed changes nothing, so
/// a stale C++ install (which never reads the value) behaves identically to
/// a corrupt one (docs/divergences.md, resolver 6).
pub fn resolve_user_slash_path<'b>(
    input: &str,
    snapshot: &'b Snapshot,
    render_buf: &'b mut RenderBuf,
) -> Result<Resolved<'b>, ResolveError> {
    // Validation happens here, not only at set time: the funnel runs on every
    // platform and a hand-built snapshot must not bypass it.
    let configured_root = snapshot
        .bare_slash_root
        .as_deref()
        .filter(|root| is_valid_windows_root(root));

    // Shape check before any slicing: `input[1..]` panics on an empty input
    // and on a multi-byte first character, and `panic = "abort"` would take
    // the CLI (or the resident broker) down instead of reporting R1.
    let Some(after_root) = input.strip_prefix('/') else {
        return Err(ResolveError::NotASlashPath);
    };

    let first_segment = after_root.split('/').next().unwrap_or_default();
    let explicit_distro = !first_segment.is_empty()
        && snapshot
            .distributions
            .iter()
            .any(|distro| eq_ignore_case(distro, first_segment));

    if let (false, Some(root)) = (explicit_distro, configured_root) {
        // Owns the input's shape checks too: the same R1-R4 errors come back
        // with the same variants, and `..` clamps at the root.
        return resolve_under_root(input, root, render_buf);
    }

    let refs: Vec<&str> = snapshot.distributions.iter().map(|s| s.as_str()).collect();
    let ctx = snapshot.context(&refs);
    resolve(input, &ctx, render_buf)
}

/// Fixed diagnostic event categories strictly conforming to PRIVACY.md.
/// No variant carries user path data, process names, or PIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagEvent {
    RouteDistribution,
    RouteDefault,
    RouteList,
    RouteFolder,
    ResolveFailure,
    EnterPassThrough,
    EnterReplayed,
    BrokerStarted,
    BrokerStopped,
    BrokerPaused,
    BrokerResumed,
    IntegrationsQueried,
    /// The packaged app mirrored one or more settings into the real hive so
    /// the unpackaged shell adapters can read them (issue #52). Category
    /// only: never which value, never what it said.
    SettingsSynced,
}

impl fmt::Display for DiagEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::RouteDistribution => "event=route_distribution",
            Self::RouteDefault => "event=route_default",
            Self::RouteList => "event=route_list",
            Self::RouteFolder => "event=route_folder",
            Self::ResolveFailure => "event=resolve_failure",
            Self::EnterPassThrough => "event=enter_passthrough",
            Self::EnterReplayed => "event=enter_replayed",
            Self::BrokerStarted => "event=broker_started",
            Self::BrokerStopped => "event=broker_stopped",
            Self::BrokerPaused => "event=broker_paused",
            Self::BrokerResumed => "event=broker_resumed",
            Self::IntegrationsQueried => "event=integrations_queried",
            Self::SettingsSynced => "event=settings_synced",
        };
        write!(f, "{name}")
    }
}

/// Logs a diagnostic category event if FSW_DIAGNOSTIC_LOG is configured.
pub fn diagnostic(event: DiagEvent) {
    if let Ok(log_path) = std::env::var("FSW_DIAGNOSTIC_LOG") {
        if !log_path.is_empty() {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                let _ = writeln!(file, "{event}");
            }
        }
    }
}

/// Resolves the parent directory of the current executable.
pub fn executable_directory() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(|path| path.parent().unwrap_or(&path).to_path_buf())
}

// ---------------------------------------------------------------------------
// Integration state and broker probes.
//
// These mirror the helpers the C++ settings app and controller each carry
// privately (`src/settings/main.cpp:72-134` and `src/controller/main.cpp:45-88`,
// `:385-391`). They live here so `fswsettings.exe` can read state in-process the
// way the C++ app does, instead of parsing `fwdslash --json`, and so there is one
// implementation rather than one per binary.
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn to_wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().collect();
    wide.push(0);
    wide
}

/// Reads a `REG_SZ`/`REG_EXPAND_SZ` value and compares it for exact equality.
pub fn registry_string_equals(path: &str, name: &str, expected: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;
        if let Ok(key) = CURRENT_USER.open(path) {
            if let Ok(value) = key.get_string(name) {
                return value == expected;
            }
        }
    }
    let _ = (path, name, expected);
    false
}

/// Whether a transactional adapter under `path` records `State = "installed"`.
#[must_use]
pub fn adapter_installed(path: &str) -> bool {
    registry_string_equals(path, "State", "installed")
}

/// The payload version recorded by an installed adapter, from the `Version`
/// value under its marker key. `None` when the key or the value is absent —
/// which is also what a pre-`Version` install looks like.
#[must_use]
pub fn adapter_version(marker_key_path: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;
        if let Ok(key) = CURRENT_USER.open(marker_key_path) {
            if let Ok(version) = key.get_string("Version") {
                return Some(version);
            }
        }
    }
    let _ = marker_key_path;
    None
}

/// Whether an installed adapter's payload predates `current_version` and
/// should be reinstalled. An adapter that is not installed is never outdated.
#[must_use]
pub fn adapter_outdated(marker_key_path: &str, current_version: &str) -> bool {
    adapter_installed(marker_key_path)
        && adapter_version(marker_key_path).as_deref() != Some(current_version)
}

/// Whether the Windows-surface integration is installed.
///
/// A packaged build declares its startup task in the manifest, so identity alone
/// is the answer; unpackaged installs write the per-user Run value.
pub fn windows_integration_installed() -> bool {
    if has_package_identity() {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;
        if let Ok(key) = CURRENT_USER.open(RUN_KEY) {
            return key.get_string(RUN_VALUE).is_ok();
        }
    }
    false
}

/// Whether `name` resolves on the current search path.
pub fn executable_available(name: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Storage::FileSystem::SearchPathW;
        let wide_name = to_wide(name);
        let mut buffer = [0u16; 32768];
        let length = SearchPathW(
            std::ptr::null(),
            wide_name.as_ptr(),
            std::ptr::null(),
            buffer.len() as u32,
            buffer.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        length > 0 && (length as usize) < buffer.len()
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

/// Whether the broker's never-shown top-level window is present on this desktop.
pub fn broker_window_exists() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
        let class = to_wide(FSW_BROKER_WINDOW_CLASS);
        !FindWindowW(class.as_ptr(), std::ptr::null()).is_null()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Asks the broker for its state, giving up after `timeout_ms`.
///
/// A stopped broker costs one `FindWindowW` regardless of the timeout (the
/// send is skipped when the window is absent). The settings window uses
/// 750 ms (`src/settings/main.cpp:827`) so a wedged broker cannot stall a
/// refresh; the CLI uses 200 ms for the same reason.
pub fn broker_state(timeout_ms: u32) -> BrokerState {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW,
        };
        let class = to_wide(FSW_BROKER_WINDOW_CLASS);
        let window = FindWindowW(class.as_ptr(), std::ptr::null());
        if window.is_null() {
            return BrokerState::Unavailable;
        }
        let mut result: usize = 0;
        let delivered = SendMessageTimeoutW(
            window,
            FSW_WM_QUERY_STATE,
            0,
            0,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout_ms,
            &mut result,
        );
        if delivered == 0 {
            BrokerState::Unavailable
        } else {
            BrokerState::from(result as isize)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = timeout_ms;
        BrokerState::Unavailable
    }
}

/// This session's id for [`FSW_STATE_CHANGED_MESSAGE`], registered once.
///
/// `RegisterWindowMessageW` is idempotent per string per session, but it is
/// still a user32 round trip, and the broker asks for this id inside its window
/// procedure — the thread that owns the low-level keyboard hook. The `OnceLock`
/// keeps every call after the first to a load. `0` means the registration
/// failed (or the platform is not Windows): callers must treat it as "no
/// notification", never as a message id.
#[must_use]
pub fn state_changed_message() -> u32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
        static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *MESSAGE.get_or_init(|| unsafe {
            RegisterWindowMessageW(to_wide(FSW_STATE_CHANGED_MESSAGE).as_ptr())
        })
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Tells every running component that this process changed shared state.
///
/// Posted, never sent: the caller — a CLI verb finishing, the broker's tray
/// toggle, a settings write — must not wait on anyone's message loop, and a
/// hung listener must not be able to hold up a write. `HWND_BROADCAST` reaches
/// top-level windows only, which is why the broker's window and the settings
/// app's watcher window are both real (never-shown) top-level windows rather
/// than message-only ones.
///
/// Call it **only after** the mutation has actually landed: a listener that
/// re-reads on a failed write would just re-render the old state, and one that
/// re-reads before the write lands would render it as well. Carries no
/// payload — the listeners re-read what they need, so nothing about what
/// changed travels in the message (PRIVACY.md).
pub fn broadcast_state_changed() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, PostMessageW};
        let message = state_changed_message();
        if message == 0 {
            return;
        }
        unsafe {
            PostMessageW(HWND_BROADCAST, message, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem minifilter probes.
//
// The driver is optional, production-gated, and absent from every shipped
// package (SECURITY.md). These two calls are how the CLI and the settings
// window report what the machine actually has instead of asserting it is never
// there.
// ---------------------------------------------------------------------------

/// The minifilter's service name, a hand copy of `ServiceName` in
/// `driver/fswfilter/fswfilter.inf`. Only the SCM probe below needs it.
#[cfg(windows)]
const FILTER_SERVICE_NAME: &str = "FswFilter";

// `fltlib`'s port-connect entry point.
//
// Declared `raw-dylib`, so the import is synthesized from this declaration
// alone: no import library, and -- the point -- no crate dependency. The
// version-island rule in docs/dependencies.md (nothing in `fsw-core`'s
// dependency closure may pull `windows-core`) is untouched by this probe.
#[cfg(windows)]
#[link(name = "fltlib", kind = "raw-dylib")]
unsafe extern "system" {
    fn FilterConnectCommunicationPort(
        lpPortName: *const u16,
        dwOptions: u32,
        lpContext: *const std::ffi::c_void,
        wSizeOfContext: u16,
        lpSecurityAttributes: *mut std::ffi::c_void,
        hPort: *mut *mut std::ffi::c_void,
    ) -> i32;
}

/// Whether the minifilter's communication port accepts a connection right now.
///
/// The handle is closed immediately: this is a probe, not the broker's
/// long-lived publish channel. `false` whenever the port is absent (no driver
/// loaded) or refuses the connect, and `false` off Windows.
#[must_use]
pub fn filter_port_available() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;

        let port = to_wide(FSW_FILTER_PORT_NAME);
        let mut handle: *mut std::ffi::c_void = std::ptr::null_mut();
        let hr = FilterConnectCommunicationPort(
            port.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            &raw mut handle,
        );
        if hr >= 0 && !handle.is_null() {
            CloseHandle(handle);
            true
        } else {
            false
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether the minifilter's kernel service is registered, and whether it is
/// running. Independent of [`filter_port_available`]: a loaded filter that has
/// not opened its port is `Running` but unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterServiceState {
    /// No `FswFilter` service is registered — every machine that has not
    /// deliberately installed the driver, which is all of them.
    NotInstalled,
    /// Registered but not running.
    Stopped,
    /// Running: the filter is loaded.
    Running,
}

/// The minifilter service's state, from the service control manager.
///
/// Read-only and unprivileged by construction: `SC_MANAGER_CONNECT` plus
/// `SERVICE_QUERY_STATUS` are rights an ordinary user already holds, and
/// nothing here starts, stops or installs anything. An SCM that will not open,
/// a service that is not there and a status that will not read all answer
/// [`FilterServiceState::NotInstalled`] — the product's default assumption.
#[must_use]
pub fn filter_service_state() -> FilterServiceState {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
            SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
        };

        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return FilterServiceState::NotInstalled;
        }
        let name = to_wide(FILTER_SERVICE_NAME);
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            CloseServiceHandle(manager);
            return FilterServiceState::NotInstalled;
        }
        let mut status = SERVICE_STATUS::default();
        let queried = QueryServiceStatus(service, &raw mut status);
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
        if queried == 0 {
            FilterServiceState::NotInstalled
        } else if status.dwCurrentState == SERVICE_RUNNING {
            FilterServiceState::Running
        } else {
            FilterServiceState::Stopped
        }
    }
    #[cfg(not(windows))]
    {
        FilterServiceState::NotInstalled
    }
}

/// Launches the broker if a packaged build has not already armed it.
///
/// A packaged build has no install-time hook and its `windows.startupTask` only
/// fires at logon, so opening the settings window is the first moment the broker
/// can be started. Unpackaged installs arrange this through `fwdslash install`.
/// Mirrors `EnsureBrokerRunning` at `src/settings/main.cpp:145-167`.
pub fn ensure_broker_running() {
    if !has_package_identity() || broker_window_exists() {
        return;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, Sleep,
        };

        let Ok(directory) = executable_directory() else {
            return;
        };
        let broker = directory.join("fswbroker.exe");
        let application = to_wide(&broker.to_string_lossy());
        let mut command = to_wide(&format!("\"{}\"", broker.display()));
        let working_directory = to_wide(&directory.to_string_lossy());

        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();

        let started = CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            working_directory.as_ptr(),
            &startup,
            &mut process,
        );
        if started == 0 {
            return;
        }
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);

        // The broker registers its message window a moment after launch; give it
        // long enough that the state we are about to read is accurate.
        for _ in 0..40 {
            if broker_window_exists() {
                return;
            }
            Sleep(50);
        }
    }
}
