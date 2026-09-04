#![cfg_attr(docsrs, feature(doc_auto_cfg))]

// The two validators are only reachable under `cfg(windows)`; on Linux the
// registry paths compile out and the import would be flagged unused.
#[cfg_attr(not(windows), allow(unused_imports))]
use fsw_path::{
    BareSlashMode, Context, RenderBuf, ResolveError, Resolved, eq_ignore_case,
    is_valid_distribution_name, is_valid_windows_root, resolve, resolve_under_root,
};
use std::fmt;
use std::path::PathBuf;

pub const FSW_BROKER_WINDOW_CLASS: &str = "ForwardSlashWindows.Broker";
pub const FSW_WM_QUERY_STATE: u32 = 0x8000 + 10; // WM_APP + 10
pub const FSW_WM_SET_PAUSED: u32 = 0x8000 + 11; // WM_APP + 11
pub const FSW_WM_SHOW_SETTINGS: u32 = 0x8000 + 12; // WM_APP + 12

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

impl Snapshot {
    pub fn current() -> Self {
        let distributions = list_registered_distributions();
        let default_distribution = get_default_distribution(&distributions);
        let bare_slash_mode = get_bare_slash_mode();
        let bare_slash_override = get_bare_slash_override();
        let bare_slash_pinned = if bare_slash_override.is_empty() {
            None
        } else {
            Some(bare_slash_override)
        };
        let bare_slash_root = get_bare_slash_root();
        let disabled = is_disabled();

        Self {
            distributions,
            default_distribution,
            bare_slash_mode,
            bare_slash_pinned,
            bare_slash_root,
            disabled,
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
        use windows_registry::CURRENT_USER;

        let mut distros = Vec::new();
        let lxss = match CURRENT_USER.open(LXSS_KEY) {
            Ok(k) => k,
            Err(_) => return distros,
        };

        if let Ok(keys) = lxss.keys() {
            for subkey_name in keys {
                if let Ok(subkey) = lxss.open(&subkey_name) {
                    if let Ok(name) = subkey.get_string("DistributionName") {
                        if is_valid_distribution_name(&name) {
                            distros.push(name);
                        }
                    }
                }
            }
        }
        distros
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Checks if a distribution name is registered (case-insensitive ordinal).
pub fn is_registered_distribution(candidate: &str) -> bool {
    list_registered_distributions()
        .iter()
        .any(|distro| eq_ignore_case(distro, candidate))
}

/// Determines the default WSL distribution.
pub fn get_default_distribution(registered: &[String]) -> Option<String> {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;

        if let Ok(lxss) = CURRENT_USER.open(LXSS_KEY) {
            if let Ok(default_guid) = lxss.get_string("DefaultDistribution") {
                if let Ok(sub) = lxss.open(&default_guid) {
                    if let Ok(name) = sub.get_string("DistributionName") {
                        if registered.iter().any(|d| eq_ignore_case(d, &name)) {
                            return Some(name);
                        }
                    }
                }
            }
        }
    }

    if registered.len() == 1 {
        return Some(registered[0].clone());
    }

    None
}

/// Reads the user's bare-slash mode from HKCU.
pub fn get_bare_slash_mode() -> BareSlashMode {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;

        if let Ok(key) = CURRENT_USER.open(FSW_SETTINGS_KEY) {
            if let Ok(val) = key.get_u32(FSW_BARE_SLASH_MODE_VALUE) {
                return if val != 0 {
                    BareSlashMode::DefaultDistribution
                } else {
                    BareSlashMode::DistributionList
                };
            }
        }
    }
    BareSlashMode::DistributionList
}

/// Reads any pinned bare-slash distribution name from HKCU.
pub fn get_bare_slash_override() -> String {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;

        if let Ok(key) = CURRENT_USER.open(FSW_SETTINGS_KEY) {
            if let Ok(val) = key.get_string(FSW_BARE_SLASH_DISTRIBUTION_VALUE) {
                return val;
            }
        }
    }
    String::new()
}

/// Reads the custom bare-slash root. An absent, empty, or malformed value is
/// no root at all — a bad stored value must degrade to today's behavior, not
/// poison every resolve.
pub fn get_bare_slash_root() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;

        if let Ok(key) = CURRENT_USER.open(FSW_SETTINGS_KEY) {
            if let Ok(val) = key.get_string(FSW_BARE_SLASH_ROOT_VALUE) {
                if is_valid_windows_root(&val) {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Returns whether forward-slash path resolution is globally paused/disabled.
pub fn is_disabled() -> bool {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;

        if let Ok(key) = CURRENT_USER.open(FSW_SETTINGS_KEY) {
            if let Ok(val) = key.get_u32(FSW_DISABLED_VALUE) {
                return val != 0;
            }
        }
    }
    false
}

/// Sets the disabled state in HKCU\Software\ForwardSlashWindows\Settings.
/// Persists the global pause flag.
///
/// The write goes through `reg.exe` (a System32 child, i.e. a process without
/// package identity) because a packaged process's own registry writes are
/// virtualized into the package's private hive — and the PowerShell module
/// reads this flag from an unpackaged shell, where virtualized writes are
/// invisible (verified 2026-09-04; see docs/compatibility.md). The BareSlash*
/// settings are deliberately NOT routed this way: they are only ever read
/// back by packaged components, so their virtualized writes are
/// self-consistent.
pub fn persist_disabled(disabled: bool) -> Result<(), u32> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let value = if disabled { "1" } else { "0" };
        let status = Command::new("reg.exe")
            .args([
                "add",
                &format!("HKCU\\{FSW_SETTINGS_KEY}"),
                "/v",
                FSW_DISABLED_VALUE,
                "/t",
                "REG_DWORD",
                "/d",
                value,
                "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| e.raw_os_error().unwrap_or(u32::MAX as i32) as u32)?;
        if status.success() {
            Ok(())
        } else {
            Err(status.code().unwrap_or(u32::MAX as i32) as u32)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = disabled;
        Ok(())
    }
}

/// Updates bare slash settings in HKCU.
pub fn write_bare_slash_settings(
    default_mode: bool,
    pinned_distribution: &str,
    root: Option<&str>,
) -> Result<(), u32> {
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;

        let key = CURRENT_USER
            .create(FSW_SETTINGS_KEY)
            .map_err(|e| e.code().0 as u32)?;
        let val = if default_mode { 1u32 } else { 0u32 };
        key.set_u32(FSW_BARE_SLASH_MODE_VALUE, val)
            .map_err(|e| e.code().0 as u32)?;

        if default_mode && !pinned_distribution.is_empty() {
            key.set_string(FSW_BARE_SLASH_DISTRIBUTION_VALUE, pinned_distribution)
                .map_err(|e| e.code().0 as u32)?;
        } else {
            let _ = key.remove_value(FSW_BARE_SLASH_DISTRIBUTION_VALUE);
        }

        match root {
            Some(path) if !path.is_empty() => {
                key.set_string(FSW_BARE_SLASH_ROOT_VALUE, path)
                    .map_err(|e| e.code().0 as u32)?;
            }
            _ => {
                let _ = key.remove_value(FSW_BARE_SLASH_ROOT_VALUE);
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (default_mode, pinned_distribution, root);
        Ok(())
    }
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

    let first_segment = input[1..].split('/').next().unwrap_or_default();
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
pub fn adapter_installed(path: &str) -> bool {
    registry_string_equals(path, "State", "installed")
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

/// Whether the broker's message-only window is present on this desktop.
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
