//! The one writer for `HKCU\Software\ForwardSlashWindows\Settings`.
//!
//! **Every** write to that key goes through [`set_setting_u32`],
//! [`set_setting_u64`], [`set_setting_string`] or [`delete_setting`]. Nothing
//! else may call `windows_registry`'s set/remove on it — see the note on the
//! value-name constants in `lib.rs`.
//!
//! # Why (issue #52)
//!
//! MSIX virtualizes HKCU: a packaged process's own registry writes land in the
//! package's private hive (`…\Packages\<family>\SystemAppData\Helium\User.dat`),
//! and a process *without* package identity — which is what the cmd and
//! PowerShell adapters run, a staged unpackaged copy of `fwdslash.exe` — never
//! sees them. That is why the settings app could switch the bare-slash mode
//! while `cd /` in PowerShell kept answering with the old one: the shells read
//! the real hive, which still said `list`.
//!
//! `reg.exe` is a System32 child with no package identity, so its writes land
//! in the real hive whoever spawns it. The adapters already route their state
//! through it (`crates/fsw-cli/src/adapters/reg.rs`); `fsw-core` cannot depend
//! on `fsw-cli`, so this module is the equivalent for the settings key.
//!
//! # The dual write
//!
//! Packaged, the real hive alone is not enough: a value already present in the
//! package hive **shadows** the real one in the merged view every packaged
//! reader gets, so a real-hive-only write would just flip the split the other
//! way (Explorer/Run/the settings window stuck on the stale value instead of
//! the shells). So packaged writes go to both hives; unpackaged, the
//! in-process API *is* the real hive and one write is the whole job. That
//! decision is [`write_plan`], kept pure and tested.
//!
//! # The self-heal
//!
//! [`sync_settings_to_real_hive`] repairs installs that already carry the
//! split: the packaged process reads its own merged view (authoritative — the
//! app wrote it), asks a child `reg.exe` what the real hive holds (an
//! unpackaged process, so it cannot see the private hive — that is the point),
//! and mirrors anything missing or different. It never deletes.

#[cfg(windows)]
use crate::FSW_SETTINGS_KEY;

/// Where one Settings write has to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePlan {
    /// Unpackaged: the in-process registry API writes the real hive already,
    /// so one write is the whole job.
    RealOnly,
    /// Packaged: `reg.exe` for the real hive (what the unpackaged shell
    /// adapters read) *and* the in-process API for the package's private hive
    /// (whose stale copy would otherwise shadow the real one).
    Both,
}

/// The pure decision behind every settings write.
#[must_use]
pub const fn write_plan(packaged: bool) -> WritePlan {
    if packaged {
        WritePlan::Both
    } else {
        WritePlan::RealOnly
    }
}

/// A settings value in the only three shapes this key ever stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// `REG_DWORD`: `Disabled`, `BareSlashMode`, `AutoUpdate`.
    Dword(u32),
    /// `REG_QWORD`: `LastUpdateCheck`.
    Qword(u64),
    /// `REG_SZ`: `BareSlashDistribution`, `BareSlashRoot`, `AvailableUpdate`.
    Sz(String),
}

// `windows_registry` preserves Win32 registry errors as HRESULTs. These are
// the only two errors that prove an opened key or value is absent; access
// denial, malformed data, and every other failure must reach the caller.
#[cfg(any(windows, test))]
const HRESULT_FILE_NOT_FOUND: u32 = 0x8007_0002;
#[cfg(any(windows, test))]
const HRESULT_PATH_NOT_FOUND: u32 = 0x8007_0003;

#[cfg(any(windows, test))]
const fn registry_item_is_absent(code: u32) -> bool {
    matches!(code, HRESULT_FILE_NOT_FOUND | HRESULT_PATH_NOT_FOUND)
}

impl SettingValue {
    /// The `/t` token for `reg add`.
    #[must_use]
    pub const fn reg_type(&self) -> &'static str {
        match self {
            Self::Dword(_) => "REG_DWORD",
            Self::Qword(_) => "REG_QWORD",
            Self::Sz(_) => "REG_SZ",
        }
    }

    /// The `/d` argument for `reg add`. Numbers go out in decimal, which
    /// `reg.exe` accepts for both integer types.
    #[must_use]
    pub fn reg_data(&self) -> String {
        match self {
            Self::Dword(value) => value.to_string(),
            Self::Qword(value) => value.to_string(),
            Self::Sz(value) => value.clone(),
        }
    }

    /// Whether the real hive already holds this exact value, as `reg query`
    /// rendered it.
    #[must_use]
    pub fn matches_raw(&self, raw: &RawSetting) -> bool {
        match self {
            Self::Dword(value) => {
                raw.kind == "REG_DWORD" && parse_reg_number(&raw.data) == Some(u64::from(*value))
            }
            Self::Qword(value) => {
                raw.kind == "REG_QWORD" && parse_reg_number(&raw.data) == Some(*value)
            }
            Self::Sz(value) => raw.kind == "REG_SZ" && &raw.data == value,
        }
    }
}

/// One value exactly as `reg query` printed it: the type token and the data
/// text, both untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSetting {
    pub kind: String,
    pub data: String,
}

/// Every type token `reg query` can print. Only the first three are ever
/// written here; the rest are recognised so a foreign value on the key is
/// parsed and compared rather than mistaken for part of another line.
const REG_TYPES: [&str; 7] = [
    "REG_SZ",
    "REG_DWORD",
    "REG_QWORD",
    "REG_EXPAND_SZ",
    "REG_MULTI_SZ",
    "REG_BINARY",
    "REG_NONE",
];

/// `reg query` renders integers as `0x…`; accept decimal too, since nothing
/// guarantees the rendering across Windows builds.
#[must_use]
pub fn parse_reg_number(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    text.parse::<u64>().ok()
}

/// Parses `reg query <key>` output into its `(name, value)` pairs.
///
/// Defensive by construction: the key header, subkey lines, blank lines and
/// `reg.exe`'s own `ERROR:` text carry no type token and are skipped, so a
/// missing key or value simply yields nothing. The separator `reg.exe` prints
/// is four spaces, and the *leftmost* `    <TYPE>    ` run splits the line —
/// so a value name or a data string containing spaces survives intact.
#[must_use]
pub fn parse_reg_query(output: &str) -> Vec<(String, RawSetting)> {
    let mut values = Vec::new();
    for line in output.lines() {
        if let Some((name, kind, data)) = split_value_line(line) {
            values.push((name, RawSetting { kind, data }));
        }
    }
    values
}

/// Splits one `name    TYPE    data` line. `None` for anything that is not a
/// value line.
fn split_value_line(line: &str) -> Option<(String, String, String)> {
    let mut best: Option<(usize, usize)> = None;
    let mut best_kind = "";
    for kind in REG_TYPES {
        let separated = format!("    {kind}    ");
        let found = match line.find(&separated) {
            Some(at) => Some((at, separated.len())),
            // A value with empty data prints the type with nothing after it.
            None => {
                let bare = format!("    {kind}");
                match line.find(&bare) {
                    Some(at) if line.get(at + bare.len()..).unwrap_or_default().trim().is_empty() => {
                        Some((at, bare.len()))
                    }
                    _ => None,
                }
            }
        };
        if let Some((at, consumed)) = found
            && best.is_none_or(|(previous, _)| at < previous)
        {
            best = Some((at, consumed));
            best_kind = kind;
        }
    }
    let (at, consumed) = best?;
    let kind = best_kind;
    let name = line.get(..at)?.trim();
    if name.is_empty() {
        return None;
    }
    let data = line.get(at + consumed..).unwrap_or_default();
    Some((name.to_string(), kind.to_string(), data.to_string()))
}

/// Normalizes the two spellings `reg.exe` uses for HKCU. Key headers, unlike
/// value rows, carry no type token, so recognizing them structurally avoids
/// interpreting localized diagnostics as proof that a key is absent.
#[cfg(any(windows, test))]
fn canonical_hkcu_key(path: &str) -> Option<String> {
    let path = path.trim();
    let (hive, tail) = path.split_once('\\').unwrap_or((path, ""));
    if !(hive.eq_ignore_ascii_case("HKCU") || hive.eq_ignore_ascii_case("HKEY_CURRENT_USER")) {
        return None;
    }
    Some(if tail.is_empty() {
        "HKCU".to_owned()
    } else {
        format!("HKCU\\{tail}")
    })
}

/// Whether a successful `reg query <parent>` proves that its exact immediate
/// child is present. `Err(())` is malformed output: successful output must
/// name the queried parent and every non-value, nonblank row must be a key
/// header. This deliberately does not inspect localized stderr text.
#[cfg(any(windows, test))]
fn query_lists_child_key(output: &str, parent: &str, child: &str) -> Result<bool, ()> {
    let parent = canonical_hkcu_key(parent).ok_or(())?;
    let target = format!("{parent}\\{child}");
    let mut saw_parent = false;
    let mut saw_target = false;
    for line in output.lines() {
        if line.trim().is_empty() || split_value_line(line).is_some() {
            continue;
        }
        let header = canonical_hkcu_key(line).ok_or(())?;
        if header.eq_ignore_ascii_case(&parent) {
            saw_parent = true;
        }
        if header.eq_ignore_ascii_case(&target) {
            saw_target = true;
        }
    }
    if saw_parent { Ok(saw_target) } else { Err(()) }
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingSettingEvidence {
    /// A successful query of the settings key proved the key is readable.
    KeyReadable,
    /// A successful query of an ancestor proved the requested child is absent.
    KeyMissing,
    /// A successful query of an ancestor proved the requested child exists.
    KeyExists,
    /// Successful `reg.exe` output did not have the expected key structure.
    Malformed,
    /// `reg.exe` could not be started, waited for, or completed in time.
    Unavailable,
}

/// Resolves the ambiguous nonzero status from `reg query <key> /v <value>`.
/// Only positive structural evidence makes an absent value a success.
#[cfg(any(windows, test))]
fn resolve_missing_setting_read(
    _value_query_error: u32,
    evidence: MissingSettingEvidence,
) -> Result<bool, u32> {
    match evidence {
        MissingSettingEvidence::KeyReadable | MissingSettingEvidence::KeyMissing => Ok(true),
        MissingSettingEvidence::KeyExists => Ok(false),
        MissingSettingEvidence::Malformed | MissingSettingEvidence::Unavailable => Err(u32::MAX),
    }
}

/// The names whose real-hive copy is missing or different, in `merged` order.
/// Absence in `merged` is never a delete: a value the packaged app does not
/// hold is simply left alone.
#[must_use]
pub fn sync_plan<'a>(
    merged: &'a [(&'a str, SettingValue)],
    real: &[(String, RawSetting)],
) -> Vec<&'a str> {
    merged
        .iter()
        .filter(|(name, value)| {
            // Registry value names are case-insensitive.
            match real
                .iter()
                .find(|(real_name, _)| real_name.eq_ignore_ascii_case(name))
            {
                Some((_, raw)) => !value.matches_raw(raw),
                None => true,
            }
        })
        .map(|(name, _)| *name)
        .collect()
}

/// The settings values a packaged process can mirror, in sync order.
///
/// `update.rs` owns the last three names; they are repeated here rather than
/// imported to keep this list one readable inventory of the key.
#[cfg(windows)]
const SYNCED_DWORDS: [&str; 3] = [
    crate::FSW_DISABLED_VALUE,
    crate::FSW_BARE_SLASH_MODE_VALUE,
    crate::update::AUTO_UPDATE_VALUE,
];
#[cfg(windows)]
const SYNCED_QWORDS: [&str; 1] = [crate::update::LAST_UPDATE_CHECK_VALUE];
#[cfg(windows)]
const SYNCED_STRINGS: [&str; 3] = [
    crate::FSW_BARE_SLASH_DISTRIBUTION_VALUE,
    crate::FSW_BARE_SLASH_ROOT_VALUE,
    crate::update::AVAILABLE_UPDATE_VALUE,
];

#[cfg(windows)]
mod imp {
    use super::{
        FSW_SETTINGS_KEY, RawSetting, SYNCED_DWORDS, SYNCED_QWORDS, SYNCED_STRINGS, SettingValue,
        WritePlan, MissingSettingEvidence, parse_reg_query, query_lists_child_key,
        registry_item_is_absent, resolve_missing_setting_read, sync_plan, write_plan,
    };
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use windows_registry::CURRENT_USER;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// `reg.exe` writes one value and exits; anything past this is a wedged
    /// child, and the callers (the broker's sweep thread, a settings-window
    /// background task) must not be parked on it forever.
    const REG_TIMEOUT: Duration = Duration::from_secs(10);
    const REG_POLL: Duration = Duration::from_millis(20);
    /// Reported when `reg.exe` could not be located, started, or finished.
    const REG_UNAVAILABLE: u32 = u32::MAX;

    /// The System32 copy, never a `reg.exe` some directory on PATH supplies.
    fn reg_exe() -> Option<String> {
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

        let mut buffer = [0u16; 260];
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 || length as usize >= buffer.len() {
            return None;
        }
        let directory = String::from_utf16_lossy(buffer.get(..length as usize)?);
        Some(format!("{directory}\\reg.exe"))
    }

    /// Runs `reg.exe` with a bounded wait, discarding its output.
    fn run_reg(arguments: &[&str]) -> Result<(), u32> {
        let Some(exe) = reg_exe() else {
            return Err(REG_UNAVAILABLE);
        };
        let spawned = Command::new(exe)
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else {
            return Err(REG_UNAVAILABLE);
        };
        let deadline = Instant::now() + REG_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(status.code().unwrap_or(u32::MAX as i32) as u32)
                    };
                }
                Ok(None) => {}
                Err(_) => return Err(REG_UNAVAILABLE),
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(REG_UNAVAILABLE);
            }
            std::thread::sleep(REG_POLL);
        }
    }

    /// Runs `reg.exe` and returns its stdout, with the same bound. The reader
    /// runs on a thread of its own so a child that never closes the pipe
    /// cannot park the caller; on timeout that thread is left to finish. A
    /// nonzero child status is deliberately an error, not an empty key: `reg
    /// query` uses the same status for an absent item and for access failures.
    fn reg_output(arguments: &[&str]) -> Result<String, u32> {
        let Some(exe) = reg_exe() else {
            return Err(REG_UNAVAILABLE);
        };
        let child = Command::new(exe)
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| REG_UNAVAILABLE)?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("fsw-reg-query".to_owned())
            .spawn(move || {
                let _ = sender.send(child.wait_with_output().ok());
            })
            .map_err(|_| REG_UNAVAILABLE)?;
        match receiver.recv_timeout(REG_TIMEOUT) {
            Ok(Some(output)) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Ok(Some(output)) => Err(output.status.code().unwrap_or(i32::MAX) as u32),
            _ => Err(REG_UNAVAILABLE),
        }
    }

    fn settings_key_argument() -> String {
        format!("HKCU\\{FSW_SETTINGS_KEY}")
    }

    fn parent_key_and_child(key: &str) -> Option<(&str, &str)> {
        key.rsplit_once('\\')
    }

    /// Proves a missing settings key by walking readable ancestors. A parent
    /// which lists its child proves the child exists, so the original failing
    /// query remains an error (for example, access denied). A parent which
    /// does not list it proves the entire descendant path is absent.
    fn settings_key_is_proven_absent(key: &str, original_error: u32) -> Result<bool, u32> {
        let mut child_key = key;
        loop {
            let Some((parent, child)) = parent_key_and_child(child_key) else {
                return Err(original_error);
            };
            match reg_output(&["query", parent]) {
                Ok(output) => {
                    let evidence = match query_lists_child_key(&output, parent, child) {
                        Ok(true) => MissingSettingEvidence::KeyExists,
                        Ok(false) => MissingSettingEvidence::KeyMissing,
                        Err(()) => MissingSettingEvidence::Malformed,
                    };
                    return resolve_missing_setting_read(original_error, evidence);
                }
                Err(REG_UNAVAILABLE) => {
                    return resolve_missing_setting_read(
                        original_error,
                        MissingSettingEvidence::Unavailable,
                    );
                }
                // This ancestor may itself be absent. Its parent can prove
                // that structurally; if none can, preserve the original error.
                Err(_) => child_key = parent,
            }
        }
    }

    /// Writes one value to the real hive through `reg.exe`.
    pub(super) fn write_real_hive(name: &str, value: &SettingValue) -> Result<(), u32> {
        let key = settings_key_argument();
        let data = value.reg_data();
        run_reg(&[
            "add",
            &key,
            "/v",
            name,
            "/t",
            value.reg_type(),
            "/d",
            &data,
            "/f",
        ])
    }

    /// Deletes one value from the real hive. An absent value is success —
    /// `reg delete` reports a failure for it, so ask first.
    fn delete_real_hive(name: &str) -> Result<(), u32> {
        let key = settings_key_argument();
        if read_real_hive_value(name)?.is_none() {
            return Ok(());
        }
        run_reg(&["delete", &key, "/v", name, "/f"])
    }

    /// One value's real-hive rendering, or `None` when it is confirmed absent.
    ///
    /// `reg query <key> /v <name>` returns the same nonzero status for a
    /// missing value and access denied. A second *successful* key query is the
    /// positive proof that the key is readable, making the former absence;
    /// without it, the original child failure is propagated.
    fn read_real_hive_value(name: &str) -> Result<Option<RawSetting>, u32> {
        let key = settings_key_argument();
        match reg_output(&["query", &key, "/v", name]) {
            Ok(output) => parse_reg_query(&output)
                .into_iter()
                .find(|(found, _)| found.eq_ignore_ascii_case(name))
                .map(|(_, raw)| Some(raw))
                // A successful query whose output cannot name the requested
                // value is malformed output, never evidence of absence.
                .ok_or(REG_UNAVAILABLE),
            // A timeout/start/wait failure is operational, not the ambiguous
            // "item missing" status that a successful parent query can
            // disambiguate.
            Err(REG_UNAVAILABLE) => Err(REG_UNAVAILABLE),
            Err(query_error) => match reg_output(&["query", &key]) {
                Ok(output) => {
                    let evidence = if query_lists_child_key(&output, &key, "__never__").is_ok() {
                        MissingSettingEvidence::KeyReadable
                    } else {
                        MissingSettingEvidence::Malformed
                    };
                    match resolve_missing_setting_read(query_error, evidence) {
                        Ok(true) => Ok(None),
                        Ok(false) => Err(query_error),
                        Err(error) => Err(error),
                    }
                }
                Err(REG_UNAVAILABLE) => Err(REG_UNAVAILABLE),
                Err(_) => settings_key_is_proven_absent(&key, query_error)
                    .and_then(|absent| absent.then_some(None).ok_or(query_error)),
            },
        }
    }

    /// The whole real-hive key, as `reg query` prints it.
    fn read_real_hive() -> Vec<(String, RawSetting)> {
        let key = settings_key_argument();
        // A missing key is not an error here: it means the real hive holds
        // nothing yet, so everything the merged view has gets mirrored.
        reg_output(&["query", &key])
            .map(|output| parse_reg_query(&output))
            .unwrap_or_default()
    }

    fn write_package_hive(name: &str, value: &SettingValue) -> Result<(), u32> {
        let key = CURRENT_USER
            .create(FSW_SETTINGS_KEY)
            .map_err(|error| error.code().0 as u32)?;
        match value {
            SettingValue::Dword(data) => key.set_u32(name, *data),
            SettingValue::Qword(data) => key.set_u64(name, *data),
            SettingValue::Sz(data) => key.set_string(name, data),
        }
        .map_err(|error| error.code().0 as u32)
    }

    /// Removes a value in-process. An absent key or value is success.
    ///
    /// Opened read+write explicitly: `Key::open` asks for `KEY_READ` alone, so
    /// `remove_value` on that handle fails with access denied — which is what
    /// silently left `BareSlashDistribution` behind on the first live run of
    /// this module. Not `create`, so deleting from a key that does not exist
    /// cannot conjure one.
    fn delete_package_hive(name: &str) -> Result<(), u32> {
        let key = match CURRENT_USER.options().read().write().open(FSW_SETTINGS_KEY) {
            Ok(key) => key,
            Err(error) if registry_item_is_absent(error.code().0 as u32) => return Ok(()),
            Err(error) => return Err(error.code().0 as u32),
        };
        match key.get_type(name) {
            Ok(_) => {}
            Err(error) if registry_item_is_absent(error.code().0 as u32) => return Ok(()),
            Err(error) => return Err(error.code().0 as u32),
        }
        key.remove_value(name)
            .map_err(|error| error.code().0 as u32)
    }

    /// The dual write. Both halves are attempted whatever the first one does —
    /// leaving the hives disagreeing is the bug this module exists to fix —
    /// and the first failure is what the caller hears about.
    pub(super) fn set_setting(name: &str, value: &SettingValue) -> Result<(), u32> {
        match write_plan(crate::has_package_identity()) {
            WritePlan::RealOnly => write_package_hive(name, value),
            WritePlan::Both => {
                let real = write_real_hive(name, value);
                let package = write_package_hive(name, value);
                real.and(package)
            }
        }
    }

    pub(super) fn delete_setting(name: &str) -> Result<(), u32> {
        match write_plan(crate::has_package_identity()) {
            WritePlan::RealOnly => delete_package_hive(name),
            WritePlan::Both => {
                let real = delete_real_hive(name);
                let package = delete_package_hive(name);
                real.and(package)
            }
        }
    }

    /// Everything the merged view holds, in sync order. Values that are absent
    /// — or an empty string, which every reader already treats as absent — are
    /// left out, so the sync can never invent one.
    fn read_merged_settings() -> Vec<(&'static str, SettingValue)> {
        let Ok(key) = CURRENT_USER.open(FSW_SETTINGS_KEY) else {
            return Vec::new();
        };
        let mut values = Vec::new();
        for name in SYNCED_DWORDS {
            if let Ok(data) = key.get_u32(name) {
                values.push((name, SettingValue::Dword(data)));
            }
        }
        for name in SYNCED_QWORDS {
            if let Ok(data) = key.get_u64(name) {
                values.push((name, SettingValue::Qword(data)));
            }
        }
        for name in SYNCED_STRINGS {
            if let Ok(data) = key.get_string(name)
                && !data.is_empty()
            {
                values.push((name, SettingValue::Sz(data)));
            }
        }
        values
    }

    pub(super) fn sync_settings_to_real_hive() -> bool {
        let merged = read_merged_settings();
        if merged.is_empty() {
            return false;
        }
        let real = read_real_hive();
        let mut wrote = false;
        for name in sync_plan(&merged, &real) {
            if let Some((_, value)) = merged.iter().find(|(candidate, _)| *candidate == name)
                && write_real_hive(name, value).is_ok()
            {
                wrote = true;
            }
        }
        if wrote {
            // Category only: never the value, never the name of a distribution
            // or a path (PRIVACY.md).
            crate::diagnostic(crate::DiagEvent::SettingsSynced);
        }
        wrote
    }
}

/// Broadcasts [`crate::FSW_STATE_CHANGED_MESSAGE`] for a write that landed, and
/// says nothing about one that did not (issue #55).
///
/// Every settings write funnels through the four functions below, so this is
/// the one place that has to remember: a running settings window and the
/// broker both re-read on the broadcast, and re-reading after a *failed* write
/// would show the same state twice while claiming something changed.
#[cfg(windows)]
fn announce(result: Result<(), u32>) -> Result<(), u32> {
    if result.is_ok() {
        crate::broadcast_state_changed();
    }
    result
}

/// Writes a `REG_DWORD` settings value through [`write_plan`].
pub fn set_setting_u32(name: &str, value: u32) -> Result<(), u32> {
    #[cfg(windows)]
    {
        announce(imp::set_setting(name, &SettingValue::Dword(value)))
    }
    #[cfg(not(windows))]
    {
        let _ = (name, value);
        Ok(())
    }
}

/// Writes a `REG_QWORD` settings value through [`write_plan`].
pub fn set_setting_u64(name: &str, value: u64) -> Result<(), u32> {
    #[cfg(windows)]
    {
        announce(imp::set_setting(name, &SettingValue::Qword(value)))
    }
    #[cfg(not(windows))]
    {
        let _ = (name, value);
        Ok(())
    }
}

/// Writes a `REG_SZ` settings value through [`write_plan`].
pub fn set_setting_string(name: &str, value: &str) -> Result<(), u32> {
    #[cfg(windows)]
    {
        announce(imp::set_setting(name, &SettingValue::Sz(value.to_owned())))
    }
    #[cfg(not(windows))]
    {
        let _ = (name, value);
        Ok(())
    }
}

/// Removes a settings value from every hive [`write_plan`] names. Deleting an
/// absent value is success.
pub fn delete_setting(name: &str) -> Result<(), u32> {
    #[cfg(windows)]
    {
        announce(imp::delete_setting(name))
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Ok(())
    }
}

/// Mirrors the packaged app's settings into the real hive, so the unpackaged
/// shell adapters read what the app actually holds (issue #52).
///
/// Only a packaged process can do this: it is the one that has both views —
/// its own merged view (the package hive over the real one) and, through a
/// child `reg.exe` with no package identity, the real hive alone. Unpackaged
/// there is only ever one hive and nothing to mirror, so this is a no-op.
///
/// The merged view wins on purpose: the packaged app is what wrote these
/// values. Nothing is ever deleted — a value only the real hive holds (an
/// unpackaged `fwdslash` wrote it) is left exactly where it is.
///
/// Returns whether anything was written, i.e. whether an install was actually
/// repaired.
pub fn sync_settings_to_real_hive() -> bool {
    if !crate::has_package_identity() {
        return false;
    }
    #[cfg(windows)]
    {
        imp::sync_settings_to_real_hive()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HRESULT_FILE_NOT_FOUND, HRESULT_PATH_NOT_FOUND, MissingSettingEvidence,
        query_lists_child_key, registry_item_is_absent, resolve_missing_setting_read,
    };

    #[test]
    fn only_typed_not_found_registry_errors_are_absence() {
        assert!(registry_item_is_absent(HRESULT_FILE_NOT_FOUND));
        assert!(registry_item_is_absent(HRESULT_PATH_NOT_FOUND));
        assert!(!registry_item_is_absent(0x8007_0005)); // access denied
        assert!(!registry_item_is_absent(u32::MAX)); // unavailable child
        assert!(!registry_item_is_absent(0x8007_000D)); // malformed data
    }

    #[test]
    fn readable_key_and_absent_child_prove_delete_is_idempotent() {
        assert_eq!(
            resolve_missing_setting_read(1, MissingSettingEvidence::KeyReadable),
            Ok(true)
        );
        let parent = "HKCU\\Software\\ForwardSlashWindows";
        let output = "HKEY_CURRENT_USER\\Software\\ForwardSlashWindows\n";
        assert_eq!(query_lists_child_key(output, parent, "Settings"), Ok(false));
        assert_eq!(
            resolve_missing_setting_read(1, MissingSettingEvidence::KeyMissing),
            Ok(true)
        );
    }

    #[test]
    fn existing_inaccessible_key_preserves_the_original_query_failure() {
        assert_eq!(
            resolve_missing_setting_read(5, MissingSettingEvidence::KeyExists),
            Ok(false)
        );
    }

    #[test]
    fn malformed_or_unavailable_probes_are_not_absence() {
        let parent = "HKCU\\Software\\ForwardSlashWindows";
        assert_eq!(query_lists_child_key("not registry output", parent, "Settings"), Err(()));
        assert_eq!(
            resolve_missing_setting_read(1, MissingSettingEvidence::Malformed),
            Err(u32::MAX)
        );
        assert_eq!(
            resolve_missing_setting_read(1, MissingSettingEvidence::Unavailable),
            Err(u32::MAX)
        );
    }
}
