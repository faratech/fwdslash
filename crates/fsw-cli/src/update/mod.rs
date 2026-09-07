//! `fwdslash update` — the self-update verb set, and the only place in the
//! product that installs a new version of the product.
//!
//! Both flavors land here. The **Store** flavor asks the Store directly (the
//! `AppInstallManager` sequence winget itself uses, then `StoreContext`, then
//! `winget upgrade`, then "tell the user"); the **GitHub** flavor keeps the
//! releases-API check in `fsw_core::update` and applies the bundle it already
//! downloaded. What they share is the hard part: an install that succeeds
//! *force-closes the package*, so every route that can terminate us registers
//! a one-shot watchdog task **first**, and that task is what brings the broker
//! (or the window) back once the package version has advanced.
//!
//! Three properties are load-bearing and easy to break:
//!
//! * **COM is initialised only here.** `CoInitializeEx` costs the `cd /` hot
//!   path nothing because no other verb reaches this module; keep it that way.
//! * **The helper never writes HKCU.** An identity-less copy of this exe writes
//!   to the *real* hive while the packaged app reads the *virtualized* one, so
//!   its writes would be invisible. It reports through
//!   [`fsw_core::update::UPDATE_RESULT_FILE`] instead, and the next packaged
//!   `update check`/`update status` folds that file into the registry.
//! * **Nothing here may panic.** `panic = "abort"` plus a `WinRT` surface that
//!   fails in ways no dev host reproduces means every call goes through
//!   `let Ok(..) = .. else` or `if let Ok(..)`.
//!
//! Exit codes are the contract the broker and the settings window are written
//! against: `0` up to date / install started, `10` update available or
//! deferred, `11` needs the user, `12` nothing to install, `1` error, `2`
//! usage, `20` wrong execution context.

// Generated file: rustfmt is never to touch it, because the committed bytes are
// compared against a fresh generation in CI (`tools/regen_install_control.py
// --check`) and any reformatting would look like drift.
#[rustfmt::skip]
pub mod install_control;

pub mod appinstall;
pub mod gc;
pub mod helper;
pub mod relaunch;
pub mod store;
#[cfg(windows)]
mod verification;

#[cfg(test)]
mod tests;

use relaunch::RelaunchMode;
use std::time::{SystemTime, UNIX_EPOCH};

/// Exit codes. Named, because call sites in two other binaries compare against
/// them and a bare `10` in a match arm reads as nothing at all.
pub const EXIT_OK: i32 = 0;
pub const EXIT_AVAILABLE: i32 = 10;
pub const EXIT_NEEDS_USER: i32 = 11;
pub const EXIT_NOTHING: i32 = 12;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_WRONG_CONTEXT: i32 = 20;

// ---------------------------------------------------------------------------
// Verbs and options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Check,
    Install,
    Status,
    /// Helper-only: drive `AppInstallManager` from an identity-less process.
    ApplyStore,
    /// Helper-only: register a downloaded GitHub bundle over the running one.
    ApplyBundle,
}

impl Verb {
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "check" => Some(Self::Check),
            "install" => Some(Self::Install),
            "status" => Some(Self::Status),
            "apply-store" => Some(Self::ApplyStore),
            "apply-bundle" => Some(Self::ApplyBundle),
            _ => None,
        }
    }

    /// Whether this verb may only run **without** package identity. The two
    /// apply verbs exist precisely because the packaged process cannot do their
    /// work, so running one from inside the package is a caller bug rather than
    /// a fallback: exit 20, not a silent degrade.
    #[must_use]
    pub fn is_helper_only(self) -> bool {
        matches!(self, Self::ApplyStore | Self::ApplyBundle)
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Install => "install",
            Self::Status => "status",
            Self::ApplyStore => "apply-store",
            Self::ApplyBundle => "apply-bundle",
        }
    }

    /// Whether this verb sweeps the updater's leftovers.
    ///
    /// Only `check` does. `status` reports what the registry already knows and
    /// has to stay free of side effects: a person or a script asking what the
    /// updater thinks must not thereby delete a scheduled task. Nothing is
    /// missed by that, because every caller that reaches `status` reaches
    /// `check` too — the broker's cycle checks first, and the settings window
    /// checks at launch.
    #[must_use]
    pub fn collects_garbage(self) -> bool {
        matches!(self, Self::Check)
    }
}

/// One rung of the install ladder. [`route_for`] picks one; `--route` and the
/// `UpdateRoute` registry value override it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// winget's own sequence against `AppInstallManager`.
    AppInstall,
    /// `StoreContext` silent download and install.
    Store,
    /// `winget upgrade --source msstore`.
    Winget,
    /// Tell the user and change nothing.
    Notify,
}

impl Route {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::AppInstall => "appinstall",
            Self::Store => "store",
            Self::Winget => "winget",
            Self::Notify => "notify",
        }
    }

    /// `Some(None)` is the valid spelling of "no override" (`auto`); a plain
    /// `None` is an unrecognised name.
    #[allow(clippy::option_option)]
    #[must_use]
    pub fn parse(name: &str) -> Option<Option<Self>> {
        match name {
            "auto" => Some(None),
            "appinstall" => Some(Some(Self::AppInstall)),
            "store" => Some(Some(Self::Store)),
            "winget" => Some(Some(Self::Winget)),
            "notify" => Some(Some(Self::Notify)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub verb: Verb,
    pub json: bool,
    pub force: bool,
    pub relaunch: RelaunchMode,
    pub route: Option<Route>,
    /// `apply-store`: the Store product id to install.
    pub product: Option<String>,
    /// `apply-bundle`: the `.msixbundle` to register.
    pub bundle: Option<String>,
    /// The package version the update replaces, for the watchdog's
    /// "has it advanced yet?" test.
    pub previous: Option<String>,
}

impl Options {
    #[must_use]
    fn new(verb: Verb) -> Self {
        Self {
            verb,
            json: false,
            force: false,
            // Bringing the resident broker back is the default, because that is
            // what the product *is* when no window is open.
            relaunch: RelaunchMode::Broker,
            route: None,
            product: None,
            bundle: None,
            previous: None,
        }
    }
}

/// Parses everything after `fwdslash update`. Pure, so the argv the helper is
/// launched with can be round-tripped in a test rather than trusted.
#[must_use]
pub fn parse_args(arguments: &[String]) -> Option<Options> {
    let verb = Verb::parse(arguments.first()?.as_str())?;
    let mut options = Options::new(verb);
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments.get(index)?.as_str();
        // A valued flag reads the next element. In final position, with no
        // value, that is a usage error and never a silent default.
        let value = |index: &mut usize| -> Option<String> {
            *index += 1;
            arguments.get(*index).cloned()
        };
        match argument {
            "--json" => options.json = true,
            "--force" => options.force = true,
            "--relaunch" => options.relaunch = RelaunchMode::parse(&value(&mut index)?)?,
            "--route" => options.route = Route::parse(&value(&mut index)?)?,
            "--product" => options.product = Some(value(&mut index)?),
            "--bundle" => options.bundle = Some(value(&mut index)?),
            "--previous" => options.previous = Some(value(&mut index)?),
            _ => return None,
        }
        index += 1;
    }
    Some(options)
}

// ---------------------------------------------------------------------------
// The pure state machine
// ---------------------------------------------------------------------------

/// Every rung an unforced install may try, in the order it tries them.
///
/// A rung that declines before queueing anything falls through to the next, so
/// these are genuine fallbacks for each other rather than one choice made up
/// front. Once a rung *has* queued work the walk stops, whatever the outcome:
/// deployment may already be under way and a second installer would race it.
///
/// Order is sanctioned APIs first. `StoreContext` is the documented way for an
/// app to install its own Store update, and `winget` is the same service
/// again. `AppInstallManager` is last: Microsoft documents it as gated by a
/// private capability restricted to its own apps, so it is never preferred —
/// but a user who turned automatic updates on would rather have the update
/// than a notification, so it stays as the rung before giving up (issue #98).
/// Before this it was *first* and its probe was always true, so the two
/// sanctioned rungs were never reached at all.
#[must_use]
pub fn auto_ladder(
    can_silently_download: bool,
    winget_available: bool,
    metered: bool,
) -> Vec<Route> {
    let mut ladder = Vec::new();
    if can_silently_download {
        ladder.push(Route::Store);
    }
    // winget downloads regardless of the user's data settings, so the cost
    // probe vetoes this rung and only this one.
    if winget_available && !metered {
        ladder.push(Route::Winget);
    }
    ladder.push(Route::AppInstall);
    ladder
}

/// Whether now is a good moment to start an install that will force the package
/// closed.
///
/// An explicit request always wins — the user pressed the button. Otherwise two
/// things veto: a settings window the user is looking at, and a busy Enter
/// worker (the broker sets that around its UI Automation handler, so an address
/// bar in flight is never yanked out from under).
#[must_use]
pub fn install_moment_ok(forced: bool, settings_window_open: bool, worker_busy: bool) -> bool {
    forced || (!settings_window_open && !worker_busy)
}

// ---------------------------------------------------------------------------
// How long an install may be watched
// ---------------------------------------------------------------------------

/// How long the **foreground** CLI watches a queued Store install before
/// handing it off. Long enough to catch the fast refusals and the zero-item
/// no-op; short enough that a settings window or a broker waiting on this
/// process is never held for the Store's own download schedule (issue #140).
pub const ADMISSION_WINDOW: std::time::Duration = std::time::Duration::from_mins(3);
/// How long the **background** helper waits, where nothing is blocked on it.
pub const INSTALL_CEILING: std::time::Duration = std::time::Duration::from_mins(45);

/// Who is waiting on an install poll, and therefore how long it may run.
///
/// This is the whole of issue #140's fix, and it belongs to every route that
/// can block: the packaged CLI is a child of the settings window or the broker,
/// so it must never poll a queued item to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPolicy {
    /// The packaged CLI, with a caller blocked on its exit code.
    Foreground { admission: std::time::Duration },
    /// The identity-less helper, from the scheduled task, with nobody waiting.
    Background { ceiling: std::time::Duration },
}

/// What a poll loop does next while work is still in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Read the status again next tick.
    Continue,
    /// Stop waiting and report the install as queued: the Store owns it now.
    HandOff,
    /// Stop waiting and report a pause: the ceiling passed with no conclusion.
    TimedOut,
}

/// The wait rule, pure so both policies are testable without a Store.
///
/// `progressed` is whether the install has ever shown the Store actually
/// working rather than holding it in a queue. A foreground caller leaves as
/// soon as that is true, because from that moment the Store may force-close
/// the package and the caller wants to close its own window first.
#[must_use]
pub fn verdict(policy: WaitPolicy, elapsed: std::time::Duration, progressed: bool) -> Verdict {
    match policy {
        WaitPolicy::Foreground { admission } => {
            if progressed || elapsed >= admission {
                Verdict::HandOff
            } else {
                Verdict::Continue
            }
        }
        WaitPolicy::Background { ceiling } => {
            if elapsed >= ceiling {
                Verdict::TimedOut
            } else {
                Verdict::Continue
            }
        }
    }
}

/// What `install` may act on.
///
/// The third case is the point of issue #90: a failed Store query and a Store
/// that answered "nothing" are different facts, and only the second licenses
/// "nothing to install".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// A successful query, and nothing pending.
    Nothing,
    /// The query could not run. Whether an update exists is unknown.
    Unknown,
    /// Something is pending, named or not.
    Offer(fsw_core::update::Offer),
}

impl Availability {
    /// The label for the `available` JSON field: a version when there is one,
    /// `None` for an unnamed offer and for both non-offers.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Offer(offer) => offer.label().map(str::to_owned),
            Self::Nothing | Self::Unknown => None,
        }
    }
}

/// What `install` should answer, before any route is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAnswer {
    /// Nothing to install: state `upToDate`, exit 12.
    Nothing,
    /// The Store could not be asked, so no installer may start: state
    /// `needsUser`, exit 11.
    Unknown,
    /// An unnamed offer inside its retry backoff: state `deferred`, exit 10.
    BackedOff,
    /// Not now, but there is something: state `deferred`, exit 10.
    Defer,
    /// Run the ladder.
    Proceed,
}

/// The whole of `install`'s gate, as one pure table.
///
/// Order matters and each step fixed a shipped bug. Availability outranks the
/// moment (exit 10 promises something to come back for), the moment outranks
/// the route (a forced route says *how*, never *whether*), and an unnamed offer
/// outranks neither but is bounded: it may be a same-version repair offer that
/// will never advance the version, so it gets one attempt per backoff rather
/// than one per cycle.
#[must_use]
pub fn install_answer(
    availability: &Availability,
    unnamed_actionable: bool,
    moment_ok: bool,
) -> InstallAnswer {
    match availability {
        Availability::Nothing => InstallAnswer::Nothing,
        Availability::Unknown => InstallAnswer::Unknown,
        Availability::Offer(offer) => {
            if matches!(offer, fsw_core::update::Offer::Unnamed) && !unnamed_actionable {
                return InstallAnswer::BackedOff;
            }
            if moment_ok {
                InstallAnswer::Proceed
            } else {
                InstallAnswer::Defer
            }
        }
    }
}

/// What `install` decides before any route runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precheck {
    /// Nothing to install: exit 12.
    Nothing,
    /// Something to install, but not now: exit 10.
    Defer,
    /// Run the ladder.
    Proceed,
}

/// The order the two gates have to be asked in.
///
/// **Availability outranks the moment**, and both outrank `--route` — which is
/// why the route is not an argument here at all. A forced route says *how* to
/// install, never *whether* there is anything to; and exit 10 means "there is
/// an update, come back later", so answering it with nothing available would
/// tell the broker to keep retrying an install that can never happen. That was
/// the shipped bug: an open settings window turned "up to date" into
/// "deferred, exit 10".
#[must_use]
pub fn install_precheck(update_available: bool, moment_ok: bool) -> Precheck {
    if !update_available {
        return Precheck::Nothing;
    }
    if moment_ok {
        Precheck::Proceed
    } else {
        Precheck::Defer
    }
}

/// The `state` string that goes with an exit code. Split out from
/// [`report_for_code`] so the mapping is testable on its own.
#[must_use]
pub fn state_for_code(code: i32) -> &'static str {
    match code {
        EXIT_OK => "installed",
        EXIT_AVAILABLE => "deferred",
        EXIT_NEEDS_USER => "needsUser",
        EXIT_NOTHING => "upToDate",
        _ => "error",
    }
}

// ---------------------------------------------------------------------------
// The helper's result file
// ---------------------------------------------------------------------------

/// What the identity-less helper reported through
/// [`fsw_core::update::UPDATE_RESULT_FILE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperResult {
    /// The Store finished the install.
    Completed,
    /// The Store paused it (battery, metered Wi-Fi, the user); retry later.
    Paused,
    /// It failed; the string is the `0x…` HRESULT the Store reported.
    Error(String),
}

/// Parses the one line the helper writes. Anything else is `None`: a truncated
/// or foreign file must not be mistaken for a verdict.
#[must_use]
pub fn parse_helper_result(text: &str) -> Option<HelperResult> {
    const ERROR: &str = "error:";
    let text = text.trim();
    if text.eq_ignore_ascii_case("completed") {
        return Some(HelperResult::Completed);
    }
    if text.eq_ignore_ascii_case("paused") {
        return Some(HelperResult::Paused);
    }
    if !text
        .get(..ERROR.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ERROR))
    {
        return None;
    }
    let code = text.get(ERROR.len()..)?.trim();
    if code.is_empty() {
        return None;
    }
    Some(HelperResult::Error(code.to_string()))
}

/// What folding a helper result into the registry does to the cached
/// `AvailableUpdate` notice: only a completed install proves the notice stale.
/// A pause or an error leaves it standing, so the next check still offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    ClearAvailable,
    KeepAvailable,
}

#[must_use]
pub fn fold_helper_result(result: &HelperResult) -> Fold {
    match result {
        HelperResult::Completed => Fold::ClearAvailable,
        HelperResult::Paused | HelperResult::Error(_) => Fold::KeepAvailable,
    }
}

/// The sentence a folded result is worth reporting as, if any. Pure half of
/// [`fold_result_file`].
#[must_use]
pub fn helper_result_detail(result: &HelperResult) -> Option<String> {
    match result {
        HelperResult::Completed => None,
        HelperResult::Paused => Some("The Store paused the last install.".to_string()),
        HelperResult::Error(code) => Some(format!("The last install failed ({code}).")),
    }
}

/// Reads, applies and **consumes** the helper's result file. Consumed rather
/// than kept, so one helper run is folded exactly once and a stale verdict can
/// never outlive the install it describes.
fn fold_result_file() -> Option<String> {
    let path =
        fsw_core::update::update_directory_path()?.join(fsw_core::update::UPDATE_RESULT_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let result = parse_helper_result(&text)?;
    if fold_helper_result(&result) == Fold::ClearAvailable {
        let _ = fsw_core::update::clear_cached_update_tag();
    }
    helper_result_detail(&result)
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// The one hand-rolled JSON line every `--json` verb prints. Field order is
/// part of the contract (goldens in `tests.rs`); there is deliberately no serde
/// anywhere in this workspace.
#[derive(Debug, Default, Clone)]
pub struct UpdateJson<'a> {
    pub flavor: &'a str,
    pub state: &'a str,
    pub available: Option<&'a str>,
    pub auto_update: bool,
    pub last_check: Option<u64>,
    pub route: Option<&'a str>,
    pub action: Option<&'a str>,
    pub detail: Option<&'a str>,
}

#[must_use]
pub fn render_json(fields: &UpdateJson) -> String {
    let string = |value: Option<&str>| -> String {
        value.map_or_else(
            || "null".to_string(),
            |text| format!("\"{}\"", crate::json_escape(text)),
        )
    };
    format!(
        "{{\"flavor\":\"{}\",\"state\":\"{}\",\"available\":{},\"autoUpdate\":{},\"lastUpdateCheck\":{},\"route\":{},\"action\":{},\"detail\":{}}}",
        crate::json_escape(fields.flavor),
        crate::json_escape(fields.state),
        string(fields.available),
        fields.auto_update,
        fields
            .last_check
            .map_or_else(|| "null".to_string(), |value| value.to_string()),
        string(fields.route),
        string(fields.action),
        string(fields.detail),
    )
}

/// `store` / `github` / `unpackaged` — the discriminator every caller keys on.
#[must_use]
pub fn flavor_name() -> &'static str {
    if !fsw_core::has_package_identity() {
        "unpackaged"
    } else if fsw_core::is_store_flavor() {
        "store"
    } else {
        "github"
    }
}

/// One verb's answer: the JSON line or a human sentence, plus the exit code.
struct Report {
    state: &'static str,
    available: Option<String>,
    route: Option<&'static str>,
    action: Option<&'static str>,
    detail: Option<String>,
    code: i32,
}

impl Report {
    fn new(state: &'static str, code: i32) -> Self {
        Self {
            state,
            available: None,
            route: None,
            action: None,
            detail: None,
            code,
        }
    }

    fn route(mut self, route: Route) -> Self {
        self.route = Some(route.name());
        self
    }

    fn action(mut self, action: &'static str) -> Self {
        self.action = Some(action);
        self
    }

    fn detail(mut self, detail: String) -> Self {
        self.detail = Some(detail);
        self
    }

    fn available(mut self, available: Option<String>) -> Self {
        self.available = available;
        self
    }

    /// `folded` is whatever the helper's result file had to say; a route's own
    /// detail outranks it.
    fn emit(self, options: &Options, folded: Option<String>) -> i32 {
        let detail = self.detail.or(folded);
        if options.json {
            println!(
                "{}",
                render_json(&UpdateJson {
                    flavor: flavor_name(),
                    state: self.state,
                    available: self.available.as_deref(),
                    auto_update: fsw_core::update::read_auto_update_enabled(),
                    last_check: fsw_core::update::last_update_check(),
                    route: self.route,
                    action: self.action,
                    detail: detail.as_deref(),
                })
            );
        } else {
            println!("state: {}", self.state);
            if let Some(available) = &self.available {
                println!("available: {available}");
            }
            if let Some(route) = self.route {
                println!("route: {route}");
            }
            if let Some(detail) = &detail {
                println!("{detail}");
            }
        }
        self.code
    }
}

// ---------------------------------------------------------------------------
// COM
// ---------------------------------------------------------------------------

/// MTA for the duration of one update verb, so a wait on a `WinRT` operation
/// needs no message pump. Entered lazily, because `fwdslash` is a short-lived
/// CLI whose other verbs — the ones the shell adapters run on every `cd` and
/// `dir` — must not pay for COM at all.
///
/// Nesting is safe and intended: a second `CoInitializeEx` on an
/// already-multithreaded apartment returns `S_FALSE` and only bumps the
/// reference count.
///
/// **It deliberately never calls `CoUninitialize`.** An update verb registers
/// completion delegates that `WinRT` holds a strong reference to, and it can
/// return before one of them fires — on a timeout, that is the expected path.
/// Uninitialising the apartment underneath a live delegate, or releasing a
/// proxy afterwards, is a use-after-uninitialise, and it is reachable exactly
/// when things are already going wrong. The process exits within moments of
/// the verb finishing and Windows tears the apartment down then, so the only
/// thing the missing call costs is tidiness in a process that is about to
/// disappear. That trade is why this is a marker rather than a guard.
struct ComScope;

impl ComScope {
    fn new() -> Self {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

        // SAFETY: no preconditions. A failure needs no handling here: every
        // `WinRT` call below reports its own HRESULT, and the routes already
        // treat an activation failure as a reason to try the next rung rather
        // than as a crash.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self
    }
}

// No `Drop`. See the type's documentation: uninitialising the apartment while
// a completion delegate is still registered would be a use-after-uninitialise,
// and the process is about to exit anyway.

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// The `UpdateRoute` escape hatch: a `REG_SZ` under the settings key that pins
/// one rung of the ladder without a rebuild, if the Store ever objects to
/// route 1.
///
/// A **read**, which is why it may use `windows_registry` directly: the merged
/// view a packaged process gets is always correct. Every settings *write* in
/// this module goes through `fsw_core::update`, which routes to
/// `fsw_core::settings_write` so a packaged write reaches the real hive too
/// (issue #52). Nothing here may call `set_*`/`remove_value`.
fn route_override() -> Option<Route> {
    let key = windows_registry::CURRENT_USER
        .open(fsw_core::FSW_SETTINGS_KEY)
        .ok()?;
    let value = key.get_string(fsw_core::update::UPDATE_ROUTE_VALUE).ok()?;
    Route::parse(value.trim()).flatten()
}

/// The package version an update would replace — what the watchdog compares
/// against. Four-part for a packaged build; the crate version is a defensive
/// fallback that only an unpackaged run can reach.
fn previous_version() -> String {
    fsw_core::package_version().unwrap_or_else(|| fsw_core::FSW_VERSION.to_string())
}

/// Dispatch for `fwdslash update …`; `arguments` is everything after `update`.
pub fn run(arguments: &[String]) -> i32 {
    let Some(options) = parse_args(arguments) else {
        eprintln!(
            "usage: fwdslash update check|install|status [--json] [--force] \
             [--relaunch app|broker|none] [--route <name>]"
        );
        return EXIT_USAGE;
    };
    // Context guard first, for every verb: the two apply verbs exist only
    // because the packaged process cannot do their work.
    if options.verb.is_helper_only() && !helper::helper_context_ok() {
        eprintln!(
            "fwdslash update {} runs only from the update helper.",
            options.verb.name()
        );
        return EXIT_WRONG_CONTEXT;
    }
    match options.verb {
        Verb::Check => cmd_check(&options),
        Verb::Install => cmd_install(&options),
        Verb::Status => cmd_status(&options),
        Verb::ApplyStore => cmd_apply_store(&options),
        Verb::ApplyBundle => cmd_apply_bundle(&options),
    }
}

/// Everything the registry already knows, and no network at all.
fn cmd_status(options: &Options) -> i32 {
    // Mirror cmd_check's order: the one-shot helper result must only ever be
    // consumed by a packaged process. An unpackaged run that folds first
    // deletes the verdict and half-applies it — `clear_cached_update_tag`
    // writes the real hive only, leaving the package hive shadowing the
    // merged view.
    if !fsw_core::has_package_identity() {
        return Report::new("disabled", EXIT_OK).emit(options, None);
    }
    let folded = fold_result_file();
    collect_for(options.verb);
    // Through the same filter as everything else: a label that is no longer
    // newer than what runs is not an offer.
    let offer = cached_offer();
    let state = if offer.is_some() {
        "available"
    } else {
        "upToDate"
    };
    Report::new(state, EXIT_OK)
        .available(offer.and_then(|offer| offer.label().map(str::to_owned)))
        .emit(options, folded)
}

fn cmd_check(options: &Options) -> i32 {
    // No identity means no package to replace: report and make no network call
    // at all. A dev build takes this path too, which is why it is `disabled`
    // and exit 0 rather than an error.
    if !fsw_core::has_package_identity() {
        return Report::new("disabled", EXIT_OK).emit(options, None);
    }
    let folded = fold_result_file();
    collect_for(options.verb);

    if !options.force
        && !fsw_core::update::check_is_due(fsw_core::update::last_update_check(), now_unix())
    {
        if let Some(offer) = cached_offer() {
            return Report::new("available", EXIT_AVAILABLE)
                .available(offer.label().map(str::to_owned))
                .emit(options, folded);
        }
        let _ = fsw_core::update::clear_update_offer();
        return Report::new("notDue", EXIT_OK).emit(options, folded);
    }

    if !fsw_core::is_store_flavor() {
        // The GitHub flavor's check also downloads and stages the bundle;
        // `fsw_core` owns that end to end and always has.
        return match fsw_core::update::run_update_check(options.force) {
            fsw_core::update::UpdateOutcome::NotDue => {
                Report::new("notDue", EXIT_OK).emit(options, folded)
            }
            fsw_core::update::UpdateOutcome::Unavailable => {
                Report::new("unavailable", EXIT_OK).emit(options, folded)
            }
            fsw_core::update::UpdateOutcome::VerificationFailed(error) => {
                Report::new("error", EXIT_ERROR)
                    .detail(error.message().to_string())
                    .emit(options, folded)
            }
            fsw_core::update::UpdateOutcome::UpToDate => {
                Report::new("upToDate", EXIT_OK).emit(options, folded)
            }
            fsw_core::update::UpdateOutcome::Ready(tag) => Report::new("available", EXIT_AVAILABLE)
                .available(Some(tag))
                .emit(options, folded),
            // Unreachable from the GitHub path, which only ever has release
            // tags, but exhaustive and correct if that ever changes.
            fsw_core::update::UpdateOutcome::ReadyUnnamed => {
                Report::new("available", EXIT_AVAILABLE).emit(options, folded)
            }
        };
    }

    let _com = ComScope::new();
    // Read before `note_check_attempt` moves it: this is the previous check's
    // time, which decides whether an empty answer is inside the cooldown.
    let previous_check = fsw_core::update::last_update_check();
    let _ = fsw_core::update::note_check_attempt();
    match store::check_store_offer() {
        Ok(Some(offer)) => {
            persist_store_offer(&offer);
            let mut report = Report::new("available", EXIT_AVAILABLE)
                .available(offer.label().map(str::to_owned));
            if offer.label().is_none() {
                // Exit 10 with no version is a legal shape the contract
                // already allows; say why, so the window can render an offer
                // it cannot name rather than a failure (issue #97).
                report =
                    report.detail("An update is available in the Microsoft Store.".to_string());
            }
            report.emit(options, folded)
        }
        Ok(None) => {
            if let Some(cached) =
                cached_offer().filter(|_| keep_cached_offer(previous_check, now_unix()))
            {
                // An empty answer minutes after a real one is the Store's
                // scan cache, not a withdrawal of the offer.
                Report::new("available", EXIT_AVAILABLE)
                    .available(cached.label().map(str::to_owned))
                    .detail(
                        "The Store answered from its cache; the earlier offer stands.".to_string(),
                    )
                    .emit(options, folded)
            } else {
                let _ = fsw_core::update::clear_update_offer();
                Report::new("upToDate", EXIT_OK).emit(options, folded)
            }
        }
        // A check that could not run is not a failure the user should be shown:
        // the Store is offline, or the account has no license yet. Exit 0.
        Err(code) => Report::new("unavailable", EXIT_OK)
            .detail(format!("The Store could not be reached ({code})."))
            .emit(options, folded),
    }
}

/// Picks the rung, probing lazily. The lower rungs cost `WinRT` round trips, so
/// they are only asked about once the rung above is out; [`route_for`] still
/// sees the whole row and remains the single definition of precedence.
fn resolve_route(explicit: Option<Route>) -> (Vec<Route>, bool) {
    // A forced route is exactly one rung and no failover: the point of the
    // escape hatch is to see that rung succeed or fail on its own.
    if let Some(route) = explicit.or_else(route_override) {
        return (vec![route], true);
    }
    // Every probe now runs, because the ladder may reach every rung. Each is
    // a `WinRT` round trip or a PATH lookup, once per install.
    let silent = store::can_silently_download();
    let winget = fsw_core::SystemBinary::Winget.path().is_some();
    let metered = winget && store::network_is_metered();
    (auto_ladder(silent, winget, metered), false)
}

/// What the Store is offering, or — when it cannot be reached — whatever the
/// last successful check recorded. `None` means there is nothing to install.
///
/// This is also the **guard on route 1b**. The staged helper has no package
/// identity, so it cannot run `GetAppAndOptionalStorePackageUpdatesAsync` for
/// itself; the Store treats `StartProductInstallWithOptionsAsync` for an
/// already-current product as a completed no-op (measured), but relying on that
/// would mean scheduling a task, staging an exe and driving an install to
/// discover there was nothing to do. Asking here, once, from the process that
/// *can* ask, is the cheap version.
#[cfg(windows)]
fn available_update() -> Availability {
    // No package version means no basis for any comparison. That is not
    // evidence of being up to date, so it is `Unknown` rather than `Nothing`.
    let Some(current) = fsw_core::package_version() else {
        return Availability::Unknown;
    };
    // What the last check recorded comes first. `install` runs seconds after
    // `check` — the broker chains them, the settings window's button follows
    // its own launch check — and the Store's install service rate-limits
    // online scans per package family: a second ask that soon is answered
    // from its cache ("Online scan not allowed due to cooldown period", 0
    // applicable), which used to read as an authoritative "up to date" and
    // erase the offer the user was about to install (issue #140).
    if let Some(offer) = fsw_core::update::offer_from_state(
        &current,
        fsw_core::update::cached_update_tag().as_deref(),
        fsw_core::update::store_update_pending(),
    ) {
        return Availability::Offer(offer);
    }
    // Nothing recorded: ask, once, from the process that can.
    match store::check_store_offer() {
        Ok(Some(offer)) => {
            let _ = fsw_core::update::note_check_attempt();
            persist_store_offer(&offer);
            Availability::Offer(offer)
        }
        Ok(None) => {
            let _ = fsw_core::update::note_check_attempt();
            Availability::Nothing
        }
        // The query failed. Saying "nothing to install" here was issue #90.
        Err(_) => Availability::Unknown,
    }
}

/// Records a Store offer: the pending flag always, and the label only when the
/// offer has one. An unnamed offer must clear a stale label rather than leave
/// it standing, or the window would print a version the Store never named.
#[cfg(windows)]
fn persist_store_offer(offer: &fsw_core::update::Offer) {
    match offer.label() {
        Some(version) => {
            let _ = fsw_core::update::set_cached_update_tag(version);
        }
        None => {
            let _ = fsw_core::update::clear_cached_update_tag();
        }
    }
    let _ = fsw_core::update::set_store_update_pending(true);
}

/// The Store's install service refuses a second online scan for the same
/// package family within a cooldown of roughly this length and answers from
/// its cache instead — which says "nothing applicable" whatever the previous
/// online scan found. Measured 2026-09-07 on 26100 (`Microsoft-Windows-Store/
/// Operational`: "Online scan not allowed due to cooldown period"). Within it,
/// an empty answer must not out-vote a newer offer the last real scan cached.
pub const STORE_SCAN_COOLDOWN_SECS: u64 = 30 * 60;

/// Whether an empty Store answer should leave a cached newer offer standing:
/// yes while the previous check was recent enough to be inside the cooldown.
/// Pure, so the rule is a test rather than a Store session.
#[must_use]
pub fn keep_cached_offer(last_check: Option<u64>, now: u64) -> bool {
    last_check.is_some_and(|last| now.saturating_sub(last) < STORE_SCAN_COOLDOWN_SECS)
}

/// The **only** call site of [`gc::collect`], so which verbs carry that side
/// effect is decided once, by the pure [`Verb::collects_garbage`], rather than
/// by which function someone happened to add a sweep to.
///
/// A packaged `check` is the moment leftovers from earlier attempts go: the
/// broker's cycle and the settings window's launch both run one.
fn collect_for(verb: Verb) {
    if verb.collects_garbage() {
        let _ = gc::collect();
    }
}

/// The persisted offer, filtered for newness. The **only** way anything in this
/// module reads the update notice: the raw `AvailableUpdate` value can name a
/// version that is no longer newer, and rendering that was issue #97.
fn cached_offer() -> Option<fsw_core::update::Offer> {
    fsw_core::update::cached_offer()
}

fn cmd_install(options: &Options) -> i32 {
    if !fsw_core::has_package_identity() {
        return Report::new("disabled", EXIT_NOTHING).emit(options, None);
    }
    let folded = fold_result_file();

    if !fsw_core::is_store_flavor() {
        return install_github_bundle(options, folded);
    }

    // One apartment for the whole Store path; the availability probe, the route
    // probes and every rung below nest inside it.
    let _com = ComScope::new();
    // Availability outranks everything, including `--route`: a forced route
    // says *how* to install, never *whether* there is anything to.
    let availability = available_update();
    let label = availability.label();
    // `worker_busy` is always false from here: only the broker knows whether
    // its Enter worker is mid-handler, so it gates before it ever invokes us.
    let answer = install_answer(
        &availability,
        fsw_core::update::unnamed_offer_actionable(
            fsw_core::update::store_update_attempt(),
            now_unix(),
        ),
        install_moment_ok(options.force, fsw_core::settings_window_exists(), false),
    );
    match answer {
        InstallAnswer::Nothing => {
            return Report::new("upToDate", EXIT_NOTHING).emit(options, folded);
        }
        // Exit 11, not 12 and not 0. Twelve would claim there is nothing to
        // install, which is the bug. Zero is unusable here: the settings
        // window reads exit 0 without `action: "queued"` as "I am about to be
        // force-closed", shows nothing and declines to restore the broker.
        InstallAnswer::Unknown => {
            return Report::new("needsUser", EXIT_NEEDS_USER)
                .route(Route::Notify)
                .detail(
                    "The Store could not be reached. Install the update from the Store."
                        .to_string(),
                )
                .emit(options, folded);
        }
        InstallAnswer::BackedOff => {
            return Report::new("deferred", EXIT_AVAILABLE)
                .available(label)
                .detail(
                    "The last attempt at this update did not advance the version; \
                     waiting before retrying."
                        .to_string(),
                )
                .emit(options, folded);
        }
        InstallAnswer::Defer => {
            return Report::new("deferred", EXIT_AVAILABLE)
                .available(label)
                .emit(options, folded);
        }
        InstallAnswer::Proceed => {}
    }

    // An unnamed offer may be a repair package that never advances the
    // version. Stamp the attempt before starting, so a failure backs off
    // instead of looping every cycle.
    if matches!(
        availability,
        Availability::Offer(fsw_core::update::Offer::Unnamed)
    ) {
        let _ = fsw_core::update::note_store_update_attempt();
    }

    let (ladder, _forced) = resolve_route(options.route);
    install_via_ladder(&ladder, options, label.as_deref(), folded)
}

/// GitHub flavor: the bundle is already downloaded and deferred-registered, so
/// applying it is one `Add-AppxPackage -ForceApplicationShutdown` from a process
/// that shutdown cannot reach.
fn install_github_bundle(options: &Options, folded: Option<String>) -> i32 {
    // Same order as the Store path: no bundle is "nothing to install" (12), and
    // that answer comes before the moment gate, because a deferral is a promise
    // that there is something to defer.
    let Some(bundle) = fsw_core::update::pending_bundle_path() else {
        return Report::new("upToDate", EXIT_NOTHING).emit(options, folded);
    };
    if install_precheck(
        true,
        install_moment_ok(options.force, fsw_core::settings_window_exists(), false),
    ) == Precheck::Defer
    {
        return Report::new("deferred", EXIT_AVAILABLE)
            .available(fsw_core::update::cached_update_tag())
            .emit(options, folded);
    }
    let Some(helper) = helper::stage_helper() else {
        return Report::new("error", EXIT_ERROR)
            .detail("The update helper could not be staged.".to_string())
            .emit(options, folded);
    };
    let previous = previous_version();
    let command = helper::apply_bundle_command(&helper, &bundle, &previous);
    if relaunch::schedule_apply(&command, options.relaunch, &previous) {
        Report::new("installing", EXIT_OK)
            .action("scheduled")
            .emit(options, folded)
    } else {
        Report::new("error", EXIT_ERROR)
            .detail("The update task could not be registered.".to_string())
            .emit(options, folded)
    }
}

/// Route 1, both phases.
///
/// 1a runs winget's sequence in-process from the packaged CLI: the spike found
/// `AppInstallManager` activates and answers queries there, and whether the
/// *install* is allowed is only knowable at runtime. Any failure before an item
/// is queued — `E_ACCESSDENIED` above all — drops to 1b, the identity-less
/// staged helper; a failure to even schedule that drops to route 2.
/// What one rung of the ladder did.
///
/// `Declined` is the **only** outcome that licenses trying another rung.
/// Once a rung has queued work, deployment may already be under way and a
/// second installer would race it, so every other outcome is terminal.
enum Rung {
    /// Nothing was queued; it is safe to fall through.
    Declined(String),
    /// Terminal. Report this and stop walking.
    Done(Report),
}

/// Route 1: `AppInstallManager`, phases 1a and 1b.
fn try_appinstall(options: &Options, available: Option<&str>) -> Rung {
    let previous = previous_version();
    // Registered but deliberately NOT run yet. Its backstop trigger is a minute
    // out, which is longer than the 1a/1b decision takes, so the script 1b may
    // replace is never a file `cmd.exe` already has open.
    let Some(watchdog) = relaunch::schedule_watchdog(options.relaunch, &previous, false) else {
        return Rung::Done(
            Report::new("error", EXIT_ERROR)
                .route(Route::AppInstall)
                .available(available.map(str::to_owned))
                .detail("The relaunch watchdog could not be registered.".to_string()),
        );
    };

    let policy = WaitPolicy::Foreground {
        admission: ADMISSION_WINDOW,
    };
    match appinstall::apply_store_update(fsw_core::STORE_PRODUCT_ID, policy) {
        // The Store has the item and the watchdog has the comeback. Nothing
        // waits on this process any longer: the settings window and the
        // broker both key on this exit and go on with their day.
        appinstall::Outcome::Queued => Rung::Done(
            Report::new("installing", EXIT_OK)
                .route(Route::AppInstall)
                .action("queued")
                .available(available.map(str::to_owned))
                .detail("The Store is installing the update in the background.".to_string()),
        ),
        appinstall::Outcome::Finished { code, result } => {
            if !appinstall_keeps_watchdog(code) {
                watchdog.cancel();
            }
            if code == EXIT_OK || code == EXIT_NOTHING {
                // We are still alive, so the Store did not force-restart us;
                // either way the cached notice is spent. `EXIT_NOTHING` counts:
                // the Store reporting nothing to install means the package is
                // already current, so a standing notice would repeat forever.
                let _ = fsw_core::update::clear_update_offer();
            }
            let mut report =
                report_for_code(code, Route::AppInstall).available(available.map(str::to_owned));
            if let Some(detail) = helper_result_detail(&result) {
                report = report.detail(detail);
            }
            Rung::Done(report)
        }
        appinstall::Outcome::NotStarted(detail) => {
            watchdog.cancel();
            // Phase 1b: the same call from the identity-less helper, which is
            // the context the API may accept when the packaged one is refused.
            if let Some(helper) = helper::stage_helper() {
                let command =
                    helper::apply_store_command(&helper, fsw_core::STORE_PRODUCT_ID, &previous);
                if relaunch::schedule_apply(&command, options.relaunch, &previous) {
                    return Rung::Done(
                        Report::new("installing", EXIT_OK)
                            .route(Route::AppInstall)
                            .action("scheduled")
                            .available(available.map(str::to_owned))
                            .detail(format!("In-process install unavailable ({detail}).")),
                    );
                }
            }
            Rung::Declined(detail)
        }
    }
}

/// Route 2: `StoreContext`'s own silent download and install.
fn try_store(options: &Options, available: Option<&str>) -> Rung {
    let previous = previous_version();
    // This one can terminate the package the moment deployment starts, so its
    // watchdog runs immediately rather than waiting for the backstop trigger.
    let Some(watchdog) = relaunch::schedule_watchdog(options.relaunch, &previous, true) else {
        return Rung::Done(
            Report::new("error", EXIT_ERROR)
                .route(Route::Store)
                .available(available.map(str::to_owned))
                .detail("The relaunch watchdog could not be registered.".to_string()),
        );
    };

    let policy = WaitPolicy::Foreground {
        admission: ADMISSION_WINDOW,
    };
    match store::silent_download_and_install(policy) {
        // The Store accepted it and is still working. Nothing waits on this
        // process any longer; the watchdog owns the comeback.
        store::Outcome::Queued => Rung::Done(
            Report::new("installing", EXIT_OK)
                .route(Route::Store)
                .action("queued")
                .available(available.map(str::to_owned))
                .detail("The Store is installing the update in the background.".to_string()),
        ),
        store::Outcome::Finished { code, detail } => {
            if !store_keeps_watchdog(code) {
                watchdog.cancel();
            }
            let mut report =
                report_for_code(code, Route::Store).available(available.map(str::to_owned));
            if let Some(detail) = detail {
                report = report.detail(format!("The Store install failed ({detail})."));
            }
            Rung::Done(report)
        }
        store::Outcome::NotStarted(detail) => {
            watchdog.cancel();
            Rung::Declined(detail)
        }
    }
}

/// Route 3: hand the whole thing to `winget`, from the scheduled task so it
/// survives the package going down.
fn try_winget(options: &Options, available: Option<&str>) -> Rung {
    let Some(command) = relaunch::winget_command(fsw_core::STORE_PRODUCT_ID) else {
        return Rung::Declined("the Windows App Installer alias is unavailable".to_string());
    };
    if store::network_is_metered() {
        // Not a decline: winget downloads regardless of the user's data
        // settings, so this is a deliberate stop rather than a rung that
        // could not run. Falling through would defeat the whole point.
        return Rung::Done(
            Report::new("deferred", EXIT_AVAILABLE)
                .route(Route::Winget)
                .available(available.map(str::to_owned))
                .detail("The network is metered.".to_string()),
        );
    }
    let previous = previous_version();
    if relaunch::schedule_apply(&command, options.relaunch, &previous) {
        Rung::Done(
            Report::new("installing", EXIT_OK)
                .route(Route::Winget)
                .action("scheduled")
                .available(available.map(str::to_owned)),
        )
    } else {
        Rung::Declined("the update task could not be registered".to_string())
    }
}

/// Runs one rung by name.
fn try_route(route: Route, options: &Options, available: Option<&str>) -> Rung {
    match route {
        Route::AppInstall => try_appinstall(options, available),
        Route::Store => try_store(options, available),
        Route::Winget => try_winget(options, available),
        Route::Notify => Rung::Done(
            Report::new("needsUser", EXIT_NEEDS_USER)
                .route(Route::Notify)
                .available(available.map(str::to_owned)),
        ),
    }
}

/// Walks the ladder, falling through every rung that declines before it
/// queued anything, and stopping at the first that did something.
fn install_via_ladder(
    ladder: &[Route],
    options: &Options,
    available: Option<&str>,
    folded: Option<String>,
) -> i32 {
    let mut declined = Vec::new();
    for route in ladder {
        match try_route(*route, options, available) {
            Rung::Done(report) => return report.emit(options, folded),
            Rung::Declined(detail) => declined.push(format!("{}: {detail}", route.name())),
        }
    }
    // Every rung declined without queueing anything. Tell the user, who can
    // still press Update in the Store themselves.
    Report::new("needsUser", EXIT_NEEDS_USER)
        .route(Route::Notify)
        .available(available.map(str::to_owned))
        .detail(format!(
            "No install route was available ({}).",
            declined.join("; ")
        ))
        .emit(options, folded)
}

/// Route 1, both phases.
///
/// 1a runs winget's sequence in-process from the packaged CLI: the spike found
/// `AppInstallManager` activates and answers queries there, and whether the
/// *install* is allowed is only knowable at runtime. Any failure before an item
/// is queued — `E_ACCESSDENIED` above all — drops to 1b, the identity-less
/// staged helper; a failure to even schedule that drops to route 2.
fn report_for_code(code: i32, route: Route) -> Report {
    Report::new(state_for_code(code), code).route(route)
}

#[must_use]
fn appinstall_keeps_watchdog(code: i32) -> bool {
    matches!(code, EXIT_OK | EXIT_AVAILABLE)
}

#[must_use]
fn store_keeps_watchdog(code: i32) -> bool {
    matches!(code, EXIT_OK | EXIT_AVAILABLE)
}

// ---------------------------------------------------------------------------
// Helper-only verbs
// ---------------------------------------------------------------------------

fn cmd_apply_store(options: &Options) -> i32 {
    let Some(product) = options.product.as_deref() else {
        eprintln!("usage: fwdslash update apply-store --product <id>");
        return EXIT_USAGE;
    };
    let _com = ComScope::new();
    let policy = WaitPolicy::Background {
        ceiling: INSTALL_CEILING,
    };
    match appinstall::apply_store_update(product, policy) {
        appinstall::Outcome::Finished { code, result } => {
            helper::write_result(&result);
            code
        }
        // Only a foreground wait hands off; the helper polls to a conclusion.
        // Reaching here means the queue already held a live item from an
        // earlier attempt, which is the same "still installing" a pause is.
        appinstall::Outcome::Queued => {
            helper::write_result(&HelperResult::Paused);
            EXIT_AVAILABLE
        }
        appinstall::Outcome::NotStarted(detail) => {
            helper::write_result(&HelperResult::Error(detail));
            EXIT_ERROR
        }
    }
}

fn cmd_apply_bundle(options: &Options) -> i32 {
    let Some(bundle) = options.bundle.as_deref() else {
        eprintln!("usage: fwdslash update apply-bundle --bundle <path>");
        return EXIT_USAGE;
    };
    match helper::register_bundle(std::path::Path::new(bundle)) {
        Ok(()) => {
            helper::write_result(&HelperResult::Completed);
            EXIT_OK
        }
        Err(error) => {
            helper::write_result(&HelperResult::Error(error.code().to_string()));
            EXIT_ERROR
        }
    }
}
