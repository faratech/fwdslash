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
    product_probe: String,
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
        product_probe: String::new(),
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
    // The macro helpers the AutoRun hook calls. Keep in step with the payload
    // lists in tools/Package.ps1 and tools/Package-Msix.ps1. fsw-autorun.cmd is
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

    // Snapshot the user's AutoRun as it is today (raw, kind-aware) but strip
    // any fwdslash hook first: a prior install whose marker was lost (an MSIX
    // uninstall runs no code) can leave `call "…fsw-autorun.cmd"` in AutoRun,
    // and snapshotting that would make a later uninstall "restore" our own hook
    // and let installed_autorun compose `call fsw & call fsw` (#37).
    let (raw_present, raw_value, original_kind) =
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
    let original_value = state::strip_fwdslash_autorun(&raw_value);
    // "Present" now means there is genuine third-party content to restore; a
    // value that was purely our hook is treated as absent so uninstall deletes
    // AutoRun rather than restoring an empty string.
    let original_present = raw_present && !original_value.is_empty();
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
    reg::set_string(MARKER_KEY, "ProductProbe", &state.product_probe)?;

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

/// The generated AutoRun hook (#37). Baking the product-presence probe in lets
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

/// Read-only classification of the cmd adapter's AutoRun state.
pub fn health() -> CmdHealth {
    let raw = match reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE) {
        Ok(Some((_, value))) => value,
        _ => String::new(),
    };
    if state::autorun_references_fwdslash(&raw) {
        if let Some(path) = state::fwdslash_autorun_path(&raw) {
            if Path::new(&path).exists() {
                return CmdHealth::Healthy;
            }
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
/// transactional uninstall restores the true AutoRun (refusing if a third party
/// changed it). If our hook survives that — a refusal, or a marker-less
/// dangling hook — strip just our own segment so the console is never left
/// calling a script that no longer exists. Returns the health *after* the
/// repair attempt.
pub fn repair() -> Result<CmdHealth, AdapterError> {
    if health() == CmdHealth::Orphaned {
        if marker_state()?.is_some() {
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

/// Whether the `fsw-autorun.cmd` the live AutoRun points at exists on disk.
fn hook_target_exists() -> bool {
    let Ok(Some((_, current))) = reg::read_raw_string(COMMAND_PROCESSOR, AUTORUN_VALUE) else {
        return false;
    };
    state::fwdslash_autorun_path(&current).is_some_and(|path| Path::new(&path).exists())
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
