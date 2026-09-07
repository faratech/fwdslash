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
    /// The Store accepted the install and it is still in flight when the
    /// foreground wait ended. The Store owns it and the watchdog owns the
    /// comeback; the caller must not start another installer and must not wait
    /// any longer (issue #140).
    Queued,
}

/// The query is a network round trip to the Store service; a minute is
/// generous and still bounded.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// `E_ABORT` — the only HRESULT invented here, for this file's own timeouts.
const E_ABORT: i32 = 0x8000_4004_u32.cast_signed();

fn timed_out<T>() -> windows_core::Result<T> {
    Err(timed_out_error())
}

fn timed_out_error() -> windows_core::Error {
    windows_core::Error::from_hresult(windows_core::HRESULT(E_ABORT))
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

/// How watching one install operation ended.
enum Watched<T> {
    /// It finished; here is its result.
    Result(T),
    /// The foreground wait ended with the install still in flight.
    Queued,
    /// It could not be watched at all; the string is the `0x…` HRESULT.
    Failed(String),
}

/// Waits on the install operation under a [`super::WaitPolicy`].
///
/// The 45-minute unconditional block this replaces was issue #140's hang in
/// its other half: the settings window and the broker are children of this
/// process, so a foreground caller must hand a queued install off rather than
/// sit on it. Deployment can terminate this process at any point once the
/// Store starts, which is what the watchdog registered beforehand is for.
fn watch_install<T, P>(
    operation: &IAsyncOperationWithProgress<T, P>,
    policy: super::WaitPolicy,
) -> Watched<T>
where
    T: windows_core::RuntimeType + 'static,
    P: windows_core::RuntimeType + 'static,
{
    let started = Instant::now();
    loop {
        match operation.Status() {
            Ok(AsyncStatus::Started) => {}
            Ok(_) => {
                return match operation.GetResults() {
                    Ok(result) => Watched::Result(result),
                    Err(error) => Watched::Failed(hex(&error)),
                };
            }
            Err(error) => return Watched::Failed(hex(&error)),
        }
        // No progress signal on this route. `IAsyncOperationWithProgress`
        // reports progress through a handler rather than a pollable property,
        // and `GetResults` on an operation that is still `Started` is invalid,
        // so there is nothing honest to read here. The admission window alone
        // bounds the foreground wait, which is the guarantee that matters.
        match super::verdict(policy, started.elapsed(), false) {
            super::Verdict::Continue => {}
            super::Verdict::HandOff => return Watched::Queued,
            super::Verdict::TimedOut => return Watched::Failed(hex(&timed_out_error())),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn pending_updates() -> windows_core::Result<IVectorView<StorePackageUpdate>> {
    let operation = StoreContext::GetDefault()?.GetAppAndOptionalStorePackageUpdatesAsync()?;
    block_on(&operation, QUERY_TIMEOUT)
}

/// What the Store is offering for this package, or `None` when it answered and
/// has nothing. `Err` means the query itself failed, which is a different fact
/// and must stay distinguishable from an empty answer (issue #90).
///
/// Packaged callers only: `StoreContext` needs package identity.
pub fn check_store_offer() -> Result<Option<fsw_core::update::Offer>, String> {
    let updates = pending_updates().map_err(|error| hex(&error))?;
    let current =
        fsw_core::package_version().ok_or_else(|| "package-version-unavailable".to_string())?;
    let entries = offer_entries(&updates)?;
    // The measurement hook for issue #97: versions and counts only, on stderr,
    // and only when asked for. The broker discards stderr and the settings
    // window surfaces it on its error branch alone, so this never reaches the
    // UI on a normal check.
    if std::env::var_os("FSW_UPDATE_EXPLAIN").is_some() {
        eprintln!("{}", fsw_core::update::explain_entries(&current, &entries));
    }
    Ok(fsw_core::update::store_offer_from_entries(
        &current, &entries,
    ))
}

/// One entry per returned update that belongs to **this** package: `Some` with
/// its version when that could be read, `None` when it could not.
///
/// The distinction matters. An entry we cannot name is still a pending update,
/// whereas an entry for some other package is not ours at all — and collapsing
/// the two, as an earlier `Option<String>` return did, silently dropped the
/// first into the second.
fn offer_entries(updates: &IVectorView<StorePackageUpdate>) -> Result<Vec<Option<String>>, String> {
    let count = updates.Size().map_err(|error| hex(&error))?;
    let mut entries = Vec::new();
    for index in 0..count {
        let Ok(update) = updates.GetAt(index) else {
            continue;
        };
        if is_our_package(&update) {
            entries.push(entry_version(&update));
        }
    }
    Ok(entries)
}

/// Whether this entry is an update for **our** package rather than some other
/// one the Store returned. Kept separate from reading the version so that
/// "not ours" and "ours but unreadable" cannot collapse into one answer: the
/// second is still a pending update.
fn is_our_package(update: &StorePackageUpdate) -> bool {
    update
        .Package()
        .and_then(|package| package.Id())
        .and_then(|id| id.Name())
        .is_ok_and(|name| name == fsw_core::STORE_IDENTITY_NAME)
}

/// This entry's four-part version, or `None` when it could not be read.
fn entry_version(update: &StorePackageUpdate) -> Option<String> {
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

/// Route 2: download and deploy silently.
///
/// Deployment terminates this process when it lands, so in the successful case
/// this function does not return at all — the watchdog task registered before
/// the call is what brings the product back.
pub fn silent_download_and_install(policy: super::WaitPolicy) -> Outcome {
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
    // The same classifier the availability query uses. A named offer and an
    // unnamed one are both installable; only a list with no entry for this
    // package is "nothing to do" (issue #97).
    let Ok(entries) = offer_entries(&updates) else {
        return Outcome::NotStarted("0x80004005".to_string());
    };
    if fsw_core::update::store_offer_from_entries(&current, &entries).is_none() {
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
    let result = match watch_install(&operation, policy) {
        Watched::Result(result) => result,
        // Handed off: the Store has it. Never an error, and never a licence
        // to start a lower rung in parallel.
        Watched::Queued => return Outcome::Queued,
        Watched::Failed(detail) => {
            return Outcome::Finished {
                code: EXIT_ERROR,
                detail: Some(detail),
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
