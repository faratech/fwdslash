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
pub mod profile;
#[cfg(windows)]
pub mod reg;
pub mod state;
#[cfg(test)]
mod tests;

pub use state::Edition;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

/// CREATE_NO_WINDOW for the real-process children.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The adapter payload directory version. Derived from the crate version, so
/// a product bump moves the payload directory and marks every deployed
/// adapter as outdated — which is what drives the upgrade in
/// [`set_integration`].
pub const PAYLOAD_VERSION: &str = env!("CARGO_PKG_VERSION");

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
        if enabled
            && edition == Some(state::Edition::PowerShell)
            && !fsw_core::executable_available("pwsh.exe")
        {
            println!("PowerShell 7 is not installed.");
            return 1;
        }

        let marker_key = match edition {
            None => fsw_core::CMD_ADAPTER_KEY.to_string(),
            Some(edition) => format!(
                "{}{}",
                fsw_core::POWERSHELL_ADAPTER_ROOT,
                edition.registry_leaf()
            ),
        };
        let label = match edition {
            None => "cmd",
            Some(edition) => edition.display_name(),
        };

        // Idempotence: an enable/disable that matches the stored marker is a
        // silent no-op, exactly like the script flow — except for an
        // `installed` marker naming an older payload than this build ships.
        // That is the upgrade path: the same transactional uninstall, then
        // the same transactional install, so a failure still rolls back.
        let mut upgrading = false;
        if fsw_core::adapter_installed(&marker_key) == enabled {
            if !enabled {
                return 0;
            }
            let installed_version = fsw_core::adapter_version(&marker_key);
            if installed_version.as_deref() == Some(PAYLOAD_VERSION) {
                // Already current, so nothing else runs today — but a machine
                // upgraded before the prune existed still has stranded
                // `PowerShell\<version>` directories and this no-op is the
                // only path the broker ever reaches for it.
                if edition.is_some() {
                    powershell::prune_orphaned_module_dirs();
                }
                return 0;
            }
            println!(
                "Upgrading the {label} adapter from {} to {PAYLOAD_VERSION}.",
                installed_version.as_deref().unwrap_or("an earlier version")
            );
            upgrading = true;
        }

        // An upgrade removes the prior adapter before installing the new one.
        // Refuse a Windows PowerShell upgrade before that destructive half if
        // a fresh ordinary shell cannot run the profile.
        if upgrading && edition == Some(state::Edition::WindowsPowerShell) {
            if let Some(error) = powershell::execution_policy_refusal(edition.unwrap()) {
                eprintln!("{error}");
                return 1;
            }
        }

        // Before staging anything: if no marker references the payload tree at
        // all, it is debris a deferred delete failed to remove — drop it rather
        // than install on top of it (#37).
        if enabled {
            prune_orphaned_payload_tree();
        }

        let controller = std::env::current_exe().unwrap_or_default();
        // An upgrade is the old payload's uninstall followed by this one's
        // install; `upgrading` is only ever set on the enable path.
        let removal = if upgrading {
            match edition {
                None => cmd::uninstall(),
                Some(edition) => powershell::uninstall(edition),
            }
        } else {
            Ok(())
        };
        let result = removal.and_then(|()| match (edition, enabled) {
            (None, true) => cmd::install(&controller),
            (None, false) => cmd::uninstall(),
            (Some(state::Edition::WindowsPowerShell), true) if upgrading => {
                powershell::install_after_policy_preflight(
                    state::Edition::WindowsPowerShell,
                    &controller,
                )
            }
            (Some(edition), true) => powershell::install(edition, &controller),
            (Some(edition), false) => powershell::uninstall(edition),
        });

        match result {
            Ok(()) => {
                // Every successful PowerShell enable sweeps the stranded
                // version directories too; uninstall already prunes its own.
                if enabled && edition.is_some() {
                    powershell::prune_orphaned_module_dirs();
                }
                0
            }
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
    // With both editions gone, nothing references the shared module tree any
    // more: drop the leftover version directories and the empty state folder.
    // Unconditional, so an install that never had a marker to remove still
    // clears directories a previous release stranded.
    #[cfg(windows)]
    powershell::prune_orphaned_module_dirs();
    worst
}

/// `DETACHED_PROCESS` — the self-clean's delayed directory delete must outlive
/// this process and not share its console.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// One adapter's health for `fwdslash doctor` / `integrations`: a display
/// label and a one-line status (#37).
pub fn health_report() -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        let mut lines = vec![("Command Prompt".to_string(), cmd_health_status())];
        let policies = execution_policy_statuses();
        for (label, edition) in [
            ("Windows PowerShell", state::Edition::WindowsPowerShell),
            ("PowerShell 7", state::Edition::PowerShell),
        ] {
            lines.push((label.to_string(), ps_health_status(edition)));
            // The policy is a property of the edition, not of the install, so
            // it is reported whether or not the adapter is on: it is the
            // difference between "installed and doing nothing" and "installed
            // and working" (#45).
            if let Some(status) = policies.iter().find(|status| status.edition == edition) {
                lines.push((
                    format!("{label} execution policy"),
                    status.status_line().to_string(),
                ));
            }
        }
        lines
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// One edition's effective execution policy, for the health lines and for
/// `fwdslash integrations --json` (#45).
#[cfg(windows)]
pub struct PolicyStatus {
    pub edition: state::Edition,
    /// What `Get-ExecutionPolicy` printed in that edition's own shell.
    pub reported: String,
    pub blocked: bool,
    /// The fix, empty unless `blocked`.
    pub remedy: String,
    /// The rendered `doctor` / `integrations` line.
    status: String,
}

#[cfg(windows)]
impl PolicyStatus {
    pub fn status_line(&self) -> &str {
        &self.status
    }
}

/// Probes each PowerShell edition's effective execution policy. One child
/// shell per edition, skipped entirely when the edition is not installed on
/// the machine (no `pwsh.exe`), and never fatal: an edition that cannot be
/// asked is simply absent from the result.
#[cfg(windows)]
pub fn execution_policy_statuses() -> Vec<PolicyStatus> {
    let mut statuses = Vec::new();
    for edition in [
        state::Edition::WindowsPowerShell,
        state::Edition::PowerShell,
    ] {
        let Some((reported, verdict)) = powershell::execution_policy_verdict(edition) else {
            continue;
        };
        let status = state::policy_health_status(&reported, &verdict);
        statuses.push(PolicyStatus {
            edition,
            reported: reported.trim().to_string(),
            blocked: verdict.is_blocked(),
            remedy: verdict
                .blocked()
                .map(|block| block.remedy.clone())
                .unwrap_or_default(),
            status,
        });
    }
    statuses
}

#[cfg(windows)]
fn ps_health_status(edition: state::Edition) -> String {
    match powershell::profile_health(edition) {
        profile::ProfileHealth::Clean => {
            if fsw_core::adapter_installed(&format!(
                "{}{}",
                fsw_core::POWERSHELL_ADAPTER_ROOT,
                edition.registry_leaf()
            )) {
                // Installed but no block: the write that should have put one
                // there is the thing to explain, and a Controlled Folder Access
                // block is the usual cause (#37).
                if powershell::profile_write_blocked(edition) {
                    "installed, but the profile is not writable — Controlled Folder Access may be blocking it".to_string()
                } else {
                    "installed, no profile block".to_string()
                }
            } else {
                "not installed".to_string()
            }
        }
        profile::ProfileHealth::Healthy => "healthy".to_string(),
        profile::ProfileHealth::Orphaned(version) => {
            format!("orphaned profile block for {version}")
        }
        profile::ProfileHealth::Stale(version) => format!("stale profile block for {version}"),
        profile::ProfileHealth::Duplicated => "duplicate profile blocks".to_string(),
    }
}

#[cfg(windows)]
fn cmd_health_status() -> String {
    match cmd::health() {
        cmd::CmdHealth::Clean => "not installed".to_string(),
        cmd::CmdHealth::Healthy => "healthy".to_string(),
        cmd::CmdHealth::Orphaned => "orphaned AutoRun hook (missing fsw-autorun.cmd)".to_string(),
    }
}

/// Repairs a single adapter's shell-integration hygiene (#37) and prints a
/// `label: status — outcome` line. Exit 0 always: a repair that cannot run is
/// reported, not fatal.
pub fn repair_integration(id: &str) -> i32 {
    #[cfg(windows)]
    {
        let controller = std::env::current_exe().unwrap_or_default();
        match id {
            "cmd" => {
                let before = cmd::repair();
                report_cmd_repair(before);
            }
            "windows-powershell" => report_ps_repair(
                "Windows PowerShell",
                state::Edition::WindowsPowerShell,
                &controller,
            ),
            "powershell" => {
                report_ps_repair("PowerShell 7", state::Edition::PowerShell, &controller)
            }
            _ => return 2,
        }
        0
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        1
    }
}

#[cfg(windows)]
fn report_cmd_repair(result: Result<cmd::CmdHealth, AdapterError>) {
    match result {
        // Health after the repair attempt: Orphaned here means the restore was
        // refused (a third party changed AutoRun) or there is no marker to
        // restore from — reported, not silently claimed as fixed.
        Ok(cmd::CmdHealth::Orphaned) => {
            println!(
                "Command Prompt: orphaned AutoRun hook — could not repair automatically (reconcile AutoRun and retry)"
            );
        }
        Ok(_) => println!("Command Prompt: healthy"),
        Err(error) => println!("Command Prompt: repair failed ({error})"),
    }
}

#[cfg(windows)]
fn report_ps_repair(label: &str, edition: state::Edition, controller: &Path) {
    match powershell::repair(edition, controller) {
        Ok(profile::ProfileHealth::Healthy | profile::ProfileHealth::Clean) => {
            println!("{label}: healthy");
        }
        Ok(profile::ProfileHealth::Orphaned(version)) => {
            println!("{label}: orphaned profile block for {version} — repaired");
        }
        Ok(profile::ProfileHealth::Stale(version)) => {
            println!("{label}: stale profile block for {version} — repaired");
        }
        Ok(profile::ProfileHealth::Duplicated) => {
            println!("{label}: duplicate profile blocks — repaired");
        }
        Err(error) => println!("{label}: repair failed ({error})"),
    }
}

/// Repairs every shell adapter's hygiene (#37). The broker's startup sweep and
/// the settings window's launch sweep both invoke this (via
/// `fwdslash repair-adapters`) so an orphaned or duplicated block self-heals on
/// the next run. Best effort; exit 0.
pub fn repair_all() -> i32 {
    #[cfg(windows)]
    {
        let controller = std::env::current_exe().unwrap_or_default();
        let _ = cmd::repair();
        for edition in [
            state::Edition::WindowsPowerShell,
            state::Edition::PowerShell,
        ] {
            let _ = powershell::repair(edition, &controller);
        }
        powershell::prune_orphaned_module_dirs();
        // A payload tree no marker names is debris from a deferred delete that
        // never completed (#37).
        prune_orphaned_payload_tree();
        0
    }
    #[cfg(not(windows))]
    {
        1
    }
}

/// The orphan self-clean (`fwdslash uninstall --orphaned`), invoked by a
/// leftover shell hook on the next shell start after the product was removed
/// without running code (an MSIX uninstall). Confirms the product is really
/// gone, then runs the transactional sweep and deletes every trace — including
/// the directory it is running from (#37 addendum).
pub fn cleanup_orphaned() -> i32 {
    #[cfg(windows)]
    {
        // Slow confirm: the cheap probe already failed for the hook to call us,
        // so re-check every signal. A transient alias blip during an in-flight
        // update must never destroy a live install.
        let mut probes = Vec::new();
        if let Some(probe) = cmd::recorded_probe() {
            probes.push(probe);
        }
        for edition in [
            state::Edition::WindowsPowerShell,
            state::Edition::PowerShell,
        ] {
            if let Some(probe) = powershell::recorded_probe(edition) {
                probes.push(probe);
            }
        }
        if product_present(&probes) {
            return 0;
        }

        // Product confirmed gone. Restore the profiles / AutoRun byte-exact
        // through the transactional sweep (cmd still refuses if a third party
        // changed AutoRun), then belt-and-braces strip anything a refused or
        // missing-recovery uninstall could have left behind.
        let _ = sweep_uninstall();
        strip_all_ps_profiles();
        // The cmd analogue: a refused uninstall leaves our `call` in AutoRun,
        // and deleting the payload under it would break every console start.
        let _ = cmd::strip_autorun_hook();

        // Wipe the settings hive + adapter markers, and the unpackaged-only Run
        // value (a packaged orphan has neither).
        let _ = reg::delete_tree("Software\\ForwardSlashWindows");
        let _ = reg::delete_value(fsw_core::RUN_KEY, fsw_core::RUN_VALUE);
        // The protocol registration goes only if it is still ours — the normal
        // uninstall refuses to remove another application's handler, and so
        // does this.
        if protocol_is_ours() {
            let _ = reg::delete_tree(fsw_core::PROTOCOL_KEY);
        }

        // Delete the payload tree, scheduling the running directory's own
        // removal for after this process exits — but never while AutoRun still
        // calls into it.
        if cmd::autorun_still_hooked() {
            return 0;
        }
        schedule_payload_delete();
        0
    }
    #[cfg(not(windows))]
    {
        1
    }
}

/// Whether `HKCU\Software\Classes\fwdslash` still names *our* handler.
///
/// `set_settings_protocol` writes `"<dir>\fswsettings.exe" "%1"` and refuses to
/// remove the key when another application has since taken the scheme; the
/// orphan self-clean keeps that promise rather than deleting a handler that is
/// no longer ours.
#[cfg(windows)]
fn protocol_is_ours() -> bool {
    use windows_registry::CURRENT_USER;

    let command_key = format!(r"{}\shell\open\command", fsw_core::PROTOCOL_KEY);
    let Ok(key) = CURRENT_USER.open(&command_key) else {
        // No handler registered at all: nothing of ours to remove.
        return false;
    };
    let Ok(command) = key.get_string("") else {
        return false;
    };
    let command = command.to_ascii_lowercase();
    command.contains("fswsettings.exe") || command.contains("fwdslash.exe")
}

/// Belt-and-braces removal of any fwdslash block left in either edition's
/// default profile, used by the orphan self-clean when a marker-driven restore
/// could not run.
#[cfg(windows)]
fn strip_all_ps_profiles() {
    let Ok(documents) = documents_dir() else {
        return;
    };
    for folder in ["WindowsPowerShell", "PowerShell"] {
        let path = documents.join(folder).join("profile.ps1");
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let cleaned = profile::strip_fwdslash_blocks(&bytes);
        if cleaned == bytes {
            continue;
        }
        if cleaned.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = write_atomic(&path, &cleaned);
        }
    }
}

/// The fixed name of the one-shot cleanup task. Fixed rather than per-run so
/// repeated self-cleans overwrite one task (`/f`) instead of accumulating them:
/// at most one can ever be left behind.
pub const CLEANUP_TASK_NAME: &str = "fwdslash-orphan-cleanup";

// The one-shot task machinery is shared with the self-updater and now lives in
// `crate::scheduled_task`. The cleanup path reaches it through
// `register_and_run`, so these two names survive only for the tests that were
// written against them here — hence `cfg(test)`, which is also what keeps them
// from reading as an unused re-export in a normal build.
#[cfg(test)]
pub use crate::scheduled_task::task_args as cleanup_task_args;
#[cfg(test)]
pub use crate::scheduled_task::task_start_time;

/// The batch file the cleanup task runs: wait for this process to exit, remove
/// the payload tree, then delete the task and itself. Every path is quoted, so
/// a space in the profile-independent `%LOCALAPPDATA%` path is safe.
#[must_use]
pub fn cleanup_script_body(payload_dir: &str, task_name: &str) -> String {
    format!(
        "@echo off\r\n\
         ping -n 3 127.0.0.1 >nul\r\n\
         reg query HKCU\\Software\\ForwardSlashWindows\\CmdAdapter >nul 2>&1 && goto done\r\n\
         reg query HKCU\\Software\\ForwardSlashWindows\\PowerShellAdapter\\WindowsPowerShell >nul 2>&1 && goto done\r\n\
         reg query HKCU\\Software\\ForwardSlashWindows\\PowerShellAdapter\\PowerShell >nul 2>&1 && goto done\r\n\
         rd /s /q \"{payload_dir}\\cmd\"\r\n\
         rd /s /q \"{payload_dir}\\PowerShell\"\r\n\
         :done\r\n\
         schtasks /delete /tn \"{task_name}\" /f >nul 2>&1\r\n\
         del /q \"%~f0\"\r\n"
    )
}

/// Whether `payload` is exactly the tree the self-clean is allowed to remove.
/// The deferred delete runs `rd /s /q`, so this is the last line of defence
/// against ever pointing it anywhere else.
#[must_use]
pub fn is_payload_tree(payload: &Path, local_app_data: &Path) -> bool {
    payload == local_app_data.join("ForwardSlashWindows")
}

/// Removes `%LOCALAPPDATA%\ForwardSlashWindows`, deferring the running
/// directory's own deletion to a **one-shot per-user scheduled task**.
///
/// A detached `cmd.exe` child is not enough: when the triggering shell was
/// itself launched inside a job object (WSL interop is the case that exposed
/// this), the whole process tree — including a `DETACHED_PROCESS` child — is
/// killed the moment the launching command returns, so the delete never ran.
/// Measured on the dev host: the detached helper spawns but never deletes, and
/// `CREATE_BREAKAWAY_FROM_JOB` does not even spawn (the job forbids breakaway,
/// `CreateProcess` fails). The Task Scheduler service starts the script in its
/// own session, outside any job we are in, so it always survives.
///
/// The task is created (backstop trigger one minute out) *and* run immediately;
/// the script waits ~2 s for this process to exit, deletes the tree, then
/// removes the task and itself. Idempotent: an already-gone tree is not an
/// error and `/f` overwrites any previous task of the same name.
#[cfg(windows)]
fn schedule_payload_delete() {
    let Ok(local_app_data) = local_app_data() else {
        return;
    };
    let payload = local_app_data.join("ForwardSlashWindows");
    if !is_payload_tree(&payload, &local_app_data) {
        return;
    }
    let task = crate::scheduled_task::OneShotTask::new(
        CLEANUP_TASK_NAME,
        cleanup_script_body(&payload.display().to_string(), CLEANUP_TASK_NAME),
    );
    if crate::scheduled_task::register_and_run(&task).is_some() {
        return;
    }
    // No usable Task Scheduler: fall back to a detached child. Try to break
    // away from any job first; if that is refused, spawn plainly — which still
    // works for an ordinary interactive shell.
    spawn_detached_delete(&payload);
}

/// The pre-scheduled-task fallback: a detached `cmd.exe` that waits, then
/// deletes. Kept because it is enough for an ordinary interactive shell.
#[cfg(windows)]
fn spawn_detached_delete(payload: &Path) {
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let system32 = Path::new(&system_root).join("System32");
    let command = format!(
        "ping -n 3 127.0.0.1 >nul & rd /s /q \"{}\\cmd\" & rd /s /q \"{}\\PowerShell\"",
        payload.display(),
        payload.display()
    );
    for flags in [
        CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB,
        CREATE_NO_WINDOW | DETACHED_PROCESS,
    ] {
        let spawned = Command::new("cmd.exe")
            .args(["/c", &command])
            .current_dir(&system32)
            .creation_flags(flags)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return;
        }
    }
}

/// Whether any adapter marker key exists at all — including a half-finished
/// `prepared`/`removing` transaction, which must not be swept out from under.
#[cfg(windows)]
fn any_adapter_marker_present() -> bool {
    use windows_registry::CURRENT_USER;

    CURRENT_USER.open(fsw_core::CMD_ADAPTER_KEY).is_ok()
        || CURRENT_USER
            .open(&format!(
                "{}WindowsPowerShell",
                fsw_core::POWERSHELL_ADAPTER_ROOT
            ))
            .is_ok()
        || CURRENT_USER
            .open(&format!("{}PowerShell", fsw_core::POWERSHELL_ADAPTER_ROOT))
            .is_ok()
}

/// The parent is shared with the updater. Delete only adapter-owned children.
#[cfg(windows)]
fn prune_adapter_directories(payload: &Path) {
    let Ok(entries) = std::fs::read_dir(payload) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.eq_ignore_ascii_case("cmd")
            || name.eq_ignore_ascii_case("PowerShell")
            || name.starts_with(".cmd-staging-")
            || name.starts_with(".cmd-rollback-")
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Stop old parent-wide deletion tasks before a new adapter is staged.
#[cfg(windows)]
pub fn prune_orphaned_payload_tree() {
    let _ = Command::new("schtasks.exe")
        .args(["/end", "/tn", CLEANUP_TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = crate::scheduled_task::delete_task(CLEANUP_TASK_NAME);
    if any_adapter_marker_present() {
        return;
    }
    let Ok(local_app_data) = local_app_data() else {
        return;
    };
    let payload = local_app_data.join("ForwardSlashWindows");
    if is_payload_tree(&payload, &local_app_data) && payload.is_dir() {
        prune_adapter_directories(&payload);
    }
    // The task that was going to do this is stale for the same reason, and it
    // is not harmless: its `rd /s /q` names the tree we are about to stage a
    // fresh payload into, and its backstop trigger can still be an hour away.
    // The script deletes its own task, but only once it has run.
    let _ = crate::scheduled_task::delete_task(CLEANUP_TASK_NAME);
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

/// The per-user app-execution alias both packaged flavors register:
/// `%LOCALAPPDATA%\Microsoft\WindowsApps\fwdslash.exe`. It is user-readable
/// (unlike the `WindowsApps` install root) and vanishes when the package is
/// removed, which makes it the cheap "is the product still installed" probe
/// the shell hooks test on every start (#37 addendum).
#[cfg(windows)]
pub fn app_execution_alias() -> Option<PathBuf> {
    local_app_data().ok().map(|dir| {
        dir.join("Microsoft")
            .join("WindowsApps")
            .join("fwdslash.exe")
    })
}

/// The product-presence probe recorded in an adapter's marker at install time.
/// Its absence arms the shell hook's self-clean.
///
/// Packaged: the package's own app-data folder,
/// `%LOCALAPPDATA%\Packages\<package family>`, resolved from the *actual*
/// family at install time so either flavor works. It exists while the package
/// is registered, survives updates (closing the in-flight-update race), and is
/// removed by an MSIX uninstall. Deliberately **not** the app-execution alias:
/// a user can turn that off under Settings > Apps > App execution aliases,
/// which would silently disable the integration and spawn a self-clean on
/// every shell start.
///
/// Unpackaged: the directory the real controller runs from.
#[cfg(windows)]
pub fn product_probe_path(controller: &Path) -> PathBuf {
    if fsw_core::has_package_identity() {
        if let (Some(family), Ok(local_app_data)) = (fsw_core::package_family(), local_app_data()) {
            return local_app_data.join("Packages").join(family);
        }
        if let Some(alias) = app_execution_alias() {
            return alias;
        }
    }
    // Unpackaged (or, defensively, a packaged build we could not resolve a
    // family for): the real controller's own directory proves it is present.
    controller
        .parent()
        .map_or_else(|| controller.to_path_buf(), Path::to_path_buf)
}

/// The slow product-presence confirm, run only after the cheap probe has
/// already failed (`--orphaned`): the alias, any recorded install directory,
/// or — last — whether either package flavor is still registered. Conservative
/// on error: an appx query that cannot even run counts as "present" so a
/// transient failure never destroys a live install.
#[cfg(windows)]
pub fn product_present(recorded_probes: &[String]) -> bool {
    // Cheap: the recorded probes (the package app-data folder, or the
    // unpackaged install directory) and the app-execution alias — plain file
    // system checks. Slow, and only paid when every cheap check has failed:
    // an appx registration query.
    let cheap_probe_present = recorded_probes
        .iter()
        .any(|probe| !probe.is_empty() && Path::new(probe).exists())
        || app_execution_alias().is_some_and(|path| path.is_file());
    let slow_confirm_present =
        !cheap_probe_present && appx_registered(fsw_core::STORE_IDENTITY_NAME);
    !state::product_confirmed_gone(cheap_probe_present, slow_confirm_present)
}

/// Whether a package with `identity_name` (shared by both flavors) is still
/// registered for this user. Spawns in-box Windows PowerShell — acceptable
/// because this only runs on the rare cleanup path. Returns `true` (present) if
/// the query cannot be run at all.
#[cfg(windows)]
fn appx_registered(identity_name: &str) -> bool {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let shell = Path::new(&system_root).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    let script = format!(
        "if (Get-AppxPackage -Name '{}') {{ exit 0 }} else {{ exit 1 }}",
        identity_name.replace('\'', "''")
    );
    Command::new(shell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(true, |status| status.success())
}

/// The real Documents folder, following OneDrive redirection
/// (`SHGetKnownFolderPath`, not `%USERPROFILE%\Documents`).
#[cfg(windows)]
pub fn documents_dir() -> Result<PathBuf, AdapterError> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

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
///
/// The size is compared afterwards: `copy` is binary for a file-to-file copy,
/// but the deployed `ForwardSlashWindows.psm1` carries an Authenticode
/// `# SIG #` block in the signed builds, and a copy that ended early — an
/// ASCII-mode truncation at a `Ctrl+Z` byte, a full disk — would deploy a
/// module whose bytes no longer match its signature. Refusing beats deploying
/// a partial payload (#45).
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
        let copied = std::fs::metadata(destination_dir.join(file_name)).map(|data| data.len());
        let original = std::fs::metadata(source).map(|data| data.len());
        if let (Ok(copied), Ok(original)) = (copied, original) {
            if copied != original {
                return Err(AdapterError::new(&format!(
                    "{} was deployed incompletely ({copied} of {original} bytes)",
                    file_name.to_string_lossy()
                )));
            }
        }
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

/// Whether a failed write to a path whose parent directory exists should be
/// reported as a Controlled Folder Access block.
///
/// Access-denied is the obvious signature. The subtle one (#37): CFA does
/// **not** always surface as `ERROR_ACCESS_DENIED` — on the dev host a blocked
/// `CreateFile` for the temp file `write_atomic` creates inside a protected
/// `Documents` subfolder came back as `ERROR_FILE_NOT_FOUND`, which used to be
/// reported as the useless "The system cannot find the file specified". A
/// "not found" only means a block when the containing folder is actually
/// there; otherwise it is a genuinely missing path.
#[must_use]
pub fn looks_like_blocked_write(error_text: &str, parent_exists: bool) -> bool {
    let denied = error_text.contains("os error 5") || error_text.contains("Access is denied");
    let not_found = error_text.contains("os error 2")
        || error_text.contains("cannot find the file")
        || error_text.contains("cannot find the path");
    denied || (not_found && parent_exists)
}

/// The user-facing explanation for a blocked profile write.
pub const BLOCKED_WRITE_GUIDANCE: &str = "was blocked by Windows Controlled Folder Access, or the folder is otherwise not writable. Allow Forward Slash Windows under Windows Security > Virus & threat protection > Ransomware protection > Allow an app through Controlled folder access, then try again.";

/// Wraps a file error with Controlled Folder Access guidance when the failure
/// looks like a Defender block against `target`.
#[cfg(windows)]
pub fn explain_file_error(error: &AdapterError, what: &str, target: &Path) -> AdapterError {
    let text = error.to_string();
    let parent_exists = target.parent().is_some_and(Path::is_dir);
    if looks_like_blocked_write(&text, parent_exists) {
        return AdapterError::new(&format!("{what} {BLOCKED_WRITE_GUIDANCE}"));
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
