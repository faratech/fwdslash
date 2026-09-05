//! The cmd adapter: installs `fsw-autorun.cmd` + the DIR/CD/PUSHD helpers +
//! a copy of the controller into `%LOCALAPPDATA%\ForwardSlashWindows\cmd`
//! and appends the AutoRun hook to `Command Processor`. A faithful port of
//! the retired `tools/Install-CmdAdapter.ps1` / `Uninstall-CmdAdapter.ps1`,
//! with every registry write routed through `reg.exe` (real hive) and every
//! read through the merged view.

use super::{reg, state, AdapterError};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

const COMMAND_PROCESSOR: &str = r"Software\Microsoft\Command Processor";
const MARKER_KEY: &str = fsw_core::CMD_ADAPTER_KEY;
const AUTORUN_VALUE: &str = "AutoRun";
/// Registry value kind stored in the marker for the restore.

fn payload_source_dir() -> Result<PathBuf, AdapterError> {
    super::payload_source_dir("cmd")
}

fn kind_from_raw(kind: &super::reg::RegKind) -> Option<&'static str> {
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
}

impl InstallState {
    fn undo(&mut self) {
        if self.autorun_changed {
            if self.original_present {
                let kind = kind_label(&self.original_kind);
                let _ = reg::set_string_kind(
                    COMMAND_PROCESSOR,
                    AUTORUN_VALUE,
                    &self.original_value,
                    kind,
                );
            } else {
                let _ = reg::delete_value(COMMAND_PROCESSOR, AUTORUN_VALUE);
            }
        }
        if self.deployed {
            let _ = std::fs::remove_dir_all(&self.install_root);
        }
        if self.renamed_old {
            let _ = std::fs::rename(&self.rollback, &self.install_root);
        }
        let _ = std::fs::remove_dir_all(&self.staging);
        let _ = reg::delete_tree(MARKER_KEY);
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
    let mut transaction = begin_install(controller)?;
    if let Err(error) = commit_install(&mut transaction) {
        transaction.undo();
        return Err(error);
    }
    println!("Forward Slash Windows cmd adapter installed for new Command Prompt sessions.");
    Ok(())
}

fn begin_install(controller: &Path) -> Result<InstallState, AdapterError> {
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
    };

    // Idempotence and interrupted-transaction refusal (marker read first).
    let marker_state = marker_state()?;
    let decision = state::decide_cmd_install(
        marker_state.is_some(),
        marker_state
            .as_deref()
            .map_or(state::MarkerState::Unknown, state::classify),
    );
    match decision {
        state::InstallDecision::Proceed => {}
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

    // Stage the payload through real-process copies: a packaged process's
    // own LocalAppData writes are virtualized, but real cmd.exe must read
    // these files, so the directories and copies go through cmd.exe children.
    let source = payload_source_dir()?;
    super::real_make_dir(&install_parent)?;
    super::real_make_dir(&state.staging)?;
    // Every file the AutoRun macros call. Keep in step with the payload
    // lists in tools/Package.ps1 and tools/Package-Msix.ps1.
    for file in [
        "fsw-autorun.cmd",
        "fsw-cd.cmd",
        "fsw-dir.cmd",
        "fsw-pushd.cmd",
    ] {
        super::real_copy_file(&source.join(file), &state.staging)?;
    }
    super::real_copy_file(controller, &state.staging)?;

    // Snapshot the user's AutoRun exactly as it is today (raw, kind-aware).
    let (original_present, original_value, original_kind) =
        match reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)? {
            Some((kind, value)) => {
                let Some(label) = kind_from_raw(&kind) else {
                    return Err(AdapterError::new(
                        "The existing Command Processor AutoRun value is not a string. No changes were made.",
                    ));
                };
                (true, value, label.to_string())
            }
            None => (false, String::new(), super::reg::RegKind::Sz.marker_label().to_string()),
        };
    state.original_present = original_present;
    state.original_value = original_value.clone();
    state.original_kind = original_kind.clone();
    state.marker_value = Some(format!(
        "call \"{}\"",
        install_root.join("fsw-autorun.cmd").display()
    ));
    Ok(state)
}

fn commit_install(state: &mut InstallState) -> Result<(), AdapterError> {
    let Some(marker_value) = state.marker_value.clone() else {
        return Err(AdapterError::new("installer was not prepared"));
    };
    let installed_value =
        state::installed_autorun(&state.original_value, &marker_value);

    // Marker first (prepared), with the full snapshot for recovery.
    reg::set_string(MARKER_KEY, "State", "prepared")?;
    reg::set_string(MARKER_KEY, "Version", super::PAYLOAD_VERSION)?;
    reg::set_string(MARKER_KEY, "TransactionId", &state.transaction_id)?;
    reg::set_string(MARKER_KEY, "InstallDirectory", &state.install_root.display().to_string())?;
    reg::set_dword(MARKER_KEY, "OriginalPresent", u32::from(state.original_present))?;
    reg::set_string(MARKER_KEY, "OriginalKind", &state.original_kind)?;
    reg::set_string(MARKER_KEY, "OriginalAutoRun", &state.original_value)?;
    reg::set_string(MARKER_KEY, "InstalledAutoRun", &installed_value)?;

    // Deploy: rename any previous install out of the way, then move staging in.
    if state.install_root.exists() {
        std::fs::rename(&state.install_root, &state.rollback)?;
        state.renamed_old = true;
    }
    std::fs::rename(&state.staging, &state.install_root)?;
    state.deployed = true;

    // The hook itself, kind-preserved.
    reg::set_string_kind(
        COMMAND_PROCESSOR,
        AUTORUN_VALUE,
        &installed_value,
        kind_label(&state.original_kind),
    )?;
    state.autorun_changed = true;

    reg::set_string(MARKER_KEY, "State", "installed")?;
    if state.renamed_old && state.rollback.exists() {
        let _ = std::fs::remove_dir_all(&state.rollback);
        state.renamed_old = false;
    }
    Ok(())
}

/// Removes the cmd adapter and restores the previous AutoRun value.
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
    let (current_present, current_value) =
        match reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE)? {
            Some((_, value)) => (true, value),
            None => (false, String::new()),
        };
    let verdict = state::judge_autorun(
        current_present,
        &current_value,
        &values.installed_autorun,
        &values.original_autorun,
    );
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
        let _ = Command::new("cmd.exe")
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
fn marker_snapshot()
-> Result<Option<(String, CmdMarkerValues)>, AdapterError> {
    use windows_registry::CURRENT_USER;

    let key = CURRENT_USER
        .open(MARKER_KEY)
        .map_err(|error| super::registry_error(error))?;
    Ok(Some((
        key.get_string("State").unwrap_or_default(),
        CmdMarkerValues {
            install_directory: key.get_string("InstallDirectory").unwrap_or_default(),
            installed_autorun: key.get_string("InstalledAutoRun").unwrap_or_default(),
            original_autorun: key.get_string("OriginalAutoRun").unwrap_or_default(),
            original_present: key.get_u32("OriginalPresent").unwrap_or(0) != 0,
            original_kind: key.get_string("OriginalKind").unwrap_or_default(),
            removal_path: key.get_string("RemovalPath").unwrap_or_default(),
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
}

/// The marker `State` text, or `None` when the key is absent.
pub fn marker_state() -> Result<Option<String>, AdapterError> {
    use windows_registry::CURRENT_USER;

    // An absent marker key means "not installed" — not an error.
    match CURRENT_USER.open(MARKER_KEY) {
        Ok(key) => Ok(Some(key.get_string("State").unwrap_or_default())),
        Err(_) => Ok(None),
    }
}
