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
//! * **Nothing here may panic.** `panic = "abort"` plus a WinRT surface that
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
pub mod helper;
pub mod relaunch;
pub mod store;

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

/// Which rung to take. Pure, so the ladder is a truth table rather than a
/// sequence of side effects, and so `--route` is testable without a Store.
///
/// `appinstall_available` means "route 1 is attemptable at all": the packaged
/// process can try it in-process (phase 1a) *or* the identity-less helper can
/// be staged for it (phase 1b). `metered` suppresses only winget, because that
/// is the one rung that downloads without consulting the user's data settings.
#[must_use]
pub fn route_for(
    override_route: Option<Route>,
    appinstall_available: bool,
    can_silently_download: bool,
    winget_available: bool,
    metered: bool,
) -> Route {
    if let Some(route) = override_route {
        return route;
    }
    if appinstall_available {
        return Route::AppInstall;
    }
    if can_silently_download {
        return Route::Store;
    }
    if winget_available && !metered {
        return Route::Winget;
    }
    Route::Notify
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
    let text = text.trim();
    if text.eq_ignore_ascii_case("completed") {
        return Some(HelperResult::Completed);
    }
    if text.eq_ignore_ascii_case("paused") {
        return Some(HelperResult::Paused);
    }
    const ERROR: &str = "error:";
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

/// MTA for the duration of one update verb, so a blocking wait on a WinRT
/// operation needs no message pump. Scoped, because `fwdslash` is a
/// short-lived CLI whose other verbs must not pay for COM at all.
///
/// Nesting is safe and intended: a second `CoInitializeEx` on an
/// already-multithreaded apartment returns `S_FALSE` and only bumps the
/// reference count, which the matching `Drop` releases again.
struct ComScope {
    initialized: bool,
}

impl ComScope {
    fn new() -> Self {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

        // SAFETY: paired with the `CoUninitialize` in `Drop`, and only when
        // this call actually took a reference on the apartment.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized: result.is_ok(),
        }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances exactly one successful CoInitializeEx above.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

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
    let folded = fold_result_file();
    if !fsw_core::has_package_identity() {
        return Report::new("disabled", EXIT_OK).emit(options, folded);
    }
    let available = fsw_core::update::cached_update_tag();
    let state = if available.is_some() {
        "available"
    } else {
        "upToDate"
    };
    Report::new(state, EXIT_OK)
        .available(available)
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

    if !options.force
        && !fsw_core::update::check_is_due(fsw_core::update::last_update_check(), now_unix())
    {
        return match fsw_core::update::cached_update_tag() {
            Some(tag) => Report::new("available", EXIT_AVAILABLE)
                .available(Some(tag))
                .emit(options, folded),
            None => Report::new("notDue", EXIT_OK).emit(options, folded),
        };
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
            fsw_core::update::UpdateOutcome::UpToDate => {
                Report::new("upToDate", EXIT_OK).emit(options, folded)
            }
            fsw_core::update::UpdateOutcome::Ready(tag) => Report::new("available", EXIT_AVAILABLE)
                .available(Some(tag))
                .emit(options, folded),
        };
    }

    let _com = ComScope::new();
    let _ = fsw_core::update::note_check_attempt();
    match store::check_store_updates() {
        Ok(versions) => match versions.first() {
            Some(version) => {
                let _ = fsw_core::update::set_cached_update_tag(version);
                Report::new("available", EXIT_AVAILABLE)
                    .available(Some(version.clone()))
                    .emit(options, folded)
            }
            None => {
                let _ = fsw_core::update::clear_cached_update_tag();
                Report::new("upToDate", EXIT_OK).emit(options, folded)
            }
        },
        // A check that could not run is not a failure the user should be shown:
        // the Store is offline, or the account has no license yet. Exit 0.
        Err(code) => Report::new("unavailable", EXIT_OK)
            .detail(format!("The Store could not be reached ({code})."))
            .emit(options, folded),
    }
}

/// Picks the rung, probing lazily. The lower rungs cost WinRT round trips, so
/// they are only asked about once the rung above is out; [`route_for`] still
/// sees the whole row and remains the single definition of precedence.
fn resolve_route(explicit: Option<Route>) -> Route {
    let override_route = explicit.or_else(route_override);
    let probing = override_route.is_none();
    let appinstall = probing && helper::appinstall_available();
    let silent = probing && !appinstall && store::can_silently_download();
    let winget =
        probing && !appinstall && !silent && fsw_core::executable_available("winget.exe");
    let metered = winget && store::network_is_metered();
    route_for(override_route, appinstall, silent, winget, metered)
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
fn available_update() -> Option<String> {
    match store::check_store_updates() {
        Ok(versions) => {
            // A real answer: keep the cached notice honest while we have one.
            let _ = fsw_core::update::note_check_attempt();
            let first = versions.into_iter().next();
            match &first {
                Some(version) => {
                    let _ = fsw_core::update::set_cached_update_tag(version);
                }
                None => {
                    let _ = fsw_core::update::clear_cached_update_tag();
                }
            }
            first
        }
        // Offline, or the Store refused: trust what the last successful check
        // recorded rather than refusing an install the user just asked for.
        Err(_) => fsw_core::update::cached_update_tag(),
    }
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
    // Nothing to install outranks everything, including `--route`: a forced
    // route says *how* to install, never *whether* there is anything to.
    let Some(available) = available_update() else {
        return Report::new("upToDate", EXIT_NOTHING).emit(options, folded);
    };
    // `worker_busy` is always false from here: only the broker knows whether
    // its Enter worker is mid-handler, so it gates before it ever invokes us.
    if install_precheck(
        true,
        install_moment_ok(options.force, fsw_core::settings_window_exists(), false),
    ) == Precheck::Defer
    {
        return Report::new("deferred", EXIT_AVAILABLE)
            .available(Some(available))
            .emit(options, folded);
    }

    match resolve_route(options.route) {
        Route::AppInstall => install_via_appinstall(options, &available, folded),
        Route::Store => install_via_store(options, &available, folded),
        Route::Winget => install_via_winget(options, &available, folded),
        Route::Notify => Report::new("needsUser", EXIT_NEEDS_USER)
            .route(Route::Notify)
            .available(Some(available))
            .emit(options, folded),
    }
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
fn install_via_appinstall(options: &Options, available: &str, folded: Option<String>) -> i32 {
    let previous = previous_version();
    // Registered but deliberately NOT run yet. Its backstop trigger is a minute
    // out, which is longer than the 1a/1b decision takes, so the script 1b may
    // replace is never a file `cmd.exe` already has open.
    let _ = relaunch::schedule_watchdog(options.relaunch, &previous, false);

    match appinstall::apply_store_update(fsw_core::STORE_PRODUCT_ID) {
        appinstall::Outcome::Finished { code, result } => {
            if code == EXIT_OK {
                // We are still alive, so the Store did not force-restart us;
                // either way the cached notice is spent.
                let _ = fsw_core::update::clear_cached_update_tag();
            }
            let mut report = report_for_code(code, Route::AppInstall).available(Some(available.to_string()));
            if let Some(detail) = helper_result_detail(&result) {
                report = report.detail(detail);
            }
            report.emit(options, folded)
        }
        appinstall::Outcome::NotStarted(detail) => {
            if let Some(helper) = helper::stage_helper() {
                let command =
                    helper::apply_store_command(&helper, fsw_core::STORE_PRODUCT_ID, &previous);
                if relaunch::schedule_apply(&command, options.relaunch, &previous) {
                    return Report::new("installing", EXIT_OK)
                        .route(Route::AppInstall)
                        .action("scheduled")
                        .available(Some(available.to_string()))
                        .detail(format!("In-process install unavailable ({detail})."))
                        .emit(options, folded);
                }
            }
            // 1b is out too: continue down the ladder rather than reporting a
            // failure the user cannot act on.
            install_via_store(options, available, folded)
        }
    }
}

/// Route 2: `StoreContext`, which only downloads silently when the user's Store
/// is set to update apps automatically and the network is unmetered.
fn install_via_store(options: &Options, available: &str, folded: Option<String>) -> i32 {
    let previous = previous_version();
    // This one can terminate the package the moment deployment starts, so its
    // watchdog runs immediately rather than waiting for the backstop trigger.
    let _ = relaunch::schedule_watchdog(options.relaunch, &previous, true);

    match store::silent_download_and_install() {
        Ok(code) => report_for_code(code, Route::Store)
            .available(Some(available.to_string()))
            .emit(options, folded),
        Err(detail) => Report::new("needsUser", EXIT_NEEDS_USER)
            .route(Route::Notify)
            .available(Some(available.to_string()))
            .detail(format!("The Store declined a silent install ({detail})."))
            .emit(options, folded),
    }
}

/// Route 3: hand the whole thing to `winget`, from the scheduled task so it
/// survives the package going down. Skipped on a metered network — winget
/// downloads regardless of the user's data settings.
fn install_via_winget(options: &Options, available: &str, folded: Option<String>) -> i32 {
    if store::network_is_metered() {
        return Report::new("deferred", EXIT_AVAILABLE)
            .route(Route::Winget)
            .available(Some(available.to_string()))
            .detail("The network is metered.".to_string())
            .emit(options, folded);
    }
    let previous = previous_version();
    if relaunch::schedule_apply(
        &relaunch::winget_command(fsw_core::STORE_PRODUCT_ID),
        options.relaunch,
        &previous,
    ) {
        Report::new("installing", EXIT_OK)
            .route(Route::Winget)
            .action("scheduled")
            .available(Some(available.to_string()))
            .emit(options, folded)
    } else {
        Report::new("needsUser", EXIT_NEEDS_USER)
            .route(Route::Notify)
            .available(Some(available.to_string()))
            .emit(options, folded)
    }
}

fn report_for_code(code: i32, route: Route) -> Report {
    Report::new(state_for_code(code), code).route(route)
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
    match appinstall::apply_store_update(product) {
        appinstall::Outcome::Finished { code, result } => {
            helper::write_result(&result);
            code
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
    if helper::register_bundle(std::path::Path::new(bundle)) {
        helper::write_result(&HelperResult::Completed);
        EXIT_OK
    } else {
        // No HRESULT is available from a PowerShell exit status; the folded
        // detail only has to say "it failed", and the user-facing route is the
        // Store page either way.
        helper::write_result(&HelperResult::Error("0x80070643".to_string()));
        EXIT_ERROR
    }
}
