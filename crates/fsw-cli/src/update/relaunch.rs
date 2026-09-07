//! The watchdog: what brings the product back after an update closed it.
//!
//! Every route that can succeed force-closes the package, and a process the
//! Store has just terminated cannot relaunch itself. A one-shot per-user
//! scheduled task can: the Task Scheduler service starts it in its own session,
//! outside the package and outside any job object we were in
//! (`crate::scheduled_task` has the full argument). So the task is registered
//! **before** the install, not after it.
//!
//! The script is a `.cmd` that optionally runs one lead command (the staged
//! helper, or `winget`), then an inline PowerShell watchdog, then deletes the
//! task and itself. The watchdog polls `Get-AppxPackage` until the installed
//! version is greater than the one that was running, up to a 45-minute
//! ceiling, then relaunches.
//!
//! Two rules make the PowerShell text safe to embed in a batch file, and both
//! are asserted by the tests:
//!
//! * **no `%`** — `cmd.exe` would expand it as a variable, and `%` is legal
//!   inside a PowerShell string, so the corruption would be silent. Hence
//!   `$env:LOCALAPPDATA` rather than `%LOCALAPPDATA%`;
//! * **no `"`** — a quote would terminate the argument `cmd.exe` is building.
//!   Single quotes are not special to `cmd.exe` and carry every literal here.
//!
//! Comparison operators follow from the same rule: `-lt`, `-gt` and `-not`,
//! never `<`, `>` or `!`. Every value spliced in is checked by
//! [`crate::scheduled_task::is_safe_task_literal`] first, so a package family
//! or version that could carry a metacharacter produces **no script at all**
//! rather than a mangled one.

use crate::scheduled_task::{OneShotTask, is_safe_task_literal};
use std::path::Path;

/// Prefix for unique task names. Each attempt gets an immutable script and a
/// distinct Scheduler definition under this prefix.
pub const WATCHDOG_TASK_NAME: &str = "fwdslash-update";
const WATCHDOG_DELAY_MINUTES: u16 = 5;

#[cfg(windows)]
static TASK_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct AttemptLock {
    path: std::path::PathBuf,
    owner: String,
}

/// A cross-session, per-user serialization gate for decisions about the lock
/// file. The file survives the updater process so tasks can own an attempt;
/// the mutex deliberately does not. Its only job is making create/reclaim and
/// compare/delete decisions indivisible.
#[cfg(windows)]
struct AttemptMutex(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl AttemptMutex {
    fn acquire(directory: &std::path::Path) -> Option<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

        let normalized = directory
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        let hash = normalized
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let name = format!("Global\\ForwardSlashWindows.UpdateAttemptLock.{hash:016x}");
        let mut wide: Vec<u16> = std::ffi::OsStr::new(&name).encode_wide().collect();
        wide.push(0);
        // SAFETY: the null security attributes request the current user's
        // default ACL; the nul-terminated name is local to this call.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: `handle` was returned by CreateMutexW and is valid here.
        let result = unsafe { WaitForSingleObject(handle, 5_000) };
        if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
            // WAIT_ABANDONED grants this decision mutex, but is not evidence
            // that a fresh file-backed task owner is stale.
            Some(Self(handle))
        } else {
            // SAFETY: this branch owns no mutex acquisition but still owns the
            // kernel handle returned above.
            unsafe { CloseHandle(handle) };
            None
        }
    }
}

#[cfg(windows)]
impl Drop for AttemptMutex {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;

        // SAFETY: AttemptMutex exists only after WaitForSingleObject granted
        // ownership. Both calls consume no Rust references and are best effort
        // during error unwinding.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

impl AttemptLock {
    #[cfg(windows)]
    fn acquire(owner: &str) -> Option<Self> {
        let directory = fsw_core::update::update_directory_path()?;
        Self::acquire_in(
            &directory,
            owner,
            std::time::Duration::from_mins(65),
            &crate::scheduled_task::task_exists,
        )
    }

    /// `owner_alive` answers whether the task named in an existing token is
    /// still registered. A token whose task is gone is an orphan — the
    /// attempt was killed between registering and running, or the script
    /// ran under a cancelled task and never reached its `findstr` line — and
    /// it is reclaimed at once rather than after `stale_after`, which used to
    /// fail every install for an hour with "the watchdog could not be
    /// registered" (issue #140).
    fn acquire_in(
        directory: &std::path::Path,
        owner: &str,
        stale_after: std::time::Duration,
        owner_alive: &dyn Fn(&str) -> bool,
    ) -> Option<Self> {
        use std::io::Write;

        let _decision = AttemptMutex::acquire(directory)?;
        std::fs::create_dir_all(directory).ok()?;
        let path = directory.join("update-attempt.lock");
        for _ in 0..2 {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                file.write_all(owner.as_bytes()).ok()?;
                return Some(Self {
                    path,
                    owner: owner.to_string(),
                });
            }
            // The XML limits every task to an hour. This mutex keeps the
            // stale observation, removal and replacement together so a
            // second contender cannot delete our fresh token.
            let aged_out = std::fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > stale_after);
            let orphaned = std::fs::read_to_string(&path)
                .ok()
                .map(|text| text.trim().to_string())
                .is_some_and(|holder| !holder.is_empty() && !owner_alive(&holder));
            if !aged_out && !orphaned {
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        None
    }

    fn release(self) {
        let Some(directory) = self.path.parent() else {
            return;
        };
        let Some(_decision) = AttemptMutex::acquire(directory) else {
            return;
        };
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(self.owner.as_str()) {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Holds the same short decision mutex through uninstall's task inventory and
/// storage sweep, preventing a fresh updater from acquiring a token between
/// those destructive steps.
#[cfg(windows)]
pub struct UninstallUpdateGuard {
    _mutex: AttemptMutex,
}

#[cfg(windows)]
pub fn lock_update_storage_for_uninstall() -> Option<UninstallUpdateGuard> {
    let directory = fsw_core::update::update_directory_path()?;
    AttemptMutex::acquire(&directory).map(|mutex| UninstallUpdateGuard { _mutex: mutex })
}

/// The relaunch ceiling, in minutes, for the watchdog's poll loop.
const WATCHDOG_MINUTES: u32 = 45;
/// Starts the broker through the app-execution alias, only when none is
/// running. Used both for a landed update in broker mode and, in every mode,
/// after the watchdog gives up on one that did not land.
const BROKER_RELAUNCH: &str = "if (-not (Get-Process -Name fswbroker -ErrorAction \
     SilentlyContinue)) { Start-Process -FilePath (Join-Path $env:LOCALAPPDATA \
     'Microsoft\\WindowsApps\\fwdslash.exe') -ArgumentList 'start' -WindowStyle Hidden }";
/// Seconds between `Get-AppxPackage` polls.
const WATCHDOG_POLL_SECONDS: u32 = 5;

/// What to bring back once the package version has advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaunchMode {
    /// The app's `App` entry point — `fswsettings.exe`. What the settings
    /// window asks for, because the user was looking at it.
    App,
    /// The resident broker, through the app-execution alias, and **only** when
    /// none is already running. The default: it is what the product is when no
    /// window is open.
    Broker,
    /// Nothing. Used when the caller knows the Store will restart the app
    /// itself, and by anyone who wants the install without the comeback.
    None,
}

impl RelaunchMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "app" => Some(Self::App),
            "broker" => Some(Self::Broker),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// The spelling [`Self::parse`] accepts. Nothing in this binary needs it —
    /// the broker and the settings window build `--relaunch <name>` command
    /// lines from it, and the round-trip test holds the two halves together.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Broker => "broker",
            Self::None => "none",
        }
    }
}

/// The PowerShell `-Command` text: wait for the package version to advance,
/// then relaunch. `None` for [`RelaunchMode::None`] (there is nothing to wait
/// for) and for any literal that is not safe to splice.
///
/// Kept separate from the batch wrapper so the "no `%`, no `\"`" rule can be
/// asserted against exactly the text that rule applies to.
#[must_use]
pub fn watchdog_powershell(
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    if mode == RelaunchMode::None {
        return None;
    }
    if !is_safe_task_literal(family)
        || !is_safe_task_literal(identity_name)
        || !is_safe_task_literal(previous_version)
    {
        return None;
    }
    // Identity names are shared by sideload and Store packages. A newer
    // sibling is not proof that the package we asked to update advanced.
    let wait = format!(
        "$deadline = (Get-Date).AddMinutes({WATCHDOG_MINUTES}); \
         $previous = [version]'{previous_version}'; \
         $ready = $false; \
         while ((Get-Date) -lt $deadline) {{ \
         foreach ($package in Get-AppxPackage -Name '{identity_name}') {{ \
         if ($package.PackageFamilyName -eq '{family}' -and [version]$package.Version -gt $previous) {{ $ready = $true }} }}; \
         if ($ready) {{ break }}; \
         Start-Sleep -Seconds {WATCHDOG_POLL_SECONDS} }}; "
    );
    let relaunch = match mode {
        // The alias, never `shell:AppsFolder\...!App`: the package's App entry
        // point is the settings window, and the broker is a startup task that
        // only fires at logon.
        RelaunchMode::Broker => BROKER_RELAUNCH.to_string(),
        RelaunchMode::App => {
            format!("Start-Process -FilePath 'shell:AppsFolder\\{family}!App'")
        }
        RelaunchMode::None => String::new(),
    };
    // Only when nothing else has already reported. A helper that finished
    // with a `paused` verdict wrote this file first, and a timeout notice
    // written over it would turn a legitimate deferral into an error.
    //
    // Then bring the broker back regardless of mode. The old package is still
    // the installed product; the caller closed the broker to make room for an
    // install that did not land, and leaving the user without it until the
    // next logon was the worst outcome of a stalled Store queue (issue #140).
    // The Store may still close it again when its download finally arrives,
    // which is what `AllowForcedAppRestart` permits.
    let timeout = format!(
        "$result = Join-Path $env:LOCALAPPDATA 'ForwardSlashWindows\\update\\{}'; if (-not (Test-Path -LiteralPath $result)) {{ $null = New-Item -ItemType Directory -Force -Path (Split-Path $result); Set-Content -LiteralPath $result -Value 'error:0x800705B4' -NoNewline }}; {BROKER_RELAUNCH}",
        fsw_core::update::UPDATE_RESULT_FILE
    );
    Some(format!(
        "{wait}if ($ready) {{ {relaunch} }} else {{ {timeout} }}"
    ))
}

/// The `winget upgrade` command line route 3 runs from the task with the
/// absolute App Installer alias path and every flag needed to prevent prompts.
#[must_use]
pub fn winget_command(product_id: &str) -> Option<String> {
    let winget = fsw_core::SystemBinary::Winget.path()?;
    winget_command_for(product_id, &winget)
}

pub(super) fn winget_command_for(product_id: &str, winget: &Path) -> Option<String> {
    let winget = batch_quoted_path(winget)?;
    Some(format!(
        "{winget} upgrade --id {product_id} --source msstore --exact --silent --force \
         --accept-package-agreements --accept-source-agreements --disable-interactivity"
    ))
}

/// The full `.cmd` body: an optional lead command, the watchdog, then the
/// self-clean. `None` when a literal was unsafe, which is a refusal to schedule
/// anything at all.
///
/// The lead command is **not** literal-checked — it carries a quoted file
/// system path, which by definition contains characters
/// [`is_safe_task_literal`] rejects. It is built here, from `current_exe()` and
/// the update directory, and never from user input.
#[derive(Clone, Copy)]
struct ScriptLaunch<'a> {
    lead: Option<&'a str>,
    powershell: Option<&'a Path>,
    schtasks: Option<&'a Path>,
}

#[must_use]
fn build_script_for_task(
    task_name: &str,
    launch: ScriptLaunch<'_>,
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
    lock: Option<&AttemptLock>,
) -> Option<String> {
    if !is_safe_task_literal(task_name) {
        return None;
    }
    // A `None` relaunch legitimately has no PowerShell line; an unsafe literal
    // does not, and must not silently degrade into one.
    let watchdog = match watchdog_powershell(family, identity_name, previous_version, mode) {
        Some(text) => Some(text),
        None if mode == RelaunchMode::None => None,
        None => return None,
    };
    let mut script = String::from("@echo off\r\n");
    if let Some(lead) = launch.lead {
        script.push_str(lead);
        script.push_str("\r\n");
    }
    if let Some(watchdog) = watchdog {
        let powershell = launch.powershell?;
        let powershell = batch_quoted_path(powershell)?;
        // Inline `-Command`, so no execution policy can block it, and
        // `powershell.exe` rather than `pwsh` because `Get-AppxPackage` lives
        // in Windows PowerShell.
        script.push_str(&powershell);
        script.push_str(" -NoProfile -NonInteractive -WindowStyle Hidden -Command ");
        script.push_str(&watchdog);
        script.push_str("\r\n");
    }
    let schtasks = launch.schtasks?;
    let schtasks = batch_quoted_path(schtasks)?;
    script.push_str(&schtasks);
    script.push_str(" /delete /tn \"");
    script.push_str(task_name);
    script.push_str("\" /f >nul 2>&1\r\n");
    if let Some(lock) = lock {
        // A cancelled older task must not remove the token a newer attempt
        // acquired. `findstr` only releases the lock when this script owns it.
        script.push_str("findstr /x /c:\"");
        script.push_str(&lock.owner);
        script.push_str("\" \"");
        script.push_str(&lock.path.display().to_string());
        script.push_str("\" >nul && del /q \"");
        script.push_str(&lock.path.display().to_string());
        script.push_str("\" >nul 2>&1\r\n");
    }
    // The XML sidecar is written next to the batch file. The running script
    // owns both artifacts, so cleanup is bounded even after an interrupted
    // scheduler registration.
    script.push_str("del /q \"%~dpn0.xml\" >nul 2>&1\r\n");
    script.push_str("del /q \"%~f0\"\r\n");
    Some(script)
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_script(
    lead: Option<&str>,
    powershell: Option<&Path>,
    schtasks: Option<&Path>,
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    build_script_for_task(
        WATCHDOG_TASK_NAME,
        ScriptLaunch {
            lead,
            powershell,
            schtasks,
        },
        family,
        identity_name,
        previous_version,
        mode,
        None,
    )
}

fn batch_quoted_path(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    if path.is_empty()
        || path.bytes().any(|byte| {
            matches!(
                byte,
                b'"' | b'%' | b'&' | b'|' | b'^' | b'<' | b'>' | b'\r' | b'\n'
            )
        })
    {
        return None;
    }
    Some(format!("\"{path}\""))
}

/// The watchdog on its own: nothing to run first, just wait and relaunch.
/// This is the script phase 1a registers before it calls the Store in-process.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
pub fn watchdog_script(
    powershell: Option<&Path>,
    schtasks: Option<&Path>,
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    build_script(
        None,
        powershell,
        schtasks,
        family,
        identity_name,
        previous_version,
        mode,
    )
}

/// The watchdog with a lead command in front of it: the staged helper, or
/// `winget`. One task, so the thing that installs and the thing that comes back
/// afterwards cannot be separated by a package shutdown landing between them.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
pub fn apply_script(
    command: &str,
    powershell: Option<&Path>,
    schtasks: Option<&Path>,
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    build_script(
        Some(command),
        powershell,
        schtasks,
        family,
        identity_name,
        previous_version,
        mode,
    )
}

/// The package family and identity name the watchdog polls for. Both flavors
/// share the identity name; the family is whichever one is actually installed.
#[cfg(windows)]
fn package_names() -> (String, String) {
    (
        fsw_core::package_family().unwrap_or_else(|| fsw_core::STORE_PACKAGE_FAMILY.to_string()),
        fsw_core::STORE_IDENTITY_NAME.to_string(),
    )
}

/// Registers the relaunch watchdog.
///
/// `run_now` decides whether it starts polling immediately or waits for its
/// delayed backstop trigger. Phase 1a delays it beyond the bounded `WinRT`
/// admission calls; fallback uses a distinct immutable apply task, never an
/// overwrite. Route 2 starts it immediately because deployment can terminate
/// us at any moment.
///
/// [`RelaunchMode::None`] schedules nothing and reports success: there is
/// nothing to bring back, so an absent task is the correct end state.
#[cfg(windows)]
pub struct Watchdog {
    name: Option<String>,
    lock: Option<AttemptLock>,
}

#[cfg(windows)]
impl Watchdog {
    pub fn cancel(self) {
        if let Some(name) = self.name {
            let _ = crate::scheduled_task::delete_task(&name);
        }
        if let Some(lock) = self.lock {
            lock.release();
        }
    }
}

#[cfg(windows)]
fn task_name(kind: &str) -> String {
    let sequence = TASK_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{WATCHDOG_TASK_NAME}-{kind}-{}-{sequence}",
        std::process::id()
    )
}

#[cfg(windows)]
pub fn schedule_watchdog(
    mode: RelaunchMode,
    previous_version: &str,
    run_now: bool,
) -> Option<Watchdog> {
    if mode == RelaunchMode::None {
        return Some(Watchdog {
            name: None,
            lock: None,
        });
    }
    let powershell = if mode == RelaunchMode::None {
        None
    } else {
        Some(fsw_core::SystemBinary::PowerShell.path()?)
    };
    let schtasks = fsw_core::SystemBinary::Schtasks.path()?;
    let (family, identity) = package_names();
    let name = task_name("watchdog");
    let lock = AttemptLock::acquire(&name)?;
    let Some(script) = build_script_for_task(
        &name,
        ScriptLaunch {
            lead: None,
            powershell: powershell.as_deref(),
            schtasks: Some(&schtasks),
        },
        &family,
        &identity,
        previous_version,
        mode,
        Some(&lock),
    ) else {
        lock.release();
        return None;
    };
    let task = OneShotTask::new(&name, script);
    let scheduled = if run_now {
        crate::scheduled_task::register_and_run(&task).is_some()
    } else {
        crate::scheduled_task::register_after(&task, WATCHDOG_DELAY_MINUTES).is_some()
    };
    if !scheduled {
        lock.release();
        return None;
    }
    Some(Watchdog {
        name: Some(name),
        lock: Some(lock),
    })
}

/// Registers and immediately runs the apply script. Always fires now: the
/// command it leads with is the install, and nothing else is going to start it.
#[cfg(windows)]
pub fn schedule_apply(command: &str, mode: RelaunchMode, previous_version: &str) -> bool {
    let powershell = if mode == RelaunchMode::None {
        None
    } else {
        let Some(powershell) = fsw_core::SystemBinary::PowerShell.path() else {
            return false;
        };
        Some(powershell)
    };
    let Some(schtasks) = fsw_core::SystemBinary::Schtasks.path() else {
        return false;
    };
    let (family, identity) = package_names();
    let name = task_name("apply");
    let Some(lock) = AttemptLock::acquire(&name) else {
        return false;
    };
    let Some(script) = build_script_for_task(
        &name,
        ScriptLaunch {
            lead: Some(command),
            powershell: powershell.as_deref(),
            schtasks: Some(&schtasks),
        },
        &family,
        &identity,
        previous_version,
        mode,
        Some(&lock),
    ) else {
        lock.release();
        return false;
    };
    let task = OneShotTask::new(&name, script);
    if crate::scheduled_task::register_and_run(&task).is_some() {
        true
    } else {
        lock.release();
        false
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod script_lock_tests {
    use super::{AttemptLock, RelaunchMode, ScriptLaunch, build_script_for_task};
    use crate::update::tests::{assert_batch_safe, powershell_line};
    use std::path::{Path, PathBuf};

    /// The only golden that exercises the lock-release tail. Nothing else
    /// builds a script with a token to release, so the `findstr` line and the
    /// PowerShell text in front of it are otherwise unchecked together.
    #[test]
    fn a_script_that_releases_its_own_token_is_still_batch_safe() {
        let lock = AttemptLock {
            path: PathBuf::from(
                r"C:\Users\me\AppData\Local\ForwardSlashWindows\update\update-attempt.lock",
            ),
            owner: "fwdslash-update-watchdog-1234-7".to_string(),
        };
        let script = build_script_for_task(
            "fwdslash-update-watchdog-1234-7",
            ScriptLaunch {
                lead: None,
                powershell: Some(Path::new(
                    r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                )),
                schtasks: Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
            },
            "32827MikeFara.fwdslash_t6j5qexy2jpp2",
            "32827MikeFara.fwdslash",
            "0.0.4.0",
            RelaunchMode::Broker,
            Some(&lock),
        )
        .expect("safe literals");
        // The whole batch scaffolding, line by line. The PowerShell body has
        // goldens of its own, so only its wrapper is pinned here.
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines.first().copied(), Some("@echo off"));
        assert!(lines.get(1).is_some_and(|line| line.starts_with(
            "\"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" \
             -NoProfile -NonInteractive -WindowStyle Hidden -Command "
        )));
        assert_eq!(
            lines.get(2).copied(),
            Some(
                "\"C:\\Windows\\System32\\schtasks.exe\" \
                 /delete /tn \"fwdslash-update-watchdog-1234-7\" /f >nul 2>&1"
            )
        );
        // The token is released only by the script that owns it: `findstr`
        // gates the delete on this attempt's own owner line.
        assert_eq!(
            lines.get(3).copied(),
            Some(
                "findstr /x /c:\"fwdslash-update-watchdog-1234-7\" \
                 \"C:\\Users\\me\\AppData\\Local\\ForwardSlashWindows\\update\\update-attempt.lock\" \
                 >nul && del /q \
                 \"C:\\Users\\me\\AppData\\Local\\ForwardSlashWindows\\update\\update-attempt.lock\" \
                 >nul 2>&1"
            )
        );
        assert_eq!(
            lines.get(4).copied(),
            Some("del /q \"%~dpn0.xml\" >nul 2>&1")
        );
        assert_eq!(lines.get(5).copied(), Some("del /q \"%~f0\""));
        assert_eq!(lines.len(), 6);
        assert_batch_safe(powershell_line(&script).expect("a powershell line"));
    }
}

#[cfg(all(test, windows))]
#[allow(clippy::panic)]
mod attempt_lock_tests {
    use super::AttemptLock;
    use std::os::windows::io::AsRawHandle;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    struct TestDirectory(std::path::PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn directory(name: &str) -> TestDirectory {
        let path = std::env::temp_dir().join(format!(
            "fsw-attempt-lock-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_else(|error| panic!("clock after epoch: {error}"))
                .as_nanos()
        ));
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("create test directory: {error}"));
        TestDirectory(path)
    }

    fn make_genuinely_old(path: &std::path::Path) {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Storage::FileSystem::SetFileTime;
        use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap_or_else(|error| panic!("open stale lock file: {error}"));
        let mut now = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // SAFETY: GetSystemTimeAsFileTime initializes the provided FILETIME.
        unsafe { GetSystemTimeAsFileTime(&raw mut now) };
        let ticks = (u64::from(now.dwHighDateTime) << 32) | u64::from(now.dwLowDateTime);
        let old = ticks - 2 * 60 * 60 * 10_000_000;
        let old = FILETIME {
            dwLowDateTime: u32::try_from(old & u64::from(u32::MAX)).unwrap_or_default(),
            dwHighDateTime: (old >> 32) as u32,
        };
        // SAFETY: the file handle is live for this call and the FILETIME points
        // to initialized memory.
        assert_ne!(
            unsafe {
                SetFileTime(
                    file.as_raw_handle(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &raw const old,
                )
            },
            0
        );
    }

    #[test]
    fn two_contenders_cannot_replace_a_fresh_reclaimed_owner() {
        let directory = directory("contention");
        let path = directory.0.join("update-attempt.lock");
        std::fs::write(&path, "dead-owner").unwrap_or_else(|error| panic!("seed lock: {error}"));
        make_genuinely_old(&path);
        let barrier = Arc::new(Barrier::new(2));
        let stale_after = Duration::from_hours(1);
        let contenders = ["owner-a", "owner-b"].map(|owner| {
            let barrier = Arc::clone(&barrier);
            let directory = directory.0.clone();
            std::thread::spawn(move || {
                barrier.wait();
                AttemptLock::acquire_in(&directory, owner, stale_after, &|_| true)
            })
        });
        let [first, second] = contenders;
        let first = first.join().unwrap_or_else(|_| panic!("first contender"));
        let second = second.join().unwrap_or_else(|_| panic!("second contender"));
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1
        );
        let winner = first.or(second).unwrap_or_else(|| panic!("one winner"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("winner token: {error}")),
            winner.owner
        );
        winner.release();
    }

    #[test]
    fn a_fresh_token_whose_task_is_gone_is_reclaimed_at_once() {
        let directory = directory("orphan");
        let path = directory.0.join("update-attempt.lock");
        std::fs::write(&path, "fwdslash-update-watchdog-18784-1")
            .unwrap_or_else(|error| panic!("seed lock: {error}"));
        let stale_after = Duration::from_hours(1);
        // The task is still registered: the token is honoured.
        assert!(AttemptLock::acquire_in(&directory.0, "owner-b", stale_after, &|_| true).is_none());
        // The task is gone: the token is an orphan and the new attempt wins.
        let lock = AttemptLock::acquire_in(&directory.0, "owner-b", stale_after, &|name| {
            assert_eq!(name, "fwdslash-update-watchdog-18784-1");
            false
        })
        .unwrap_or_else(|| panic!("orphan reclaimed"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("token: {error}")),
            "owner-b"
        );
        lock.release();
        assert!(!path.exists());
    }

    #[test]
    fn release_never_deletes_a_foreign_owner_token() {
        let directory = directory("foreign-owner");
        let owner =
            AttemptLock::acquire_in(&directory.0, "owner-a", Duration::from_secs(60), &|_| true)
                .unwrap_or_else(|| panic!("first owner"));
        std::fs::write(&owner.path, "owner-b")
            .unwrap_or_else(|error| panic!("replace token for test: {error}"));
        owner.release();
        assert_eq!(
            std::fs::read_to_string(directory.0.join("update-attempt.lock"))
                .unwrap_or_else(|error| panic!("foreign token: {error}")),
            "owner-b"
        );
    }
}
