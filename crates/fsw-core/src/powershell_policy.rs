//! Bounded, native Windows PowerShell execution-policy probing.
//!
//! This deliberately asks the System32 Windows PowerShell executable without
//! a profile or inherited process policy override. It never changes policy.

use std::fmt;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(windows)]
const PROBE_OUTPUT_LIMIT: u64 = 512;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Restricted,
    AllSigned,
    RemoteSigned,
    Unrestricted,
    Bypass,
    Undefined,
}

impl ExecutionPolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restricted => "Restricted",
            Self::AllSigned => "AllSigned",
            Self::RemoteSigned => "RemoteSigned",
            Self::Unrestricted => "Unrestricted",
            Self::Bypass => "Bypass",
            Self::Undefined => "Undefined",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    Unavailable,
    Failed,
    TimedOut,
    MalformedOutput,
}

impl fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::Unavailable => "the native Windows PowerShell executable could not be started",
            Self::Failed => "the native Windows PowerShell probe failed",
            Self::TimedOut => "the native Windows PowerShell probe timed out",
            Self::MalformedOutput => {
                "the native Windows PowerShell probe returned an unknown policy"
            }
        };
        write!(
            formatter,
            "Could not verify the effective Windows PowerShell execution policy: {detail}."
        )
    }
}

impl std::error::Error for ProbeError {}

/// The exact remedy for a user-controlled Restricted policy. Group Policy has
/// higher precedence, so never promise this command can override it.
#[must_use]
pub const fn restricted_policy_message() -> &'static str {
    "Windows PowerShell is blocked by the Restricted execution policy. Run `Set-ExecutionPolicy -Scope CurrentUser -ExecutionPolicy RemoteSigned` in Windows PowerShell, then try again. If execution policy is managed by Group Policy, contact your administrator."
}

pub fn parse_effective_execution_policy(output: &str) -> Result<ExecutionPolicy, ProbeError> {
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(line) = lines.next() else {
        return Err(ProbeError::MalformedOutput);
    };
    if lines.next().is_some() {
        return Err(ProbeError::MalformedOutput);
    }
    match line {
        "Restricted" => Ok(ExecutionPolicy::Restricted),
        "AllSigned" => Ok(ExecutionPolicy::AllSigned),
        "RemoteSigned" => Ok(ExecutionPolicy::RemoteSigned),
        "Unrestricted" => Ok(ExecutionPolicy::Unrestricted),
        "Bypass" => Ok(ExecutionPolicy::Bypass),
        "Undefined" => Ok(ExecutionPolicy::Undefined),
        _ => Err(ProbeError::MalformedOutput),
    }
}

fn probe_command(shell: &Path) -> Command {
    let mut command = Command::new(shell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-ExecutionPolicy",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // A parent process launched with `-ExecutionPolicy Bypass` sets this
        // variable. Removing it observes an ordinary fresh user session.
        .env_remove("PSExecutionPolicyPreference");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(windows)]
fn native_windows_powershell() -> Result<String, ProbeError> {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = [0u16; 260];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(ProbeError::Unavailable);
    }
    let Some(directory) = buffer.get(..length as usize) else {
        return Err(ProbeError::Unavailable);
    };
    Ok(format!(
        "{}\\WindowsPowerShell\\v1.0\\powershell.exe",
        String::from_utf16_lossy(directory)
    ))
}

#[cfg(windows)]
pub fn probe_windows_powershell() -> Result<ExecutionPolicy, ProbeError> {
    use std::io::Read;
    use std::time::Instant;

    let shell = native_windows_powershell()?;
    let mut child = probe_command(Path::new(&shell))
        .spawn()
        .map_err(|_| ProbeError::Unavailable)?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::TimedOut);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::Failed);
            }
        }
    };
    if !status.success() {
        return Err(ProbeError::Failed);
    }
    let mut output = String::new();
    child
        .stdout
        .take()
        .ok_or(ProbeError::Failed)?
        .take(PROBE_OUTPUT_LIMIT)
        .read_to_string(&mut output)
        .map_err(|_| ProbeError::Failed)?;
    parse_effective_execution_policy(&output)
}

#[cfg(not(windows))]
pub fn probe_windows_powershell() -> Result<ExecutionPolicy, ProbeError> {
    Err(ProbeError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::{ExecutionPolicy, ProbeError, parse_effective_execution_policy, probe_command};
    use std::path::Path;

    #[test]
    fn parses_restricted_default_and_remote_signed_policy() {
        assert_eq!(
            parse_effective_execution_policy("Restricted\r\n"),
            Ok(ExecutionPolicy::Restricted)
        );
        assert_eq!(
            parse_effective_execution_policy("Undefined\n"),
            Ok(ExecutionPolicy::Undefined)
        );
        assert_eq!(
            parse_effective_execution_policy("RemoteSigned\n"),
            Ok(ExecutionPolicy::RemoteSigned)
        );
    }

    #[test]
    fn rejects_failed_or_malformed_probe_answers() {
        assert_eq!(
            parse_effective_execution_policy("\n"),
            Err(ProbeError::MalformedOutput)
        );
        assert_eq!(
            parse_effective_execution_policy("Unknown\n"),
            Err(ProbeError::MalformedOutput)
        );
        assert_eq!(
            parse_effective_execution_policy("Restricted\nRemoteSigned\n"),
            Err(ProbeError::MalformedOutput)
        );
        assert!(
            ProbeError::Unavailable
                .to_string()
                .contains("Could not verify")
        );
        assert!(ProbeError::Failed.to_string().contains("Could not verify"));
        assert!(ProbeError::TimedOut.to_string().contains("timed out"));
    }

    #[test]
    fn probe_removes_the_inherited_process_policy_override() {
        let command = probe_command(Path::new("powershell.exe"));
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new("PSExecutionPolicyPreference") && value.is_none()
        }));
    }
}
