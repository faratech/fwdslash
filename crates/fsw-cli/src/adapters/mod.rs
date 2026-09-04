//! Native adapter installers for the shell integrations.
//!
//! Replaces the retired `tools/Install-*.ps1` / `Uninstall-*.ps1` scripts:
//! `fwdslash integration <id> enable|disable` runs entirely in this process
//! (~50 ms) instead of spawning PowerShell. Registry state lives in the real
//! hive via `reg.exe` (see `reg.rs`); file operations are direct because
//! packaged file writes to `%LOCALAPPDATA%` and `Documents` are not
//! virtualized (verified 2026-09-04).
//!
//! This module also owns the adapter payload version: `PAYLOAD_VERSION` names
//! the shared `PowerShell\<version>` module directory and is embedded in the
//! profile marker blocks.

#[cfg(windows)]
pub mod cmd;
#[cfg(windows)]
pub mod powershell;
#[cfg(windows)]
pub mod reg;
pub mod profile;
pub mod state;
#[cfg(test)]
mod tests;

#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
pub use state::Edition;

/// CREATE_NO_WINDOW for the real-process children.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The adapter payload directory version. One of the product's `0.0.2`
/// copies — bump it together with the other version strings (CLAUDE.md).
pub const PAYLOAD_VERSION: &str = "0.0.2";

/// A user-facing adapter failure. The message is shown verbatim.
#[derive(Debug, Clone)]
pub struct AdapterError {
    message: String,
}

impl AdapterError {
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl core::fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AdapterError {}

impl From<std::io::Error> for AdapterError {
    fn from(error: std::io::Error) -> Self {
        Self::new(&format!("file operation failed ({error})."))
    }
}

/// Maps a `windows-registry` error (its `Error` type is private, so this
/// takes any Display — a mapping helper rather than a `From` impl).
pub fn registry_error(error: impl core::fmt::Display) -> AdapterError {
    AdapterError::new(&format!("registry read failed ({error})."))
}

/// Handles `fwdslash integration <id> <enable|disable>` for the shell
/// adapters (the `windows` integration is registry-only and stays in
/// `main.rs`). Exit codes: 0 success/no-op, 1 failure, 2 unknown id.
pub fn set_integration(id: &str, enabled: bool) -> i32 {
    #[cfg(windows)]
    {
        let edition = match id {
            "cmd" => None,
            "windows-powershell" => Some(state::Edition::WindowsPowerShell),
            "powershell" => Some(state::Edition::PowerShell),
            _ => return 2,
        };

        // PowerShell 7 must exist before its adapter can be installed.
        if enabled && edition == Some(state::Edition::PowerShell)
            && !fsw_core::executable_available("pwsh.exe")
        {
            println!("PowerShell 7 is not installed.");
            return 1;
        }

        let controller = std::env::current_exe().unwrap_or_default();
        let result = match edition {
            None => {
                if fsw_core::adapter_installed(fsw_core::CMD_ADAPTER_KEY) == enabled {
                    return 0;
                }
                if enabled {
                    cmd::install(&controller)
                } else {
                    cmd::uninstall()
                }
            }
            Some(edition) => {
                // Idempotence: an enable/disable that matches the stored
                // marker is a silent no-op, exactly like the script flow.
                let key = format!(
                    "Software\\ForwardSlashWindows\\PowerShellAdapter\\{}",
                    edition.registry_leaf()
                );
                if fsw_core::adapter_installed(&key) == enabled {
                    return 0;
                }
                if enabled {
                    powershell::install(edition, &controller)
                } else {
                    powershell::uninstall(edition)
                }
            }
            #[allow(unreachable_patterns)]
            Some(_) => return 2,
        };

        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (id, enabled);
        1
    }
}

/// Best-effort removal of every shell adapter during `fwdslash uninstall`.
/// Individual failures are reported but do not stop the others.
pub fn sweep_uninstall() -> i32 {
    let mut worst = 0;
    #[cfg(windows)]
    for (id, label) in [
        ("cmd", "Command Prompt"),
        ("windows-powershell", "Windows PowerShell"),
        ("powershell", "PowerShell 7"),
    ] {
        let installed = match id {
            "cmd" => fsw_core::adapter_installed(fsw_core::CMD_ADAPTER_KEY),
            "windows-powershell" => fsw_core::adapter_installed(&format!(
                "{}WindowsPowerShell",
                fsw_core::POWERSHELL_ADAPTER_ROOT
            )),
            _ => fsw_core::adapter_installed(&format!(
                "{}PowerShell",
                fsw_core::POWERSHELL_ADAPTER_ROOT
            )),
        };
        if !installed {
            continue;
        }
        let code = set_integration(id, false);
        if code != 0 {
            println!("The {label} adapter could not be removed automatically.");
            worst = 1;
        }
    }
    worst
}

/// The payload directory for an adapter kind, relative to the executable:
/// a packaged install carries `shell\` beside the exes; a dev build run from
/// `target\<triple>\release` falls back to the repo checkout.
#[cfg(windows)]
pub fn payload_source_dir(kind: &str) -> Result<PathBuf, AdapterError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(|| AdapterError::new("could not locate the executable directory"))?;
    let packaged = exe_dir.join("shell").join(kind);
    if packaged.is_dir() {
        return Ok(packaged);
    }
    for ancestor in exe_dir.ancestors().skip(1) {
        let repo = ancestor.join("shell").join(kind);
        if repo.is_dir() {
            return Ok(repo);
        }
    }
    Err(AdapterError::new(&format!(
        "the {kind} shell payload could not be located next to the executable or in the repository"
    )))
}

#[cfg(windows)]
pub fn local_app_data() -> Result<PathBuf, AdapterError> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| AdapterError::new("%LOCALAPPDATA% is not set."))
}

/// The real Documents folder, following OneDrive redirection
/// (`SHGetKnownFolderPath`, not `%USERPROFILE%\Documents`).
#[cfg(windows)]
pub fn documents_dir() -> Result<PathBuf, AdapterError> {
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Documents};

    unsafe {
        let mut path = std::ptr::null_mut();
        let status = SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut path);
        if status != 0 || path.is_null() {
            return Err(AdapterError::new("could not locate the Documents folder"));
        }
        let mut len = 0usize;
        while *path.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(path, len));
        windows_sys::Win32::System::Com::CoTaskMemFree(path.cast());
        Ok(PathBuf::from(text))
    }
}

/// Collision-resistant transaction id for staging/rollback names: tick plus
/// process id. Not a GUID — it only has to be unique within one user session.
#[cfg(windows)]
pub fn new_transaction_id() -> String {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;

    format!("{:x}-{:x}", unsafe { GetTickCount64() }, std::process::id())
}

/// Creates a directory through `cmd.exe` — a real process, so the directory
/// lands in the real file system even when this process is packaged (MSIX
/// virtualization would otherwise redirect it into LocalCache, where
/// unpackaged shells can never see the adapter payload). Already-existing
/// directories are fine.
#[cfg(windows)]
pub fn real_make_dir(path: &Path) -> Result<(), AdapterError> {
    let output = Command::new("cmd.exe")
        .args(["/c", "mkdir"])
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| AdapterError::new(&format!("mkdir could not be started ({error}).")))?;
    if output.status.success() || path.is_dir() {
        return Ok(());
    }
    Err(AdapterError::new(&format!(
        "could not create the adapter directory: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Copies one file through `cmd.exe` (real process — see `real_make_dir`).
/// Sources may live in the package's WindowsApps directory (readable by path);
/// destinations land in the real file system.
#[cfg(windows)]
pub fn real_copy_file(source: &Path, destination_dir: &Path) -> Result<(), AdapterError> {
    let file_name = source
        .file_name()
        .ok_or_else(|| AdapterError::new("invalid payload source name"))?;
    let output = Command::new("cmd.exe")
        .args(["/c", "copy", "/y"])
        .arg(source)
        .arg(destination_dir.join(file_name))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| AdapterError::new(&format!("copy could not be started ({error}).")))?;
    if output.status.success() && destination_dir.join(file_name).is_file() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        return Err(AdapterError::new(&format!(
            "could not deploy {}",
            file_name.to_string_lossy()
        )));
    }
    Err(AdapterError::new(&format!(
        "could not deploy {}: {}",
        file_name.to_string_lossy(),
        detail
    )))
}

/// Wraps a file error with Controlled Folder Access guidance when the failure
/// looks like a Defender block (access denied against a protected folder).
#[cfg(windows)]
pub fn explain_file_error(error: &AdapterError, what: &str) -> AdapterError {
    let text = error.to_string();
    if text.contains("os error 5") || text.contains("Access is denied") {
        return AdapterError::new(&format!(
            "{what} was blocked by Windows Controlled Folder Access. Allow Forward Slash Windows under Windows Security > Virus & threat protection > Ransomware protection > Allow an app through Controlled folder access, then try again."
        ));
    }
    AdapterError::new(&text)
}

/// Atomic file replacement: write to a sibling temp file, flush, then rename
/// over the destination (Windows rename replaces existing files).
#[cfg(windows)]
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AdapterError> {
    use std::io::Write;

    let directory = path
        .parent()
        .ok_or_else(|| AdapterError::new("invalid profile path"))?;
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".fsw-{}.tmp", new_transaction_id()));
    let result = (|| -> Result<(), AdapterError> {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
