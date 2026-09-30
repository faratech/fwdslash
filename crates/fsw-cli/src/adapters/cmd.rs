//! The cmd adapter: installs `fsw-autorun.cmd` + the DIR/CD/PUSHD helpers +
//! a copy of the controller into `%LOCALAPPDATA%\ForwardSlashWindows\cmd`
//! and appends the `AutoRun` hook to `Command Processor`. A faithful port of
//! the retired `tools/Install-CmdAdapter.ps1` / `Uninstall-CmdAdapter.ps1`,
//! with every registry write routed through `reg.exe` (real hive) and every
//! read through the merged view.

use super::{AdapterError, reg, state};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMMAND_PROCESSOR: &str = r"Software\Microsoft\Command Processor";
const MARKER_KEY: &str = fsw_core::CMD_ADAPTER_KEY;
const AUTORUN_VALUE: &str = "AutoRun";

fn payload_source_dir() -> Result<PathBuf, AdapterError> {
    super::payload_source_dir("cmd")
}

fn kind_from_raw(kind: super::reg::RegKind) -> Option<&'static str> {
    match kind {
        super::reg::RegKind::Sz => Some(super::reg::RegKind::Sz.marker_label()),
        super::reg::RegKind::ExpandSz => Some(super::reg::RegKind::ExpandSz.marker_label()),
        super::reg::RegKind::Dword => None,
    }
}

fn kind_label(kind: &str) -> super::reg::RegKind {
    super::reg::RegKind::from_marker_label(kind).unwrap_or(super::reg::RegKind::Sz)
}

/// Rollback state for the install transaction; `undo` mirrors the script's
/// catch block in the exact same order.
// Each flag records an independent completed rollback step; collapsing them
// would make an interrupted install restore resources it never changed.
#[allow(clippy::struct_excessive_bools)]
struct InstallState {
    transaction_id: String,
    staging: PathBuf,
    rollback: PathBuf,
    install_root: PathBuf,
    marker_value: Option<String>,
    autorun_changed: bool,
    deployed: bool,
    renamed_old: bool,
    original_present: bool,
    original_value: String,
    original_kind: String,
    product_probe: String,
    previous_marker: Option<(String, CmdMarkerValues)>,
    previous_autorun: Option<(reg::RegKind, String)>,
}

impl InstallState {
    fn undo(&mut self) -> Result<(), AdapterError> {
        if self.autorun_changed {
            let current = reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)?;
            let installed = state::installed_autorun(
                &self.original_value,
                self.marker_value.as_deref().unwrap_or_default(),
            );
            if current.as_ref() != Some(&(kind_label(&self.original_kind), installed)) {
                return Err(AdapterError::new(
                    "AutoRun changed during rollback; your value and the recovery state were preserved.",
                ));
            }
            if let Some((kind, value)) = &self.previous_autorun {
                reg::set_string_kind(COMMAND_PROCESSOR, AUTORUN_VALUE, value, *kind)?;
            } else {
                reg::delete_value(COMMAND_PROCESSOR, AUTORUN_VALUE)?;
            }
        }
        if self.deployed {
            std::fs::remove_dir_all(&self.install_root)?;
        }
        if self.renamed_old {
            std::fs::rename(&self.rollback, &self.install_root)?;
        }
        let _ = std::fs::remove_dir_all(&self.staging);
        if let Some((state, values)) = &self.previous_marker {
            restore_marker(state, values)?;
        } else if marker_snapshot()?.is_some() {
            reg::delete_tree(MARKER_KEY)?;
        }
        Ok(())
    }
}

/// Installs the cmd adapter. `controller` is the running `fwdslash.exe`.
pub fn install(controller: &Path) -> Result<(), AdapterError> {
    if !controller.is_file() {
        return Err(AdapterError::new(&format!(
            "fwdslash.exe was not found: {}",
            controller.display()
        )));
    }
    // Everything from the first staging directory onward is transactional:
    // any failure runs the catch-block undo before surfacing the error.
    let mut transaction = begin_install(controller, false)?;
    if let Err(error) = commit_install(&mut transaction) {
        if let Err(rollback) = transaction.undo() {
            return Err(AdapterError::new(&format!(
                "{error} Recovery remains pending: {rollback}"
            )));
        }
        return Err(error);
    }
    println!("Forward Slash Windows cmd adapter installed for new Command Prompt sessions.");
    Ok(())
}

/// Upgrade the live payload without removing the functioning hook or snapshot.
pub fn upgrade(controller: &Path) -> Result<(), AdapterError> {
    let mut transaction = begin_install(controller, true)?;
    if let Err(error) = commit_install(&mut transaction) {
        if let Err(rollback) = transaction.undo() {
            return Err(AdapterError::new(&format!(
                "{error} Recovery remains pending: {rollback}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

fn begin_install(controller: &Path, upgrading: bool) -> Result<InstallState, AdapterError> {
    let install_root = super::local_app_data()?
        .join("ForwardSlashWindows")
        .join("cmd");
    let install_parent = install_root
        .parent()
        .ok_or_else(|| AdapterError::new("invalid adapter install path"))?
        .to_path_buf();
    let transaction_id = super::new_transaction_id();
    let mut state = InstallState {
        transaction_id: transaction_id.clone(),
        staging: install_parent.join(format!(".cmd-staging-{transaction_id}")),
        rollback: install_parent.join(format!(".cmd-rollback-{transaction_id}")),
        install_root: install_root.clone(),
        marker_value: None,
        autorun_changed: false,
        deployed: false,
        renamed_old: false,
        original_present: false,
        original_value: String::new(),
        original_kind: super::reg::RegKind::Sz.marker_label().to_string(),
        product_probe: String::new(),
        previous_marker: marker_snapshot()?,
        previous_autorun: reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)?,
    };

    // Idempotence and interrupted-transaction refusal (marker read first).
    let marker_state = marker_state();
    let decision = state::decide_cmd_install(
        marker_state.is_some(),
        marker_state
            .as_deref()
            .map_or(state::MarkerState::Unknown, state::classify),
    );
    match decision {
        state::InstallDecision::Proceed => {}
        state::InstallDecision::AlreadyInstalled if upgrading => {}
        state::InstallDecision::AlreadyInstalled => {
            return Err(AdapterError::new(
                "The cmd adapter is already installed. Uninstall it before reinstalling.",
            ));
        }
        state::InstallDecision::RecoverRequired => {
            return Err(AdapterError::new(
                "An incomplete cmd adapter transaction exists. Run \"fwdslash integration cmd disable\" to recover it.",
            ));
        }
    }

    stage_cmd_payload(&mut state, controller, &install_parent)?;

    // Snapshot the user's AutoRun as it is today (raw, kind-aware) but strip
    // any fwdslash hook first: a prior install whose marker was lost (an MSIX
    // uninstall runs no code) can leave `call "…fsw-autorun.cmd"` in AutoRun,
    // and snapshotting that would make a later uninstall "restore" our own hook
    // and let installed_autorun compose `call fsw & call fsw` (#37).
    let (raw_present, raw_value, original_kind) = match reg::read_raw_string(
        COMMAND_PROCESSOR,
        AUTORUN_VALUE,
    )? {
        Some((kind, value)) => {
            let Some(label) = kind_from_raw(kind) else {
                return Err(AdapterError::new(
                    "The existing Command Processor AutoRun value is not a string. No changes were made.",
                ));
            };
            (true, value, label.to_string())
        }
        None => (
            false,
            String::new(),
            super::reg::RegKind::Sz.marker_label().to_string(),
        ),
    };
    let original_value = state::strip_fwdslash_autorun(&raw_value);
    // Preserve empty originals and their kind, but discard orphaned hook-only values.
    let original_present =
        state::original_autorun_present(raw_present, &raw_value, &original_value);
    state.original_present = original_present;
    state.original_value.clone_from(&original_value);
    state.original_kind.clone_from(&original_kind);
    state.marker_value = Some(format!(
        "call \"{}\"",
        install_root.join("fsw-autorun.cmd").display()
    ));
    if upgrading {
        retain_upgrade_snapshot(&mut state)?;
    }
    Ok(state)
}

fn retain_upgrade_snapshot(state: &mut InstallState) -> Result<(), AdapterError> {
    if let Some((_, previous)) = &state.previous_marker {
        let current = state
            .previous_autorun
            .as_ref()
            .map(|(_, text)| text.as_str());
        if current.is_none()
            || state
                .previous_autorun
                .as_ref()
                .is_some_and(|(kind, _)| *kind != kind_label(&previous.original_kind))
            || state::judge_autorun(
                current.is_some(),
                current.unwrap_or_default(),
                &previous.installed_autorun,
                &previous.original_autorun,
            ) == state::AutorunVerdict::Changed
        {
            let _ = std::fs::remove_dir_all(&state.staging);
            return Err(AdapterError::new(
                "Command Processor AutoRun changed after installation. No changes were made.",
            ));
        }
        state.original_present = previous.original_present;
        state.original_value.clone_from(&previous.original_autorun);
        state.original_kind.clone_from(&previous.original_kind);
    }
    Ok(())
}

fn stage_cmd_payload(
    state: &mut InstallState,
    controller: &Path,
    install_parent: &Path,
) -> Result<(), AdapterError> {
    // Stage the payload through real-process copies: a packaged process's
    // own LocalAppData writes are virtualized, but real cmd.exe must read
    // these files, so the directories and copies go through cmd.exe children.
    let source = payload_source_dir()?;
    super::real_make_dir(install_parent)?;
    super::real_make_dir(&state.staging)?;
    // The macro helpers the AutoRun hook calls. Keep in step with the payload
    // lists in tools/Package-Msix.ps1 and tools/package_msix.py. fsw-autorun.cmd is
    // *generated* below rather than copied, so the product-presence probe can
    // be baked in (#37).
    for file in ["fsw-cd.cmd", "fsw-dir.cmd", "fsw-pushd.cmd"] {
        super::real_copy_file(&source.join(file), &state.staging)?;
    }
    super::real_copy_file(controller, &state.staging)?;
    // Generate the AutoRun hook with the product-presence probe baked in.
    // Direct write — %LOCALAPPDATA%\ForwardSlashWindows writes are real for
    // this package, so the generated file is visible to unpackaged consoles.
    let probe = super::product_probe_path(controller);
    let alias = super::app_execution_alias().unwrap_or_default();
    state.product_probe = probe.display().to_string();
    std::fs::write(
        state.staging.join("fsw-autorun.cmd"),
        generate_autorun(&state.product_probe, &alias.display().to_string()),
    )?;

    Ok(())
}

fn commit_install(state: &mut InstallState) -> Result<(), AdapterError> {
    let Some(marker_value) = state.marker_value.clone() else {
        return Err(AdapterError::new("installer was not prepared"));
    };
    let installed_value = state::installed_autorun(&state.original_value, &marker_value);

    // Marker first (prepared), with the full snapshot for recovery.
    // Publish recovery ownership before any rename. Keep the old snapshot
    // intact through the entire upgrade instead of uninstalling it first.
    reg::set_string(
        MARKER_KEY,
        "RollbackPath",
        &state.rollback.display().to_string(),
    )?;
    reg::set_string(
        MARKER_KEY,
        "StagingPath",
        &state.staging.display().to_string(),
    )?;
    if let Some((_, previous)) = &state.previous_marker {
        reg::set_string(MARKER_KEY, "UpgradeVersion", &previous.version)?;
        reg::set_dword(MARKER_KEY, "UpgradePending", 1)?;
        reg::set_string(MARKER_KEY, "UpgradeProductProbe", &previous.product_probe)?;
    }
    reg::set_string(MARKER_KEY, "State", "prepared")?;
    reg::set_string(MARKER_KEY, "Version", super::PAYLOAD_VERSION)?;
    reg::set_string(MARKER_KEY, "TransactionId", &state.transaction_id)?;
    reg::set_string(
        MARKER_KEY,
        "InstallDirectory",
        &state.install_root.display().to_string(),
    )?;
    reg::set_dword(
        MARKER_KEY,
        "OriginalPresent",
        u32::from(state.original_present),
    )?;
    reg::set_string(MARKER_KEY, "OriginalKind", &state.original_kind)?;
    reg::set_string(MARKER_KEY, "OriginalAutoRun", &state.original_value)?;
    reg::set_string(MARKER_KEY, "InstalledAutoRun", &installed_value)?;
    reg::set_string(MARKER_KEY, "ProductProbe", &state.product_probe)?;

    // Deploy: rename any previous install out of the way, then move staging in.
    if state.install_root.exists() {
        std::fs::rename(&state.install_root, &state.rollback)?;
        state.renamed_old = true;
    }
    std::fs::rename(&state.staging, &state.install_root)?;
    state.deployed = true;

    // The hook itself, kind-preserved.
    let current = reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)?;
    if current != state.previous_autorun {
        return Err(AdapterError::new(
            "AutoRun changed during installation; your value was preserved.",
        ));
    }
    if current.as_ref().map(|(_, value)| value.as_str()) != Some(&installed_value) {
        reg::set_string_kind(
            COMMAND_PROCESSOR,
            AUTORUN_VALUE,
            &installed_value,
            kind_label(&state.original_kind),
        )?;
        state.autorun_changed = true;
    }

    reg::set_string(MARKER_KEY, "State", "installed")?;
    if state.renamed_old && state.rollback.exists() {
        let _ = std::fs::remove_dir_all(&state.rollback);
        state.renamed_old = false;
    }
    let _ = cleanup_upgrade_marker();
    Ok(())
}

/// Removes the cmd adapter and restores the previous `AutoRun` value.
pub fn uninstall() -> Result<(), AdapterError> {
    let Some((text, values)) = marker_snapshot()? else {
        println!("Forward Slash Windows cmd adapter is not installed.");
        return Ok(());
    };
    let marker_present = true;
    let state_kind = state::classify(&text);
    let install_root = if values.install_directory.is_empty() {
        super::local_app_data()
            .map(|dir| dir.join("ForwardSlashWindows\\cmd"))
            .unwrap_or_default()
    } else {
        PathBuf::from(&values.install_directory)
    };

    // Current AutoRun, from the same raw read the installer used.
    let current = reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)?;
    let verdict = judge_current_autorun(current.as_ref(), &values);
    match state::decide_cmd_uninstall(marker_present, state_kind, verdict) {
        state::UninstallDecision::NotInstalled | state::UninstallDecision::Proceed => {}
        state::UninstallDecision::UnknownState => {
            return Err(AdapterError::new(&format!(
                "Unknown cmd adapter transaction state '{text}'. No changes were made."
            )));
        }
        state::UninstallDecision::AutoRunChanged => {
            return Err(AdapterError::new(
                "Command Processor AutoRun changed after installation. Refusing to overwrite it; reconcile that value and retry.",
            ));
        }
    }

    // Recoverable removal: rename the install dir out first, recorded in the
    // marker so an interrupted uninstall can complete.
    let mut renamed = false;
    let mut removal_path = values.removal_path.clone();
    let mut have_removal_path = !removal_path.is_empty();
    if state_kind == state::MarkerState::Removing && have_removal_path {
        renamed = Path::new(&removal_path).exists();
    } else if !install_root.as_os_str().is_empty() && install_root.exists() {
        let removal = format!(
            "{}.removing-{transaction_hint}",
            install_root.display(),
            transaction_hint = super::new_transaction_id()
        );
        reg::set_string(MARKER_KEY, "RemovalPath", &removal)?;
        reg::set_string(MARKER_KEY, "State", "removing")?;
        std::fs::rename(&install_root, &removal)?;
        removal_path = removal;
        have_removal_path = true;
        renamed = true;
    }

    // Restore the user's AutoRun, then drop the marker and the removal dir.
    let restore_result = if values.original_present {
        reg::set_string_kind(
            COMMAND_PROCESSOR,
            AUTORUN_VALUE,
            &values.original_autorun,
            kind_label(&values.original_kind),
        )
    } else {
        reg::delete_value(COMMAND_PROCESSOR, AUTORUN_VALUE)
    };
    if let Err(error) = restore_result {
        if !renamed {
            return Err(error);
        }
        if have_removal_path && !install_root.exists() {
            std::fs::rename(&removal_path, &install_root)?;
        }
        return Err(error);
    }

    reg::delete_tree(MARKER_KEY)?;
    if renamed && Path::new(&removal_path).exists() {
        let Some(cmd) = fsw_core::SystemBinary::Cmd.path() else {
            return Err(AdapterError::new("cmd.exe was not found."));
        };
        let _ = Command::new(cmd)
            .args(["/c", "rmdir", "/s", "/q"])
            .arg(&removal_path)
            .creation_flags(0x0800_0000)
            .status();
    }
    println!(
        "Forward Slash Windows cmd adapter uninstalled and the previous AutoRun value restored."
    );
    println!("Already-open Command Prompt windows keep their in-memory macros until closed.");
    Ok(())
}

/// Reads the cmd marker key: `(State, values)` or `None` when absent.
#[allow(clippy::type_complexity)]
fn marker_snapshot() -> Result<Option<(String, CmdMarkerValues)>, AdapterError> {
    marker_snapshot_at(MARKER_KEY)
}

fn marker_snapshot_at(marker_key: &str) -> Result<Option<(String, CmdMarkerValues)>, AdapterError> {
    use windows_registry::CURRENT_USER;

    let key = match CURRENT_USER.open(marker_key) {
        Ok(key) => key,
        Err(error) if error.code().0.cast_unsigned() == 0x8007_0002 => return Ok(None),
        Err(error) => return Err(super::registry_error(error)),
    };
    Ok(Some((
        key.get_string("State").unwrap_or_default(),
        CmdMarkerValues {
            install_directory: key.get_string("InstallDirectory").unwrap_or_default(),
            installed_autorun: key.get_string("InstalledAutoRun").unwrap_or_default(),
            original_autorun: key.get_string("OriginalAutoRun").unwrap_or_default(),
            original_present: key.get_u32("OriginalPresent").unwrap_or(0) != 0,
            original_kind: key.get_string("OriginalKind").unwrap_or_default(),
            removal_path: key.get_string("RemovalPath").unwrap_or_default(),
            version: key.get_string("Version").unwrap_or_default(),
            product_probe: key.get_string("ProductProbe").unwrap_or_default(),
            transaction_id: key.get_string("TransactionId").unwrap_or_default(),
            rollback_path: key.get_string("RollbackPath").unwrap_or_default(),
            staging_path: key.get_string("StagingPath").unwrap_or_default(),
            upgrade_version: key.get_string("UpgradeVersion").unwrap_or_default(),
            upgrade_product_probe: key.get_string("UpgradeProductProbe").unwrap_or_default(),
            upgrade_pending: key.get_u32("UpgradePending").unwrap_or(0) != 0
                || key.get_string("UpgradeVersion").is_ok(),
        },
    )))
}

#[derive(Debug, Default, Clone)]
struct CmdMarkerValues {
    install_directory: String,
    installed_autorun: String,
    original_autorun: String,
    original_present: bool,
    original_kind: String,
    removal_path: String,
    version: String,
    product_probe: String,
    transaction_id: String,
    rollback_path: String,
    staging_path: String,
    upgrade_version: String,
    upgrade_product_probe: String,
    upgrade_pending: bool,
}

fn judge_current_autorun(
    current: Option<&(reg::RegKind, String)>,
    values: &CmdMarkerValues,
) -> state::AutorunVerdict {
    if current
        .as_ref()
        .is_some_and(|(kind, _)| *kind != kind_label(&values.original_kind))
        || (current.is_none() && values.original_present)
    {
        return state::AutorunVerdict::Changed;
    }
    state::judge_autorun(
        current.is_some(),
        current
            .as_ref()
            .map(|(_, value)| value.as_str())
            .unwrap_or_default(),
        &values.installed_autorun,
        &values.original_autorun,
    )
}

fn restore_marker(marker_state: &str, values: &CmdMarkerValues) -> Result<(), AdapterError> {
    for (name, value) in [
        ("Version", &values.version),
        ("TransactionId", &values.transaction_id),
        ("InstallDirectory", &values.install_directory),
        ("OriginalKind", &values.original_kind),
        ("OriginalAutoRun", &values.original_autorun),
        ("InstalledAutoRun", &values.installed_autorun),
        ("ProductProbe", &values.product_probe),
    ] {
        reg::set_string(MARKER_KEY, name, value)?;
    }
    reg::set_dword(
        MARKER_KEY,
        "OriginalPresent",
        u32::from(values.original_present),
    )?;
    // Publish the restored milestone before consuming any recovery field.
    reg::set_string(MARKER_KEY, "State", marker_state)?;
    cleanup_upgrade_marker()
}

fn cleanup_upgrade_marker() -> Result<(), AdapterError> {
    // The sentinel must go first: missing dependent paths are harmless once
    // installed is durable, and must never look like an unfinished rollback.
    for name in [
        "UpgradePending",
        "UpgradeVersion",
        "UpgradeProductProbe",
        "RollbackPath",
        "StagingPath",
    ] {
        reg::delete_value(MARKER_KEY, name)?;
    }
    Ok(())
}

/// Revert an interrupted payload-only upgrade before any pruning occurs.
pub fn recover_upgrade() -> Result<(), AdapterError> {
    let Some((marker_state, values)) = marker_snapshot()? else {
        return Ok(());
    };
    let root = super::local_app_data()?
        .join("ForwardSlashWindows")
        .join("cmd");
    recover_upgrade_at(&root, &marker_state, values, |values, cleanup_only| {
        if cleanup_only {
            cleanup_upgrade_marker()
        } else {
            restore_marker("installed", values)
        }
    })
}

fn recover_upgrade_at(
    root: &Path,
    marker_state: &str,
    mut values: CmdMarkerValues,
    mut finish: impl FnMut(&CmdMarkerValues, bool) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    if marker_state == "installed" {
        return finish(&values, true);
    }
    if !values.upgrade_pending {
        return Ok(());
    }
    let rollback = PathBuf::from(&values.rollback_path);
    let staging = PathBuf::from(&values.staging_path);
    if rollback.parent() != root.parent()
        || staging.parent() != root.parent()
        || !rollback
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".cmd-rollback-"))
        || !staging
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".cmd-staging-"))
    {
        return Err(AdapterError::new(
            "The cmd upgrade recovery paths are invalid; no files were changed.",
        ));
    }
    if marker_state != "installed" && rollback.is_dir() {
        if root.exists() {
            std::fs::remove_dir_all(root)?;
        }
        std::fs::rename(&rollback, root)?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    if marker_state != "installed" {
        values.version.clone_from(&values.upgrade_version);
        values
            .product_probe
            .clone_from(&values.upgrade_product_probe);
    }
    finish(&values, false)
}

pub fn active_upgrade_paths() -> Vec<PathBuf> {
    marker_snapshot()
        .ok()
        .flatten()
        .map_or_else(Vec::new, |(_, values)| {
            [values.rollback_path, values.staging_path]
                .into_iter()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect()
        })
}

/// The generated `AutoRun` hook (#37). Baking the product-presence probe in lets
/// the hook install the macros only while the product is present, self-clean
/// when it is gone, and cost nothing (one `if exist`) on a normal shell start —
/// with the macros never routing through an orphaned controller copy.
fn generate_autorun(probe: &str, alias: &str) -> String {
    format!(
        "@echo off\r\n\
         if exist \"{probe}\" goto fsw_present\r\n\
         if exist \"{alias}\" goto fsw_present\r\n\
         goto fsw_gone\r\n\
         :fsw_present\r\n\
         doskey dir=call \"%~dp0fsw-dir.cmd\" $*\r\n\
         doskey ls=call \"%~dp0fsw-dir.cmd\" $*\r\n\
         doskey cd=call \"%~dp0fsw-cd.cmd\" $*\r\n\
         doskey chdir=call \"%~dp0fsw-cd.cmd\" $*\r\n\
         doskey pushd=call \"%~dp0fsw-pushd.cmd\" $*\r\n\
         goto :eof\r\n\
         :fsw_gone\r\n\
         if exist \"%~dp0fwdslash.exe\" start \"\" /b \"%~dp0fwdslash.exe\" uninstall --orphaned >nul 2>&1\r\n"
    )
}

/// The health of the cmd `AutoRun` hook, for `fwdslash doctor` /
/// `integrations` (#37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdHealth {
    /// No hook and no installed marker.
    Clean,
    /// A hook whose `fsw-autorun.cmd` target exists.
    Healthy,
    /// A hook pointing at a missing `fsw-autorun.cmd`, or an installed marker
    /// whose hook has vanished.
    Orphaned,
}

/// Read-only classification of the cmd adapter's `AutoRun` state.
pub fn health() -> CmdHealth {
    let raw = match reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE) {
        Ok(Some((_, value))) => value,
        _ => String::new(),
    };
    if state::autorun_references_fwdslash(&raw) {
        if let Some(path) = state::fwdslash_autorun_path(&raw)
            && Path::new(&path).exists()
        {
            return CmdHealth::Healthy;
        }
        return CmdHealth::Orphaned;
    }
    if fsw_core::adapter_installed(MARKER_KEY) {
        CmdHealth::Orphaned
    } else {
        CmdHealth::Clean
    }
}

/// The product-presence probe recorded by the cmd marker, if any — one input to
/// the orphan self-clean's slow confirm.
pub fn recorded_probe() -> Option<String> {
    use windows_registry::CURRENT_USER;
    CURRENT_USER
        .open(MARKER_KEY)
        .ok()
        .and_then(|key| key.get_string("ProductProbe").ok())
        .filter(|probe| !probe.is_empty())
}

/// Whether the live `AutoRun` still routes through a fwdslash hook. The payload
/// must never be deleted while this is true, or every console start prints
/// "The system cannot find the path specified" with nothing left to fix it.
pub fn autorun_still_hooked() -> bool {
    matches!(
        reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE),
        Ok(Some((_, value))) if state::autorun_references_fwdslash(&value)
    )
}

/// Removes **only** fwdslash's own `call "…fsw-autorun.cmd"` segment from the
/// live `AutoRun`, preserving every third-party segment byte-for-byte and its
/// registry kind, and deleting the value outright when nothing else remains.
///
/// This is the cmd analogue of `strip_all_ps_profiles`: the transactional
/// uninstall deliberately *refuses* when a third party edited `AutoRun` after
/// we installed, which would otherwise strand our `call` in a value whose
/// target we are about to delete (#37). Stripping is always safe because it
/// only ever removes segments we wrote.
pub fn strip_autorun_hook() -> Result<(), AdapterError> {
    let Some((kind, current)) = reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)? else {
        return Ok(());
    };
    if !state::autorun_references_fwdslash(&current) {
        return Ok(());
    }
    let stripped = state::strip_fwdslash_autorun(&current);
    if stripped.is_empty() {
        reg::delete_value(COMMAND_PROCESSOR, AUTORUN_VALUE)
    } else {
        reg::set_string_kind(COMMAND_PROCESSOR, AUTORUN_VALUE, &stripped, kind)
    }
}

/// Detect-and-repair for the cmd adapter (#37). Detection is the point; when
/// the hook is orphaned *and* the marker is still present, the existing
/// transactional uninstall restores the true `AutoRun` (refusing if a third party
/// changed it). If our hook survives that — a refusal, or a marker-less
/// dangling hook — strip just our own segment so the console is never left
/// calling a script that no longer exists. Returns the health *after* the
/// repair attempt.
pub fn repair() -> Result<CmdHealth, AdapterError> {
    recover_upgrade()?;
    if health() == CmdHealth::Orphaned {
        if marker_state().is_some() {
            let _ = uninstall();
        }
        // Only strip a hook whose target is actually gone: a healthy hook is
        // the working integration, not debris.
        if autorun_still_hooked() && !hook_target_exists() {
            strip_autorun_hook()?;
        }
    }
    Ok(health())
}

/// Whether the `fsw-autorun.cmd` the live `AutoRun` points at exists on disk.
fn hook_target_exists() -> bool {
    let Ok(Some((_, current))) = reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE) else {
        return false;
    };
    state::fwdslash_autorun_path(&current).is_some_and(|path| Path::new(&path).exists())
}

/// The `RemovalPath` a cmd uninstall is currently using, or `None`.
///
/// The rename-aside prune (#127) must never delete the directory an in-flight
/// (or interrupted-but-resumable) uninstall recorded: while the marker says
/// `removing`, that directory is recovery state.
pub fn active_removal_path() -> Option<String> {
    use windows_registry::CURRENT_USER;

    let key = CURRENT_USER.open(MARKER_KEY).ok()?;
    if state::classify(&key.get_string("State").unwrap_or_default()) != state::MarkerState::Removing
    {
        return None;
    }
    key.get_string("RemovalPath")
        .ok()
        .filter(|path| !path.is_empty())
}

/// The marker `State` text, or `None` when the key is absent.
pub fn marker_state() -> Option<String> {
    use windows_registry::CURRENT_USER;

    // An absent marker key means "not installed" — not an error.
    match CURRENT_USER.open(MARKER_KEY) {
        Ok(key) => Some(key.get_string("State").unwrap_or_default()),
        Err(_) => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod regression_tests {
    use super::*;

    #[test]
    fn absent_marker_is_a_noop_in_the_actual_registry_reader() {
        let key = format!(
            "Software\\FswAdapterAbsent\\{}",
            super::super::new_transaction_id()
        );
        assert!(marker_snapshot_at(&key).expect("absent key").is_none());
    }

    #[test]
    fn missing_and_kind_only_autorun_changes_are_not_overwritten() {
        let values = CmdMarkerValues {
            original_present: true,
            original_kind: "String".to_owned(),
            original_autorun: "old".to_owned(),
            installed_autorun: "hook".to_owned(),
            ..CmdMarkerValues::default()
        };
        assert_eq!(
            judge_current_autorun(None, &values),
            state::AutorunVerdict::Changed
        );
        assert_eq!(
            judge_current_autorun(Some(&(reg::RegKind::ExpandSz, "hook".to_owned())), &values),
            state::AutorunVerdict::Changed
        );
        assert_eq!(
            judge_current_autorun(Some(&(reg::RegKind::Sz, "hook".to_owned())), &values),
            state::AutorunVerdict::Matches
        );
        let absent_original = CmdMarkerValues {
            original_present: false,
            ..values
        };
        assert_eq!(
            judge_current_autorun(None, &absent_original),
            state::AutorunVerdict::Matches
        );
    }

    #[test]
    fn committed_upgrade_accepts_partially_removed_recovery_paths() {
        let values = CmdMarkerValues {
            upgrade_pending: true,
            upgrade_version: "old".to_owned(),
            version: "new".to_owned(),
            rollback_path: String::new(),
            staging_path: String::new(),
            ..CmdMarkerValues::default()
        };
        let mut cleaned = false;
        recover_upgrade_at(
            Path::new(r"C:\unused-fixture-root"),
            "installed",
            values,
            |values, cleanup_only| {
                assert!(cleanup_only);
                assert_eq!(values.version, "new");
                cleaned = true;
                Ok(())
            },
        )
        .expect("cleanup after commit");
        assert!(cleaned);
    }

    #[test]
    fn interrupted_upgrade_preserves_working_payload_and_retries_marker_failure_even_without_old_version()
     {
        let directory = std::env::temp_dir().join(format!(
            "fsw-cmd-recovery-{}",
            super::super::new_transaction_id()
        ));
        std::fs::create_dir(&directory).expect("fixture");
        let root = directory.join("cmd");
        let rollback = directory.join(".cmd-rollback-ab-cd");
        let staging = directory.join(".cmd-staging-ab-cd");
        std::fs::create_dir(&rollback).expect("rollback");
        std::fs::write(rollback.join("fwdslash.exe"), b"old working payload").expect("old binary");
        std::fs::create_dir(&staging).expect("staging");
        let values = CmdMarkerValues {
            upgrade_pending: true,
            version: "new".to_owned(),
            rollback_path: rollback.display().to_string(),
            staging_path: staging.display().to_string(),
            ..CmdMarkerValues::default()
        };
        assert!(
            recover_upgrade_at(&root, "prepared", values.clone(), |_, _| Err(
                AdapterError::new("injected marker failure")
            ))
            .is_err()
        );
        assert_eq!(
            std::fs::read(root.join("fwdslash.exe")).expect("restored binary"),
            b"old working payload"
        );
        recover_upgrade_at(&root, "prepared", values, |values, cleanup_only| {
            assert!(!cleanup_only);
            assert!(values.version.is_empty());
            Ok(())
        })
        .expect("marker retry");
        assert_eq!(
            std::fs::read(root.join("fwdslash.exe")).expect("binary after retry"),
            b"old working payload"
        );
        std::fs::remove_dir_all(directory).expect("cleanup");
    }
}
