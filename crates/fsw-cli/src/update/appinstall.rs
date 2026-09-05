//! Route 1: winget's own install sequence, against the vendored
//! `Windows.ApplicationModel.Store.Preview.InstallControl` bindings.
//!
//! The sequence is copied from winget's `MSStore.cpp`, which is the only
//! published description of how to drive a Store install without a UI:
//!
//! 1. `AppInstallManager::new()`;
//! 2. `GetFreeUserEntitlementAsync(productId, "", "")` — best effort. A user
//!    who already owns the app does not need it and a failure here is not a
//!    reason to stop;
//! 3. `IAppInstallManager6::StartProductInstallWithOptionsAsync(productId, "",
//!    "fwdslash", "", options)` with `AllowForcedAppRestart` on and both toast
//!    modes `NoToast` — silent, and the Store is allowed to close us;
//! 4. poll each returned `AppInstallItem`'s `GetCurrentStatus()` once a second
//!    until every one of them is out of the progressing states.
//!
//! The spike (PR 1) established that `AppInstallManager` activates *both*
//! identity-less and from inside the installed Store package, so this file is
//! used from both — phase 1a in-process, phase 1b from the staged helper. What
//! it must never do is assume: every WinRT call is `let Ok(..) = .. else`,
//! because a refusal here is a route change, not a crash.

use super::install_control::{
    AppInstallManager, AppInstallOptions, AppInstallState, AppInstallationToastNotificationMode,
};
use super::{EXIT_AVAILABLE, EXIT_ERROR, EXIT_NOTHING, EXIT_OK, HelperResult};
use std::time::{Duration, Instant};
use windows_core::HSTRING;
use windows_future::{AsyncStatus, IAsyncOperation};

/// How long to wait for the Store to finish before giving up. The Store's own
/// queue can be slow enough that anything shorter turns a working install into
/// a reported failure.
pub const INSTALL_CEILING: Duration = Duration::from_secs(45 * 60);
/// How often the item statuses are re-read. One second is what winget uses.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The entitlement and the start call are the only two awaits that must not
/// hang; neither has any business taking minutes.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// `E_ABORT` — the only HRESULT this file invents, for its own timeouts.
const E_ABORT: i32 = 0x8000_4004_u32 as i32;

/// What one attempt at route 1 did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An install was queued and polled to a conclusion. `code` is the verb's
    /// exit code and `result` is what the helper writes to its result file.
    Finished { code: i32, result: HelperResult },
    /// Nothing was ever queued — activation, the options object or the start
    /// call refused. The string is the `0x…` HRESULT. **This is a route
    /// change**, not a failure to report: phase 1a answers it by falling to
    /// the identity-less helper, and the helper by falling to route 2.
    NotStarted(String),
}

fn hex(error: &windows_core::Error) -> String {
    format!("0x{:08X}", error.code().0.cast_unsigned())
}

/// windows-future 0.3.2 has no `get()` and its `Async::join()` is private, so
/// blocking on an operation means polling `Status()` and then `GetResults()`.
/// Identical in shape to the spike probe, deliberately.
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
            return Err(windows_core::Error::from_hresult(windows_core::HRESULT(
                E_ABORT,
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Where one `AppInstallState` leaves the poll loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// Still working; read it again next tick.
    Continue,
    /// Terminal, with this exit code.
    Finished(i32),
}

/// The state-to-exit-code table, split out so it can be tested against all
/// fourteen values without a Store.
///
/// The `Paused*` family is **not** an error: the Store paused the download for
/// battery or for a metered Wi-Fi and will resume it, so it maps to "update
/// available, try again later" (exit 10) and never falls through to the error
/// arm. `ReadyToDownload` is the fourteenth value — added after the docs were
/// written, and the one a naive `match` silently drops into the error arm.
#[must_use]
pub fn code_for(state: AppInstallState) -> Poll {
    match state {
        AppInstallState::Pending
        | AppInstallState::Starting
        | AppInstallState::AcquiringLicense
        | AppInstallState::Downloading
        | AppInstallState::RestoringData
        | AppInstallState::Installing
        | AppInstallState::ReadyToDownload => Poll::Continue,
        AppInstallState::Completed => Poll::Finished(EXIT_OK),
        AppInstallState::Paused
        | AppInstallState::PausedLowBattery
        | AppInstallState::PausedWiFiRecommended
        | AppInstallState::PausedWiFiRequired => Poll::Finished(EXIT_AVAILABLE),
        // Canceled, Error, and anything a future Windows adds: treat an
        // unrecognised state as terminal rather than polling for 45 minutes.
        _ => Poll::Finished(EXIT_ERROR),
    }
}

/// What the helper records for an exit code, so a later packaged run can fold
/// it into the registry.
#[must_use]
pub fn result_for(code: i32, error: Option<&str>) -> HelperResult {
    match code {
        EXIT_OK | EXIT_NOTHING => HelperResult::Completed,
        EXIT_AVAILABLE => HelperResult::Paused,
        _ => HelperResult::Error(error.unwrap_or("0x80004005").to_string()),
    }
}

/// Runs the whole sequence for `product_id`.
pub fn apply_store_update(product_id: &str) -> Outcome {
    let product = HSTRING::from(product_id);
    let empty = HSTRING::new();

    let Ok(manager) = AppInstallManager::new() else {
        return Outcome::NotStarted("0x80040154".to_string());
    };

    // Best effort: an account that already owns the app does not need this,
    // and a store that refuses it may still install.
    if let Ok(operation) = manager.GetFreeUserEntitlementAsync(&product, &empty, &empty) {
        let _ = block_on(&operation, CALL_TIMEOUT);
    }

    let Ok(options) = AppInstallOptions::new() else {
        return Outcome::NotStarted("0x80040154".to_string());
    };
    // Silent and restartable: no toast at either end, and the Store is allowed
    // to close the running package to complete the install — which is exactly
    // why the watchdog task is registered before this function is called.
    let _ = options.SetAllowForcedAppRestart(true);
    let _ = options.SetInstallInProgressToastNotificationMode(
        AppInstallationToastNotificationMode::NoToast,
    );
    let _ = options
        .SetCompletedInstallToastNotificationMode(AppInstallationToastNotificationMode::NoToast);
    // An update, never a repair: a repair would reinstall the current version.
    let _ = options.SetRepair(false);

    let clientid = HSTRING::from("fwdslash");
    let items = match manager
        .StartProductInstallWithOptionsAsync(&product, &empty, &clientid, &empty, &options)
    {
        Ok(operation) => match block_on(&operation, CALL_TIMEOUT) {
            Ok(items) => items,
            Err(error) => return Outcome::NotStarted(hex(&error)),
        },
        Err(error) => return Outcome::NotStarted(hex(&error)),
    };

    let Ok(count) = items.Size() else {
        return Outcome::NotStarted("0x80004005".to_string());
    };
    if count == 0 {
        // The Store queued nothing: there was nothing newer to install. Not a
        // failure, and not something to report as an install either.
        return Outcome::Finished {
            code: EXIT_NOTHING,
            result: HelperResult::Completed,
        };
    }

    let deadline = Instant::now() + INSTALL_CEILING;
    loop {
        let mut worst: Option<i32> = None;
        let mut error_code: Option<String> = None;
        let mut still_working = false;
        for index in 0..count {
            let Ok(item) = items.GetAt(index) else {
                continue;
            };
            let Ok(status) = item.GetCurrentStatus() else {
                continue;
            };
            let Ok(state) = status.InstallState() else {
                continue;
            };
            match code_for(state) {
                Poll::Continue => still_working = true,
                Poll::Finished(code) => {
                    if code == EXIT_ERROR && error_code.is_none() {
                        if let Ok(hresult) = status.ErrorCode() {
                            error_code = Some(format!("0x{:08X}", hresult.0.cast_unsigned()));
                        }
                    }
                    // An error outranks a pause outranks a completion, so the
                    // reported outcome is the worst thing that happened to any
                    // item rather than whichever one finished last.
                    worst = Some(match worst {
                        Some(previous) if severity(previous) >= severity(code) => previous,
                        _ => code,
                    });
                }
            }
        }
        // A terminal failure ends the wait even while another item downloads:
        // that item is installing something we are not going to get.
        if let Some(code) = worst
            && (code != EXIT_OK || !still_working)
        {
            return Outcome::Finished {
                code,
                result: result_for(code, error_code.as_deref()),
            };
        }
        if !still_working {
            return Outcome::Finished {
                code: EXIT_OK,
                result: HelperResult::Completed,
            };
        }
        if Instant::now() >= deadline {
            return Outcome::Finished {
                code: EXIT_AVAILABLE,
                result: HelperResult::Paused,
            };
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Error beats pause beats completion. Only used to pick the worst of several
/// items' outcomes.
#[must_use]
pub fn severity(code: i32) -> u8 {
    match code {
        EXIT_OK | EXIT_NOTHING => 0,
        EXIT_AVAILABLE => 1,
        _ => 2,
    }
}
