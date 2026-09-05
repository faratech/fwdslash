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

/// The query is a network round trip to the Store service; a minute is
/// generous and still bounded.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// A silent download plus deployment. The same 45-minute ceiling route 1 uses.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// `E_ABORT` — the only HRESULT invented here, for this file's own timeouts.
const E_ABORT: i32 = 0x8000_4004_u32 as i32;

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
    let count = updates.Size().map_err(|error| hex(&error))?;
    let mut versions = Vec::new();
    for index in 0..count {
        let Ok(update) = updates.GetAt(index) else {
            continue;
        };
        // The version is the useful part: the broker balloons once per version
        // and the settings card prints it. A package whose version cannot be
        // read still counts as an update, under a name that says so.
        versions.push(update_version(&update).unwrap_or_else(|| "unknown".to_string()));
    }
    Ok(versions)
}

fn update_version(update: &StorePackageUpdate) -> Option<String> {
    let version = update.Package().ok()?.Id().ok()?.Version().ok()?;
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

/// Route 2: download and deploy silently. Returns the verb's exit code.
///
/// Deployment terminates this process when it lands, so in the successful case
/// this function does not return at all — the watchdog task registered before
/// the call is what brings the product back.
pub fn silent_download_and_install() -> Result<i32, String> {
    let context = StoreContext::GetDefault().map_err(|error| hex(&error))?;
    let updates = pending_updates().map_err(|error| hex(&error))?;
    if updates.Size().map_err(|error| hex(&error))? == 0 {
        return Ok(EXIT_NOTHING);
    }
    if !context
        .CanSilentlyDownloadStorePackageUpdates()
        .unwrap_or(false)
    {
        return Err("0x80070005".to_string());
    }
    let operation = context
        .TrySilentDownloadAndInstallStorePackageUpdatesAsync(&updates)
        .map_err(|error| hex(&error))?;
    let result = block_on_progress(&operation, INSTALL_TIMEOUT).map_err(|error| hex(&error))?;
    let state = result.OverallState().map_err(|error| hex(&error))?;
    Ok(code_for_state(state))
}

/// `StorePackageUpdateState` to exit code. The three `Error*` states that name
/// a *condition* (battery, Wi-Fi) are retry-later, exactly as route 1 treats
/// the `Paused*` family; only `OtherError` and `Canceled` are failures.
#[must_use]
pub fn code_for_state(state: StorePackageUpdateState) -> i32 {
    match state {
        StorePackageUpdateState::Completed => EXIT_OK,
        // Still in flight when the wait returned: the deployment is queued and
        // will land, which is the same "started" the caller wanted.
        StorePackageUpdateState::Pending
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
