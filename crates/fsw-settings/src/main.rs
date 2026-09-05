#![windows_subsystem = "windows"]
// The reactor view DSL is designed to be glob-imported, and `State` mirrors the
// C++ `RefreshState()` locals one for one.
#![allow(clippy::wildcard_imports, clippy::struct_excessive_bools)]

//! Settings window for Forward Slash Windows.
//!
//! This is a port of `src/settings/main.cpp` onto `windows-reactor`, and it is meant
//! to be indistinguishable from it. State is read in-process, exactly as the C++ app
//! reads it; `fwdslash.exe` is spawned only to *change* state. Anything that differs
//! on purpose belongs in `docs/divergences.md`.

use fsw_core::{
    BrokerState, CMD_ADAPTER_KEY, FSW_BROKER_WINDOW_CLASS, FSW_VERSION, FilterServiceState,
    POWERSHELL_ADAPTER_ROOT, STORE_PRODUCT_ID, SettingsValues, adapter_installed,
    adapter_outdated, adapter_version, broker_state, broker_window_exists, ensure_broker_running,
    executable_available, executable_directory, filter_port_available, filter_service_state,
    get_default_distribution, has_package_identity, is_store_flavor,
    list_registered_distributions, package_architecture, package_version,
    sync_settings_to_real_hive, update, windows_integration_installed,
};
use fsw_core::update::UpdateOutcome;
use fsw_path::{BareSlashMode, eq_ignore_case, is_valid_windows_root};
use std::env;
use std::path::PathBuf;
use std::process::Command;
use windows_reactor::*;

mod folder_picker;
mod state_watch;

/// The WSL provider root. Bare `/` may resolve elsewhere depending on bare-slash mode,
/// so "Open WSL root" targets this literally, as `src/settings/main.cpp:562` does.
const WSL_ROOT: &str = r"\\wsl.localhost";

/// Icon resource id from app.rc, kept in step with `include/fsw_resources.h`.
const IDI_FSW_APP: u16 = 101;

/// The `show_result` action phrase for a folder-root change. `ControllerFinished`
/// carries the phrase, so it is also how the handler recognizes the one action
/// that clears the pending folder selection.
const ROOT_ACTION: &str = "Bare slash opens the chosen folder";

/// The action phrase reserved for one step of the automatic adapter upgrade.
/// `ControllerFinished` matches on it to advance the upgrade queue, so it never
/// reaches `show_result` -- the upgrade reports itself once, when the queue drains.
const UPGRADE_ACTION: &str = "Terminal integration upgrade";

/// The `pending` phrase for an explicit "Check now". It only disables the
/// controls and shows the ProgressRing; the answer arrives as
/// `Msg::UpdateCheckFinished`, never as `ControllerFinished`.
const CHECK_ACTION: &str = "Update check";

/// The `pending` phrase while `update install` runs.
const INSTALL_ACTION: &str = "Update install";

/// The `show_result` phrase for the manual Repair integrations button (#56).
const REPAIR_ACTION: &str = "Integrations repaired";
const AUTO_UPDATE_ACTION: &str = "Automatic updates";
const OPEN_WSL_ACTION: &str = "Opening the WSL root";
const OPEN_STORE_ACTION: &str = "Opening the Microsoft Store";

/// `fwdslash update` exit codes, mirrored from `crates/fsw-cli/src/update`.
/// The contract between the two binaries is exactly these numbers plus the
/// one-line JSON, so name them here rather than match on bare integers.
const UPDATE_EXIT_OK: i32 = 0;
const UPDATE_EXIT_AVAILABLE: i32 = 10;
const UPDATE_EXIT_NEEDS_USER: i32 = 11;
const UPDATE_EXIT_NOTHING: i32 = 12;

/// The Store product page for this app. `ms-windows-store:` is the shell
/// protocol the Store registers; `ShellExecuteW` on it opens the Store app.
/// The product id comes from `fsw_core` so nothing here can drift from what
/// the CLI's Store routes address.
fn store_product_uri() -> String {
    format!("ms-windows-store://pdp/?productid={STORE_PRODUCT_ID}")
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    General,
    Windows,
    Terminals,
    About,
}

impl Section {
    /// Maps a deep-link tag onto a page.
    ///
    /// The three terminal integrations each have their own URI but share one page,
    /// matching the `terminals` grouping at `src/settings/main.cpp:844-846`.
    fn from_tag(tag: &str) -> Self {
        match tag.trim_matches('/').to_ascii_lowercase().as_str() {
            "windows" => Self::Windows,
            "terminals" | "cmd" | "windows-powershell" | "powershell" => Self::Terminals,
            "about" => Self::About,
            _ => Self::General,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Windows => "windows",
            Self::Terminals => "terminals",
            Self::About => "about",
        }
    }
}

// ---------------------------------------------------------------------------
// Integrations
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Integration {
    Windows,
    Cmd,
    WindowsPowerShell,
    PowerShell7,
}

impl Integration {
    fn id(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Cmd => "cmd",
            Self::WindowsPowerShell => "windows-powershell",
            Self::PowerShell7 => "powershell",
        }
    }

    fn installed(self, state: &State) -> bool {
        match self {
            Self::Windows => state.windows,
            // One source of truth: the adapter row read in `State::read`.
            _ => state
                .adapter(self)
                .is_some_and(|adapter| adapter.installed),
        }
    }

    /// The name the version and upgrade notices use for this adapter.
    fn display_name(self) -> &'static str {
        match self {
            Self::Windows => "Windows surfaces",
            Self::Cmd => "Command Prompt",
            Self::WindowsPowerShell => "Windows PowerShell",
            Self::PowerShell7 => "PowerShell 7",
        }
    }

    /// The action phrase the `InfoBar` reports, per `src/settings/main.cpp:599-677`.
    fn action(self, enabled: bool) -> &'static str {
        match (self, enabled) {
            (Self::Windows, true) => "Windows surfaces installed",
            (Self::Windows, false) => "Windows surfaces removed",
            (Self::Cmd, true) => "Command Prompt installed",
            (Self::Cmd, false) => "Command Prompt removed",
            (Self::WindowsPowerShell, true) => "Windows PowerShell installed",
            (Self::WindowsPowerShell, false) => "Windows PowerShell removed",
            (Self::PowerShell7, true) => "PowerShell 7 installed",
            (Self::PowerShell7, false) => "PowerShell 7 removed",
        }
    }

    /// Whether a change only takes effect in newly opened shells.
    fn is_terminal(self) -> bool {
        !matches!(self, Self::Windows)
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One shell adapter's deployed state: whether its marker key says `installed`,
/// the payload version it recorded, and whether that predates this build.
///
/// `outdated` is `adapter_outdated`, which is already false for an adapter that
/// is not installed; the pair is kept separate so the About page can tell
/// "not installed" from "installed and current".
#[derive(Clone, Debug, PartialEq, Eq)]
struct AdapterStatus {
    integration: Integration,
    installed: bool,
    version: Option<String>,
    outdated: bool,
}

/// The optional filesystem minifilter, as both pages report it.
///
/// The four states are the ones `fwdslash driver status` prints; `fsw-core`
/// exposes the two probes (`filter_port_available`, `filter_service_state`),
/// not the wording, so the mapping lives here and in `driver_state()` in
/// `crates/fsw-cli/src/main.rs`. Keep the two in step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriverStatus {
    NotInstalled,
    InstalledNotLoaded,
    LoadedNotConnected,
    Connected,
}

impl DriverStatus {
    /// A port that answers outranks whatever the SCM says; otherwise the
    /// service decides. Both probes are read-only and never elevate.
    fn read() -> Self {
        if filter_port_available() {
            return Self::Connected;
        }
        match filter_service_state() {
            FilterServiceState::NotInstalled => Self::NotInstalled,
            FilterServiceState::Stopped => Self::InstalledNotLoaded,
            FilterServiceState::Running => Self::LoadedNotConnected,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::InstalledNotLoaded => "installed, not loaded",
            Self::LoadedNotConnected => "loaded, not connected",
            Self::Connected => "connected",
        }
    }
}

/// Everything the window renders, read straight from HKCU and the broker window.
///
/// Mirrors `RefreshState()` at `src/settings/main.cpp:754-841`. Reading in-process
/// rather than parsing `fwdslash --json` is deliberate: it is what the C++ does, and
/// a text contract between two binaries fails silently and totally when one field
/// name drifts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    disabled: bool,
    packaged: bool,
    windows: bool,
    /// Cmd, Windows PowerShell and PowerShell 7, in that order.
    adapters: Vec<AdapterStatus>,
    powershell7_available: bool,
    bare_mode: BareSlashMode,
    pinned: String,
    root: Option<String>,
    store_flavor: bool,
    auto_update: bool,
    /// The version the last check found, from `AvailableUpdate`. This is the
    /// plan's `update_available`; it already existed under this name.
    update_tag: Option<String>,
    /// Unix time of the last check attempt, whatever it concluded. Rendered
    /// through `format_last_check` on the About page.
    last_check: Option<u64>,
    distributions: Vec<String>,
    wsl_default: Option<String>,
    broker: BrokerState,
    broker_window: bool,
    /// The optional filesystem minifilter, probed live.
    driver: DriverStatus,
    /// A downloaded update bundle is waiting to be registered (GitHub flavor).
    update_bundle_ready: bool,
}

/// `pwsh.exe` availability, resolved once per process.
///
/// `executable_available` is a PATH-wide `SearchPathW`; a PATH entry on an
/// offline share made every page click wait for the SMB timeout. Whether
/// PowerShell 7 is installed cannot change usefully while the window is open.
fn powershell7_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| executable_available("pwsh.exe"))
}

impl State {
    fn read() -> Self {
        let distributions = list_registered_distributions();
        let wsl_default = get_default_distribution(&distributions);
        // One open of the settings key for all four preferences, instead of
        // four single-value getters.
        let settings = SettingsValues::read();
        let windows_powershell_key = format!("{POWERSHELL_ADAPTER_ROOT}WindowsPowerShell");
        let powershell7_key = format!("{POWERSHELL_ADAPTER_ROOT}PowerShell");
        let adapters = [
            (Integration::Cmd, CMD_ADAPTER_KEY),
            (Integration::WindowsPowerShell, windows_powershell_key.as_str()),
            (Integration::PowerShell7, powershell7_key.as_str()),
        ]
        .into_iter()
        .map(|(integration, key)| AdapterStatus {
            integration,
            installed: adapter_installed(key),
            version: adapter_version(key),
            // The adapter payload version is the crate version.
            outdated: adapter_outdated(key, FSW_VERSION),
        })
        .collect();
        Self {
            disabled: settings.disabled,
            packaged: has_package_identity(),
            windows: windows_integration_installed(),
            adapters,
            powershell7_available: powershell7_available(),
            bare_mode: settings.bare_slash_mode,
            pinned: settings.bare_slash_pinned.unwrap_or_default(),
            root: settings.bare_slash_root,
            store_flavor: is_store_flavor(),
            auto_update: update::read_auto_update_enabled(),
            update_tag: update::cached_update_tag(),
            last_check: update::last_update_check(),
            distributions,
            wsl_default,
            // 250 ms so a wedged broker cannot stall a refresh (main.cpp:827
            // used 750; this runs off the UI thread now, but the window still
            // waits on the result to repaint).
            broker: broker_state(250),
            broker_window: broker_window_exists(),
            driver: DriverStatus::read(),
            update_bundle_ready: update::pending_bundle_path().is_some(),
        }
    }

    /// The adapter row for `integration`, or `None` for `Integration::Windows`,
    /// which is not a shell adapter and carries no payload version.
    fn adapter(&self, integration: Integration) -> Option<&AdapterStatus> {
        self.adapters
            .iter()
            .find(|adapter| adapter.integration == integration)
    }

    /// Installed adapters whose payload predates this build, in page order.
    /// `integration <id> enable` upgrades one in place.
    fn outdated_adapters(&self) -> Vec<Integration> {
        self.adapters
            .iter()
            .filter(|adapter| adapter.installed && adapter.outdated)
            .map(|adapter| adapter.integration)
            .collect()
    }

    /// The secondary line under a Terminals toggle: what is actually deployed.
    fn adapter_detail(&self, integration: Integration) -> Option<String> {
        let adapter = self.adapter(integration)?;
        if !adapter.installed {
            return None;
        }
        // A pre-`Version` install records no version at all, and is outdated
        // by that fact alone.
        let version = adapter.version.as_deref().unwrap_or("unknown");
        Some(if adapter.outdated {
            format!("Installed payload {version} \u{2014} updating to {FSW_VERSION}")
        } else {
            format!("Installed payload {version}")
        })
    }

    /// The About page's line for one adapter.
    fn adapter_component_line(&self, integration: Integration) -> String {
        let label = integration.display_name();
        let Some(adapter) = self.adapter(integration) else {
            return format!("{label} adapter: not installed");
        };
        if !adapter.installed {
            return format!("{label} adapter: not installed");
        }
        let version = adapter.version.as_deref().unwrap_or("unknown");
        if adapter.outdated {
            format!("{label} adapter: {version} (update pending)")
        } else {
            format!("{label} adapter: {version}")
        }
    }

    /// The About page's broker line. Deliberately not `status_text`'s wording:
    /// that string is a byte-for-byte port of `src/settings/main.cpp:832-838`
    /// and says "disabled" where this says "paused".
    fn broker_component_line(&self) -> &'static str {
        match self.broker {
            BrokerState::Active => "Broker: active",
            BrokerState::Paused => "Broker: paused",
            BrokerState::Unavailable if self.broker_window => "Broker: hook unavailable",
            BrokerState::Unavailable => "Broker: stopped",
        }
    }

    /// The About page's filesystem-driver line — the same four states the
    /// General page's status text shows.
    fn driver_component_line(&self) -> String {
        format!("Filesystem driver: {}", self.driver.label())
    }

    /// The About page's "when did this last look for an update" line.
    ///
    /// Always shown, including when the answer is "never": a switch that
    /// claims to check daily and a page that says nothing about it is how a
    /// silently broken updater stays silent.
    fn last_check_line(&self) -> String {
        format!(
            "Last update check: {}",
            update::format_last_check(now_unix(), self.last_check)
        )
    }

    /// The About page's "there is a newer version" line, or `None`.
    fn update_available_line(&self) -> Option<String> {
        let tag = self.update_tag.as_deref()?;
        Some(format!(
            "Update available: {}",
            tag.strip_prefix('v').unwrap_or(tag)
        ))
    }

    /// Which of the two package flavors is running, or neither.
    fn flavor_component_line(&self) -> &'static str {
        if !self.packaged {
            "Flavor: unpackaged"
        } else if self.store_flavor {
            "Flavor: Microsoft Store"
        } else {
            "Flavor: GitHub"
        }
    }

    fn is_list_mode(&self) -> bool {
        self.bare_mode == BareSlashMode::DistributionList
    }

    /// Item 0 is `Follow the Windows default`; the rest are registered distributions.
    fn distribution_options(&self) -> Vec<String> {
        let mut options = Vec::with_capacity(self.distributions.len() + 1);
        options.push("Follow the Windows default".to_string());
        options.extend(self.distributions.iter().cloned());
        options
    }

    /// The combo index the view declares, derived from the pinned registry value.
    ///
    /// `view` and `update` both call this so the "did the user actually change it"
    /// comparison in `update` is exact by construction and cannot drift.
    fn distribution_index(&self) -> usize {
        if self.pinned.is_empty() {
            return 0;
        }
        self.distributions
            .iter()
            .position(|distribution| eq_ignore_case(distribution, &self.pinned))
            .map_or(0, |index| index + 1)
    }

    /// What bare `/` actually opens: the pin if there is one, else the WSL default.
    fn effective_distribution(&self) -> Option<&str> {
        let index = self.distribution_index();
        if index > 0 {
            self.distributions.get(index - 1).map(String::as_str)
        } else {
            self.wsl_default.as_deref()
        }
    }

    fn bare_target_caption(&self) -> String {
        if let Some(root) = &self.root {
            return format!("/ opens {root}");
        }
        match self.effective_distribution() {
            Some(distribution) => format!("/ opens {WSL_ROOT}\\{distribution}"),
            None => "No WSL distribution is available for / yet.".to_string(),
        }
    }

    /// True when the folder choice is live: a radio selection alone does not
    /// count until a valid root has been applied.
    fn is_folder_mode(&self) -> bool {
        self.root.is_some()
    }

    /// Ported from `src/settings/main.cpp:832-838`. The broker line is
    /// verbatim; the driver line is not. The C++ hardcodes
    /// "not installed (production-gated)" — this reports what the machine
    /// actually has, from the service and the filter port
    /// (docs/divergences.md, settings window).
    fn status_text(&self) -> String {
        let broker = if self.windows {
            match self.broker {
                BrokerState::Active => "active",
                BrokerState::Paused => "disabled",
                BrokerState::Unavailable if self.broker_window => "hook unavailable",
                BrokerState::Unavailable => "stopped",
            }
        } else {
            "not installed"
        };
        format!(
            "Windows broker: {broker}\nFilesystem driver: {}",
            self.driver.label()
        )
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Msg {
    Navigate(Option<String>),
    PaneOpenChanged(bool),
    ColorSchemeChanged(ColorScheme),
    ToggleGlobal(bool),
    BareSlashListChecked(bool),
    BareSlashDefaultChecked(bool),
    FolderRadioChecked(bool),
    RootTextChanged(String),
    ApplyRoot,
    BrowseRoot,
    /// A `fwdslash update check` finished. `explicit` marks the one the user
    /// pressed Check now for: that one always reports, while the launch check
    /// is silent unless it found something.
    UpdateCheckFinished {
        outcome: update::UpdateOutcome,
        explicit: bool,
    },
    /// The Check now button.
    CheckForUpdates,
    SetAutoUpdate(bool),
    AutoUpdateFinished {
        enabled: bool,
        succeeded: bool,
    },
    SelectDistribution(Option<usize>),
    ToggleIntegration(Integration, bool),
    OpenWslRoot,
    ShellOpenFinished {
        action: &'static str,
        succeeded: bool,
    },
    RefreshStatus,
    DismissNotice,
    /// Install whatever the CLI can install now: the Store's update for the
    /// Store flavor, the downloaded bundle for the GitHub one.
    InstallUpdate,
    /// `fwdslash update install` finished, with the contract's exit code and
    /// the child's two streams.
    UpdateInstallFinished {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// The notice's action button; only the Store link uses it today.
    OpenStorePage,
    /// The manual Repair integrations button (#56) — what the broker's balloon
    /// tells the user to press.
    RepairIntegrations,
    /// The manual repair finished, or never started because the broker was
    /// already sweeping.
    RepairFinished(RepairOutcome),
    /// A background `State::read()` completed. Every refresh is off-thread: the
    /// read touches the registry, the broker window and the update directory.
    StateLoaded(State),
    /// The launch adapter sweep — the upgrade queue and the repair after it —
    /// finished. Carries the state read that followed it, and is the one
    /// message that releases the cross-process sweep lock (#56).
    LaunchSweepFinished(State),
    /// One turn of the external-change watch ended (issue #55): either a
    /// component broadcast that it changed something, or the safety poll.
    /// Carries no state — the read it may start is a separate task.
    ExternalStateChanged(state_watch::Wake),
    /// The state read an `ExternalStateChanged` started. Separate from
    /// `StateLoaded` because this one is coalesced and compared: a poll that
    /// finds nothing new must not touch the model at all.
    StateRefreshed(State),
    /// `ensure_broker_running()` finished off-thread; the broker column of the
    /// status line may have changed.
    BrokerProbed,
    /// A `fwdslash.exe` invocation finished off-thread. `action` is the
    /// `show_result` phrase the request was started with.
    ControllerFinished {
        action: &'static str,
        terminal: bool,
        succeeded: bool,
        /// The controller's stderr, so an actionable failure — a Controlled
        /// Folder Access block, say — reaches the InfoBar instead of the
        /// generic "failed" (#37).
        detail: String,
    },
}

/// What the manual Repair integrations button ended up doing (#56).
#[derive(Clone, Debug, PartialEq, Eq)]
enum RepairOutcome {
    Repaired,
    /// The broker's startup sweep holds the adapter-sweep mutex: the same work
    /// is already running, so the button did nothing on purpose.
    Busy,
    /// `repair-adapters` refused; the string is its stderr.
    Failed(String),
}

/// An action a notice offers beyond dismissal.
///
/// Reactor's `InfoBar` has no action-button slot, so `banners()` renders it as
/// a `Button` directly beneath the bar. It lives inside [`Notice`] rather than
/// beside it so that replacing the notice can never leave a stale button
/// pointing at the previous message's action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeAction {
    /// "Open Microsoft Store" — the last rung of the install ladder, where
    /// the only thing left is for the user to press Update in the Store.
    OpenStore,
}

impl NoticeAction {
    fn label(self) -> &'static str {
        match self {
            Self::OpenStore => "Open Microsoft Store",
        }
    }
}

/// The single dismissible bar at the top of the window.
#[derive(Clone, Debug, PartialEq)]
struct Notice {
    severity: InfoBarSeverity,
    title: &'static str,
    message: String,
    action: Option<NoticeAction>,
}

impl Notice {
    fn new(severity: InfoBarSeverity, title: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity,
            title,
            message: message.into(),
            action: None,
        }
    }

    fn with_action(mut self, action: NoticeAction) -> Self {
        self.action = Some(action);
        self
    }
}

/// The automatic shell-adapter upgrade in flight.
///
/// The broker runs the same upgrade at logon, so by the time this window opens
/// there is usually nothing to do; when there is, it happens without being
/// asked. One `fwdslash integration <id> enable` per adapter, sequentially --
/// the CLI transaction is per adapter, and two at once would race the shared
/// payload directory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Upgrade {
    /// The adapter whose `integration <id> enable` is running now.
    current: Integration,
    /// Adapters still waiting, popped from the back.
    queue: Vec<Integration>,
    /// Adapters the CLI reported success for.
    done: Vec<Integration>,
    /// At least one step failed; the summary becomes an error.
    failed: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct SettingsModel {
    section: Section,
    pane_open: bool,
    color_scheme: ColorScheme,
    state: State,
    /// The TextBox draft for the custom root; committed only by Apply.
    root_draft: String,
    /// The folder radio is selected but not yet committed (no root stored).
    /// UI intent only — the stored root is what survives a restart.
    folder_selected: bool,
    /// The current notice is the update-available notice: dismissing it must
    /// also clear the persisted AvailableUpdate value.
    notice_is_update: bool,
    notice: Option<Notice>,
    /// The `show_result` phrase of the controller invocation in flight, or
    /// `None`. Every control that mutates state is disabled while it is set,
    /// so a second request can never race the first.
    pending: Option<&'static str>,
    /// The running adapter upgrade, or `None` when none is in flight.
    upgrade: Option<Upgrade>,
    /// An upgrade has already been started (or found unnecessary) this process.
    /// Every refresh re-reads the adapter versions, and a failed upgrade would
    /// otherwise restart itself on every one of them.
    upgrade_attempted: bool,
    /// Guards the one-shot detect-and-repair sweep (#37) so it runs once per
    /// window, after any upgrade queue has drained.
    repair_started: bool,
    /// This process holds the cross-process adapter-sweep lock (#56), taken
    /// for the whole launch sweep — the upgrade queue *and* the repair that
    /// follows it — and released when the repair reports back.
    sweep_held: bool,
    /// Keeps the external-change watch (#55) down to one state read at a time,
    /// however many notifications a burst of writes produces.
    external_reads: state_watch::ReadCoalescer,
}

impl SettingsModel {
    /// Whether state-mutating controls accept input right now.
    fn controls_enabled(&self) -> bool {
        self.pending.is_none()
    }

    /// Starts a `fwdslash.exe` invocation on the thread pool. The UI thread
    /// never waits on the controller: `integration windows-powershell enable`
    /// loads the user's whole profile and can take 15 s.
    fn start_controller(
        &mut self,
        context: &ComponentContext<Self>,
        action: &'static str,
        terminal: bool,
        arguments: Vec<String>,
    ) {
        self.pending = Some(action);
        context.spawn_background(move |_| {
            let (succeeded, detail) = run_controller_detailed(arguments);
            Msg::ControllerFinished {
                action,
                terminal,
                succeeded,
                detail,
            }
        });
    }

    /// The single notification path, reproducing `ShowResult`
    /// (`src/settings/main.cpp:874-887`). Only Success and Error are ever used.
    fn show_result(&mut self, succeeded: bool, action: &str, terminal: bool, detail: &str) {
        let mut message = action.to_string();
        if succeeded && terminal {
            message.push_str(". Reopen affected terminals.");
        } else if !succeeded {
            message.push_str(" failed. Existing settings were left in place.");
            // The controller already explains an actionable failure (a
            // Controlled Folder Access block, a missing PowerShell 7); show
            // that rather than make the user run the CLI to find out (#37).
            let detail = detail.trim();
            if !detail.is_empty() {
                message.push(' ');
                message.push_str(detail);
            }
        }
        self.notice = Some(Notice::new(
            if succeeded {
                InfoBarSeverity::Success
            } else {
                InfoBarSeverity::Error
            },
            if succeeded {
                "Updated"
            } else {
                "Could not update integration"
            },
            message,
        ));
    }

    /// Re-reads state off the UI thread; the result arrives as
    /// `Msg::StateLoaded`.
    fn refresh(context: &ComponentContext<Self>) {
        context.spawn_background(|_| Msg::StateLoaded(State::read()));
    }

    /// Arms one turn of the external-change watch (#55).
    ///
    /// The wait blocks — on the broadcast, or on the poll interval — so it
    /// runs as background work like every other blocking call this window
    /// makes, and the loop continues because handling the message it produces
    /// arms the next turn. Nothing re-arms it if the component goes away,
    /// which is the point: a retired scope never sees the message.
    fn arm_state_watch(context: &ComponentContext<Self>) {
        context.spawn_background(|_| Msg::ExternalStateChanged(state_watch::wait()));
    }

    /// Starts the coalesced state read behind an external notification.
    fn read_external_state(context: &ComponentContext<Self>) {
        context.spawn_background(|_| Msg::StateRefreshed(State::read()));
    }

    /// Starts the automatic upgrade if any installed adapter is outdated.
    ///
    /// Called for every `State` the model accepts. Nothing is asked of the
    /// user: an outdated payload is a bug to fix, not a decision to make.
    fn maybe_start_upgrade(&mut self, context: &ComponentContext<Self>) {
        if self.upgrade_attempted || self.upgrade.is_some() || self.pending.is_some() {
            return;
        }
        let mut queue = self.state.outdated_adapters();
        if queue.is_empty() {
            return;
        }
        // Popped from the back, so reverse to keep page order.
        queue.reverse();
        let Some(current) = queue.pop() else { return };
        self.upgrade_attempted = true;
        // The broker runs this identical sweep at startup, and an update that
        // restarts the app starts both within seconds of each other (#56).
        // Whoever loses that race deletes a payload directory the winner's
        // child is running out of. Stand down: the holder is doing this work.
        if !sweep_lock::acquire() {
            return;
        }
        self.sweep_held = true;
        self.upgrade = Some(Upgrade {
            current,
            queue,
            done: Vec::new(),
            failed: false,
        });
        self.start_upgrade_step(context, current);
    }

    /// `integration <id> enable` on an installed-but-outdated marker reinstalls
    /// the payload, so one enable per adapter is the whole upgrade. It is
    /// transactional and idempotent: if the broker already ran it at logon,
    /// this exits 0 with nothing to do, which is a success.
    fn start_upgrade_step(&mut self, context: &ComponentContext<Self>, integration: Integration) {
        self.start_controller(
            context,
            UPGRADE_ACTION,
            true,
            vec![
                "integration".to_string(),
                integration.id().to_string(),
                "enable".to_string(),
            ],
        );
    }

    /// Records one finished step and either starts the next or reports the
    /// whole upgrade once.
    fn advance_upgrade(&mut self, succeeded: bool, context: &ComponentContext<Self>) {
        let Some(mut upgrade) = self.upgrade.take() else {
            return;
        };
        if succeeded {
            upgrade.done.push(upgrade.current);
        } else {
            upgrade.failed = true;
        }
        if let Some(next) = upgrade.queue.pop() {
            upgrade.current = next;
            self.upgrade = Some(upgrade);
            self.start_upgrade_step(context, next);
            return;
        }
        self.notice = Some(if upgrade.failed {
            Notice::new(
                InfoBarSeverity::Error,
                "Some terminal integrations could not be updated",
                "Turn the affected integration off and on again on the Terminals page, \
                 or press Repair integrations.",
            )
        } else {
            let names: Vec<&str> = upgrade
                .done
                .iter()
                .map(|integration| integration.display_name())
                .collect();
            let verb = if names.len() == 1 { "is" } else { "are" };
            Notice::new(
                InfoBarSeverity::Success,
                "Terminal integrations updated",
                format!("{} {verb} now on {FSW_VERSION}", names.join(", ")),
            )
        });
        // Not the update-available notice, so dismissal must not clear the
        // persisted AvailableUpdate value.
        self.notice_is_update = false;
        // The upgrade queue has drained; now sweep any remaining hygiene
        // problems (orphaned/duplicate blocks the version bump did not touch).
        self.start_repair_sweep(context);
        Self::refresh(context);
    }

    /// Starts the one-shot detect-and-repair sweep (#37): `fwdslash
    /// repair-adapters` off the UI thread, then a state reload so a healed
    /// adapter shows healthy. No-op after the first call.
    fn start_repair_sweep(&mut self, context: &ComponentContext<Self>) {
        if self.repair_started {
            return;
        }
        self.repair_started = true;
        // The upgrade queue already holds the sweep lock when it hands over to
        // this; when there was no queue, take it here. Either way the launch
        // sweep runs under one lock from end to end (#56).
        if !self.sweep_held {
            if !sweep_lock::acquire() {
                return;
            }
            self.sweep_held = true;
        }
        context.spawn_background(|_| {
            let _ = run_controller(["repair-adapters"]);
            Msg::LaunchSweepFinished(State::read())
        });
    }
}

impl Component for SettingsModel {
    type Input = Section;
    type Message = Msg;

    fn create(input: &Self::Input, context: &ComponentContext<Self>) -> Self {
        // A packaged build runs nothing at install time and its startup task only
        // fires at logon, so opening this window is the first chance to arm the
        // broker. Without this a Store install does nothing at all. Off the UI
        // thread: the probe spins for up to 2 s waiting for the broker window.
        context.spawn_background(|_| {
            // The launch sweep's first job (issue #52): a packaged app writes
            // HKCU into its own private hive, which the shell adapters — an
            // unpackaged fwdslash.exe — cannot see. Mirror what this install
            // holds into the real hive before the broker starts reading it.
            // Unpackaged, and packaged once the hives agree, this does
            // nothing.
            let _ = sync_settings_to_real_hive();
            ensure_broker_running();
            Msg::BrokerProbed
        });
        // The daily update check, for both flavors now: the CLI owns every
        // route, asks the Store for the Store build and the releases API for
        // the GitHub one, and enforces the 24 h cadence itself (no `--force`
        // here). Off the UI thread, because a Store round trip or a `curl.exe`
        // timeout would otherwise stall the first frame. Silent unless it
        // finds something — see `Msg::UpdateCheckFinished`.
        if update::update_check_allowed(has_package_identity(), update::read_auto_update_enabled()) {
            context.spawn_background(|_| {
                let (code, stdout, _) = run_controller_code(["update", "check", "--json"]);
                Msg::UpdateCheckFinished {
                    outcome: check_outcome(code, &stdout),
                    explicit: false,
                }
            });
        }
        let mut model = Self {
            section: *input,
            pane_open: false,
            color_scheme: ColorScheme::Dark,
            // The only synchronous read: the first frame has nothing to show
            // without it.
            state: State::read(),
            root_draft: String::new(),
            folder_selected: false,
            notice_is_update: false,
            notice: None,
            pending: None,
            upgrade: None,
            upgrade_attempted: false,
            repair_started: false,
            sweep_held: false,
            external_reads: state_watch::ReadCoalescer::default(),
        };
        // Listen for state this window did not change (#55): the CLI, the
        // broker's tray toggle, a shell adapter's staged copy of `fwdslash`.
        // The listener window is created on its own thread; a failure to
        // create it leaves the safety poll, so nothing here is fatal.
        state_watch::start();
        Self::arm_state_watch(context);
        // An outdated adapter is repaired on sight, at the first state the
        // window ever sees.
        model.maybe_start_upgrade(context);
        // If nothing is being upgraded, sweep the adapters' hygiene now;
        // otherwise the sweep runs when the upgrade queue drains, so the two
        // never reinstall the same adapter at once (#37).
        if model.upgrade.is_none() {
            model.start_repair_sweep(context);
        }
        model
    }

    fn update(&mut self, message: Self::Message, context: &ComponentContext<Self>) {
        match message {
            Msg::Navigate(Some(tag)) => {
                let section = Section::from_tag(&tag);
                if section == self.section {
                    return;
                }
                self.section = section;
                // Stands in for the C++ `window_.Activated` refresh, which reactor
                // has no equivalent for. See docs/divergences.md. Every page,
                // About included -- its Components card is live state, and the
                // read is off the UI thread.
                Self::refresh(context);
            }
            // A cleared selection is WinUI normalizing, never a user choice.
            Msg::Navigate(None) => {}
            Msg::PaneOpenChanged(open) => self.pane_open = open,
            Msg::ColorSchemeChanged(scheme) => self.color_scheme = scheme,

            Msg::ToggleGlobal(enabled) => {
                if enabled != self.state.disabled {
                    // The switch already shows the stored state; nothing changed.
                    return;
                }
                self.start_controller(
                    context,
                    if enabled {
                        "Resolution enabled"
                    } else {
                        "Resolution disabled"
                    },
                    false,
                    vec![if enabled { "enable" } else { "disable" }.to_string()],
                );
            }

            // Both radios share a group, so checking one unchecks the other and
            // WinUI raises the sibling's handler with `false`. Only the transition
            // to checked is a user action, which is what the C++ `Checked` handler
            // sees; and re-checking the already-active mode is not a change at all.
            Msg::BareSlashListChecked(checked) => {
                // A folder root can coexist with either underlying mode, so
                // the echo guard must account for it: picking this radio
                // while a root is live is a real change, not an echo.
                if !checked
                    || (self.state.is_list_mode() && self.state.root.is_none() && !self.folder_selected)
                {
                    return;
                }
                self.folder_selected = false;
                self.start_controller(
                    context,
                    "Bare slash shows all distributions",
                    false,
                    vec!["bare-slash".to_string(), "list".to_string()],
                );
            }
            Msg::BareSlashDefaultChecked(checked) => {
                if !checked
                    || (!self.state.is_list_mode() && self.state.root.is_none() && !self.folder_selected)
                {
                    return;
                }
                self.folder_selected = false;
                self.start_controller(
                    context,
                    "Bare slash opens the default distribution",
                    false,
                    vec!["bare-slash".to_string(), "default".to_string()],
                );
            }
            Msg::FolderRadioChecked(checked) => {
                // Selecting the radio only reveals the folder controls; the
                // Apply button validates and commits. Unchecking is the WinUI
                // echo of another radio winning.
                if !checked || self.folder_selected || self.state.root.is_some() {
                    return;
                }
                self.folder_selected = true;
                // Deterministic re-render: every other view-input change ends
                // in a refresh, so this one must too (divergences #5 flow).
                Self::refresh(context);
                return;
            }
            Msg::RootTextChanged(text) => {
                if self.root_draft == text {
                    return;
                }
                self.root_draft = text;
            }
            Msg::BrowseRoot => {
                // Modal picker on the UI thread; it pumps its own messages.
                let Some(path) = folder_picker::pick_folder() else {
                    return; // cancelled
                };
                self.root_draft = path.clone();
                self.folder_selected = true;
                if !is_valid_windows_root(&path) || Some(path.as_str()) == self.state.root.as_deref()
                {
                    return;
                }
                self.start_controller(
                    context,
                    ROOT_ACTION,
                    false,
                    vec!["bare-slash".to_string(), "root".to_string(), path],
                );
            }
            Msg::ApplyRoot => {
                // Echo guard: re-pressing Apply with an unchanged draft must
                // not re-invoke the controller (divergences #5).
                let candidate = self.root_draft.trim();
                if Some(candidate) == self.state.root.as_deref() {
                    return;
                }
                if !is_valid_windows_root(candidate) {
                    self.notice = Some(Notice::new(
                        InfoBarSeverity::Error,
                        "Could not set the folder root",
                        "Use an absolute path like C:\\code or \\\\wsl.localhost\\Ubuntu\\home\\me.",
                    ));
                    return;
                }
                let candidate = candidate.to_string();
                self.start_controller(
                    context,
                    ROOT_ACTION,
                    false,
                    vec!["bare-slash".to_string(), "root".to_string(), candidate],
                );
            }

            Msg::SelectDistribution(index) => {
                // `None` means the items source was replaced and WinUI reset the
                // selection to -1. Acting on it would clear the user's pin.
                let Some(index) = index else { return };
                if index == self.state.distribution_index() {
                    return;
                }
                let mut arguments = vec!["bare-slash".to_string(), "default".to_string()];
                if index > 0 {
                    let Some(distribution) = self.state.distributions.get(index - 1) else {
                        return;
                    };
                    arguments.push(distribution.clone());
                }
                self.start_controller(context, "Bare slash default updated", false, arguments);
            }

            Msg::ToggleIntegration(integration, enabled) => {
                if enabled == integration.installed(&self.state) {
                    return;
                }
                self.start_controller(
                    context,
                    integration.action(enabled),
                    integration.is_terminal(),
                    vec![
                        "integration".to_string(),
                        integration.id().to_string(),
                        if enabled { "enable" } else { "disable" }.to_string(),
                    ],
                );
            }

            Msg::OpenWslRoot => {
                if self.pending.is_some() {
                    return;
                }
                self.pending = Some(OPEN_WSL_ACTION);
                context.spawn_background(|_| Msg::ShellOpenFinished {
                    action: OPEN_WSL_ACTION,
                    succeeded: open_wsl_root(),
                });
            }
            Msg::ShellOpenFinished { action, succeeded } => {
                self.pending = None;
                if !succeeded {
                    self.show_result(false, action, false, "");
                }
            }
            Msg::RefreshStatus => {
                Self::refresh(context);
                self.show_result(true, "Status refreshed", false, "");
            }
            Msg::CheckForUpdates => {
                if self.pending.is_some() {
                    return;
                }
                // `--force` bypasses the CLI's 24 h cadence: the user pressed a
                // button labelled "Check now" and deserves a round trip rather
                // than yesterday's answer.
                self.pending = Some(CHECK_ACTION);
                context.spawn_background(|_| {
                    let (code, stdout, _) =
                        run_controller_code(["update", "check", "--force", "--json"]);
                    Msg::UpdateCheckFinished {
                        outcome: check_outcome(code, &stdout),
                        explicit: true,
                    }
                });
            }
            Msg::UpdateCheckFinished { outcome, explicit } => {
                if explicit {
                    self.pending = None;
                }
                match outcome {
                    UpdateOutcome::Ready(tag) => {
                        let short = tag.strip_prefix('v').unwrap_or(&tag);
                        self.notice = Some(Notice::new(
                            InfoBarSeverity::Informational,
                            "Update available",
                            if self.state.store_flavor {
                                format!(
                                    "Version {short} is available in the Microsoft Store. \
                                     Install it now, or let the Store install it on its own \
                                     schedule."
                                )
                            } else {
                                format!(
                                    "Version {short} was downloaded. It applies after you sign \
                                     out and back in, or restart Forward Slash Windows now."
                                )
                            },
                        ));
                        self.notice_is_update = true;
                    }
                    // Everything else is silent unless the user asked: the
                    // launch check is background work they did not request.
                    UpdateOutcome::UpToDate | UpdateOutcome::NotDue if explicit => {
                        self.notice = Some(Notice::new(
                            InfoBarSeverity::Success,
                            "Up to date",
                            format!("Forward Slash Windows {FSW_VERSION} is the latest version."),
                        ));
                        self.notice_is_update = false;
                    }
                    UpdateOutcome::Unavailable if explicit => {
                        self.notice = Some(Notice::new(
                            InfoBarSeverity::Informational,
                            "Could not check for updates",
                            "The update service could not be reached. The check runs again on \
                             its own.",
                        ));
                        self.notice_is_update = false;
                    }
                    // A silent check that concluded nothing: no bar, but the
                    // refresh below still runs.
                    _ => {}
                }
                // The CLI wrote LastUpdateCheck and AvailableUpdate, and a
                // GitHub check may have downloaded the bundle: everything the
                // About card and the install banner render just changed. This
                // happens whatever the outcome, including the silent ones —
                // "Last update check" is exactly the line a check that found
                // nothing has to move.
                Self::refresh(context);
            }
            Msg::InstallUpdate => {
                if self.pending.is_some() {
                    return;
                }
                self.pending = Some(INSTALL_ACTION);
                context.spawn_background(|_| {
                    let broker_window_before_install = broker_window_exists();
                    // Ask the broker to close first so it removes its
                    // notification icon itself; a forced shutdown by the
                    // installer would leave a ghost icon behind. It waits up to
                    // 3 s, which is one reason this is off the UI thread.
                    close_broker_window();
                    // The apply itself lives in the CLI: it stages the
                    // identity-less helper, registers the relaunch watchdog and
                    // only then lets the install force this package down.
                    // `--force` because the user just asked for it explicitly.
                    let (code, stdout, stderr) = run_controller_code([
                        "update", "install", "--force", "--relaunch", "app", "--json",
                    ]);
                    // Exit 0 hands control to the update machinery, including
                    // its watchdog restart. Every other result leaves this
                    // process alive, so restore only the broker that was
                    // resident before the attempted install. Its persisted
                    // disabled flag preserves an existing pause preference.
                    if should_restore_broker_after_install(code, broker_window_before_install) {
                        ensure_broker_running();
                    }
                    Msg::UpdateInstallFinished {
                        code,
                        stdout,
                        stderr,
                    }
                });
            }
            Msg::UpdateInstallFinished {
                code,
                stdout,
                stderr,
            } => {
                self.pending = None;
                // The JSON line is the CLI's own record; the exit code is the
                // contract this window acts on.
                let _ = stdout;
                let Some(notice) = install_notice(code, &stderr) else {
                    // The install is under way and will force this package
                    // down. Leave through the window's own close path, the
                    // reactor's only process-exit route, so the watchdog's
                    // relaunch is not racing a half-closed window.
                    request_close();
                    return;
                };
                self.notice = Some(notice);
                self.notice_is_update = false;
                Self::refresh(context);
            }
            Msg::OpenStorePage => {
                if self.pending.is_some() {
                    return;
                }
                self.pending = Some(OPEN_STORE_ACTION);
                context.spawn_background(|_| Msg::ShellOpenFinished {
                    action: OPEN_STORE_ACTION,
                    succeeded: shell_open(&store_product_uri()),
                });
            }
            Msg::RepairIntegrations => {
                if self.pending.is_some() {
                    return;
                }
                self.pending = Some(REPAIR_ACTION);
                context.spawn_background(|_| {
                    // Serialised against the broker's own sweep (#56): the two
                    // would otherwise delete a payload tree out from under each
                    // other's child process.
                    let Some(_lock) = sweep_lock::acquire_guard() else {
                        return Msg::RepairFinished(RepairOutcome::Busy);
                    };
                    let (succeeded, detail) = run_controller_detailed(["repair-adapters"]);
                    Msg::RepairFinished(if succeeded {
                        RepairOutcome::Repaired
                    } else {
                        RepairOutcome::Failed(detail)
                    })
                });
            }
            Msg::RepairFinished(outcome) => {
                self.pending = None;
                self.notice = Some(match outcome {
                    RepairOutcome::Repaired => Notice::new(
                        InfoBarSeverity::Success,
                        "Integrations repaired",
                        "Terminal integrations were checked and brought up to date. Reopen \
                         affected terminals.",
                    ),
                    RepairOutcome::Busy => Notice::new(
                        InfoBarSeverity::Informational,
                        "Integrations are already being updated",
                        "A background update of the terminal integrations is running. Try \
                         again in a moment.",
                    ),
                    RepairOutcome::Failed(detail) => Notice::new(
                        InfoBarSeverity::Error,
                        "Could not repair integrations",
                        if detail.trim().is_empty() {
                            "The integrations could not be repaired.".to_string()
                        } else {
                            detail.trim().to_string()
                        },
                    ),
                });
                self.notice_is_update = false;
                Self::refresh(context);
            }
            Msg::SetAutoUpdate(enabled) => {
                if enabled == self.state.auto_update || self.pending.is_some() {
                    return;
                }
                self.pending = Some(AUTO_UPDATE_ACTION);
                context.spawn_background(move |_| Msg::AutoUpdateFinished {
                    enabled,
                    succeeded: update::set_auto_update_enabled(enabled).is_ok(),
                });
            }
            Msg::AutoUpdateFinished { enabled, succeeded } => {
                self.pending = None;
                self.show_result(
                    succeeded,
                    if enabled {
                        "Automatic updates enabled"
                    } else {
                        "Automatic updates disabled"
                    },
                    false,
                    "",
                );
                // Deterministic, rather than waiting on the broadcast the write
                // just fired (#55): the switch renders `state.auto_update`, and
                // a failed write must leave it showing what is actually stored.
                Self::refresh(context);
            }
            Msg::DismissNotice => {
                if self.notice_is_update {
                    // An update notice is persisted until a newer check
                    // replaces it; dismissal clears that too.
                    let _ = update::dismiss_update();
                    self.notice_is_update = false;
                }
                self.notice = None;
            }
            Msg::ControllerFinished {
                action,
                terminal,
                succeeded,
                detail,
            } => {
                self.pending = None;
                if action == UPGRADE_ACTION {
                    // The upgrade reports itself once, when the queue drains.
                    self.advance_upgrade(succeeded, context);
                    return;
                }
                if succeeded && action == ROOT_ACTION {
                    self.folder_selected = false;
                }
                self.show_result(succeeded, action, terminal, &detail);
                Self::refresh(context);
            }
            Msg::StateLoaded(state) => {
                self.state = state;
                self.maybe_start_upgrade(context);
            }
            Msg::LaunchSweepFinished(state) => {
                // The whole launch sweep is over; let the broker have the lock.
                if self.sweep_held {
                    sweep_lock::release();
                    self.sweep_held = false;
                }
                self.state = state;
                self.maybe_start_upgrade(context);
            }

            Msg::ExternalStateChanged(wake) => {
                // Re-arm first: every path below can return, and the watch has
                // to outlive all of them.
                Self::arm_state_watch(context);
                if !state_watch::should_read(wake, state_watch::window_visible()) {
                    return;
                }
                if self.external_reads.wake() {
                    Self::read_external_state(context);
                }
            }
            Msg::StateRefreshed(state) => {
                // A notification arrived while this read was running: one more
                // read covers whatever it did not see.
                if self.external_reads.finished() {
                    Self::read_external_state(context);
                }
                // The poll runs forever and finds nothing almost every time.
                // Assigning an equal state would republish the whole view for
                // no reason -- and, worse, restart the upgrade check on a
                // timer.
                if state == self.state {
                    return;
                }
                self.state = state;
                self.maybe_start_upgrade(context);
            }
            Msg::BrokerProbed => Self::refresh(context),
        }
    }

    fn view(&self, _input: &Self::Input, context: &mut ViewContext<Self>) -> View {
        context.window_title("Forward Slash Windows");
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .icon_resource(IDI_FSW_APP)
                .theme(match self.color_scheme {
                    ColorScheme::Dark => WindowTheme::Dark,
                    ColorScheme::Light => WindowTheme::Light,
                }),
        );
        context.on_color_scheme(context.callback(Msg::ColorSchemeChanged));

        // The C++ sets TitleBar.IconSource (main.cpp:387-392). ImageIconSource
        // fail-fasts under the unpackaged Windows App SDK, so the icon goes in the
        // TitleBar's LeftHeader slot instead: same leading-edge position (the
        // control draws it ahead of the title), automatic drag regions because it
        // is non-interactive content inside the TitleBar, and ImageIcon decodes
        // embedded PNG bytes in place -- the one route that never constructs the
        // fail-fasting ImageIconSource. The window icon proper (taskbar and
        // Alt-Tab) is applied from the IDI_FSW_APP resource in app.rc via
        // WM_SETICON, as src/settings/main.cpp does against the HWND.
        let title_bar = TitleBar::new()
            .title("Forward Slash Windows")
            // On the TitleBar, not the NavigationView (main.cpp:386).
            .is_pane_toggle_button_visible(false)
            .grid_row(0)
            .slot(
                TitleBarSlot::LeftHeader,
                ImageIcon::new()
                    .source_data(EncodedImage::from_static(include_bytes!(
                        "../../../assets/fwdslash-titlebar.png"
                    )))
                    .height(16.0)
                    .margin(Thickness::new(16.0, 0.0, 12.0, 0.0)),
            );

        let navigation = NavigationView::new()
            // The C++ pins LeftCompact (main.cpp:396), which forces WinUI's DisplayMode
            // to Compact and therefore SplitView CompactOverlay: opening the pane draws
            // it *over* the page and clips the text mid-word. `Left` forces DisplayMode
            // Expanded / SplitView CompactInline instead: same 48px icon rail while
            // closed, content pushed aside when open. Deliberate divergence, recorded
            // in docs/divergences.md.
            .pane_display_mode(NavigationViewPaneDisplayMode::Left)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .is_settings_visible(false)
            .is_pane_open(self.pane_open)
            // The documented default (NavigationView page: "don't override").
            .open_pane_length(320.0)
            .grid_row(1)
            .on_is_pane_open_changed(context.callback(Msg::PaneOpenChanged))
            .on_selected_tag_changed(context.callback(Msg::Navigate))
            .slots([
                SlotView::collection(
                    NavigationViewSlot::MenuItems,
                    [
                        self.nav_item(Section::General, "General", Symbol::Home),
                        self.nav_item(Section::Windows, "Windows", Symbol::Folder),
                        self.nav_item(Section::Terminals, "Terminals", Symbol::AllApps),
                        self.nav_item(Section::About, "About", Symbol::Help),
                    ],
                ),
                SlotView::new(NavigationViewSlot::Content, self.surface(context)),
            ]);

        Grid::new()
            .rows([GridLength::Auto, GridLength::Star(1.0)])
            .children((title_bar, navigation))
    }
}

impl SettingsModel {
    fn nav_item(&self, section: Section, label: &'static str, symbol: Symbol) -> KeyedView {
        KeyedView::new(
            section.tag(),
            NavigationViewItem::new()
                .tag(section.tag())
                .is_selected(section == self.section)
                .slots([
                    SlotView::new(NavigationViewItemSlot::Content, TextBlock::new().text(label)),
                    SlotView::new(
                        NavigationViewItemSlot::Icon,
                        SymbolIcon::new().symbol(symbol),
                    ),
                ]),
        )
    }

    /// The content surface: an `InfoBar` in a fixed row above a scrolling page, so a
    /// notification never pushes the page down or scrolls out of view
    /// (`src/settings/main.cpp:410-422`).
    fn surface(&self, context: &mut ViewContext<Self>) -> View {
        let notice: View = match &self.notice {
            Some(notice) => InfoBar::new()
                .title(notice.title)
                .message(&notice.message)
                .severity(notice.severity)
                .is_open(true)
                .is_closable(true)
                .margin(Thickness::new(0.0, 0.0, 0.0, 16.0))
                .grid_row(0)
                .on_closed(context.callback(|()| Msg::DismissNotice))
                .into(),
            None => InfoBar::new()
                .is_open(false)
                .is_closable(true)
                .margin(Thickness::new(0.0, 0.0, 0.0, 16.0))
                .grid_row(0)
                .into(),
        };

        let page = match self.section {
            Section::General => self.view_general(context),
            Section::Windows => self.view_windows(context),
            Section::Terminals => self.view_terminals(context),
            Section::About => self.view_about(),
        };

        Border::new()
            .padding(Thickness::uniform(24.0))
            .content(
                Grid::new()
                    .rows([GridLength::Auto, GridLength::Auto, GridLength::Star(1.0)])
                    .children((
                        notice,
                        self.banners(context),
                        ScrollViewer::new()
                            .grid_row(2)
                            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                            .content(page),
                    )),
            )
    }

    /// The second fixed row: standing notices and actions that are derived from
    /// state rather than from a single completed command, plus the in-flight
    /// indicator. Kept out of `self.notice` so a routine "Updated" result never
    /// hides the adapter-upgrade progress, and vice versa.
    ///
    /// Reactor's `InfoBar` exposes no action-button slot, so each action is a
    /// `Button` rendered directly beneath its bar.
    fn banners(&self, context: &mut ViewContext<Self>) -> View {
        // Both flavors now: the Store build drives the Store's own installer
        // through the CLI, the GitHub build registers the bundle it already
        // downloaded.
        let install_label = install_banner_label(
            self.state.packaged,
            self.state.store_flavor,
            self.state.update_bundle_ready,
            self.state.update_tag.is_some(),
        );
        let notice_action = self.notice.as_ref().and_then(|notice| notice.action);
        if self.upgrade.is_none()
            && install_label.is_none()
            && notice_action.is_none()
            && self.pending.is_none()
        {
            return View::empty();
        }

        // Progress only -- there is no button, because there is no decision to
        // make. The result lands in the dismissible notice above.
        let upgrade_notice: View = match &self.upgrade {
            Some(upgrade) => InfoBar::new()
                .title("Updating terminal integrations\u{2026}")
                .message(format!(
                    "{} adapter \u{2192} {FSW_VERSION}",
                    upgrade.current.display_name()
                ))
                .severity(InfoBarSeverity::Informational)
                .is_open(true)
                .is_closable(false)
                .into(),
            None => View::empty(),
        };
        let install_action: View = match install_label {
            Some(label) => Button::new()
                .is_enabled(self.controls_enabled())
                .horizontal_alignment(HorizontalAlignment::Left)
                .on_click(context.message(Msg::InstallUpdate))
                .content(label)
                .into(),
            None => View::empty(),
        };
        // Reactor's InfoBar has no action slot, so the notice's action is a
        // button of its own directly under the bar.
        let notice_action: View = match notice_action {
            Some(action @ NoticeAction::OpenStore) => Button::new()
                .horizontal_alignment(HorizontalAlignment::Left)
                .on_click(context.message(Msg::OpenStorePage))
                .content(action.label())
                .into(),
            None => View::empty(),
        };
        let progress: View = if self.pending.is_some() {
            ProgressRing::new()
                .is_active(true)
                .is_indeterminate(true)
                .width(20.0)
                .height(20.0)
                .horizontal_alignment(HorizontalAlignment::Left)
                .into()
        } else {
            View::empty()
        };

        StackPanel::new()
            .spacing(8.0)
            .margin(Thickness::new(0.0, 0.0, 0.0, 16.0))
            .grid_row(1)
            .children((upgrade_notice, notice_action, install_action, progress))
    }

    fn view_general(&self, context: &mut ViewContext<Self>) -> View {
        let state = &self.state;
        // The folder choice: the app's first free-form input. Reactor's
        // TextBox has no submit-on-enter, so an explicit Apply button commits
        // the draft; validation failure keeps the previous root.
        let folder_picker: View = if !state.is_folder_mode() && !self.folder_selected {
            View::empty()
        } else {
            StackPanel::new()
                .spacing(8.0)
                .children((
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(8.0)
                        .children((
                            TextBox::new()
                                .min_width(240.0)
                                .placeholder_text(r"C:\code or \\wsl.localhost\Ubuntu\home")
                                .text(self.root_draft.clone())
                                .is_enabled(self.controls_enabled())
                                .on_text_changed(context.callback(Msg::RootTextChanged)),
                            Button::new()
                                .is_enabled(self.controls_enabled())
                                .on_click(context.message(Msg::BrowseRoot))
                                .content("Browse\u{2026}"),
                            Button::new()
                                .is_enabled(self.controls_enabled())
                                .on_click(context.message(Msg::ApplyRoot))
                                .content("Apply folder"),
                        )),
                    body("Typing / opens this folder; /name goes inside it.")
                        .foreground(ThemeBrush::TextSecondary),
                ))
        };

        let picker: View = if state.is_list_mode() || state.is_folder_mode() || self.folder_selected
        {
            View::empty()
        } else {
            StackPanel::new()
                .spacing(8.0)
                .children((
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(8.0)
                        .children((
                            body("Distribution:").vertical_alignment(VerticalAlignment::Center),
                            ComboBox::new()
                                .min_width(240.0)
                                .automation_name("Default distribution for bare slash")
                                .items_source(state.distribution_options())
                                .selected_index(Some(state.distribution_index()))
                                .is_enabled(self.controls_enabled())
                                .on_selection_changed(
                                    context.callback(Msg::SelectDistribution),
                                ),
                        )),
                    body(state.bare_target_caption()).foreground(ThemeBrush::TextSecondary),
                ))
        };

        page_stack(16.0)
            .children((
                page_header(
                    "Forward Slash Windows",
                    "Use Linux-style WSL paths in the Windows places you choose.",
                ),
                toggle_card(
                    "Forward-slash resolution",
                    "Disable temporarily without removing selected integrations.",
                    ToggleSwitch::new()
                        .is_on(!state.disabled)
                        .is_enabled(self.controls_enabled())
                        .automation_name("Enable forward-slash resolution")
                        .on_toggled(context.callback(Msg::ToggleGlobal))
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center)
                        .slots([
                            SlotView::new(
                                ToggleSwitchSlot::OnContent,
                                TextBlock::new().text("Enabled"),
                            ),
                            SlotView::new(
                                ToggleSwitchSlot::OffContent,
                                TextBlock::new().text("Disabled"),
                            ),
                        ]),
                ),
                card(StackPanel::new().spacing(8.0).children((
                    strong("Bare slash ( / ) behavior"),
                    body("Choose what typing only / means on enabled surfaces.").foreground(ThemeBrush::TextSecondary),
                    RadioButton::new()
                        .group_name("BareSlashMode")
                        .automation_name("Show all distributions")
                        .is_enabled(self.controls_enabled())
                        .is_checked(state.is_list_mode() && state.root.is_none() && !self.folder_selected)
                        .on_checked(context.callback(Msg::BareSlashListChecked))
                        .content("Show all distributions"),
                    RadioButton::new()
                        .group_name("BareSlashMode")
                        .automation_name("Open my default distribution")
                        .is_enabled(self.controls_enabled())
                        .is_checked(!state.is_list_mode() && state.root.is_none() && !self.folder_selected)
                        .on_checked(context.callback(Msg::BareSlashDefaultChecked))
                        .content("Open my default distribution"),
                    RadioButton::new()
                        .group_name("BareSlashMode")
                        .automation_name("Open a folder I choose")
                        .is_enabled(self.controls_enabled())
                        .is_checked(state.is_folder_mode() || self.folder_selected)
                        .on_checked(context.callback(Msg::FolderRadioChecked))
                        .content("Open a folder I choose"),
                    picker,
                    folder_picker,
                ))),
                // Automatic updates: both flavors, for any packaged build. The
                // Store build asks the Store for its own update instead of
                // GitHub, and is off by default — the Store already updates
                // the app on its own schedule, so driving it from in here is
                // something the user opts into.
                if state.packaged {
                    toggle_card_detail(
                        "Automatic updates",
                        if state.store_flavor {
                            "Let fwdslash install Store updates in the background. Off by \
                             default; the Store still updates the app on its own schedule."
                        } else {
                            "Check GitHub daily and install new versions automatically."
                        },
                        Some(state.last_check_line()),
                        ToggleSwitch::new()
                            .is_on(state.auto_update)
                            .is_enabled(self.controls_enabled())
                            .automation_name("Automatic updates")
                            .on_toggled(context.callback(Msg::SetAutoUpdate))
                            .grid_column(1)
                            .vertical_alignment(VerticalAlignment::Center),
                    )
                } else {
                    View::empty()
                },
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(12.0)
                    .children((
                        Button::new()
                            .on_click(context.message(Msg::OpenWslRoot))
                            .content("Open WSL root"),
                        Button::new()
                            .on_click(context.message(Msg::RefreshStatus))
                            .content("Refresh status"),
                        // Independent of the Automatic updates switch: asking
                        // once, now, is not the same decision as checking every
                        // day. Hidden only where there is no package to update.
                        if state.packaged {
                            Button::new()
                                .is_enabled(self.controls_enabled())
                                .automation_name("Check for updates now")
                                .on_click(context.message(Msg::CheckForUpdates))
                                .content("Check now")
                                .into()
                        } else {
                            View::empty()
                        },
                    )),
                body(state.status_text()).foreground(ThemeBrush::TextSecondary),
            ))
    }

    fn view_windows(&self, context: &mut ViewContext<Self>) -> View {
        let state = &self.state;
        // Windows owns logon start for a packaged build, so the toggle would have
        // nothing to write; point at where the user can actually change it.
        let managed: View = if state.packaged {
            body("Installed with the app. Turn startup on or off under Settings > Apps > Startup.")
                .foreground(ThemeBrush::TextSecondary)
                .into()
        } else {
            View::empty()
        };

        page_stack(16.0)
            .children((
                page_header(
                    "Windows surfaces",
                    "Native navigation through the address bar and shell entry points.",
                ),
                toggle_card(
                    "Explorer, Run, and Search",
                    "Installs the per-user broker and startup entry. Turning this off stops the \
                     broker and removes its startup registration.",
                    integration_toggle(Integration::Windows, self, context)
                        .is_enabled(!state.packaged && self.controls_enabled())
                        .automation_name("Install Windows surface integration"),
                ),
                body(
                    "Invalid slash paths are blocked instead of being sent to Edge or web search.",
                )
                .foreground(ThemeBrush::TextSecondary),
                managed,
            ))
    }

    fn view_terminals(&self, context: &mut ViewContext<Self>) -> View {
        let state = &self.state;
        page_stack(12.0)
            .children((
                page_header(
                    "Terminal integrations",
                    "Each shell is independent and can be removed without changing the others.",
                ),
                toggle_card_detail(
                    "Command Prompt",
                    "Adds reversible dir and ls DOSKEY adapters for new cmd.exe sessions.",
                    state.adapter_detail(Integration::Cmd),
                    integration_toggle(Integration::Cmd, self, context)
                        .automation_name("Install Command Prompt integration"),
                ),
                toggle_card_detail(
                    "Windows PowerShell 5.1",
                    "Adds a guarded profile import and preserves normal Get-ChildItem behavior.",
                    state.adapter_detail(Integration::WindowsPowerShell),
                    integration_toggle(Integration::WindowsPowerShell, self, context)
                        .automation_name("Install Windows PowerShell integration"),
                ),
                toggle_card_detail(
                    "PowerShell 7",
                    if state.powershell7_available {
                        "Adds the same reversible adapter to the PowerShell 7 profile."
                    } else {
                        "PowerShell 7 is not installed on this computer."
                    },
                    state.adapter_detail(Integration::PowerShell7),
                    integration_toggle(Integration::PowerShell7, self, context)
                        .is_enabled(
                            (state.powershell7_available
                                || Integration::PowerShell7.installed(state))
                                && self.controls_enabled(),
                        )
                        .automation_name("Install PowerShell 7 integration"),
                ),
                body(
                    "Profile and AutoRun changes apply to newly opened terminal sessions. \
                     Existing sessions retain what they already loaded.",
                )
                .foreground(ThemeBrush::TextSecondary)
                .margin(Thickness::new(0.0, 4.0, 0.0, 0.0)),
                // The button the broker's "could not be updated automatically"
                // balloon points at (#56). Before this the balloon said "Open
                // Settings to retry" and there was nothing to press: the retry
                // happened by accident, in the launch sweep.
                card(StackPanel::new().spacing(8.0).children((
                    strong("Repair integrations"),
                    body(
                        "Re-applies each installed adapter and cleans up an orphaned or \
                         duplicated block left by an interrupted update.",
                    )
                    .foreground(ThemeBrush::TextSecondary),
                    Button::new()
                        .is_enabled(self.controls_enabled())
                        .horizontal_alignment(HorizontalAlignment::Left)
                        .automation_name("Repair integrations")
                        .on_click(context.message(Msg::RepairIntegrations))
                        .content("Repair integrations"),
                ))),
            ))
    }

    /// The running package version (the MSIX identity when packaged, the crate
    /// version otherwise) and the package architecture.
    fn package_label() -> String {
        format!(
            "{} ({})",
            package_version().as_deref().unwrap_or(FSW_VERSION),
            package_architecture()
                .unwrap_or_else(|| env::var("PROCESSOR_ARCHITECTURE").unwrap_or_default()),
        )
    }

    /// What is actually deployed on this machine, one line per component.
    ///
    /// Rendered from `State`, which `Msg::Navigate` refreshes for this page too:
    /// the broker line and the three adapter versions are live.
    fn components_card(&self) -> View {
        let state = &self.state;
        card(StackPanel::new().spacing(4.0).children((
            strong("Components"),
            body(state.broker_component_line()).foreground(ThemeBrush::TextSecondary),
            body(state.driver_component_line()).foreground(ThemeBrush::TextSecondary),
            body(state.adapter_component_line(Integration::Cmd))
                .foreground(ThemeBrush::TextSecondary),
            body(state.adapter_component_line(Integration::WindowsPowerShell))
                .foreground(ThemeBrush::TextSecondary),
            body(state.adapter_component_line(Integration::PowerShell7))
                .foreground(ThemeBrush::TextSecondary),
            body(format!("Package: {}", Self::package_label()))
                .foreground(ThemeBrush::TextSecondary),
            body(state.flavor_component_line()).foreground(ThemeBrush::TextSecondary),
            body(state.last_check_line()).foreground(ThemeBrush::TextSecondary),
            match state.update_available_line() {
                Some(line) => body(line).foreground(ThemeBrush::TextSecondary).into(),
                None => View::empty(),
            },
        )))
    }

    // The only `expect`s in the crate: `navigate_uri` on a compile-time
    // constant string is infallible by construction.
    #[allow(clippy::expect_used)]
    fn view_about(&self) -> View {
        let subtitle = format!("Forward Slash Windows {}", Self::package_label());
        page_stack(16.0)
            .children((
                // page_header demands &'static str; the subtitle is dynamic.
                StackPanel::new().spacing(4.0).children((
                    TextBlock::new()
                        .text("About")
                        .font_size(28.0)
                        .font_weight(FontWeight::SEMI_BOLD)
                        .text_wrapping(TextWrapping::Wrap),
                    body(&subtitle).foreground(ThemeBrush::TextSecondary),
                )),
                self.components_card(),
                body(
                    "Maps /Distro/path to \\\\wsl.localhost\\Distro\\path, and / to either the \
                     WSL distribution list or your default distribution, on supported Windows \
                     surfaces.",
                ),
                Border::new()
                    .padding(Thickness::new(18.0, 16.0, 18.0, 16.0))
                    .corner_radius(CornerRadius::uniform(8.0))
                    .border_thickness(Thickness::uniform(1.0))
                    .background(ThemeBrush::CardBackground)
                    .border_brush(ThemeBrush::CardStroke)
                    .content(StackPanel::new().spacing(4.0).children((
                        TextBlock::new()
                            .text("Mike Fara")
                            .font_size(16.0)
                            .font_weight(FontWeight::SEMI_BOLD)
                            .text_wrapping(TextWrapping::Wrap),
                        body("Fara Technologies LLC"),
                        body("New York, United States").foreground(ThemeBrush::TextSecondary),
                    ))),
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(8.0)
                    .children((
                        HyperlinkButton::new()
                            .navigate_uri("https://github.com/faratech/fwdslash")
                            .expect("static URI")
                            .content("GitHub repository"),
                        HyperlinkButton::new()
                            .navigate_uri(
                                "https://github.com/faratech/fwdslash/blob/main/LICENSE",
                            )
                            .expect("static URI")
                            .content("MIT License"),
                    )),
                body("Open-source software licensed under the MIT License.").foreground(ThemeBrush::TextSecondary),
            ))
    }
}

// ---------------------------------------------------------------------------
// Shared builders, mirroring the helpers at src/settings/main.cpp:221-298
// ---------------------------------------------------------------------------

/// What one `fwdslash update install` exit code means on screen.
///
/// `None` is the one outcome with nothing to show: the install started, it is
/// about to force this package down, and the window closes instead. Everything
/// else — including "there was nothing to install after all" — leaves the user
/// a sentence, because they pressed a button and are waiting for an answer.
#[must_use]
fn install_notice(code: i32, stderr: &str) -> Option<Notice> {
    Some(match code {
        UPDATE_EXIT_OK => return None,
        UPDATE_EXIT_AVAILABLE => Notice::new(
            InfoBarSeverity::Informational,
            "Update downloaded",
            "It installs the next time fwdslash restarts.",
        ),
        UPDATE_EXIT_NEEDS_USER => Notice::new(
            InfoBarSeverity::Informational,
            "Finish the update in the Microsoft Store",
            "This update has to be installed from the Store on this computer.",
        )
        .with_action(NoticeAction::OpenStore),
        UPDATE_EXIT_NOTHING => Notice::new(
            InfoBarSeverity::Success,
            "Already up to date",
            format!("Forward Slash Windows {FSW_VERSION} is the latest version."),
        ),
        // Anything else is the CLI's error exit, a usage or context error, or
        // a controller that could not be started at all. Its stderr explains an
        // actionable failure the way `ControllerFinished` does.
        _ => {
            let detail = stderr.trim();
            Notice::new(
                InfoBarSeverity::Error,
                "Could not start the update",
                if detail.is_empty() {
                    "The update could not be started.".to_string()
                } else {
                    format!("The update could not be started. {detail}")
                },
            )
        }
    })
}

/// A successful install takes ownership of shutdown and restart through its
/// watchdog. Every other CLI exit returns to this still-running UI, so only a
/// broker that was previously serving input should be brought back.
#[must_use]
fn should_restore_broker_after_install(code: i32, broker_window_before_install: bool) -> bool {
    code != UPDATE_EXIT_OK && broker_window_before_install
}

/// The install banner's button label, or `None` when no banner belongs on
/// screen.
///
/// Unpackaged builds never show it — there is nothing they could install, and
/// a dev build must not offer to replace itself. `bundle_ready` is the GitHub
/// flavor's downloaded `.msixbundle`; `update_available` is the version the
/// last check recorded, which is all the Store flavor ever has locally. Either
/// one is enough: the CLI decides what "install" actually means.
#[must_use]
fn install_banner_label(
    packaged: bool,
    store_flavor: bool,
    bundle_ready: bool,
    update_available: bool,
) -> Option<&'static str> {
    if !packaged || !(bundle_ready || update_available) {
        return None;
    }
    // The Store's installer force-closes the app and the Store's own restart
    // brings it back; the GitHub bundle is registered over a running package,
    // which is a restart the user is agreeing to.
    Some(if store_flavor {
        "Install now"
    } else {
        "Restart to update"
    })
}

fn body(text: impl Into<String>) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(14.0)
        .text_wrapping(TextWrapping::Wrap)
}

fn strong(text: impl Into<String>) -> TextBlock {
    body(text).font_weight(FontWeight::SEMI_BOLD)
}

fn page_stack(spacing: f64) -> StackPanel {
    StackPanel::new()
        .spacing(spacing)
        .max_width(720.0)
        .horizontal_alignment(HorizontalAlignment::Left)
}

fn page_header(title: &'static str, subtitle: &'static str) -> View {
    StackPanel::new().spacing(4.0).children((
        TextBlock::new()
            .text(title)
            .font_size(28.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .text_wrapping(TextWrapping::Wrap),
        body(subtitle).foreground(ThemeBrush::TextSecondary),
    ))
}

fn card(content: impl Into<View>) -> View {
    Border::new()
        .padding(Thickness::uniform(18.0))
        .corner_radius(CornerRadius::uniform(8.0))
        .border_thickness(Thickness::uniform(1.0))
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .content(content)
}

fn toggle_card(title: &'static str, description: &'static str, toggle: impl Into<View>) -> View {
    toggle_card_detail(title, description, None, toggle)
}

/// `toggle_card` with a second, dynamic secondary line -- what the adapter
/// actually has deployed. `toggle_card`'s descriptions are `&'static str`
/// literals; this one is built per render from `State`.
fn toggle_card_detail(
    title: &'static str,
    description: &'static str,
    detail: Option<String>,
    toggle: impl Into<View>,
) -> View {
    let detail: View = match detail {
        Some(text) => body(text).foreground(ThemeBrush::TextSecondary).into(),
        None => View::empty(),
    };
    card(
        Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(20.0)
            .children((
                StackPanel::new()
                    .grid_column(0)
                    .spacing(3.0)
                    .vertical_alignment(VerticalAlignment::Center)
                    .children((
                        strong(title),
                        body(description).foreground(ThemeBrush::TextSecondary),
                        detail,
                    )),
                toggle,
            )),
    )
}

fn integration_toggle(
    integration: Integration,
    model: &SettingsModel,
    context: &mut ViewContext<SettingsModel>,
) -> ToggleSwitch {
    ToggleSwitch::new()
        .is_on(integration.installed(&model.state))
        // Callers narrow this further (the packaged Windows toggle, the missing
        // pwsh.exe toggle); a later `is_enabled` wins, so they must AND this in.
        .is_enabled(model.controls_enabled())
        .grid_column(1)
        .vertical_alignment(VerticalAlignment::Center)
        .on_toggled(
            context.callback(move |enabled: bool| Msg::ToggleIntegration(integration, enabled)),
        )
}

// ---------------------------------------------------------------------------
// Talking to the controller
// ---------------------------------------------------------------------------

/// `fwdslash.exe` beside this executable, as the C++ resolves it
/// (`src/settings/main.cpp:170-171`). Deliberately no PATH fallback: a bare
/// executable name would be resolved against the search path.
fn controller_path() -> Option<PathBuf> {
    let controller = executable_directory().ok()?.join("fwdslash.exe");
    controller.is_file().then_some(controller)
}

fn run_controller<I, S>(arguments: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    run_controller_detailed(arguments).0
}

/// Runs the controller and returns `(succeeded, stderr)`. The stderr text is
/// what carries an actionable explanation — a Controlled Folder Access block,
/// a missing PowerShell 7 — to the InfoBar (#37).
fn run_controller_detailed<I, S>(arguments: I) -> (bool, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let (code, _stdout, stderr) = run_controller_code(arguments);
    (code == 0, stderr)
}

/// Runs the controller and returns `(exit code, stdout, stderr)`.
///
/// The `update` verbs are the reason this exists: their contract is a set of
/// exit codes (0/10/11/12/1/2/20) plus one JSON line on stdout, and neither of
/// the two wrappers above can carry either. `-1` is "the controller could not
/// be started at all", which is deliberately not one of the CLI's codes.
///
/// Blocks until the child exits — every caller runs it on the thread pool.
fn run_controller_code<I, S>(arguments: I) -> (i32, String, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let Some(controller) = controller_path() else {
        return (-1, String::new(), String::new());
    };
    let mut command = Command::new(controller);
    for argument in arguments {
        command.arg(argument.as_ref());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    match command.output() {
        Ok(output) => (
            // `code()` is `None` only for a signal, which Windows has no
            // concept of.
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ),
        Err(_) => (-1, String::new(), String::new()),
    }
}

/// The value of one string field of the CLI's one-line update JSON, or `None`
/// when the field is absent or `null`.
///
/// Hand-rolled on purpose: there is no serde anywhere in this workspace, and
/// this reads exactly two fields of a line the CLI renders from a fixed
/// `format!`. It understands the shape that `render_json` produces —
/// `"key":"value"` or `"key":null`, no whitespace, `\"` and `\\` escapes — and
/// answers `None` for anything else rather than guessing.
#[must_use]
fn json_string_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = line.get(start..)?;
    let mut characters = rest.chars();
    if characters.next()? != '"' {
        return None; // `null`, a number, or a shape this does not parse.
    }
    let mut value = String::new();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(value),
            '\\' => value.push(characters.next()?),
            other => value.push(other),
        }
    }
    // An unterminated string is malformed, not an empty value.
    None
}

/// Maps one `fwdslash update check --json` answer onto the outcome the window
/// already knows how to render.
///
/// The exit code carries the decision and the JSON carries the version, which
/// is why both are needed: exit 10 without an `available` field is a check
/// that found something it could not name, and that is not something to
/// announce.
#[must_use]
fn check_outcome(code: i32, stdout: &str) -> UpdateOutcome {
    let line = stdout.lines().next_back().unwrap_or_default();
    if code == UPDATE_EXIT_AVAILABLE {
        return match json_string_field(line, "available") {
            Some(tag) if !tag.is_empty() => UpdateOutcome::Ready(tag),
            _ => UpdateOutcome::Unavailable,
        };
    }
    match json_string_field(line, "state").as_deref() {
        Some("upToDate") => UpdateOutcome::UpToDate,
        Some("notDue") => UpdateOutcome::NotDue,
        // `disabled` (no package identity), `unavailable` (the service could
        // not be reached) and anything this build does not recognise all mean
        // the same thing to the window: no answer worth acting on.
        _ => UpdateOutcome::Unavailable,
    }
}

/// Seconds since the Unix epoch, for `format_last_check`.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Opens the WSL provider root through the shell, as `main.cpp:561-563` does.
fn open_wsl_root() -> bool {
    shell_open(WSL_ROOT)
}

/// `ShellExecuteW("open", target)`. Used for the WSL provider root and for the
/// `ms-windows-store:` product page.
fn shell_open(target: &str) -> bool {
    #[cfg(windows)]
    {
        // The reactor pool is deliberately opaque about its apartment model.
        // ShellExecute can activate STA-only shell extensions, so give each
        // operation a fresh STA rather than relying on the pool worker.
        let target = target.to_owned();
        std::thread::Builder::new()
            .name("fsw-shell-open".to_owned())
            .spawn(move || shell_open_sta(&target))
            .ok()
            .and_then(|handle| handle.join().ok())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        false
    }
}

#[cfg(windows)]
fn shell_open_sta(target: &str) -> bool {
    unsafe {
        use windows_sys::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        };
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        // A new OS thread has no apartment yet. Keep the init and uninit on
        // this same thread, as required for a shell extension's COM objects.
        if CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        ) < 0
        {
            return false;
        }
        let verb = to_wide("open");
        let target = to_wide(target);
        let result = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        CoUninitialize();
        // ShellExecuteW returns a fake HINSTANCE; anything above 32 is success.
        result as isize > 32
    }
}

/// The cross-process lock that keeps the broker's adapter sweep and this
/// window's repair from running at the same time (issue #56).
///
/// Existence, not ownership: whoever creates the named mutex first keeps the
/// handle for the length of its work, and the other sees `ERROR_ALREADY_EXISTS`
/// and stands down. No wait is ever performed, so the guard is not tied to the
/// thread that took it — which matters here, because it is taken on the thread
/// pool.
/// The lock is held by the *process*, not by a scope: the launch sweep spans
/// several messages and several thread-pool tasks, so the handle lives in a
/// static and [`Guard`] is a zero-sized token that only decides *when* the
/// release happens. That also keeps it `Send`, which a raw `HANDLE` is not.
mod sweep_lock {
    use std::sync::atomic::{AtomicIsize, Ordering};

    /// The named mutex handle while this process holds the lock; `0` when it
    /// does not. `-1` records "the lock could not be evaluated", which is
    /// treated as held-by-us so the work still runs.
    static HELD: AtomicIsize = AtomicIsize::new(0);

    /// Takes the cross-process sweep lock. `false` means somebody else is
    /// sweeping — the broker at startup, or this process not having released
    /// its own yet.
    pub(crate) fn acquire() -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{
                CloseHandle, ERROR_ALREADY_EXISTS, GetLastError,
            };
            use windows_sys::Win32::System::Threading::CreateMutexW;

            if HELD.load(Ordering::Acquire) != 0 {
                return false;
            }
            let name = super::to_wide(fsw_core::FSW_ADAPTER_SWEEP_MUTEX);
            // SAFETY: a named mutex with a NUL-terminated name; the handle is
            // either closed here or parked in `HELD` until `release`.
            unsafe {
                let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
                if handle.is_null() {
                    // Unknowable rather than held: do the work, which is
                    // idempotent, rather than never do it.
                    HELD.store(-1, Ordering::Release);
                    return true;
                }
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    CloseHandle(handle);
                    return false;
                }
                HELD.store(handle as isize, Ordering::Release);
                true
            }
        }
        #[cfg(not(windows))]
        {
            HELD.store(-1, Ordering::Release);
            true
        }
    }

    /// Releases the lock. A no-op when this process does not hold it.
    pub(crate) fn release() {
        let handle = HELD.swap(0, Ordering::AcqRel);
        #[cfg(windows)]
        if handle > 0 {
            // SAFETY: the handle came from `CreateMutexW` in `acquire` and is
            // closed exactly once, because the swap took it out of the static.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle as _) };
        }
        #[cfg(not(windows))]
        let _ = handle;
    }

    /// Releases the lock when it goes out of scope — for the one-shot manual
    /// repair, which begins and ends inside a single thread-pool task.
    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            release();
        }
    }

    /// [`acquire`] with an RAII release.
    pub(crate) fn acquire_guard() -> Option<Guard> {
        acquire().then_some(Guard)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parses the deep link, matching `InitialSection()` at `main.cpp:190-219`.
fn initial_section() -> Section {
    const PREFIX: &str = "fwdslash://settings/";
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let lowered = argument.to_ascii_lowercase();
        if let Some(rest) = lowered.strip_prefix(PREFIX) {
            // Trim any query or fragment the shell appended.
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            return Section::from_tag(&rest[..end]);
        }
        if lowered == "--section"
            && let Some(next) = arguments.next()
        {
            return Section::from_tag(&next);
        }
    }
    Section::General
}

fn log_crash(message: &str) {
    if let Ok(local) = env::var("LOCALAPPDATA") {
        let directory = PathBuf::from(local).join("ForwardSlashWindows");
        let _ = std::fs::create_dir_all(&directory);
        let _ = std::fs::write(directory.join("settings-crash.log"), message);
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| log_crash(&format!("panic: {info}"))));
    if activate_existing_instance() {
        return;
    }
    if let Err(error) = App::run_component::<SettingsModel>(initial_section()) {
        log_crash(&format!("App::run_component error: {error:?}"));
    }
}

const SETTINGS_MUTEX_NAME: &str = "Local\\ForwardSlashWindows.Settings";
const WINDOW_TITLE: &str = "Forward Slash Windows";
/// The image name every settings instance runs under. The raise path matches
/// on it so a same-titled window belonging to anything else cannot be raised.
#[cfg(windows)]
const SETTINGS_IMAGE_NAME: &str = "fswsettings.exe";

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Keeps the settings window single-instance, the way the broker is kept
/// single-instance by its well-known window class: a named mutex decides
/// ownership (`Local\ForwardSlashWindows.Settings`, following the
/// `Local\ForwardSlashWindows.Broker` convention), and a second launch raises
/// the existing window instead of opening a duplicate.
///
/// Closing the window exits the process (the reactor routes `Window.Closed` to
/// `exit_ui_thread`), so a live mutex holder always has a window to raise --
/// after the poll below, which covers the beat WinUI takes to materialize it.
///
/// The mutex handle is intentionally leaked so it lives until process exit;
/// the kernel releases it when the owning process terminates.
fn activate_existing_instance() -> bool {
    #[cfg(windows)]
    unsafe {
        use std::thread::sleep;
        use std::time::Duration;
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetForegroundWindow, ShowWindow, SW_RESTORE,
        };

        let name = to_wide(SETTINGS_MUTEX_NAME);
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if mutex.is_null() {
            // Fail closed: an error here means ownership cannot be decided,
            // and silently running a second instance is exactly the
            // duplicate-window failure this guard exists to prevent.
            let code = GetLastError();
            log_crash(&format!("single-instance mutex creation failed: Win32 error {code}"));
            show_startup_error(&format!(
                "Forward Slash Windows could not verify that it is not already running \
                 (Win32 error {code})."
            ));
            std::process::exit(2);
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            // We own the mutex: first instance.
            return false;
        }
        // Another instance owns the mutex. Its window may still be materializing
        // (the WinUI activation path takes a beat), so poll before concluding
        // anything -- 10 s, not 2 s.
        for _ in 0..200 {
            let window = find_settings_window();
            if !window.is_null() {
                ShowWindow(window, SW_RESTORE);
                SetForegroundWindow(window);
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        show_startup_error(
            "Forward Slash Windows is already running, but its window could not be \
             restored. Quit it from Task Manager and try again.",
        );
        true
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The other instance's settings window: a top-level window whose title is
/// `WINDOW_TITLE` **and** whose owning process image is `fswsettings.exe`.
///
/// A bare `FindWindowW(NULL, WINDOW_TITLE)` is not enough. The broker owns a
/// never-shown top-level window, and anything else on the desktop is free to
/// use the same caption; raising the first Z-order match put a 0x0 caption-only
/// window on screen instead of the real settings window.
#[cfg(windows)]
unsafe fn find_settings_window() -> windows_sys::Win32::Foundation::HWND {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    };

    struct Match {
        title: Vec<u16>,
        found: HWND,
    }
    unsafe extern "system" fn on_window(window: HWND, lparam: isize) -> i32 {
        unsafe {
            let state = &mut *(lparam as *mut Match);
            let length = GetWindowTextLengthW(window);
            if length <= 0 {
                return 1;
            }
            let mut text = vec![0u16; (length as usize) + 1];
            GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32);
            // `title` carries its NUL; compare the caption against the rest.
            if text.get(..length as usize) != state.title.get(..state.title.len() - 1) {
                return 1;
            }
            let mut owner = 0u32;
            GetWindowThreadProcessId(window, &raw mut owner);
            if owner != 0 && process_image_is_settings(owner) {
                state.found = window;
                return 0;
            }
            1
        }
    }

    let mut state = Match {
        title: to_wide(WINDOW_TITLE),
        found: std::ptr::null_mut(),
    };
    unsafe { EnumWindows(Some(on_window), (&raw mut state) as isize) };
    state.found
}

/// Whether `pid` runs `fswsettings.exe`, compared on the basename only and
/// case-insensitively. An unreadable process is never a match.
#[cfg(windows)]
unsafe fn process_image_is_settings(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut image = [0u16; 1024];
    let mut length = image.len() as u32;
    let queried = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, image.as_mut_ptr(), &mut length)
    };
    unsafe { CloseHandle(handle) };
    if queried == 0 {
        return false;
    }
    let Some(slice) = image.get(..length as usize) else {
        return false;
    };
    String::from_utf16_lossy(slice)
        .rsplit(['\\', '/'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case(SETTINGS_IMAGE_NAME))
}

/// Asks the broker to close and waits up to 3 s for its window to go away.
///
/// The broker owns the product's only notification icon and removes it on its
/// own `WM_CLOSE`; letting the installer force it down instead would leave a
/// ghost icon in the notification area until the next shell restart. Best
/// effort -- no broker is not an error.
#[cfg(windows)]
fn close_broker_window() {
    use std::thread::sleep;
    use std::time::Duration;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

    let class = to_wide(FSW_BROKER_WINDOW_CLASS);
    unsafe {
        let window = FindWindowW(class.as_ptr(), std::ptr::null());
        if window.is_null() {
            return;
        }
        PostMessageW(window, WM_CLOSE, 0, 0);
        for _ in 0..30 {
            if FindWindowW(class.as_ptr(), std::ptr::null()).is_null() {
                return;
            }
            sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(not(windows))]
fn close_broker_window() {}

/// Requests the ordinary shutdown by closing our own window. The reactor's only
/// process-exit route is WinUI's `Window.Closed`, so `WM_CLOSE` -- never
/// `DestroyWindow` -- is how this app exits.
#[cfg(windows)]
fn request_close() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

    let window = folder_picker::current_process_window();
    if window == 0 {
        return;
    }
    unsafe { PostMessageW(window as _, WM_CLOSE, 0, 0) };
}

#[cfg(not(windows))]
fn request_close() {}

/// Modal error shown when startup cannot proceed. Category text only.
#[cfg(windows)]
fn show_startup_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
    };
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            to_wide(message).as_ptr(),
            to_wide(WINDOW_TITLE).as_ptr(),
            MB_ICONERROR | MB_OK | MB_SETFOREGROUND,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// The pure decisions only: which banner the window offers, how a CLI answer
// becomes an outcome, and what an install exit code means on screen. Every one
// of them is a place where a wrong answer is invisible until a user is looking
// at it.
#[cfg(test)]
mod tests {
    use super::{
        InfoBarSeverity, NoticeAction, UPDATE_EXIT_AVAILABLE, UPDATE_EXIT_NEEDS_USER,
        UPDATE_EXIT_NOTHING, UPDATE_EXIT_OK, UpdateOutcome, check_outcome, install_banner_label,
        install_notice, json_string_field, store_product_uri,
    };

    /// A line in the exact shape `render_json` produces.
    fn json_line(state: &str, available: &str) -> String {
        format!(
            "{{\"flavor\":\"store\",\"state\":\"{state}\",\"available\":{available},\
             \"autoUpdate\":true,\"lastUpdateCheck\":1730000000,\"route\":null,\
             \"action\":null,\"detail\":null}}"
        )
    }

    // -- the JSON extractor ------------------------------------------------

    #[test]
    fn a_string_field_is_read_to_its_closing_quote() {
        let line = json_line("available", "\"0.0.5\"");
        assert_eq!(json_string_field(&line, "state").as_deref(), Some("available"));
        assert_eq!(json_string_field(&line, "available").as_deref(), Some("0.0.5"));
        assert_eq!(json_string_field(&line, "flavor").as_deref(), Some("store"));
    }

    #[test]
    fn null_and_absent_and_numeric_fields_are_all_none() {
        let line = json_line("upToDate", "null");
        assert_eq!(json_string_field(&line, "available"), None);
        assert_eq!(json_string_field(&line, "route"), None);
        assert_eq!(json_string_field(&line, "nosuchfield"), None);
        // A number is not a string, and guessing at one would be worse.
        assert_eq!(json_string_field(&line, "lastUpdateCheck"), None);
    }

    #[test]
    fn escapes_survive_the_extractor() {
        let line = r#"{"detail":"C:\\Users\\me said \"no\"","state":"error"}"#;
        assert_eq!(
            json_string_field(line, "detail").as_deref(),
            Some(r#"C:\Users\me said "no""#)
        );
        assert_eq!(json_string_field(line, "state").as_deref(), Some("error"));
    }

    #[test]
    fn a_malformed_line_never_yields_a_half_value() {
        assert_eq!(json_string_field("", "state"), None);
        assert_eq!(json_string_field("not json at all", "state"), None);
        // Unterminated string.
        assert_eq!(json_string_field(r#"{"state":"upToDa"#, "state"), None);
    }

    // -- check answers -----------------------------------------------------

    #[test]
    fn exit_ten_with_a_version_is_the_only_ready() {
        assert_eq!(
            check_outcome(UPDATE_EXIT_AVAILABLE, &json_line("available", "\"0.0.5\"")),
            UpdateOutcome::Ready("0.0.5".to_string())
        );
        // Exit 10 that cannot name a version is nothing to announce.
        assert_eq!(
            check_outcome(UPDATE_EXIT_AVAILABLE, &json_line("available", "null")),
            UpdateOutcome::Unavailable
        );
        assert_eq!(
            check_outcome(UPDATE_EXIT_AVAILABLE, &json_line("available", "\"\"")),
            UpdateOutcome::Unavailable
        );
    }

    #[test]
    fn the_state_field_decides_the_rest() {
        assert_eq!(
            check_outcome(UPDATE_EXIT_OK, &json_line("upToDate", "null")),
            UpdateOutcome::UpToDate
        );
        assert_eq!(
            check_outcome(UPDATE_EXIT_OK, &json_line("notDue", "null")),
            UpdateOutcome::NotDue
        );
        // `disabled` is an unpackaged build; `unavailable` is an unreachable
        // service. Neither is worth a word on screen.
        assert_eq!(
            check_outcome(UPDATE_EXIT_OK, &json_line("disabled", "null")),
            UpdateOutcome::Unavailable
        );
        assert_eq!(
            check_outcome(UPDATE_EXIT_OK, &json_line("unavailable", "null")),
            UpdateOutcome::Unavailable
        );
        // A controller that could not be started prints nothing at all.
        assert_eq!(check_outcome(-1, ""), UpdateOutcome::Unavailable);
    }

    #[test]
    fn only_the_last_line_of_stdout_is_the_json() {
        // Nothing writes to stdout before the JSON today; if something ever
        // does, the contract line is still the last one.
        let stdout = format!("a stray line\n{}\n", json_line("upToDate", "null"));
        assert_eq!(
            check_outcome(UPDATE_EXIT_OK, &stdout),
            UpdateOutcome::UpToDate
        );
    }

    // -- the install banner matrix ----------------------------------------

    #[test]
    fn an_unpackaged_build_never_offers_to_install() {
        assert_eq!(install_banner_label(false, false, true, true), None);
        assert_eq!(install_banner_label(false, true, true, true), None);
    }

    #[test]
    fn nothing_to_install_means_no_banner() {
        assert_eq!(install_banner_label(true, true, false, false), None);
        assert_eq!(install_banner_label(true, false, false, false), None);
    }

    #[test]
    fn each_flavor_labels_the_button_for_what_it_does() {
        // Store: a recorded available version is all it ever has locally.
        assert_eq!(
            install_banner_label(true, true, false, true),
            Some("Install now")
        );
        // GitHub: the downloaded bundle, and the check that found it.
        assert_eq!(
            install_banner_label(true, false, true, false),
            Some("Restart to update")
        );
        assert_eq!(
            install_banner_label(true, false, true, true),
            Some("Restart to update")
        );
    }

    // -- install exit code to what the window shows ------------------------

    #[test]
    fn a_started_install_leaves_no_notice_because_the_window_closes() {
        assert!(install_notice(UPDATE_EXIT_OK, "").is_none());
    }

    #[test]
    fn failed_or_deferred_installs_restore_only_a_previously_running_broker() {
        for code in [-1, UPDATE_EXIT_AVAILABLE, UPDATE_EXIT_NEEDS_USER, UPDATE_EXIT_NOTHING, 1] {
            assert!(super::should_restore_broker_after_install(code, true));
        }
        assert!(!super::should_restore_broker_after_install(
            UPDATE_EXIT_OK,
            true
        ));
        assert!(!super::should_restore_broker_after_install(
            UPDATE_EXIT_NOTHING,
            false
        ));
    }

    #[test]
    fn only_the_store_hand_off_carries_an_action_button() {
        assert_eq!(
            install_notice(UPDATE_EXIT_NEEDS_USER, "").map(|notice| notice.action),
            Some(Some(NoticeAction::OpenStore))
        );
        for code in [UPDATE_EXIT_AVAILABLE, UPDATE_EXIT_NOTHING, 1] {
            assert_eq!(
                install_notice(code, "").map(|notice| notice.action),
                Some(None),
                "code {code} must not offer an action"
            );
        }
    }

    #[test]
    fn only_a_failure_is_an_error_bar() {
        assert_eq!(
            install_notice(1, "").map(|notice| notice.severity),
            Some(InfoBarSeverity::Error)
        );
        assert_eq!(
            install_notice(UPDATE_EXIT_NOTHING, "").map(|notice| notice.severity),
            Some(InfoBarSeverity::Success)
        );
        assert_eq!(
            install_notice(UPDATE_EXIT_AVAILABLE, "").map(|notice| notice.severity),
            Some(InfoBarSeverity::Informational)
        );
    }

    #[test]
    fn a_failure_carries_the_controllers_own_explanation() {
        assert_eq!(
            install_notice(1, "  ").map(|notice| notice.message),
            Some("The update could not be started.".to_string())
        );
        assert_eq!(
            install_notice(1, "The Store refused (0x80070005).\n").map(|notice| notice.message),
            Some("The update could not be started. The Store refused (0x80070005).".to_string())
        );
    }

    // -- the Store link ----------------------------------------------------

    #[test]
    fn the_store_link_addresses_this_product() {
        assert_eq!(
            store_product_uri(),
            "ms-windows-store://pdp/?productid=9P51CM0MTMK2"
        );
    }
}
