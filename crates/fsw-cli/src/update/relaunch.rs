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

impl AttemptLock {
    fn acquire(owner: &str) -> Option<Self> {
        use std::io::Write;
        use std::time::Duration;
        let directory = fsw_core::update::update_directory_path()?;
        std::fs::create_dir_all(&directory).ok()?;
        let path = directory.join("update-attempt.lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(owner.as_bytes()).ok()?;
                    return Some(Self {
                        path,
                        owner: owner.to_string(),
                    });
                }
                Err(_) => {
                    // The XML limits every task to an hour. A lock older than
                    // that plus a small clock/filesystem margin is abandoned
                    // state after a crash, not a live updater.
                    let stale = std::fs::metadata(&path)
                        .ok()
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(65 * 60));
                    if !stale {
                        return None;
                    }
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        None
    }

    fn release(self) {
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(self.owner.as_str()) {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// The relaunch ceiling, in minutes, for the watchdog's poll loop.
const WATCHDOG_MINUTES: u32 = 45;
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
        RelaunchMode::Broker => "if (-not (Get-Process -Name fswbroker -ErrorAction \
             SilentlyContinue)) { Start-Process -FilePath (Join-Path $env:LOCALAPPDATA \
             'Microsoft\\WindowsApps\\fwdslash.exe') -ArgumentList 'start' -WindowStyle \
             Hidden }"
            .to_string(),
        RelaunchMode::App => {
            format!("Start-Process -FilePath 'shell:AppsFolder\\{family}!App'")
        }
        RelaunchMode::None => String::new(),
    };
    let timeout = format!(
        "$result = Join-Path $env:LOCALAPPDATA 'ForwardSlashWindows\\update\\{}'; $null = New-Item -ItemType Directory -Force -Path (Split-Path $result); Set-Content -LiteralPath $result -Value 'error:0x800705B4' -NoNewline",
        fsw_core::update::UPDATE_RESULT_FILE
    );
    Some(format!(
        "{wait}if ($ready) {{ {relaunch} }} else {{ {timeout} }}"
    ))
}

/// The `winget upgrade` command line route 3 runs from the task. Plain enough
/// that `cmd.exe` needs nothing quoted, and every flag is there to stop winget
/// asking a question nobody is present to answer.
#[must_use]
pub fn winget_command(product_id: &str) -> String {
    format!(
        "winget.exe upgrade --id {product_id} --source msstore --exact --silent --force \
         --accept-package-agreements --accept-source-agreements --disable-interactivity"
    )
}

/// The full `.cmd` body: an optional lead command, the watchdog, then the
/// self-clean. `None` when a literal was unsafe, which is a refusal to schedule
/// anything at all.
///
/// The lead command is **not** literal-checked — it carries a quoted file
/// system path, which by definition contains characters
/// [`is_safe_task_literal`] rejects. It is built here, from `current_exe()` and
/// the update directory, and never from user input.
#[must_use]
fn build_script_for_task(
    task_name: &str,
    lead: Option<&str>,
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
    if let Some(lead) = lead {
        script.push_str(lead);
        script.push_str("\r\n");
    }
    if let Some(watchdog) = watchdog {
        // Inline `-Command`, so no execution policy can block it, and
        // `powershell.exe` rather than `pwsh` because `Get-AppxPackage` lives
        // in Windows PowerShell.
        script.push_str("powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -Command ");
        script.push_str(&watchdog);
        script.push_str("\r\n");
    }
    script.push_str("schtasks /delete /tn \"");
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

fn build_script(
    lead: Option<&str>,
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    build_script_for_task(
        WATCHDOG_TASK_NAME,
        lead,
        family,
        identity_name,
        previous_version,
        mode,
        None,
    )
}

/// The watchdog on its own: nothing to run first, just wait and relaunch.
/// This is the script phase 1a registers before it calls the Store in-process.
#[must_use]
pub fn watchdog_script(
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    build_script(None, family, identity_name, previous_version, mode)
}

/// The watchdog with a lead command in front of it: the staged helper, or
/// `winget`. One task, so the thing that installs and the thing that comes back
/// afterwards cannot be separated by a package shutdown landing between them.
#[must_use]
pub fn apply_script(
    command: &str,
    family: &str,
    identity_name: &str,
    previous_version: &str,
    mode: RelaunchMode,
) -> Option<String> {
    build_script(Some(command), family, identity_name, previous_version, mode)
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
/// delayed backstop trigger. Phase 1a delays it beyond the bounded WinRT
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
    let (family, identity) = package_names();
    let name = task_name("watchdog");
    let Some(lock) = AttemptLock::acquire(&name) else {
        return None;
    };
    let Some(script) = build_script_for_task(
        &name,
        None,
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
    let (family, identity) = package_names();
    let name = task_name("apply");
    let Some(lock) = AttemptLock::acquire(&name) else {
        return false;
    };
    let Some(script) = build_script_for_task(
        &name,
        Some(command),
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
