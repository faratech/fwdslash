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

/// How a bounded wait ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Waited<T> {
    /// The work finished and produced this.
    Ready(T),
    /// The deadline passed with no answer.
    TimedOut,
    /// The work reported a failure, or could not be subscribed to at all.
    Failed(String),
}

/// Waits for a callback, bounded by a deadline.
///
/// `subscribe` is handed the sender and is expected to arrange for exactly one
/// send. Taking it as a closure is what makes the deadline testable: a fake
/// that never sends proves the timeout without a Store, which is the gap in
/// every callback-driven implementation of this that I looked at — they hand
/// control to `WinRT` correctly and then have nothing to say if the callback
/// never comes.
///
/// A send after the deadline lands on a dropped receiver and is discarded.
/// That is the normal shape of a late answer, not an error.
pub fn wait_bounded<T>(
    subscribe: impl FnOnce(std::sync::mpsc::Sender<Result<T, String>>) -> Result<(), String>,
    timeout: Duration,
) -> Waited<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Err(detail) = subscribe(sender) {
        return Waited::Failed(detail);
    }
    match receiver.recv_timeout(timeout) {
        Ok(Ok(value)) => Waited::Ready(value),
        Ok(Err(detail)) => Waited::Failed(detail),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Waited::TimedOut,
        // Every sender was dropped without sending: the callback was released
        // without ever running. Not a timeout, and emphatically not "nothing
        // to install" — that conflation is what issue #90 was about.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Waited::Failed("the update check was released without an answer".to_string())
        }
    }
}

/// Subscribes to one `WinRT` operation and reduces its result to plain Rust
/// **inside the callback**.
///
/// That reduction is mandatory, not stylistic: the callback's `Send` bound is
/// on the closure, and `IVectorView` is not `Send`, so the collection cannot
/// cross the channel even though every element type can. `reduce` therefore
/// runs on the delegate's own thread, which is safe here because the process
/// is an MTA and `StoreContext`, `StorePackageUpdate` and
/// `StorePackageUpdateResult` are all agile.
fn subscribe_operation<T, R>(
    operation: &IAsyncOperation<T>,
    reduce: impl FnOnce(windows_core::Result<T>) -> Result<R, String> + Send + 'static,
    sender: std::sync::mpsc::Sender<Result<R, String>>,
) -> Result<(), String>
where
    T: windows_core::RuntimeType + 'static,
    R: Send + 'static,
{
    // `when` fires synchronously if the operation has already completed, so
    // there is no window between creating it and subscribing in which an
    // answer could be lost.
    operation
        .when(move |result| {
            let _ = sender.send(reduce(result));
        })
        .map_err(|error| hex(&error))
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

/// The install path's wait, which cannot be callback-driven.
///
/// `TrySilentDownloadAndInstallStorePackageUpdatesAsync` takes the live
/// `IVectorView` back, and `IVectorView` is not `Send`, so the collection can
/// neither cross a channel nor be produced on a delegate's thread and used
/// here. Reducing it to plain Rust — the trick that makes the *query* path
/// callback-driven — would destroy the thing the install needs.
///
/// Polling is therefore the honest choice here rather than a leftover. It is
/// bounded, it runs in an MTA process with nothing else on the thread, and
/// the caller bounds it again from outside.
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
    let current =
        fsw_core::package_version().ok_or_else(|| "package-version-unavailable".to_string())?;
    let context = StoreContext::GetDefault().map_err(|error| hex(&error))?;
    let operation = context
        .GetAppAndOptionalStorePackageUpdatesAsync()
        .map_err(|error| hex(&error))?;

    // The whole reduction happens in the callback, because `IVectorView` is
    // not `Send` and cannot cross the channel. What comes back is one owned
    // `Vec<Option<String>>`.
    let explain = std::env::var_os("FSW_UPDATE_EXPLAIN").is_some();
    let for_callback = current.clone();
    let waited = wait_bounded(
        |sender| {
            subscribe_operation(
                &operation,
                move |result| {
                    let updates = result.map_err(|error| hex(&error))?;
                    let entries = offer_entries(&updates)?;
                    // The measurement hook for issue #97: versions and counts
                    // only, and only when asked for. The broker discards
                    // stderr and the settings window surfaces it on its error
                    // branch alone, so this never reaches the UI on a normal
                    // check.
                    if explain {
                        eprintln!(
                            "{}",
                            fsw_core::update::explain_entries(&for_callback, &entries)
                        );
                    }
                    Ok(entries)
                },
                sender,
            )
        },
        QUERY_TIMEOUT,
    );

    let entries = match waited {
        Waited::Ready(entries) => entries,
        Waited::TimedOut => {
            // The operation is still live and `WinRT` still holds the delegate.
            // Leak the proxy rather than release it: this process is about to
            // exit, and the apartment is deliberately never uninitialised (see
            // `ComScope`), so a late completion has somewhere valid to land.
            std::mem::forget(operation);
            return Err(hex(&timed_out_error()));
        }
        Waited::Failed(detail) => {
            std::mem::forget(operation);
            return Err(detail);
        }
    };

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
