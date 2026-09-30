//! The Windows PowerShell 5.1 and PowerShell 7 adapters: deploys the module
//! (`ForwardSlashWindows.psm1` + a controller copy) into the shared,
//! **version-free** `%LOCALAPPDATA%\ForwardSlashWindows\PowerShell\payload`
//! directory, adds a guarded import block to the edition's `profile.ps1`, and
//! verifies the aliases load in a real child shell. Ported from the retired
//! `tools/Install-PowerShellAdapter.ps1` / `Uninstall-PowerShellAdapter.ps1`
//! with registry writes routed through `reg.exe`.
//!
//! The payload directory used to be named for the payload version, which put
//! the version into `$m` and `$c` and made every release rewrite a file under
//! `Documents` — where Controlled Folder Access silently refused it (#127).
//! It is now swapped in place by the same rename-aside transaction the cmd
//! adapter uses, so the profile block is byte-identical across releases and an
//! upgrade is a `%LOCALAPPDATA%` operation alone. The pre-#127
//! `PowerShell\<version>` directories stay put until an explicit
//! `fwdslash integration <id> enable` migrates the block that names them.

use super::{AdapterError, Edition, profile, reg, state, transaction::ProfileSnapshot};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER_ROOT: &str = "Software\\ForwardSlashWindows\\PowerShellAdapter";
/// The version-free payload directory (#127). Its name must never change
/// again: it is spelled into every deployed profile block.
const PAYLOAD_DIR_NAME: &str = "payload";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
/// The policy probe runs a single cmdlet in a `-NoProfile` shell, so it is
/// bounded by process start-up alone. Shorter than the verification budget on
/// purpose: it runs before anything has been written, and a hung probe must
/// fall through to the old behaviour rather than stall an install.
const POLICY_TIMEOUT: Duration = Duration::from_secs(10);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Installs the adapter for `edition`. `controller` is the running
/// `fwdslash.exe`.
pub fn install(edition: Edition, controller: &Path) -> Result<(), AdapterError> {
    recover_install_transaction(edition)?;
    if !controller.is_file() {
        return Err(AdapterError::new(&format!(
            "fwdslash.exe was not found: {}",
            controller.display()
        )));
    }
    // Preflight: a Restricted (or AllSigned) machine can never load the block
    // we are about to write, so refuse before touching the profile, the module
    // directory or the marker — there is nothing to roll back and the message
    // carries the one-line fix (#45). A shell that cannot be started at all
    // falls through to the pre-existing behaviour rather than adding a new way
    // to fail.
    if let Some(error) = execution_policy_refusal(edition) {
        return Err(error);
    }
    // A prior blocked/interrupted removal must be resumable from enable too.
    // Uninstall retains its marker and recovery files if it still cannot
    // modify the profile; never force the marker away to bypass that failure.
    recover_unmarked_state(edition)?;
    if matches!(
        read_marker_state(&marker_key(edition)).as_deref(),
        Some("prepared" | "removing")
    ) {
        uninstall(edition)?;
    }
    // Already installed: the scripts reported it and exited 0.
    let Some(mut transaction) = begin_install(edition, false)? else {
        println!(
            "The {} adapter is already installed.",
            edition.display_name()
        );
        return Ok(());
    };
    if let Err(error) = commit_install(&mut transaction) {
        if let Err(rollback) = transaction.undo() {
            return Err(AdapterError::new(&format!(
                "{error} Recovery remains pending: {rollback}"
            )));
        }
        return Err(error);
    }
    println!(
        "Forward Slash Windows installed for {}. Open a new session to use it.",
        edition.display_name()
    );
    Ok(())
}

/// `%LOCALAPPDATA%\ForwardSlashWindows\PowerShell`.
fn install_root() -> Result<PathBuf, AdapterError> {
    Ok(super::local_app_data()?
        .join("ForwardSlashWindows")
        .join("PowerShell"))
}

/// The deployed module, in the version-free payload directory.
fn deployed_module_path() -> Result<PathBuf, AdapterError> {
    Ok(install_root()?
        .join(PAYLOAD_DIR_NAME)
        .join("ForwardSlashWindows.psm1"))
}

/// The staged controller copy beside the module.
fn deployed_controller_path() -> Result<PathBuf, AdapterError> {
    Ok(install_root()?.join(PAYLOAD_DIR_NAME).join("fwdslash.exe"))
}

/// The rename-aside swap of the shared payload directory — the cmd adapter's
/// mechanism, reused rather than reinvented (#127): stage a fresh copy beside
/// the live one, rename the live one to `payload.removing-<id>`, rename the
/// staging directory into place, then drop the old one.
///
/// `undo` puts the previous directory back, so a failure anywhere after the
/// swap leaves the working payload exactly as it was.
struct PayloadSwap {
    root: PathBuf,
    staging: PathBuf,
    rollback: PathBuf,
    deployed: bool,
    renamed: bool,
}

impl PayloadSwap {
    fn new() -> Result<Self, AdapterError> {
        let root = install_root()?.join(PAYLOAD_DIR_NAME);
        let id = super::new_transaction_id();
        Ok(Self {
            staging: PathBuf::from(format!("{}.staging-{id}", root.display())),
            rollback: PathBuf::from(format!("{}.removing-{id}", root.display())),
            root,
            deployed: false,
            renamed: false,
        })
    }

    /// Deploys the payload unless the live directory already holds exactly
    /// these bytes. Skipping matters: the two editions share one directory, so
    /// enabling the second must not rename a payload the first is loading.
    fn ensure(&mut self, controller: &Path) -> Result<(), AdapterError> {
        recover_payload_swap()?;
        let module_source =
            super::payload_source_dir("powershell")?.join("ForwardSlashWindows.psm1");
        if payload_matches(&self.root, &module_source, controller) {
            return Ok(());
        }
        if let Some(parent) = self.root.parent() {
            super::real_make_dir(parent)?;
        }
        super::real_make_dir(&self.staging)?;
        super::real_copy_file(&module_source, &self.staging)?;
        super::real_copy_file(controller, &self.staging)?;
        let transaction_id = self
            .staging
            .to_string_lossy()
            .rsplit_once(".staging-")
            .map(|(_, id)| id.to_owned())
            .ok_or_else(|| AdapterError::new("invalid payload transaction path"))?;
        super::write_atomic(
            &install_root()?.join("payload.transaction"),
            transaction_id.as_bytes(),
        )?;
        if self.root.exists() {
            std::fs::rename(&self.root, &self.rollback)?;
            self.renamed = true;
        }
        std::fs::rename(&self.staging, &self.root)?;
        self.deployed = true;
        Ok(())
    }

    /// Drops the renamed-aside copy once the transaction has committed.
    fn finish(&mut self) -> Result<(), AdapterError> {
        if self.renamed && self.rollback.exists() {
            std::fs::remove_dir_all(&self.rollback)?;
            self.renamed = false;
        }
        let root = self
            .root
            .parent()
            .ok_or_else(|| AdapterError::new("invalid payload root"))?;
        if self.owns_journal(root)? {
            std::fs::remove_file(root.join("payload.transaction"))?;
        }
        Ok(())
    }

    fn owns_journal(&self, root: &Path) -> Result<bool, AdapterError> {
        let id = self
            .staging
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit_once(".staging-").map(|(_, id)| id))
            .ok_or_else(|| AdapterError::new("invalid payload transaction name"))?;
        let active = match std::fs::read_to_string(root.join("payload.transaction")) {
            Ok(active) => active,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(active.strip_prefix("committed\n").unwrap_or(&active) == id)
    }

    fn commit(&self) -> Result<(), AdapterError> {
        if !self.deployed {
            return Ok(());
        }
        if !self.owns_journal(&install_root()?)? {
            return Err(AdapterError::new(
                "The payload transaction lost its ownership record; recovery files were preserved.",
            ));
        }
        let id = self
            .staging
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit_once(".staging-").map(|(_, id)| id))
            .ok_or_else(|| AdapterError::new("invalid payload transaction name"))?;
        super::write_atomic(
            &install_root()?.join("payload.transaction"),
            format!("committed\n{id}").as_bytes(),
        )
    }

    fn undo(&mut self) -> Result<(), AdapterError> {
        if self.deployed {
            std::fs::remove_dir_all(&self.root)?;
            self.deployed = false;
        }
        if self.renamed {
            std::fs::rename(&self.rollback, &self.root)?;
            self.renamed = false;
        }
        let _ = std::fs::remove_dir_all(&self.staging);
        let root = install_root()?;
        if self.owns_journal(&root)? {
            let _ = std::fs::remove_file(root.join("payload.transaction"));
        }
        Ok(())
    }
}

/// Concrete ownership for the one shared payload rename, independent of the
/// edition marker (which may not yet exist on a first install).
pub fn active_payload_paths() -> Result<Vec<PathBuf>, AdapterError> {
    active_payload_paths_at(&install_root()?)
}

pub fn recover_all(user_initiated: bool) -> Result<(), AdapterError> {
    for edition in [Edition::WindowsPowerShell, Edition::PowerShell] {
        recover_install_transaction_policy(edition, user_initiated)?;
    }
    recover_payload_swap()
}

fn payload_swap_is_owned(root: &Path, recovery: &Path) -> Result<bool, AdapterError> {
    let expected = match std::fs::read_to_string(recovery.join("payload.transaction-id")) {
        Ok(id) => id,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let active = match std::fs::read_to_string(root.join("payload.transaction")) {
        Ok(id) => id,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(active.strip_prefix("committed\n").unwrap_or(&active) == expected)
}

fn complete_owned_payload_swap(root: &Path, recovery: &Path) -> Result<(), AdapterError> {
    if !payload_swap_is_owned(root, recovery)? {
        return Ok(());
    }
    for path in active_payload_paths_at(root)? {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
    }
    std::fs::remove_file(root.join("payload.transaction"))?;
    Ok(())
}

fn active_payload_paths_at(root: &Path) -> Result<Vec<PathBuf>, AdapterError> {
    let id = match std::fs::read_to_string(root.join("payload.transaction")) {
        Ok(id) => id,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let id = id.strip_prefix("committed\n").unwrap_or(&id);
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(AdapterError::new(
            "The shared PowerShell payload transaction is invalid; all recovery files were preserved.",
        ));
    }
    Ok(vec![
        root.join(format!("payload.staging-{id}")),
        root.join(format!("payload.removing-{id}")),
    ])
}

pub fn has_recovery_state() -> bool {
    install_root()
        .is_ok_and(|root| root.join("state").is_dir() || root.join("payload.transaction").exists())
}

fn recover_payload_swap() -> Result<(), AdapterError> {
    recover_payload_swap_at(&install_root()?)
}

fn recover_payload_swap_at(install_root: &Path) -> Result<(), AdapterError> {
    let paths = active_payload_paths_at(install_root)?;
    let [staging, rollback] = paths.as_slice() else {
        return Ok(());
    };
    let root = install_root.join(PAYLOAD_DIR_NAME);
    let committed = std::fs::read_to_string(install_root.join("payload.transaction"))?
        .starts_with("committed\n");
    if rollback.is_dir() && !committed {
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        std::fs::rename(rollback, &root)?;
    }
    if committed && rollback.exists() {
        std::fs::remove_dir_all(rollback)?;
    }
    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    std::fs::remove_file(install_root.join("payload.transaction"))?;
    Ok(())
}

/// Whether the deployed payload already is this build's: both files present and
/// byte-identical to their sources. PE alignment often gives different builds
/// the same file length, so length alone cannot establish this.
fn payload_matches(root: &Path, module_source: &Path, controller: &Path) -> bool {
    let same_bytes =
        |deployed: &Path, source: &Path| match (std::fs::read(deployed), std::fs::read(source)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        };
    root.is_dir()
        && same_bytes(&root.join("ForwardSlashWindows.psm1"), module_source)
        && same_bytes(&root.join("fwdslash.exe"), controller)
}

// These flags independently record rollback milestones in the original script.
#[allow(clippy::struct_excessive_bools)]
struct InstallTransaction {
    edition: Edition,
    transaction_id: String,
    payload: PayloadSwap,
    state_root: PathBuf,
    state_staging: PathBuf,
    state_deployed: bool,
    profile_path: PathBuf,
    original_present: bool,
    original_bytes: Vec<u8>,
    block_bytes: Vec<u8>,
    profile_changed: bool,
    original_snapshot: ProfileSnapshot,
    installed_snapshot: Option<ProfileSnapshot>,
    previous_marker: Option<MarkerValues>,
    state_rollback: PathBuf,
    state_renamed: bool,
    state_journal: PathBuf,
}

/// `None` = already installed (friendly no-op).
fn begin_install(
    edition: Edition,
    replacing: bool,
) -> Result<Option<InstallTransaction>, AdapterError> {
    let marker_key = marker_key(edition);
    let marker_state = read_marker_state(&marker_key);
    match state::decide_ps_install(
        marker_state.is_some(),
        marker_state.map_or(state::MarkerState::Unknown, |text| state::classify(&text)),
    ) {
        state::InstallDecision::Proceed => {}
        state::InstallDecision::AlreadyInstalled if replacing => {}
        state::InstallDecision::AlreadyInstalled => return Ok(None),
        state::InstallDecision::RecoverRequired => {
            return Err(AdapterError::new(&format!(
                "An incomplete {} adapter transaction exists. Run \"fwdslash integration {} disable\" to recover it.",
                edition.display_name(),
                edition.cli_id(),
            )));
        }
    }

    let documents = super::documents_dir()?;
    let profile_path = documents.join(edition.folder_name()).join("profile.ps1");
    let install_root = install_root()?;
    let transaction_id = super::new_transaction_id();

    let original_snapshot = ProfileSnapshot::read(&profile_path)?;
    let original_present = original_snapshot.bytes.is_some();
    let original_bytes = original_snapshot.bytes.clone().unwrap_or_default();

    Ok(Some(InstallTransaction {
        edition,
        transaction_id: transaction_id.clone(),
        payload: PayloadSwap::new()?,
        state_root: install_root.join("state").join(edition.folder_name()),
        state_staging: PathBuf::from(format!(
            "{}.staging-{}",
            install_root
                .join("state")
                .join(edition.folder_name())
                .display(),
            transaction_id
        )),
        state_deployed: false,
        profile_path,
        original_present,
        original_bytes,
        block_bytes: Vec::new(),
        profile_changed: false,
        original_snapshot,
        installed_snapshot: None,
        previous_marker: read_marker(&marker_key),
        state_rollback: install_root.join("state").join(format!(
            "{}.removing-{transaction_id}",
            edition.folder_name()
        )),
        state_renamed: false,
        state_journal: install_root
            .join("state")
            .join(format!("{}.transaction", edition.folder_name())),
    }))
}

fn stage_profile_recovery(
    transaction: &mut InstallTransaction,
) -> Result<(Vec<u8>, bool), AdapterError> {
    // The *true* original is the profile with every prior fwdslash block
    // stripped: installing over a profile a previous version (or a duplicate
    // enable) already touched must not append a second block or preserve a
    // stale one, and uninstall must be able to restore the genuine pre-fwdslash
    // profile (#37). The raw bytes stay on the transaction for exact rollback.
    let previous_block = transaction
        .previous_marker
        .as_ref()
        .and_then(|marker| std::fs::read(marker.state_directory.join("profile.block")).ok());
    let original_without_recorded = previous_block
        .as_deref()
        .and_then(|block| profile::remove_block(&transaction.original_bytes, block))
        .unwrap_or_else(|| transaction.original_bytes.clone());
    let true_original = profile::strip_fwdslash_blocks(&original_without_recorded);
    let true_original_present = profile::original_profile_present(
        transaction.original_present,
        &transaction.original_bytes,
        &true_original,
    ) || (transaction.original_present
        && transaction
            .previous_marker
            .as_ref()
            .is_some_and(|marker| marker.original_present));

    // State directory with the recovery files, staged then renamed.
    if let Some(parent) = transaction.state_root.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(&transaction.state_staging)?;
    let payload_id = transaction
        .payload
        .staging
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once(".staging-").map(|(_, id)| id))
        .ok_or_else(|| AdapterError::new("invalid payload staging name"))?;
    std::fs::write(
        transaction.state_staging.join("payload.transaction-id"),
        payload_id,
    )?;
    std::fs::write(
        transaction.state_staging.join("profile.original"),
        &true_original,
    )?;
    std::fs::write(
        transaction.state_staging.join("profile.before"),
        &transaction.original_bytes,
    )?;
    std::fs::write(
        transaction.state_staging.join("profile.path"),
        transaction.profile_path.to_string_lossy().as_bytes(),
    )?;
    std::fs::write(
        transaction.state_staging.join("profile.before-present"),
        if transaction.original_present {
            b"1"
        } else {
            b"0"
        },
    )?;
    std::fs::write(
        transaction.state_staging.join("profile.original-present"),
        if true_original_present { b"1" } else { b"0" },
    )?;
    let block = block_for(true_original_present)?;
    let encoding = profile::detect_encoding(&true_original);
    transaction.block_bytes = profile::encode(&block, encoding);
    std::fs::write(
        transaction.state_staging.join("profile.block"),
        &transaction.block_bytes,
    )?;
    let mut installed_bytes = true_original;
    installed_bytes.extend_from_slice(&transaction.block_bytes);
    std::fs::write(
        transaction.state_staging.join("profile.installed"),
        &installed_bytes,
    )?;
    if let Some(previous) = &transaction.previous_marker {
        std::fs::write(
            transaction.state_staging.join("marker.previous-version"),
            &previous.version,
        )?;
        std::fs::write(
            transaction.state_staging.join("marker.previous-probe"),
            &previous.product_probe,
        )?;
        std::fs::write(
            transaction
                .state_staging
                .join("marker.previous-original-present"),
            if previous.original_present {
                b"1"
            } else {
                b"0"
            },
        )?;
    }
    super::write_atomic(
        &transaction.state_journal,
        transaction.transaction_id.as_bytes(),
    )?;
    Ok((installed_bytes, true_original_present))
}

fn commit_install(transaction: &mut InstallTransaction) -> Result<(), AdapterError> {
    let edition = transaction.edition;

    // The controller to deploy and to probe is the running executable itself.
    let running = std::env::current_exe()
        .map_err(|error| AdapterError::new(&format!("could not locate fwdslash.exe ({error}).")))?;

    // Shared payload directory, swapped by rename-aside and skipped entirely
    // when it already holds these bytes. Real-process copies — the real
    // powershell.exe child must be able to load the module, so it cannot be
    // allowed to land in this process's virtualized view.
    transaction.payload.ensure(&running)?;

    let (installed_bytes, true_original_present) = stage_profile_recovery(transaction)?;
    let probe_path = super::product_probe_path(&running);
    // Marker (prepared) with the recovery locations.
    let key = marker_key(edition);
    reg::set_string(&key, "Version", super::PAYLOAD_VERSION)?;
    reg::set_string(&key, "TransactionId", &transaction.transaction_id)?;
    reg::set_string(
        &key,
        "ProfilePath",
        &transaction.profile_path.display().to_string(),
    )?;
    reg::set_string(
        &key,
        "StateDirectory",
        &transaction.state_staging.display().to_string(),
    )?;
    reg::set_string(&key, "ProductProbe", &probe_path.display().to_string())?;
    // OriginalPresent also preserves an existing empty file. A profile that
    // consisted solely of orphaned blocks has no genuine original to restore.
    reg::set_dword(&key, "OriginalPresent", u32::from(true_original_present))?;
    reg::set_string(&key, "State", "prepared")?;
    if transaction.state_root.exists() {
        reg::set_string(
            &key,
            "PreviousStateDirectory",
            &transaction.state_rollback.display().to_string(),
        )?;
        std::fs::rename(&transaction.state_root, &transaction.state_rollback)?;
        transaction.state_renamed = true;
    }
    std::fs::rename(&transaction.state_staging, &transaction.state_root)?;
    transaction.state_deployed = true;
    reg::set_string(
        &key,
        "StateDirectory",
        &transaction.state_root.display().to_string(),
    )?;

    // Installed profile = true original + one current guarded block. A write
    // that would not change a byte is skipped outright: `Documents` is a
    // Controlled Folder Access target and there is nothing to gain by touching
    // it (#127).
    if installed_bytes != transaction.original_bytes || !transaction.original_present {
        transaction.profile_changed = true;
        transaction.installed_snapshot = Some(
            transaction
                .original_snapshot
                .replace(&transaction.profile_path, Some(&installed_bytes))
                .map_err(|error| {
                    super::explain_file_error(
                        &error,
                        "The PowerShell profile update",
                        &transaction.profile_path,
                    )
                })?,
        );
    }

    reg::set_string(&key, "State", "installed")?;
    verify_aliases(edition)?;
    super::write_atomic(
        &transaction.state_journal,
        format!("committed\n{}", transaction.transaction_id).as_bytes(),
    )?;
    // The install is committed. Obsolete files may still be locked: retain
    // committed ownership for retry rather than failing or undoing the install.
    let _ = finish_install_cleanup(transaction);
    Ok(())
}

fn finish_install_cleanup(transaction: &mut InstallTransaction) -> Result<(), AdapterError> {
    let key = marker_key(transaction.edition);
    finish_install_cleanup_with(transaction, || {
        reg::delete_value(&key, "PreviousStateDirectory")
    })
}

fn finish_install_cleanup_with(
    transaction: &mut InstallTransaction,
    mut cleanup_marker: impl FnMut() -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    transaction.payload.finish()?;
    if transaction.state_renamed && transaction.state_rollback.exists() {
        std::fs::remove_dir_all(&transaction.state_rollback)?;
        transaction.state_renamed = false;
    }
    cleanup_marker()?;
    std::fs::remove_file(&transaction.state_journal)?;
    Ok(())
}

/// The guarded block this build writes, for `original_non_empty` originals.
/// Constant across releases by construction: every path in it is version-free.
fn block_for(original_non_empty: bool) -> Result<String, AdapterError> {
    let running = std::env::current_exe()
        .map_err(|error| AdapterError::new(&format!("could not locate fwdslash.exe ({error}).")))?;
    let probe_path = super::product_probe_path(&running);
    let alias_path = super::app_execution_alias().unwrap_or_default();
    Ok(profile::block_text(&profile::BlockParams {
        module_path: &deployed_module_path()?.display().to_string(),
        probe_path: &probe_path.display().to_string(),
        alias_path: &alias_path.display().to_string(),
        controller_path: &deployed_controller_path()?.display().to_string(),
        original_non_empty,
    }))
}

impl InstallTransaction {
    /// The script's catch block, in the same order.
    fn undo(&mut self) -> Result<(), AdapterError> {
        // A caught error and a later process restart use the same recovery.
        // Snapshots survive until registry restoration also lands.
        if self.state_journal.exists() {
            return recover_install_transaction(self.edition);
        }
        if self.profile_changed {
            let Some(installed) = &self.installed_snapshot else {
                return Err(AdapterError::new(
                    "The profile write did not complete; the original profile and recovery files were retained.",
                ));
            };
            installed.replace(&self.profile_path, self.original_snapshot.bytes.as_deref())?;
        }
        let key = marker_key(self.edition);
        if self.state_deployed {
            std::fs::remove_dir_all(&self.state_root)?;
        }
        if self.state_renamed {
            std::fs::rename(&self.state_rollback, &self.state_root)?;
        }
        if let Some(previous) = &self.previous_marker {
            restore_ps_marker(&key, previous)?;
        } else if read_marker_checked(&key)?.is_some() {
            reg::delete_tree(&key)?;
        }
        let _ = std::fs::remove_dir_all(&self.state_staging);
        self.payload.undo()?;
        let _ = std::fs::remove_file(&self.state_journal);
        Ok(())
    }
}

/// Removes the adapter for `edition`, restoring the guarded profile.
pub fn uninstall(edition: Edition) -> Result<(), AdapterError> {
    recover_install_transaction(edition)?;
    recover_unmarked_state(edition)?;
    let key = marker_key(edition);
    let Some(values) = read_marker_checked(&key)? else {
        println!("The {} adapter is not installed.", edition.display_name());
        return Ok(());
    };
    let marker_state = state::classify(&values.state);
    match state::decide_ps_uninstall(true, marker_state) {
        state::UninstallDecision::NotInstalled | state::UninstallDecision::Proceed => {}
        state::UninstallDecision::UnknownState => {
            return Err(AdapterError::new(&format!(
                "Unknown {} adapter transaction state '{}'.",
                edition.display_name(),
                values.state
            )));
        }
        state::UninstallDecision::AutoRunChanged => unreachable!("cmd-only refusal"),
    }

    // Recovery files are mandatory: without them the profile cannot be
    // restored exactly, so refuse rather than guess.
    let state_root = recovery_state_root(edition, &values)?;
    let original_file = state_root.join("profile.original");
    let block_file = state_root.join("profile.block");
    if !original_file.is_file() || !block_file.is_file() {
        return Err(AdapterError::new(
            "The recovery files are missing; refusing to modify the PowerShell profile.",
        ));
    }
    let block_bytes = std::fs::read(&block_file)?;
    reg::set_string(&key, "State", "removing")?;

    let profile_removal = (|| -> Result<(), AdapterError> {
        let snapshot = ProfileSnapshot::read(&values.profile_path)?;
        let Some(current) = snapshot.bytes.as_deref() else {
            return Ok(());
        };
        if marker_state == state::MarkerState::Prepared
            && state_root.join("profile.before").is_file()
        {
            let before = std::fs::read(state_root.join("profile.before"))?;
            let installed =
                std::fs::read(state_root.join("profile.installed")).unwrap_or_else(|_| {
                    let mut bytes = std::fs::read(&original_file).unwrap_or_default();
                    bytes.extend_from_slice(&block_bytes);
                    bytes
                });
            if current == before {
                return Ok(());
            }
            if current != installed {
                return Err(AdapterError::new(
                    "The profile differs from the interrupted installation. Your edits and recovery files were preserved.",
                ));
            }
            let existed = std::fs::read(state_root.join("profile.before-present"))? == b"1";
            snapshot.replace(&values.profile_path, existed.then_some(before.as_slice()))?;
            return Ok(());
        }
        // Fast path: excise the exact block we recorded. Belt and braces: then
        // strip every remaining fwdslash fence (an older version, a duplicate,
        // an externally edited block) so what survives is the genuine
        // pre-fwdslash profile (#37). Stripping only ever removes our own
        // fenced regions, never third-party content, so the old
        // "changed externally" refusal is no longer needed to protect it.
        let remaining =
            profile::remove_block(current, &block_bytes).unwrap_or_else(|| current.to_vec());
        let cleaned = profile::strip_fwdslash_blocks(&remaining);
        if profile::should_delete_profile(cleaned.len(), values.original_present) {
            snapshot.replace(&values.profile_path, None)?;
        } else if cleaned != current {
            snapshot.replace(&values.profile_path, Some(&cleaned))?;
        }
        Ok(())
    })();
    if let Err(error) = profile_removal {
        // Neither atomic replacement nor a failed deletion changed the old
        // profile. Restore its previous transaction state and retain every
        // recovery artifact. A pre-existing removing marker stays retryable.
        let error = super::explain_file_error(
            &error,
            "The PowerShell profile removal",
            &values.profile_path,
        );
        if let Err(restore_error) = reg::set_string(&key, "State", &values.state) {
            return Err(AdapterError::new(&format!(
                "{error} The previous adapter state could not be restored ({restore_error}); recovery files were retained."
            )));
        }
        return Err(error);
    }

    reg::delete_tree(&key)?;
    if state_root.exists() {
        std::fs::remove_dir_all(&state_root)?;
    }

    cleanup_removed_payload(edition, &values)
}

fn cleanup_removed_payload(edition: Edition, values: &MarkerValues) -> Result<(), AdapterError> {
    // The shared payload goes away with this edition unless the other edition
    // still has a marker. Both the version-free `payload` directory and any
    // legacy `PowerShell\<version>` directory this install deployed are
    // considered: an upgrade removes the directory it actually created.
    let root = install_root()?;
    let other_marker = marker_key(state::other_edition(edition));
    cleanup_shared_payload_at(
        &root,
        marker_version(values),
        read_marker_checked(&other_marker),
    )?;
    // Belt and braces: a version directory no marker names must never survive
    // an uninstall or an upgrade.
    prune_orphaned_module_dirs();
    println!(
        "Forward Slash Windows removed from {}. Already-open sessions retain loaded aliases until closed.",
        edition.display_name()
    );
    Ok(())
}

fn cleanup_shared_payload_at(
    root: &Path,
    deployed_version: &str,
    other: Result<Option<MarkerValues>, AdapterError>,
) -> Result<(), AdapterError> {
    let other = other?;
    let other_version = other.as_ref().map(marker_version).map(str::to_owned);
    if state::remove_shared_module(other_version.as_deref(), deployed_version) {
        let module_root = root.join(deployed_version);
        if module_root.exists() {
            std::fs::remove_dir_all(&module_root)?;
        }
    }
    if other.is_none() {
        let payload_root = root.join(PAYLOAD_DIR_NAME);
        if payload_root.exists() {
            std::fs::remove_dir_all(&payload_root)?;
        }
    }
    Ok(())
}

fn marker_key(edition: Edition) -> String {
    format!("{MARKER_ROOT}\\{}", edition.registry_leaf())
}

fn recovery_state_root(edition: Edition, values: &MarkerValues) -> Result<PathBuf, AdapterError> {
    let canonical = install_root()?.join("state").join(edition.folder_name());
    let recorded = &values.state_directory;
    let owned = recorded == &canonical
        || (recorded.parent() == canonical.parent()
            && recorded.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with(&format!("{}.staging-", edition.folder_name()))
            }));
    if !owned {
        return Err(AdapterError::new(
            "The PowerShell recovery directory is not adapter-owned; no files were changed.",
        ));
    }
    if recorded.is_dir() {
        return Ok(recorded.clone());
    }
    // A crash between publishing the state directory and updating its marker
    // leaves the recorded staging name absent and the complete final name live.
    Ok(canonical)
}

#[allow(clippy::type_complexity)]
fn state_transaction_paths(
    root: &Path,
    edition: Edition,
) -> Result<Option<(bool, PathBuf, PathBuf, PathBuf)>, AdapterError> {
    let state = root.join("state");
    let journal = state.join(format!("{}.transaction", edition.folder_name()));
    let text = match std::fs::read_to_string(&journal) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (committed, id) = text
        .strip_prefix("committed\n")
        .or_else(|| text.strip_prefix("rolled-back\n"))
        .map_or((false, text.as_str()), |id| (true, id));
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(AdapterError::new(
            "The PowerShell transaction record is invalid; recovery data was preserved.",
        ));
    }
    Ok(Some((
        committed,
        state.join(format!("{}.staging-{id}", edition.folder_name())),
        state.join(format!("{}.removing-{id}", edition.folder_name())),
        journal,
    )))
}

fn recover_install_transaction(edition: Edition) -> Result<(), AdapterError> {
    recover_install_transaction_policy(edition, true)
}

fn recover_install_transaction_policy(
    edition: Edition,
    user_initiated: bool,
) -> Result<(), AdapterError> {
    let root = install_root()?;
    let Some((_, staging, _, _)) = state_transaction_paths(&root, edition)? else {
        return Ok(());
    };
    let path = {
        let recovery = if staging.is_dir() {
            staging
        } else {
            root.join("state").join(edition.folder_name())
        };
        std::fs::read_to_string(recovery.join("profile.path"))
            .ok()
            .map(PathBuf::from)
    };
    let path = match path {
        Some(path) => path,
        None => super::documents_dir()?
            .join(edition.folder_name())
            .join("profile.ps1"),
    };
    recover_install_transaction_at_policy(&root, edition, &path, user_initiated, |previous| {
        let key = marker_key(edition);
        if let Some(previous) = previous {
            restore_ps_marker(&key, &previous)
        } else if read_marker_checked(&key)?.is_some() {
            reg::delete_tree(&key)
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn recover_install_transaction_at(
    root: &Path,
    edition: Edition,
    path: &Path,
    restore_marker: impl FnMut(Option<MarkerValues>) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    recover_install_transaction_at_policy(root, edition, path, true, restore_marker)
}

fn recover_install_transaction_at_policy(
    root: &Path,
    edition: Edition,
    path: &Path,
    user_initiated: bool,
    mut restore_marker: impl FnMut(Option<MarkerValues>) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    let Some((committed, staging, rollback, journal)) = state_transaction_paths(root, edition)?
    else {
        return Ok(());
    };
    let canonical = root.join("state").join(edition.folder_name());
    if committed {
        complete_owned_payload_swap(root, &canonical)?;
        if rollback.exists() {
            std::fs::remove_dir_all(&rollback)?;
        }
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::remove_file(journal)?;
        return Ok(());
    }
    let published = !staging.is_dir();
    let recovery = if published { &canonical } else { &staging };
    let before = std::fs::read(recovery.join("profile.before"))?;
    let before_present = std::fs::read(recovery.join("profile.before-present"))? == b"1";
    let installed = std::fs::read(recovery.join("profile.installed"))?;
    let snapshot = ProfileSnapshot::read(path)?;
    if snapshot.bytes.as_deref() != before_present.then_some(before.as_slice()) {
        if snapshot.bytes.as_deref() != Some(installed.as_slice()) {
            return Err(AdapterError::new(
                "The profile changed during an interrupted transaction. Your edits and recovery files were preserved.",
            ));
        }
        if !user_initiated {
            return Err(AdapterError::needs_confirmation(&format!(
                "The interrupted {} integration needs to restore your PowerShell profile. Run \"fwdslash integration {} enable\" to finish recovery. Your profile and recovery files were preserved.",
                edition.display_name(),
                edition.cli_id()
            )));
        }
        snapshot.replace(path, before_present.then_some(before.as_slice()))?;
    }
    let previous = match std::fs::read_to_string(recovery.join("marker.previous-version")) {
        Ok(version) => Some(MarkerValues {
            state: "installed".to_owned(),
            version,
            profile_path: path.to_path_buf(),
            state_directory: canonical.clone(),
            original_present: std::fs::read(recovery.join("marker.previous-original-present"))?
                == b"1",
            product_probe: std::fs::read_to_string(recovery.join("marker.previous-probe"))?,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if published {
        std::fs::rename(&canonical, &staging)?;
    }
    if rollback.exists() {
        std::fs::rename(&rollback, &canonical)?;
    }
    restore_marker(previous)?;
    if payload_swap_is_owned(root, &staging)? {
        recover_payload_swap_at(root)?;
    }
    let id = journal
        .file_name()
        .and_then(|_| staging.file_name())
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once(".staging-").map(|(_, id)| id))
        .ok_or_else(|| AdapterError::new("invalid recovery transaction path"))?;
    super::write_atomic(&journal, format!("rolled-back\n{id}").as_bytes())?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::remove_file(journal)?;
    Ok(())
}

/// Adopt the concrete snapshots left by older installs that published their
/// edition directory before creating the marker. Never discard that directory
/// merely because another edition already keeps the shared tree installed.
fn recover_unmarked_state(edition: Edition) -> Result<(), AdapterError> {
    let key = marker_key(edition);
    if read_marker_checked(&key)?.is_some() {
        return Ok(());
    }
    let state_root = install_root()?.join("state").join(edition.folder_name());
    if !state_root.exists() {
        return Ok(());
    }
    let original = std::fs::read(state_root.join("profile.original")).map_err(|_| AdapterError::new("An unmarked PowerShell recovery directory exists but its snapshots are incomplete; it was preserved."))?;
    let block = std::fs::read(state_root.join("profile.block"))?;
    if !state_root.join("profile.before").is_file() {
        std::fs::write(state_root.join("profile.before"), &original)?;
        // Older orphan snapshots cannot prove an empty file was absent. Keep
        // it, rather than deleting an existing empty original again (#66).
        std::fs::write(state_root.join("profile.before-present"), b"1")?;
    }
    if !state_root.join("profile.installed").is_file() {
        let mut installed = original.clone();
        installed.extend_from_slice(&block);
        std::fs::write(state_root.join("profile.installed"), installed)?;
    }
    let profile_path = super::documents_dir()?
        .join(edition.folder_name())
        .join("profile.ps1");
    reg::set_string(&key, "ProfilePath", &profile_path.display().to_string())?;
    reg::set_string(&key, "StateDirectory", &state_root.display().to_string())?;
    let original_present = std::fs::read(state_root.join("profile.original-present"))
        .map_or(true, |bytes| bytes == b"1");
    reg::set_dword(&key, "OriginalPresent", u32::from(original_present))?;
    reg::set_string(&key, "Version", super::PAYLOAD_VERSION)?;
    reg::set_string(&key, "State", "prepared")
}

/// The payload version a marker deployed. Markers written before the `Version`
/// value existed read as empty; those installs predate the shared-directory
/// scheme's only other name, so they are treated as this build's payload.
fn marker_version(values: &MarkerValues) -> &str {
    if values.version.is_empty() {
        super::PAYLOAD_VERSION
    } else {
        &values.version
    }
}

/// Deletes every `%LOCALAPPDATA%\ForwardSlashWindows\PowerShell\<version>`
/// directory that no live adapter marker names, plus `PowerShell\state` once
/// it is empty.
///
/// Upgrading both editions in turn used to strand one directory per release
/// (0.0.1, 0.0.2 and 0.0.3 were all observed side by side), and nothing ever
/// came back for them. This runs at the end of every uninstall, after every
/// successful PowerShell `enable` — including the already-at-this-version
/// no-op, which is the only thing an already-upgraded machine still reaches —
/// and from the `fwdslash uninstall` sweep.
///
/// Best effort throughout: an absent tree is not an error, every failure is
/// ignored, and nothing is printed. No path is ever logged (`PRIVACY.md`).
pub fn prune_orphaned_module_dirs() {
    let Ok(local_app_data) = super::local_app_data() else {
        return;
    };
    let install_root = local_app_data
        .join("ForwardSlashWindows")
        .join("PowerShell");
    if !install_root.is_dir() {
        return;
    }

    // Unreadable or incomplete ownership is not permission to delete. In
    // particular, interrupted upgrades still need their old recovery payload.
    let mut referenced: Vec<String> = Vec::new();
    let Ok(documents) = super::documents_dir() else {
        return;
    };
    for edition in [Edition::WindowsPowerShell, Edition::PowerShell] {
        let default_profile = documents.join(edition.folder_name()).join("profile.ps1");
        let profile_path = match windows_registry::CURRENT_USER.open(marker_key(edition)) {
            Ok(key) => {
                let Ok(marker_state) = key.get_string("State") else {
                    return;
                };
                if marker_state != "installed" {
                    return;
                }
                // Read as an integrity check only: an unreadable or empty
                // marker is incomplete ownership, and incomplete ownership is
                // not permission to delete. It is deliberately NOT pushed into
                // `referenced` — since #127 `Version` is the version of record
                // and names no directory, so treating it as a reference would
                // strand the directory matching the current version forever.
                let Ok(version) = key.get_string("Version") else {
                    return;
                };
                if version.is_empty() {
                    return;
                }
                let Ok(path) = key.get_string("ProfilePath") else {
                    return;
                };
                if path.is_empty() {
                    return;
                }
                PathBuf::from(path)
            }
            Err(error) if error.code().0.cast_unsigned() == 0x8007_0002 => default_profile,
            Err(_) => return,
        };
        // A surviving profile block can still load an older module even when
        // a previous broken repair deleted its registry marker.
        match std::fs::read(profile_path) {
            Ok(bytes) => {
                referenced.extend(
                    profile::parse_blocks(&bytes)
                        .into_iter()
                        .map(|block| block.version),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return,
        }
    }

    let Ok(entries) = std::fs::read_dir(&install_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == PAYLOAD_DIR_NAME {
            // The version-free payload is referenced by every stable block and
            // is removed by uninstall, never by this sweep (#127).
            continue;
        }
        if name == "state" {
            // The per-edition state directories are removed by uninstall; the
            // parent goes only when the last one is gone.
            if std::fs::read_dir(&path).is_ok_and(|mut dir| dir.next().is_none()) {
                let _ = std::fs::remove_dir(&path);
            }
            continue;
        }
        if referenced.iter().any(|version| version == name) {
            continue;
        }
        // This sweep owns version payloads, not arbitrary user directories or
        // staging directories that another installer may still be using.
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() != 3
            || !parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

fn read_marker(key: &str) -> Option<MarkerValues> {
    read_marker_checked(key).ok().flatten()
}

fn read_marker_checked(key: &str) -> Result<Option<MarkerValues>, AdapterError> {
    use windows_registry::CURRENT_USER;

    // An absent marker key means "not installed" — not an error.
    let key = match CURRENT_USER.open(key) {
        Ok(key) => key,
        Err(error) if error.code().0.cast_unsigned() == 0x8007_0002 => return Ok(None),
        Err(error) => return Err(super::registry_error(error)),
    };
    Ok(Some(MarkerValues {
        state: key.get_string("State").unwrap_or_default(),
        version: key.get_string("Version").unwrap_or_default(),
        profile_path: PathBuf::from(key.get_string("ProfilePath").unwrap_or_default()),
        state_directory: PathBuf::from(key.get_string("StateDirectory").unwrap_or_default()),
        original_present: key.get_u32("OriginalPresent").unwrap_or(0) != 0,
        product_probe: key.get_string("ProductProbe").unwrap_or_default(),
    }))
}

fn restore_ps_marker(key: &str, values: &MarkerValues) -> Result<(), AdapterError> {
    reg::set_string(key, "Version", &values.version)?;
    reg::set_string(
        key,
        "ProfilePath",
        &values.profile_path.display().to_string(),
    )?;
    reg::set_string(
        key,
        "StateDirectory",
        &values.state_directory.display().to_string(),
    )?;
    reg::set_dword(key, "OriginalPresent", u32::from(values.original_present))?;
    reg::set_string(key, "ProductProbe", &values.product_probe)?;
    reg::delete_value(key, "PreviousStateDirectory")?;
    reg::set_string(key, "State", &values.state)
}

#[derive(Debug, Default, Clone)]
struct MarkerValues {
    state: String,
    /// The payload version this install deployed; empty for a marker written
    /// before the value existed.
    version: String,
    profile_path: PathBuf,
    state_directory: PathBuf,
    original_present: bool,
    /// The product-presence probe recorded at install time; empty for a marker
    /// written before the value existed.
    product_probe: String,
}

fn read_marker_state(key: &str) -> Option<String> {
    use windows_registry::CURRENT_USER;

    match CURRENT_USER.open(key) {
        Ok(key) => Some(key.get_string("State").unwrap_or_default()),
        Err(_) => None,
    }
}

/// A snapshot of one edition's on-disk state for the detect-and-repair sweep
/// (#37): its profile health, whether its marker says installed, and whether
/// the *current* payload's module is present.
struct Inspection {
    health: profile::ProfileHealth,
    marker_installed: bool,
    current_module_present: bool,
    profile_path: PathBuf,
    profile_exists: bool,
}

fn inspect(edition: Edition) -> Result<Inspection, AdapterError> {
    let marker = read_marker(&marker_key(edition));
    let marker_installed = marker
        .as_ref()
        .is_some_and(|values| state::classify(&values.state) == state::MarkerState::Installed);

    let profile_path = match marker
        .as_ref()
        .filter(|values| !values.profile_path.as_os_str().is_empty())
    {
        Some(values) => values.profile_path.clone(),
        None => super::documents_dir()?
            .join(edition.folder_name())
            .join("profile.ps1"),
    };

    let current_module_present = deployed_module_path()?.is_file();

    let (profile_exists, bytes) = if profile_path.is_file() {
        (true, std::fs::read(&profile_path).unwrap_or_default())
    } else {
        (false, Vec::new())
    };

    // What this build would write, with no blank-line prefix: `ParsedBlock::text`
    // is the fence-to-fence region and carries no prefix either, so the two are
    // directly comparable. A stable-fence block that differs is content drift
    // (#134) — the fence cannot carry a version to say so (#127).
    let desired_block = block_for(false).unwrap_or_default();
    let presence: Vec<profile::BlockPresence> = profile::parse_blocks(&bytes)
        .into_iter()
        .map(|block| profile::BlockPresence {
            version: block.version,
            module_present: block
                .module_path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_file()),
            matches_current: !desired_block.is_empty() && block.text == desired_block,
        })
        .collect();
    let health = profile::classify_profile(&presence);

    Ok(Inspection {
        health,
        marker_installed,
        current_module_present,
        profile_path,
        profile_exists,
    })
}

/// The read-only profile health of `edition`, for `fwdslash doctor` /
/// `integrations`. Never writes.
pub fn profile_health(edition: Edition) -> profile::ProfileHealth {
    inspect(edition).map_or(profile::ProfileHealth::Clean, |i| i.health)
}

/// Whether `edition`'s profile directory refuses a write — the signature of a
/// Controlled Folder Access block (#37). Probes the same way `write_atomic`
/// does, by creating and removing a temp file, and only when the directory is
/// actually there. Cheap and only reached for an adapter whose marker claims
/// installed while its profile carries no block.
pub fn profile_write_blocked(edition: Edition) -> bool {
    let Ok(inspection) = inspect(edition) else {
        return false;
    };
    let Some(parent) = inspection.profile_path.parent() else {
        return false;
    };
    if !parent.is_dir() {
        return false;
    }
    let probe = parent.join(format!(".fsw-probe-{}.tmp", super::new_transaction_id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            false
        }
        Err(error) => super::looks_like_blocked_write(&error.to_string(), true),
    }
}

/// The product-presence probe recorded in `edition`'s marker, if any — one
/// input to the orphan self-clean's slow confirm.
pub fn recorded_probe(edition: Edition) -> Option<String> {
    read_marker(&marker_key(edition))
        .map(|values| values.product_probe)
        .filter(|probe| !probe.is_empty())
}

/// Detect-and-repair for one edition (#37). Returns the health that was found
/// *before* any repair, so the caller can report what it fixed.
pub fn repair(
    edition: Edition,
    controller: &Path,
    user_initiated: bool,
) -> Result<profile::ProfileHealth, AdapterError> {
    let inspection = inspect(edition)?;
    let action = profile::decide_profile_repair(
        &inspection.health,
        inspection.marker_installed,
        inspection.current_module_present,
        user_initiated,
    );
    match action {
        // `NeedsConfirmation`: a background sweep found work it may not do. The
        // existing block keeps working and the caller reports that the user has
        // to confirm (#127) — so, like `Nothing`, this changes nothing here.
        profile::ProfileAction::Nothing | profile::ProfileAction::NeedsConfirmation => {}
        profile::ProfileAction::RemoveBlocks => remove_blocks_from_profile(&inspection)?,
        // Both "write one current block" and "reinstall" mean the adapter
        // should be installed: one replacement transaction strips the
        // true original, redeploys the module when it is missing, writes
        // exactly one current guarded block and refreshes the marker/state.
        profile::ProfileAction::WriteCurrentBlock | profile::ProfileAction::Reinstall => {
            reinstall(edition, controller)?;
        }
    }
    Ok(inspection.health)
}

/// Strips every fwdslash block from a profile that should no longer carry one
/// (its marker is gone), deleting a profile that was purely our own block(s).
fn remove_blocks_from_profile(inspection: &Inspection) -> Result<(), AdapterError> {
    if !inspection.profile_exists {
        return Ok(());
    }
    let snapshot = ProfileSnapshot::read(&inspection.profile_path)?;
    let current = snapshot.bytes.as_deref().unwrap_or_default();
    let cleaned = profile::strip_fwdslash_blocks(current);
    if cleaned == current {
        return Ok(());
    }
    if cleaned.is_empty() {
        snapshot
            .replace(&inspection.profile_path, None)
            .map_err(|error| {
                super::explain_file_error(
                    &error,
                    "The PowerShell profile update",
                    &inspection.profile_path,
                )
            })?;
    } else {
        snapshot
            .replace(&inspection.profile_path, Some(&cleaned))
            .map_err(|error| {
                super::explain_file_error(
                    &error,
                    "The PowerShell profile update",
                    &inspection.profile_path,
                )
            })?;
    }
    Ok(())
}

/// What an in-place upgrade managed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// The payload is now this build's and the marker records it.
    Upgraded,
    /// The profile carries a block this build would have to rewrite — a legacy
    /// versioned block, a duplicate, an orphan — and this run is a background
    /// sweep, which may not write `Documents` (#127). Nothing was changed.
    NeedsConfirmation,
}

/// Brings an installed adapter up to this build **without touching the user's
/// profile** whenever that is possible (#127).
///
/// The block is byte-identical across releases, so the ordinary upgrade is:
/// swap the `%LOCALAPPDATA%` payload directory, refresh the recovery copy of
/// the block, and record the new `Version`. Only a profile that still carries a
/// pre-#127 versioned block (or a duplicate, or an orphan) needs a `Documents`
/// write, and that runs solely from an explicit user action.
pub fn upgrade(
    edition: Edition,
    controller: &Path,
    user_initiated: bool,
) -> Result<UpgradeOutcome, AdapterError> {
    recover_install_transaction_policy(edition, user_initiated)?;
    let key = marker_key(edition);
    let Some(values) = read_marker_checked(&key)? else {
        // No marker to upgrade from: a plain install is the whole job.
        return install(edition, controller).map(|()| UpgradeOutcome::Upgraded);
    };
    let profile_path = if values.profile_path.as_os_str().is_empty() {
        super::documents_dir()?
            .join(edition.folder_name())
            .join("profile.ps1")
    } else {
        values.profile_path.clone()
    };
    let current = std::fs::read(&profile_path).unwrap_or_default();
    let encoding = profile::detect_encoding(&current);
    let desired = profile::encode(&block_for(values.original_present)?, encoding);
    let blocks = profile::parse_blocks(&current);
    let stable = blocks.len() == 1
        && blocks.first().is_some_and(|block| !block.is_legacy())
        && profile::find_subslice(&current, &desired).is_some();
    if !stable {
        if !user_initiated {
            return Ok(UpgradeOutcome::NeedsConfirmation);
        }
        return migrate(edition, controller).map(|()| UpgradeOutcome::Upgraded);
    }

    // Payload-only upgrade. Nothing under Documents is opened, let alone
    // written, so Controlled Folder Access is never consulted.
    let mut payload = PayloadSwap::new()?;
    if let Err(error) = payload.ensure(controller) {
        if let Err(rollback) = payload.undo() {
            return Err(AdapterError::new(&format!(
                "{error} Recovery remains pending: {rollback}"
            )));
        }
        return Err(error);
    }

    // Keep the recovery copy of the block in step so uninstall still excises
    // exactly what is deployed. It lives in %LOCALAPPDATA%, not Documents.
    if values.state_directory.is_dir() {
        std::fs::write(values.state_directory.join("profile.block"), &desired)?;
    }
    let running = std::env::current_exe().unwrap_or_else(|_| controller.to_path_buf());
    // Promote before publishing the version. Restart cleanup must retain the
    // new bytes even if metadata succeeds but the process dies before finish.
    payload.commit()?;
    reg::set_string(&key, "Version", super::PAYLOAD_VERSION)?;
    reg::set_string(
        &key,
        "ProductProbe",
        &super::product_probe_path(&running).display().to_string(),
    )?;
    let _ = payload.finish();
    println!(
        "The {} adapter payload is now on {}. Your PowerShell profile was not modified.",
        edition.display_name(),
        super::PAYLOAD_VERSION
    );
    Ok(UpgradeOutcome::Upgraded)
}

/// The one remaining `Documents` write: rewrites a legacy versioned block to
/// the stable form, through a single recoverable replacement transaction so the
/// byte-exact snapshot and restore guarantees are unchanged. Only ever called
/// for an explicit user action (#127).
pub fn migrate(edition: Edition, controller: &Path) -> Result<(), AdapterError> {
    reinstall(edition, controller)
}

/// Reinstall only after successful removal. A blocked removal must retain its
/// marker and recovery files rather than orphaning the still-active profile.
fn reinstall(edition: Edition, controller: &Path) -> Result<(), AdapterError> {
    if let Some(error) = execution_policy_refusal(edition) {
        return Err(error);
    }
    if !controller.is_file() {
        return Err(AdapterError::new("fwdslash.exe was not found."));
    }
    let Some(mut transaction) = begin_install(edition, true)? else {
        return Ok(());
    };
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

/// The executable that *is* `edition`, for both the alias verification and the
/// execution-policy probe. `None` only for PowerShell 7 when `pwsh.exe` is not
/// on PATH.
fn shell_path(edition: Edition) -> Option<String> {
    match edition {
        Edition::PowerShell => search_path("pwsh.exe"),
        Edition::WindowsPowerShell => fsw_core::SystemBinary::PowerShell
            .path()
            .map(|path| path.display().to_string()),
    }
}

/// The execution policy `edition`'s own shell reports as effective, or `None`
/// when the shell is absent, refuses to start, or does not answer in time.
///
/// Deliberately spawned **without** `-ExecutionPolicy`: the whole point is to
/// observe the policy the user's own sessions get, including a process-scope
/// `PSExecutionPolicyPreference` this process inherited, which is exactly what
/// `verify_aliases` runs under. `Get-ExecutionPolicy` is a cmdlet, not a
/// script, so it answers even under Restricted.
pub fn effective_execution_policy(edition: Edition) -> Option<String> {
    use std::io::Read;

    let shell = shell_path(edition)?;
    let mut child = Command::new(&shell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-ExecutionPolicy",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + POLICY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    // The process has exited and the answer is one short word, so the pipe
    // buffer already holds all of it.
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The policy verdict for `edition`, with the string the shell reported.
/// `None` when the shell could not be asked at all.
pub fn execution_policy_verdict(edition: Edition) -> Option<(String, state::PolicyVerdict)> {
    let reported = effective_execution_policy(edition)?;
    let verdict = state::classify_execution_policy(edition, &reported);
    Some((reported, verdict))
}

/// The install-time refusal for a blocking policy, or `None` when the policy
/// allows scripts or could not be read.
fn execution_policy_refusal(edition: Edition) -> Option<AdapterError> {
    let (_, verdict) = execution_policy_verdict(edition)?;
    let block = verdict.blocked()?;
    Some(AdapterError::new(&state::policy_install_error(block)))
}

/// Spawns the edition's shell and confirms both aliases resolve to the
/// adapter function. Fifteen-second budget; kill and report on timeout.
fn verify_aliases(edition: Edition) -> Result<(), AdapterError> {
    let Some(shell) = shell_path(edition) else {
        return Err(AdapterError::new(
            "pwsh.exe could not be located for verification.",
        ));
    };

    let encoded = profile::base64_utf16le(profile::VERIFY_SCRIPT);
    let mut child = spawn_verification_shell(
        &shell,
        &["-NoLogo", "-NonInteractive", "-EncodedCommand", &encoded],
    )?;

    let deadline = Instant::now() + VERIFY_TIMEOUT;
    while deadline > Instant::now() {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                if status.code() == Some(42)
                    && let Some(block) = execution_policy_verdict(edition)
                        .as_ref()
                        .and_then(|(_, verdict)| verdict.blocked())
                {
                    return Err(AdapterError::new(&state::policy_verify_error(block)));
                }
                return Err(AdapterError::new(&format!(
                    "{} did not load the Forward Slash Windows profile adapter. The installation was rolled back.",
                    edition.display_name()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AdapterError::new(&format!(
                    "verification shell failed ({error})."
                )));
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(AdapterError::new(&format!(
        "{} profile verification timed out. The installation was rolled back.",
        edition.display_name()
    )))
}

fn spawn_verification_shell(
    shell: &str,
    arguments: &[&str],
) -> Result<std::process::Child, AdapterError> {
    Command::new(shell)
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            AdapterError::new(&format!(
                "verification shell could not be started ({error})."
            ))
        })
}

fn search_path(file: &str) -> Option<String> {
    // PATH scan for a single executable name, mirroring `executable_available`
    // but returning the resolved path.
    let path = std::env::var("PATH").unwrap_or_default();
    for directory in path.split(';') {
        if directory.is_empty() {
            continue;
        }
        let candidate = Path::new(directory).join(file);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod regression_tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fsw-adapter-regression-{}",
                super::super::new_transaction_id()
            ));
            std::fs::create_dir(&path).expect("fixture");
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn equal_length_payload_changes_are_deployed_and_identical_bytes_are_skipped() {
        let fixture = Fixture::new();
        let root = fixture.0.join("payload");
        std::fs::create_dir(&root).expect("payload");
        let module = fixture.0.join("ForwardSlashWindows.psm1");
        let controller = fixture.0.join("fwdslash.exe");
        std::fs::write(&module, b"module new").expect("module source");
        std::fs::write(&controller, b"binary new").expect("binary source");
        std::fs::write(root.join("ForwardSlashWindows.psm1"), b"module old").expect("old module");
        std::fs::write(root.join("fwdslash.exe"), b"binary old").expect("old binary");
        assert!(!payload_matches(&root, &module, &controller));
        std::fs::copy(&module, root.join("ForwardSlashWindows.psm1")).expect("module upgrade");
        assert!(!payload_matches(&root, &module, &controller));
        std::fs::copy(&controller, root.join("fwdslash.exe")).expect("binary upgrade");
        assert!(payload_matches(&root, &module, &controller));
    }

    fn pending_fixture(root: &Path, published: bool, replaced_profile: bool) -> PathBuf {
        let edition = Edition::PowerShell;
        let state = root.join("state");
        let staging = state.join(format!("{}.staging-ab-cd", edition.folder_name()));
        let canonical = state.join(edition.folder_name());
        let rollback = state.join(format!("{}.removing-ab-cd", edition.folder_name()));
        std::fs::create_dir_all(&staging).expect("state staging");
        for (name, bytes) in [
            ("profile.before", b"user original".as_slice()),
            ("profile.before-present", b"1"),
            ("profile.installed", b"user original + new block"),
            ("marker.previous-version", b"0.0.9"),
            ("marker.previous-probe", b"old probe"),
            ("marker.previous-original-present", b"1"),
        ] {
            std::fs::write(staging.join(name), bytes).expect("snapshot");
        }
        std::fs::write(
            state.join(format!("{}.transaction", edition.folder_name())),
            b"ab-cd",
        )
        .expect("journal");
        std::fs::create_dir_all(&canonical).expect("previous state");
        std::fs::write(canonical.join("profile.original"), b"old recovery")
            .expect("previous snapshot");
        if published {
            std::fs::rename(&canonical, &rollback).expect("old state rename");
            std::fs::rename(&staging, &canonical).expect("new state publish");
        }
        let profile = root.join("profile.ps1");
        std::fs::write(
            &profile,
            if replaced_profile {
                b"user original + new block".as_slice()
            } else {
                b"user original"
            },
        )
        .expect("profile");
        profile
    }

    #[test]
    fn interrupted_install_recovers_before_and_after_state_publication_and_profile_write() {
        for (published, replaced) in [(false, false), (true, false), (true, true)] {
            let fixture = Fixture::new();
            let profile = pending_fixture(&fixture.0, published, replaced);
            let mut restored = false;
            recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |previous| {
                let previous = previous.expect("previous marker");
                assert_eq!(previous.version, "0.0.9");
                assert_eq!(previous.state, "installed");
                restored = true;
                Ok(())
            })
            .expect("recover");
            assert!(restored);
            assert_eq!(std::fs::read(&profile).expect("profile"), b"user original");
            assert_eq!(
                std::fs::read(fixture.0.join("state/PowerShell/profile.original"))
                    .expect("old recovery"),
                b"old recovery"
            );
            assert!(!fixture.0.join("state/PowerShell.transaction").exists());
        }
    }

    #[test]
    fn failed_marker_restoration_keeps_snapshots_for_a_second_recovery_attempt() {
        let fixture = Fixture::new();
        let profile = pending_fixture(&fixture.0, true, true);
        assert!(
            recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |_| Err(
                AdapterError::new("simulated registry failure")
            ))
            .is_err()
        );
        assert!(fixture.0.join("state/PowerShell.transaction").exists());
        assert!(
            fixture
                .0
                .join("state/PowerShell.staging-ab-cd/profile.before")
                .is_file()
        );
        recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |_| Ok(()))
            .expect("retry");
        assert!(!fixture.0.join("state/PowerShell.transaction").exists());
        assert_eq!(std::fs::read(&profile).expect("profile"), b"user original");
    }

    #[test]
    fn failed_profile_restore_preserves_external_edits_and_all_recovery_files() {
        let fixture = Fixture::new();
        let profile = pending_fixture(&fixture.0, true, true);
        std::fs::write(&profile, b"external user edit").expect("external edit");
        let mut registry_called = false;
        assert!(
            recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |_| {
                registry_called = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!registry_called);
        assert_eq!(
            std::fs::read(&profile).expect("profile"),
            b"external user edit"
        );
        assert!(fixture.0.join("state/PowerShell/profile.before").is_file());
        assert!(
            fixture
                .0
                .join("state/PowerShell.removing-ab-cd/profile.original")
                .is_file()
        );
        assert!(fixture.0.join("state/PowerShell.transaction").is_file());
    }

    #[test]
    fn verification_output_larger_than_pipe_capacity_does_not_block() {
        let shell = fsw_core::SystemBinary::PowerShell
            .path()
            .expect("PowerShell");
        let script =
            "$s = 'x' * 1048576; [Console]::Out.Write($s); [Console]::Error.Write($s); exit 0";
        let mut child = spawn_verification_shell(
            &shell.to_string_lossy(),
            &["-NoProfile", "-NonInteractive", "-Command", script],
        )
        .expect("child");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                assert!(status.success());
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                assert!(
                    Instant::now() < deadline,
                    "output stalled the verification child"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn unknown_other_edition_ownership_preserves_shared_and_legacy_payloads() {
        let fixture = Fixture::new();
        for name in ["payload", "0.0.9"] {
            std::fs::create_dir(fixture.0.join(name)).expect("payload");
            std::fs::write(fixture.0.join(name).join("fwdslash.exe"), b"working").expect("binary");
        }
        assert!(
            cleanup_shared_payload_at(
                &fixture.0,
                "0.0.9",
                Err(AdapterError::new("simulated marker access denied"))
            )
            .is_err()
        );
        assert!(fixture.0.join("payload/fwdslash.exe").is_file());
        assert!(fixture.0.join("0.0.9/fwdslash.exe").is_file());
    }

    #[test]
    fn empty_original_existence_survives_the_production_reinstall_snapshot_path() {
        let fixture = Fixture::new();
        let profile = fixture.0.join("profile.ps1");
        let old_state = fixture.0.join("old-state");
        std::fs::create_dir(&old_state).expect("old state");
        let old_block = block_for(true).expect("old block").into_bytes();
        std::fs::write(&profile, &old_block).expect("installed profile");
        std::fs::write(old_state.join("profile.block"), &old_block).expect("old block snapshot");
        let mut transaction = InstallTransaction {
            edition: Edition::PowerShell,
            transaction_id: "ab-cd".to_owned(),
            payload: PayloadSwap {
                root: fixture.0.join("payload"),
                staging: fixture.0.join("payload.staging-ab-cd"),
                rollback: fixture.0.join("payload.removing-ab-cd"),
                deployed: false,
                renamed: false,
            },
            state_root: fixture.0.join("state"),
            state_staging: fixture.0.join("state.staging-ab-cd"),
            state_deployed: false,
            profile_path: profile.clone(),
            original_present: true,
            original_bytes: old_block,
            block_bytes: Vec::new(),
            profile_changed: false,
            original_snapshot: ProfileSnapshot::read(&profile).expect("snapshot"),
            installed_snapshot: None,
            previous_marker: Some(MarkerValues {
                original_present: true,
                state_directory: old_state,
                ..MarkerValues::default()
            }),
            state_rollback: fixture.0.join("state.removing-ab-cd"),
            state_renamed: false,
            state_journal: fixture.0.join("state.transaction"),
        };
        let (_, original_present) =
            stage_profile_recovery(&mut transaction).expect("production reinstall staging");
        assert!(original_present);
        assert!(
            std::fs::read(transaction.state_staging.join("profile.original"))
                .expect("empty original")
                .is_empty()
        );
        assert_eq!(
            std::fs::read(transaction.state_staging.join("profile.original-present"))
                .expect("presence"),
            b"1"
        );
    }

    #[test]
    fn rolled_back_cleanup_retries_after_snapshot_directory_has_been_removed() {
        let fixture = Fixture::new();
        let profile = pending_fixture(&fixture.0, true, true);
        recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |_| Ok(()))
            .expect("rollback");
        std::fs::write(
            fixture.0.join("state/PowerShell.transaction"),
            b"rolled-back\nab-cd",
        )
        .expect("cleanup phase persisted before interrupted journal deletion");
        recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |_| {
            Err(AdapterError::new("must not restore metadata again"))
        })
        .expect("cleanup-only retry");
        assert_eq!(
            std::fs::read(&profile).expect("original profile"),
            b"user original"
        );
        assert!(!fixture.0.join("state/PowerShell.transaction").exists());
    }

    #[test]
    fn committed_edition_cannot_consume_another_editions_pending_payload_journal() {
        let fixture = Fixture::new();
        let profile_b = pending_fixture(&fixture.0, true, true);
        let state_a = fixture.0.join("state/WindowsPowerShell");
        std::fs::create_dir(&state_a).expect("A committed state");
        std::fs::write(state_a.join("payload.transaction-id"), b"a-a").expect("A owner");
        std::fs::write(
            fixture.0.join("state/WindowsPowerShell.transaction"),
            b"committed\na-a",
        )
        .expect("A committed cleanup");
        std::fs::write(
            fixture.0.join("state/PowerShell/payload.transaction-id"),
            b"b-b",
        )
        .expect("B owner");
        std::fs::write(fixture.0.join("payload.transaction"), b"b-b").expect("B pending payload");
        std::fs::create_dir(fixture.0.join("payload")).expect("B new payload");
        std::fs::write(fixture.0.join("payload/fwdslash.exe"), b"B new").expect("B binary");
        std::fs::create_dir(fixture.0.join("payload.removing-b-b")).expect("B rollback payload");
        std::fs::write(
            fixture.0.join("payload.removing-b-b/fwdslash.exe"),
            b"A working",
        )
        .expect("A working binary");
        recover_install_transaction_at(
            &fixture.0,
            Edition::WindowsPowerShell,
            &fixture.0.join("A-profile"),
            |_| Ok(()),
        )
        .expect("A cleanup");
        assert!(fixture.0.join("payload.transaction").is_file());
        assert!(
            fixture
                .0
                .join("payload.removing-b-b/fwdslash.exe")
                .is_file()
        );
        recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile_b, |_| Ok(()))
            .expect("B rollback");
        assert_eq!(
            std::fs::read(fixture.0.join("payload/fwdslash.exe")).expect("restored A payload"),
            b"A working"
        );
        assert!(!fixture.0.join("payload.transaction").exists());
    }

    #[test]
    fn committed_payload_only_upgrade_keeps_new_bytes_when_version_was_published_before_cleanup() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("payload")).expect("new payload");
        std::fs::write(
            fixture.0.join("payload/fwdslash.exe"),
            b"new current-version binary",
        )
        .expect("new binary");
        std::fs::create_dir(fixture.0.join("payload.removing-ab-cd")).expect("old payload");
        std::fs::write(
            fixture.0.join("payload.removing-ab-cd/fwdslash.exe"),
            b"old binary",
        )
        .expect("old binary");
        std::fs::write(fixture.0.join("payload.transaction"), b"committed\nab-cd")
            .expect("promotion before marker version update");
        recover_payload_swap_at(&fixture.0).expect("restart cleanup");
        assert_eq!(
            std::fs::read(fixture.0.join("payload/fwdslash.exe")).expect("current binary"),
            b"new current-version binary"
        );
        assert!(!fixture.0.join("payload.removing-ab-cd").exists());
    }

    #[test]
    fn background_recovery_never_writes_the_profile_and_retains_the_pending_transaction() {
        let fixture = Fixture::new();
        let profile = pending_fixture(&fixture.0, true, true);
        let mut registry_called = false;
        let error = recover_install_transaction_at_policy(
            &fixture.0,
            Edition::PowerShell,
            &profile,
            false,
            |_| {
                registry_called = true;
                Ok(())
            },
        )
        .expect_err("confirmation required");
        assert!(error.confirmation);
        assert!(!registry_called);
        assert_eq!(
            std::fs::read(&profile).expect("unchanged profile"),
            b"user original + new block"
        );
        assert!(fixture.0.join("state/PowerShell.transaction").is_file());
        assert!(
            fixture
                .0
                .join("state/PowerShell.removing-ab-cd/profile.original")
                .is_file()
        );
    }

    #[test]
    fn locked_obsolete_backups_retain_committed_journals_until_cleanup_retry_succeeds() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let fixture = Fixture::new();
        let profile = pending_fixture(&fixture.0, true, true);
        let state_root = fixture.0.join("state/PowerShell");
        let state_rollback = fixture.0.join("state/PowerShell.removing-ab-cd");
        let state_journal = fixture.0.join("state/PowerShell.transaction");
        std::fs::write(&state_journal, b"committed\nab-cd").expect("committed state phase");
        std::fs::write(state_root.join("payload.transaction-id"), b"b-b").expect("payload owner");
        std::fs::create_dir(fixture.0.join("payload")).expect("current payload");
        std::fs::write(
            fixture.0.join("payload/fwdslash.exe"),
            b"current working binary",
        )
        .expect("current binary");
        let payload_rollback = fixture.0.join("payload.removing-b-b");
        std::fs::create_dir(&payload_rollback).expect("old payload");
        std::fs::write(payload_rollback.join("fwdslash.exe"), b"obsolete binary")
            .expect("obsolete binary");
        std::fs::write(fixture.0.join("payload.transaction"), b"committed\nb-b")
            .expect("committed payload phase");
        let mut transaction = InstallTransaction {
            edition: Edition::PowerShell,
            transaction_id: "ab-cd".to_owned(),
            payload: PayloadSwap {
                root: fixture.0.join("payload"),
                staging: fixture.0.join("payload.staging-b-b"),
                rollback: payload_rollback.clone(),
                deployed: true,
                renamed: true,
            },
            state_root,
            state_staging: fixture.0.join("state/PowerShell.staging-ab-cd"),
            state_deployed: true,
            profile_path: profile.clone(),
            original_present: true,
            original_bytes: b"user original".to_vec(),
            block_bytes: Vec::new(),
            profile_changed: true,
            original_snapshot: ProfileSnapshot::read(&profile).expect("snapshot"),
            installed_snapshot: None,
            previous_marker: None,
            state_rollback: state_rollback.clone(),
            state_renamed: true,
            state_journal: state_journal.clone(),
        };
        let payload_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(payload_rollback.join("fwdslash.exe"))
            .expect("hold obsolete payload");
        assert!(finish_install_cleanup_with(&mut transaction, || Ok(())).is_err());
        assert!(state_journal.is_file());
        assert!(fixture.0.join("payload.transaction").is_file());
        drop(payload_lock);
        let state_lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(state_rollback.join("profile.original"))
            .expect("hold obsolete state");
        assert!(finish_install_cleanup_with(&mut transaction, || Ok(())).is_err());
        assert!(state_journal.is_file());
        assert!(!fixture.0.join("payload.transaction").exists());
        drop(state_lock);
        recover_install_transaction_at(&fixture.0, Edition::PowerShell, &profile, |_| {
            Err(AdapterError::new(
                "committed cleanup must not roll back metadata",
            ))
        })
        .expect("committed cleanup retry");
        assert!(!state_journal.exists());
        assert!(!state_rollback.exists());
        assert_eq!(
            std::fs::read(fixture.0.join("payload/fwdslash.exe")).expect("working binary"),
            b"current working binary"
        );
        assert_eq!(
            std::fs::read(&profile).expect("installed profile"),
            b"user original + new block"
        );
    }
}
