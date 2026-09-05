//! The update helper: a byte-identical copy of `fwdslash.exe` that runs
//! **without package identity**.
//!
//! Two things need that. `AppInstallManager` may refuse an install to a
//! packaged caller (the private capability the API is documented as gated by),
//! and `Add-AppxPackage -ForceApplicationShutdown` cannot run from inside the
//! package it is about to shut down. A staged copy in
//! `%LOCALAPPDATA%\ForwardSlashWindows\update\` is not part of the package, so
//! neither restriction reaches it.
//!
//! The copy has one hard rule: **it never writes HKCU.** MSIX virtualizes the
//! packaged app's view of the hive, so a write from the identity-less helper
//! lands in the real hive and the packaged app never sees it — a bug that would
//! be invisible on an unpackaged dev build, where the two views are the same.
//! It reports through [`fsw_core::update::UPDATE_RESULT_FILE`] instead, and the
//! next packaged `update check`/`update status` folds that file into the
//! registry.

use super::HelperResult;
use std::path::{Path, PathBuf};

/// The staged copy's file name. Distinct from `fwdslash.exe` on purpose: it is
/// what a user finds in Task Manager during an update, and what an antivirus
/// heuristic will name if this ever trips one.
pub const HELPER_NAME: &str = "fwdslash-helper.exe";

/// Whether this process is allowed to run a helper-only verb — that is, whether
/// it has *no* package identity. The whole point of the copy is the context it
/// runs in, so a packaged caller is exit 20 rather than a silent degrade.
#[must_use]
pub fn helper_context_ok() -> bool {
    !fsw_core::has_package_identity()
}

/// Where the staged copy lives, if `%LOCALAPPDATA%` is readable at all.
#[must_use]
pub fn helper_path() -> Option<PathBuf> {
    Some(fsw_core::update::update_directory_path()?.join(HELPER_NAME))
}

/// Whether route 1 is attemptable: either we are packaged (phase 1a runs the
/// sequence in-process) or the helper directory exists to stage into (phase
/// 1b). Deliberately cheap — it must not stage anything to answer.
#[must_use]
pub fn appinstall_available() -> bool {
    fsw_core::has_package_identity() || helper_path().is_some()
}

/// Copies this executable to the update directory under [`HELPER_NAME`].
///
/// The copy goes through `adapters::real_copy_file`, i.e. a `cmd.exe` child, for
/// the same reason the shell payloads do: the source lives in the package's
/// `WindowsApps` directory. `real_copy_file` keeps the source's file name, so
/// the rename to `fwdslash-helper.exe` is a second step; an existing copy is
/// removed first, because Windows will not rename over a file that is there.
#[cfg(windows)]
#[must_use]
pub fn stage_helper() -> Option<PathBuf> {
    let directory = fsw_core::update::update_directory_path()?;
    std::fs::create_dir_all(&directory).ok()?;
    let source = std::env::current_exe().ok()?;
    let staged = directory.join(source.file_name()?);
    let destination = directory.join(HELPER_NAME);

    // A leftover from a previous update, or the exe we are about to overwrite.
    let _ = std::fs::remove_file(&destination);
    crate::adapters::real_copy_file(&source, &directory).ok()?;
    if staged == destination {
        // Only reachable from a dev build already named fwdslash-helper.exe.
        return destination.is_file().then_some(destination);
    }
    std::fs::rename(&staged, &destination).ok()?;
    destination.is_file().then_some(destination)
}

/// The `.cmd` line that runs the helper's Store install. Quoted, because the
/// path contains separators and may contain spaces; every other token is a
/// literal this file controls.
#[must_use]
pub fn apply_store_command(helper: &Path, product_id: &str, previous_version: &str) -> String {
    format!(
        "\"{}\" update apply-store --product {product_id} --previous {previous_version}",
        helper.display()
    )
}

/// The `.cmd` line that registers a downloaded GitHub bundle.
#[must_use]
pub fn apply_bundle_command(helper: &Path, bundle: &Path, previous_version: &str) -> String {
    format!(
        "\"{}\" update apply-bundle --bundle \"{}\" --previous {previous_version}",
        helper.display(),
        bundle.display()
    )
}

/// Records the helper's verdict for the next packaged run to fold in. Silent on
/// failure: a helper that cannot write its result file has still done (or not
/// done) the install, and there is nobody to tell.
#[cfg(windows)]
pub fn write_result(result: &HelperResult) {
    let Some(directory) = fsw_core::update::update_directory_path() else {
        return;
    };
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let text = match result {
        HelperResult::Completed => "completed".to_string(),
        HelperResult::Paused => "paused".to_string(),
        HelperResult::Error(code) => format!("error:{code}"),
    };
    let _ = std::fs::write(
        directory.join(fsw_core::update::UPDATE_RESULT_FILE),
        text.as_bytes(),
    );
}

/// Registers a downloaded bundle over the running package.
///
/// `-ForceApplicationShutdown` because the broker is resident: without it the
/// registration is deferred to a launch that would never come. Windows
/// PowerShell rather than `pwsh`, because `Add-AppxPackage` lives there, and
/// through a real child process so the deployment is not attributed to a
/// package that is about to stop existing.
#[cfg(windows)]
#[must_use]
pub fn register_bundle(bundle: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if !bundle.is_file() {
        return false;
    }
    // Single-quoted PowerShell strings escape a quote by doubling it.
    let quoted = bundle.to_string_lossy().replace('\'', "''");
    let script = format!("Add-AppxPackage -Path '{quoted}' -ForceApplicationShutdown");
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
