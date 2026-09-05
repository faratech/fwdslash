//! The real-hive registry bridge for the adapter installers.
//!
//! Rule: **adapter registry state lives in the real hive — write it with
//! reg.exe, read it with windows-registry.** A packaged process's own
//! registry writes are virtualized into the package's private hive (verified
//! 2026-09-04), while `reg.exe` spawned as a System32 child writes through to
//! the real hive, where unpackaged shells — the whole point of the adapters —
//! read them. Reads stay on `windows-registry`, whose merged view is always
//! correct and can also see values written by the unpackaged dev build.
//!
//! `read_raw_string` exists because `windows-registry`'s string conversions
//! accept expanded `REG_EXPAND_SZ` data (RegQueryValueExW without
//! RRF_NOEXPAND expands), which would corrupt the kind-preserving AutoRun
//! snapshot.

use super::AdapterError;
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// RRF_NOEXPAND: return REG_EXPAND_SZ data verbatim.
const RRF_NOEXPAND: u32 = 0x1000_0000;

/// Registry value kinds the adapters may write or preserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegKind {
    Sz,
    ExpandSz,
    /// Recognised so a DWORD `AutoRun` is refused rather than rewritten as a
    /// string; the adapters never construct or write this kind.
    #[allow(dead_code)]
    Dword,
}

impl RegKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sz => "REG_SZ",
            Self::ExpandSz => "REG_EXPAND_SZ",
            Self::Dword => "REG_DWORD",
        }
    }

    /// The label stored in the marker's `OriginalKind` value.
    pub fn marker_label(self) -> &'static str {
        match self {
            Self::Sz => "String",
            Self::ExpandSz => "ExpandString",
            Self::Dword => "Dword",
        }
    }

    /// Parses a marker `OriginalKind` label; `None` for kinds the adapters
    /// must not overwrite.
    pub fn from_marker_label(label: &str) -> Option<Self> {
        match label {
            "String" => Some(Self::Sz),
            "ExpandString" => Some(Self::ExpandSz),
            _ => None,
        }
    }
}

fn reg_exe() -> Result<String, AdapterError> {
    let mut buffer = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize >= buffer.len() {
        return Err(AdapterError::new(
            "could not locate the System directory for reg.exe",
        ));
    }
    Ok(format!(
        "{}\\reg.exe",
        String::from_utf16_lossy(&buffer[..len as usize])
    ))
}

fn run(arguments: &[&str]) -> Result<(), AdapterError> {
    let exe = reg_exe()?;
    let output = Command::new(&exe)
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| AdapterError::new(&format!("reg.exe could not be started ({error}).")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Err(AdapterError::new(&format!(
        "registry update failed: {}",
        detail.trim()
    )))
}

/// Sets a string value in the real hive. `reg.exe` creates intermediate keys.
pub fn set_string(subkey: &str, name: &str, data: &str) -> Result<(), AdapterError> {
    run(&[
        "add",
        &format!("HKCU\\{subkey}"),
        "/v",
        name,
        "/t",
        "REG_SZ",
        "/d",
        data,
        "/f",
    ])
}

/// Sets a string value preserving an existing `REG_EXPAND_SZ` kind.
pub fn set_string_kind(
    subkey: &str,
    name: &str,
    data: &str,
    kind: RegKind,
) -> Result<(), AdapterError> {
    debug_assert!(matches!(kind, RegKind::Sz | RegKind::ExpandSz));
    run(&[
        "add",
        &format!("HKCU\\{subkey}"),
        "/v",
        name,
        "/t",
        kind.as_str(),
        "/d",
        data,
        "/f",
    ])
}

/// Sets a DWORD value in the real hive.
pub fn set_dword(subkey: &str, name: &str, value: u32) -> Result<(), AdapterError> {
    run(&[
        "add",
        &format!("HKCU\\{subkey}"),
        "/v",
        name,
        "/t",
        "REG_DWORD",
        "/d",
        &value.to_string(),
        "/f",
    ])
}

/// Deletes a value; deleting an absent value succeeds.
pub fn delete_value(subkey: &str, name: &str) -> Result<(), AdapterError> {
    run(&[
        "delete",
        &format!("HKCU\\{subkey}"),
        "/v",
        name,
        "/f",
    ])
}

/// Deletes a key and everything under it.
pub fn delete_tree(subkey: &str) -> Result<(), AdapterError> {
    run(&["delete", &format!("HKCU\\{subkey}"), "/f"])
}

/// Reads a string value from the real-hive merged view WITHOUT expanding
/// `REG_EXPAND_SZ` data — the AutoRun snapshot must preserve `%VAR%`
/// references exactly. `Ok(None)` when the value does not exist; other types
/// than `REG_SZ`/`REG_EXPAND_SZ` read as their raw text and are rejected by
/// the caller's kind check.
pub fn read_raw_string(
    subkey: &str,
    name: &str,
) -> Result<Option<(RegKind, String)>, AdapterError> {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    const ERROR_MORE_DATA: u32 = 234;
    const ERROR_FILE_NOT_FOUND: u32 = 2;

    unsafe {
        let subkey = to_wide(subkey);
        let name = to_wide(name);
        let key = HKEY_CURRENT_USER;
        // Two passes: size first, then the data with RRF_NOEXPAND so
        // REG_EXPAND_SZ comes back verbatim.
        let mut kind = 0u32;
        let mut size = 0u32;
        let status = RegGetValueW(
            key,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        );
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status == ERROR_MORE_DATA || status == 0 {
            // size is in bytes (incl. the terminator); retry until stable.
            for _ in 0..2 {
                let mut data = vec![0u8; size as usize];
                let mut kind2 = 0u32;
                let mut len = size;
                let status = RegGetValueW(
                    key,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                    &mut kind2,
                    data.as_mut_ptr().cast(),
                    &mut len,
                );
                if status == ERROR_MORE_DATA {
                    size = len;
                    continue;
                }
                if status != 0 {
                    return Err(AdapterError::new(&format!(
                        "registry read failed with error {status}"
                    )));
                }
                data.truncate(len as usize);
                let text = decode_reg_string(&data);
                let kind = match kind2 {
                    1 => RegKind::Sz,
                    2 => RegKind::ExpandSz,
                    other => {
                        return Err(AdapterError::new(&format!(
                            "the existing value has an unsupported registry type ({other})"
                        )));
                    }
                };
                return Ok(Some((kind, text)));
            }
            return Err(AdapterError::new("registry read did not stabilize"));
        }
        Err(AdapterError::new(&format!(
            "registry read failed with error {status}"
        )))
    }
}

/// Decodes the raw `REG_SZ`/`REG_EXPAND_SZ` bytes `RegGetValueW` returns.
///
/// **Order is the whole point.** Bytes become UTF-16 code units *first* (a
/// trailing odd byte, which a well-formed value never has, is ignored), and
/// only then are the trailing `0u16` terminators stripped — one or more,
/// since `RegGetValueW` may report a size that carries a second NUL.
///
/// 0.0.2 did it the other way round: it popped trailing zero *bytes* off the
/// buffer before pairing them. Every value whose last character is ASCII ends
/// in a zero high byte (`"` is `22 00`), so that pop ate half of the final
/// code unit and left an odd byte count whose last pair `chunks_exact(2)`
/// silently discarded — the returned string was one character short. That is
/// what made `judge_autorun` see `call "…fsw-autorun.cmd` against an
/// `InstalledAutoRun` of `call "…fsw-autorun.cmd"` and refuse the upgrade,
/// and what truncated the `OriginalAutoRun` snapshot of every 0.0.2 install.
#[must_use]
pub fn decode_reg_string(bytes: &[u8]) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    let mut iter = bytes.iter().copied();
    while let (Some(low), Some(high)) = (iter.next(), iter.next()) {
        units.push(u16::from_le_bytes([low, high]));
    }
    while units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16_lossy(&units)
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
