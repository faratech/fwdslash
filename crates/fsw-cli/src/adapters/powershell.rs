//! The Windows PowerShell 5.1 and PowerShell 7 adapters: deploys the module
//! (`ForwardSlashWindows.psm1` + a controller copy) into a shared
//! `%LOCALAPPDATA%\ForwardSlashWindows\PowerShell\<version>` directory, adds
//! a guarded import block to the edition's `profile.ps1`, and verifies the
//! aliases load in a real child shell. Ported from the retired
//! `tools/Install-PowerShellAdapter.ps1` / `Uninstall-PowerShellAdapter.ps1`
//! with registry writes routed through `reg.exe`.

use super::{profile, reg, state, AdapterError, Edition};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

const MARKER_ROOT: &str = "Software\\ForwardSlashWindows\\PowerShellAdapter";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Installs the adapter for `edition`. `controller` is the running
/// `fwdslash.exe`.
pub fn install(edition: Edition, controller: &Path) -> Result<(), AdapterError> {
    if !controller.is_file() {
        return Err(AdapterError::new(&format!(
            "fwdslash.exe was not found: {}",
            controller.display()
        )));
    }
    let mut transaction = match begin_install(edition)? {
        Some(transaction) => transaction,
        // Already installed: the scripts reported it and exited 0.
        None => {
            println!(
                "The {} adapter is already installed.",
                edition.display_name()
            );
            return Ok(());
        }
    };
    if let Err(error) = commit_install(&mut transaction) {
        transaction.undo();
        return Err(error);
    }
    println!(
        "Forward Slash Windows installed for {}. Open a new session to use it.",
        edition.display_name()
    );
    Ok(())
}

struct InstallTransaction {
    edition: Edition,
    transaction_id: String,
    module_root: PathBuf,
    module_staging: PathBuf,
    module_deployed: bool,
    state_root: PathBuf,
    state_staging: PathBuf,
    state_deployed: bool,
    profile_path: PathBuf,
    original_present: bool,
    original_bytes: Vec<u8>,
    block_bytes: Vec<u8>,
    profile_changed: bool,
}

/// `None` = already installed (friendly no-op).
fn begin_install(edition: Edition) -> Result<Option<InstallTransaction>, AdapterError> {
    let marker_key = marker_key(edition);
    let marker_state = read_marker_state(&marker_key)?;
    match state::decide_ps_install(marker_state.is_some(), marker_state.map_or(
        state::MarkerState::Unknown,
        |text| state::classify(&text),
    )) {
        state::InstallDecision::Proceed => {}
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
    let profile_path = documents
        .join(edition.folder_name())
        .join("profile.ps1");
    let install_root = super::local_app_data()?
        .join("ForwardSlashWindows")
        .join("PowerShell");
    let module_root = install_root.join(super::PAYLOAD_VERSION);
    let transaction_id = super::new_transaction_id();

    let original_present = profile_path.is_file();
    let original_bytes = if original_present {
        std::fs::read(&profile_path)?
    } else {
        Vec::new()
    };

    Ok(Some(InstallTransaction {
        edition,
        transaction_id,
        module_root: module_root.clone(),
        module_staging: PathBuf::from(format!(
            "{}.staging-{}",
            module_root.display(),
            super::new_transaction_id()
        )),
        module_deployed: false,
        state_root: install_root.join("state").join(edition.folder_name()),
        state_staging: PathBuf::from(format!(
            "{}.staging-{}",
            install_root.join("state").join(edition.folder_name()).display(),
            super::new_transaction_id()
        )),
        state_deployed: false,
        profile_path,
        original_present,
        original_bytes,
        block_bytes: Vec::new(),
        profile_changed: false,
    }))
}

fn commit_install(transaction: &mut InstallTransaction) -> Result<(), AdapterError> {
    let edition = transaction.edition;

    // The controller to deploy and to probe is the running executable itself.
    let running = std::env::current_exe()
        .map_err(|error| AdapterError::new(&format!("could not locate fwdslash.exe ({error}).")))?;

    // Shared module directory: deploy once, skip if the other edition (or a
    // previous install) already deployed it. Real-process copies — the real
    // powershell.exe child must be able to load the module, so it cannot be
    // allowed to land in this process's virtualized view.
    if !transaction.module_root.is_dir() {
        if let Some(parent) = transaction.module_root.parent() {
            super::real_make_dir(parent)?;
        }
        super::real_make_dir(&transaction.module_staging)?;
        super::real_copy_file(
            &super::payload_source_dir("powershell")?.join("ForwardSlashWindows.psm1"),
            &transaction.module_staging,
        )?;
        super::real_copy_file(&running, &transaction.module_staging)?;
        std::fs::rename(&transaction.module_staging, &transaction.module_root)?;
        transaction.module_deployed = true;
    }

    // The *true* original is the profile with every prior fwdslash block
    // stripped: installing over a profile a previous version (or a duplicate
    // enable) already touched must not append a second block or preserve a
    // stale one, and uninstall must be able to restore the genuine pre-fwdslash
    // profile (#37). The raw bytes stay on the transaction for exact rollback.
    let true_original = profile::strip_fwdslash_blocks(&transaction.original_bytes);
    let true_original_present = !true_original.is_empty();

    // State directory with the recovery files, staged then renamed.
    if let Some(parent) = transaction.state_root.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(&transaction.state_staging)?;
    std::fs::write(
        transaction.state_staging.join("profile.original"),
        &true_original,
    )?;
    let module_path = transaction.module_root.join("ForwardSlashWindows.psm1");
    let controller_path = transaction.module_root.join("fwdslash.exe");
    let probe_path = super::product_probe_path(&running);
    let block = profile::block_text(&profile::BlockParams {
        version: super::PAYLOAD_VERSION,
        transaction_id: &transaction.transaction_id,
        module_path: &module_path.display().to_string(),
        probe_path: &probe_path.display().to_string(),
        controller_path: &controller_path.display().to_string(),
        original_non_empty: true_original_present,
    });
    let encoding = profile::detect_encoding(&true_original);
    transaction.block_bytes = profile::encode(&block, encoding);
    std::fs::write(
        transaction.state_staging.join("profile.block"),
        &transaction.block_bytes,
    )?;
    std::fs::rename(&transaction.state_staging, &transaction.state_root)?;
    transaction.state_deployed = true;

    // Marker (prepared) with the recovery locations.
    let key = marker_key(edition);
    reg::set_string(&key, "State", "prepared")?;
    reg::set_string(&key, "Version", super::PAYLOAD_VERSION)?;
    reg::set_string(&key, "TransactionId", &transaction.transaction_id)?;
    reg::set_string(&key, "ProfilePath", &transaction.profile_path.display().to_string())?;
    reg::set_string(&key, "StateDirectory", &transaction.state_root.display().to_string())?;
    reg::set_string(&key, "ProductProbe", &probe_path.display().to_string())?;
    // OriginalPresent tracks whether there is *genuine* content to restore, so
    // a profile that was purely our own block(s) is deleted on removal, not
    // left as an empty file.
    reg::set_dword(&key, "OriginalPresent", u32::from(true_original_present))?;

    // Installed profile = true original + one current guarded block.
    let mut installed_bytes = true_original;
    installed_bytes.extend_from_slice(&transaction.block_bytes);
    super::write_atomic(&transaction.profile_path, &installed_bytes)
        .map_err(|error| super::explain_file_error(&error, "The PowerShell profile update"))?;
    transaction.profile_changed = true;

    reg::set_string(&key, "State", "installed")?;
    verify_aliases(edition)?;
    Ok(())
}

impl InstallTransaction {
    /// The script's catch block, in the same order.
    fn undo(&mut self) {
        if self.profile_changed {
            if self.original_present {
                let _ = std::fs::write(&self.profile_path, &self.original_bytes);
            } else {
                let _ = std::fs::remove_file(&self.profile_path);
            }
        }
        let key = marker_key(self.edition);
        let _ = reg::delete_tree(&key);
        if self.state_deployed {
            let _ = std::fs::remove_dir_all(&self.state_root);
        }
        let _ = std::fs::remove_dir_all(&self.state_staging);
        if self.module_deployed {
            let _ = std::fs::remove_dir_all(&self.module_root);
        }
        let _ = std::fs::remove_dir_all(&self.module_staging);
    }
}

/// Removes the adapter for `edition`, restoring the guarded profile.
pub fn uninstall(edition: Edition) -> Result<(), AdapterError> {
    let key = marker_key(edition);
    let Some(values) = read_marker(&key)? else {
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
    let state_root = PathBuf::from(&values.state_directory);
    let original_file = state_root.join("profile.original");
    let block_file = state_root.join("profile.block");
    if !original_file.is_file() || !block_file.is_file() {
        return Err(AdapterError::new(
            "The recovery files are missing; refusing to modify the PowerShell profile.",
        ));
    }
    let block_bytes = std::fs::read(&block_file)?;
    reg::set_string(&key, "State", "removing")?;

    if values.profile_path.is_file() {
        let current = std::fs::read(&values.profile_path)?;
        // Fast path: excise the exact block we recorded. Belt and braces: then
        // strip every remaining fwdslash fence (an older version, a duplicate,
        // an externally edited block) so what survives is the genuine
        // pre-fwdslash profile (#37). Stripping only ever removes our own
        // fenced regions, never third-party content, so the old
        // "changed externally" refusal is no longer needed to protect it.
        let remaining = profile::remove_block(&current, &block_bytes).unwrap_or(current);
        let cleaned = profile::strip_fwdslash_blocks(&remaining);
        if profile::should_delete_profile(cleaned.len(), values.original_present) {
            std::fs::remove_file(&values.profile_path)?;
        } else {
            super::write_atomic(&values.profile_path, &cleaned)?;
        }
    }

    reg::delete_tree(&key)?;
    if state_root.exists() {
        std::fs::remove_dir_all(&state_root)?;
    }

    // The shared module directory goes away with this edition unless the
    // other edition's marker records the SAME version. Its name is the payload
    // version this install deployed, not the one this build ships: an upgrade
    // removes the directory it actually created.
    let deployed_version = marker_version(&values);
    let other_marker = marker_key(state::other_edition(edition));
    let other_version = read_marker(&other_marker)?
        .as_ref()
        .map(marker_version)
        .map(str::to_owned);
    if state::remove_shared_module(other_version.as_deref(), deployed_version) {
        let module_root = super::local_app_data()?
            .join("ForwardSlashWindows")
            .join("PowerShell")
            .join(deployed_version);
        if module_root.exists() {
            std::fs::remove_dir_all(&module_root)?;
        }
    }
    // Belt and braces: a version directory no marker names must never survive
    // an uninstall or an upgrade.
    prune_orphaned_module_dirs();
    println!(
        "Forward Slash Windows removed from {}. Already-open sessions retain loaded aliases until closed.",
        edition.display_name()
    );
    Ok(())
}

fn marker_key(edition: Edition) -> String {
    format!("{MARKER_ROOT}\\{}", edition.registry_leaf())
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

    // Versions a marker still points at. A marker that cannot be read counts
    // as absent, which is the same conservative answer uninstall uses.
    let mut referenced: Vec<String> = Vec::new();
    for edition in [Edition::WindowsPowerShell, Edition::PowerShell] {
        if let Ok(Some(values)) = read_marker(&marker_key(edition)) {
            referenced.push(marker_version(&values).to_string());
        }
    }

    let Ok(entries) = std::fs::read_dir(&install_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
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
        let _ = std::fs::remove_dir_all(&path);
    }
}

fn read_marker(key: &str) -> Result<Option<MarkerValues>, AdapterError> {
    use windows_registry::CURRENT_USER;

    // An absent marker key means "not installed" — not an error.
    let key = match CURRENT_USER.open(key) {
        Ok(opened) => opened,
        Err(_) => return Ok(None),
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

fn read_marker_state(key: &str) -> Result<Option<String>, AdapterError> {
    use windows_registry::CURRENT_USER;

    match CURRENT_USER.open(key) {
        Ok(key) => Ok(Some(key.get_string("State").unwrap_or_default())),
        Err(_) => Ok(None),
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
    let marker = read_marker(&marker_key(edition))?;
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

    let current_module = super::local_app_data()?
        .join("ForwardSlashWindows")
        .join("PowerShell")
        .join(super::PAYLOAD_VERSION)
        .join("ForwardSlashWindows.psm1");
    let current_module_present = current_module.is_file();

    let (profile_exists, bytes) = if profile_path.is_file() {
        (true, std::fs::read(&profile_path).unwrap_or_default())
    } else {
        (false, Vec::new())
    };

    let presence: Vec<profile::BlockPresence> = profile::parse_blocks(&bytes)
        .into_iter()
        .map(|block| profile::BlockPresence {
            version: block.version,
            module_present: block
                .module_path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_file()),
        })
        .collect();
    let health = profile::classify_profile(&presence, super::PAYLOAD_VERSION);

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

/// The product-presence probe recorded in `edition`'s marker, if any — one
/// input to the orphan self-clean's slow confirm.
pub fn recorded_probe(edition: Edition) -> Option<String> {
    read_marker(&marker_key(edition))
        .ok()
        .flatten()
        .map(|values| values.product_probe)
        .filter(|probe| !probe.is_empty())
}

/// Detect-and-repair for one edition (#37). Returns the health that was found
/// *before* any repair, so the caller can report what it fixed.
pub fn repair(edition: Edition, controller: &Path) -> Result<profile::ProfileHealth, AdapterError> {
    let inspection = inspect(edition)?;
    let action = profile::decide_profile_repair(
        &inspection.health,
        inspection.marker_installed,
        inspection.current_module_present,
    );
    match action {
        profile::ProfileAction::Nothing => {}
        profile::ProfileAction::RemoveBlocks => remove_blocks_from_profile(&inspection)?,
        // Both "write one current block" and "reinstall" mean the adapter
        // should be installed: the transactional uninstall+install strips the
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
    let current = std::fs::read(&inspection.profile_path)?;
    let cleaned = profile::strip_fwdslash_blocks(&current);
    if cleaned == current {
        return Ok(());
    }
    if cleaned.is_empty() {
        std::fs::remove_file(&inspection.profile_path)?;
    } else {
        super::write_atomic(&inspection.profile_path, &cleaned)?;
    }
    Ok(())
}

/// A clean-slate reinstall used by repair: tear the adapter down (best effort),
/// force the marker away so `begin_install` proceeds, then install fresh.
fn reinstall(edition: Edition, controller: &Path) -> Result<(), AdapterError> {
    let _ = uninstall(edition);
    let _ = reg::delete_tree(&marker_key(edition));
    install(edition, controller)
}

/// Spawns the edition's shell and confirms both aliases resolve to the
/// adapter function. Fifteen-second budget; kill and report on timeout.
fn verify_aliases(edition: Edition) -> Result<(), AdapterError> {
    let shell = match edition {
        Edition::PowerShell => {
            let Some(path) = search_path("pwsh.exe") else {
                return Err(AdapterError::new("pwsh.exe could not be located for verification."));
            };
            path
        }
        Edition::WindowsPowerShell => {
            let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
            Path::new(&system_root)
                .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe")
                .display()
                .to_string()
        }
    };

    let encoded = profile::base64_utf16le(profile::VERIFY_SCRIPT);
    let mut child = Command::new(&shell)
        .args(["-NoLogo", "-NonInteractive", "-EncodedCommand", &encoded])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AdapterError::new(&format!("verification shell could not be started ({error}).")))?;

    let deadline = Instant::now() + VERIFY_TIMEOUT;
    while deadline > Instant::now() {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(AdapterError::new(&format!(
                    "{} did not load the Forward Slash Windows profile adapter. The installation was rolled back.",
                    edition.display_name()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
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
