//! `Windows.Services.Store` — the sanctioned half of the update path, plus the
//! network-cost probe route 3 is gated on.
//!
//! `StoreContext` is the only API Microsoft documents for an app to ask about
//! its own Store updates, and it is side-effect free: querying costs nothing
//! and shows nothing. It needs **package identity** — the spike measured
//! `0x803F6101` (the app is not published) for an identity-less caller — so
//! every entry point here is packaged-only by construction and the CLI reports
//! `disabled` rather than calling in without identity.
//!
//! Silently *installing* through it is a different matter: the Store only
//! allows that when the user's own "Update apps automatically" setting is on
//! and the network is unmetered (`CanSilentlyDownloadStorePackageUpdates`),
//! which is why this is route 2 and not route 1.

use super::{EXIT_AVAILABLE, EXIT_ERROR, EXIT_NOTHING, EXIT_OK};
use std::time::{Duration, Instant};
use windows::Services::Store::{StoreContext, StorePackageUpdate, StorePackageUpdateState};
use windows_collections::IVectorView;
use windows_future::{AsyncStatus, IAsyncOperation, IAsyncOperationWithProgress};

/// Where the silent Store route stopped.  Once the async install operation was
/// created, Windows may have accepted deployment even if a later status read
/// fails, so callers must never start another installer from that outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The Store refused before an install operation existed.  It is safe to
    /// fall through to the next route.
    NotStarted(String),
    /// An operation was created (or an up-to-date answer was returned).  This
    /// route is terminal because deployment may already be in progress.
    Finished { code: i32, detail: Option<String> },
}

/// The query is a network round trip to the Store service; a minute is
/// generous and still bounded.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// A silent download plus deployment. The same 45-minute ceiling route 1 uses.
const INSTALL_TIMEOUT: Duration = Duration::from_mins(45);
/// `E_ABORT` — the only HRESULT invented here, for this file's own timeouts.
const E_ABORT: i32 = 0x8000_4004_u32.cast_signed();

fn timed_out<T>() -> windows_core::Result<T> {
    Err(windows_core::Error::from_hresult(windows_core::HRESULT(
        E_ABORT,
    )))
}

fn hex(error: &windows_core::Error) -> String {
    format!("0x{:08X}", error.code().0.cast_unsigned())
}

fn block_on<T: windows_core::RuntimeType + 'static>(
    operation: &IAsyncOperation<T>,
    timeout: Duration,
) -> windows_core::Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if operation.Status()? != AsyncStatus::Started {
            return operation.GetResults();
        }
        if Instant::now() >= deadline {
            return timed_out();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The same wait for the progress-reporting flavor of the operation. The
/// progress itself is discarded: nothing here has a progress bar to drive.
fn block_on_progress<T, P>(
    operation: &IAsyncOperationWithProgress<T, P>,
    timeout: Duration,
) -> windows_core::Result<T>
where
    T: windows_core::RuntimeType + 'static,
    P: windows_core::RuntimeType + 'static,
{
    let deadline = Instant::now() + timeout;
    loop {
        if operation.Status()? != AsyncStatus::Started {
            return operation.GetResults();
        }
        if Instant::now() >= deadline {
            return timed_out();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn pending_updates() -> windows_core::Result<IVectorView<StorePackageUpdate>> {
    let operation = StoreContext::GetDefault()?.GetAppAndOptionalStorePackageUpdatesAsync()?;
    block_on(&operation, QUERY_TIMEOUT)
}

/// The versions the Store is offering, newest first as it returns them. Empty
/// means up to date. Packaged callers only.
pub fn check_store_updates() -> Result<Vec<String>, String> {
    let updates = pending_updates().map_err(|error| hex(&error))?;
    let current =
        fsw_core::package_version().ok_or_else(|| "package-version-unavailable".to_string())?;
    let count = updates.Size().map_err(|error| hex(&error))?;
    let mut versions = Vec::new();
    for index in 0..count {
        let Ok(update) = updates.GetAt(index) else {
            continue;
        };
        // The version is the useful part: the broker balloons once per version
        // and the settings card prints it. A package whose version cannot be
        // read still counts as an update, under a name that says so.
        if let Some(version) = update_version(&update)
            && fsw_core::update::is_newer_package_version(&current, &version)
        {
            versions.push(version);
        }
    }
    Ok(versions)
}

fn update_version(update: &StorePackageUpdate) -> Option<String> {
    let id = update.Package().ok()?.Id().ok()?;
    if id.Name().ok()? != fsw_core::STORE_IDENTITY_NAME {
        return None;
    }
    let version = id.Version().ok()?;
    Some(format!(
        "{}.{}.{}.{}",
        version.Major, version.Minor, version.Build, version.Revision
    ))
}

/// Whether the Store would download an update without asking. False for every
/// reason — the user's Store setting is off, the network is metered, there is
/// no identity — because the caller's only use for it is choosing a route.
#[must_use]
pub fn can_silently_download() -> bool {
    StoreContext::GetDefault()
        .and_then(|context| context.CanSilentlyDownloadStorePackageUpdates())
        .unwrap_or(false)
}

/// Route 2: download and deploy silently.
///
/// Deployment terminates this process when it lands, so in the successful case
/// this function does not return at all — the watchdog task registered before
/// the call is what brings the product back.
pub fn silent_download_and_install() -> Outcome {
    let context = match StoreContext::GetDefault() {
        Ok(context) => context,
        Err(error) => return Outcome::NotStarted(hex(&error)),
    };
    let updates = match pending_updates() {
        Ok(updates) => updates,
        Err(error) => return Outcome::NotStarted(hex(&error)),
    };
    let count = match updates.Size() {
        Ok(count) => count,
        Err(error) => return Outcome::NotStarted(hex(&error)),
    };
    if count == 0 {
        return Outcome::Finished {
            code: EXIT_NOTHING,
            detail: None,
        };
    }
    let Some(current) = fsw_core::package_version() else {
        return Outcome::NotStarted("package-version-unavailable".to_string());
    };
    let has_newer_app = (0..count).any(|index| {
        updates
            .GetAt(index)
            .ok()
            .and_then(|update| update_version(&update))
            .is_some_and(|version| fsw_core::update::is_newer_package_version(&current, &version))
    });
    if !has_newer_app {
        return Outcome::Finished {
            code: EXIT_NOTHING,
            detail: None,
        };
    }
    let can_silently_download = match context.CanSilentlyDownloadStorePackageUpdates() {
        Ok(value) => value,
        Err(error) => return Outcome::NotStarted(hex(&error)),
    };
    if !can_silently_download {
        return Outcome::NotStarted("0x80070005".to_string());
    }
    let operation = match context.TrySilentDownloadAndInstallStorePackageUpdatesAsync(&updates) {
        Ok(operation) => operation,
        Err(error) => return Outcome::NotStarted(hex(&error)),
    };
    let result = match block_on_progress(&operation, INSTALL_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            return Outcome::Finished {
                code: EXIT_ERROR,
                detail: Some(hex(&error)),
            };
        }
    };
    let state = match result.OverallState() {
        Ok(state) => state,
        Err(error) => {
            return Outcome::Finished {
                code: EXIT_ERROR,
                detail: Some(hex(&error)),
            };
        }
    };
    Outcome::Finished {
        code: code_for_state(state),
        detail: None,
    }
}

/// `StorePackageUpdateState` to exit code. The three `Error*` states that name
/// a *condition* (battery, Wi-Fi) are retry-later, exactly as route 1 treats
/// the `Paused*` family; only `OtherError` and `Canceled` are failures.
#[must_use]
pub fn code_for_state(state: StorePackageUpdateState) -> i32 {
    match state {
        StorePackageUpdateState::Completed
        // Still in flight when the wait returned: the deployment is queued and
        // will land, which is the same "started" the caller wanted.
        | StorePackageUpdateState::Pending
        | StorePackageUpdateState::Downloading
        | StorePackageUpdateState::Deploying => EXIT_OK,
        StorePackageUpdateState::ErrorLowBattery
        | StorePackageUpdateState::ErrorWiFiRecommended
        | StorePackageUpdateState::ErrorWiFiRequired => EXIT_AVAILABLE,
        _ => EXIT_ERROR,
    }
}

/// Whether the internet connection charges for data.
///
/// `Fixed` (a capped plan) and `Variable` (pay per byte) both count, and so
/// does every failure: no profile, no cost object, an unreadable cost. An
/// unknown network is treated as metered because the only caller is route 3,
/// and the cost of being wrong is a download the user is billed for.
#[must_use]
pub fn network_is_metered() -> bool {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};

    let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() else {
        return true;
    };
    let Ok(cost) = profile.GetConnectionCost() else {
        return true;
    };
    let Ok(kind) = cost.NetworkCostType() else {
        return true;
    };
    kind != NetworkCostType::Unrestricted
}
