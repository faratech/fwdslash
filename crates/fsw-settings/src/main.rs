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
    BrokerState, CMD_ADAPTER_KEY, FSW_BROKER_WINDOW_CLASS, FSW_VERSION, POWERSHELL_ADAPTER_ROOT,
    SettingsValues, adapter_installed, adapter_outdated, adapter_version, broker_state,
    broker_window_exists,
    ensure_broker_running, executable_available, executable_directory, get_default_distribution,
    has_package_identity, is_store_flavor, list_registered_distributions, package_architecture,
    package_version, update, windows_integration_installed,
};
use fsw_core::update::UpdateOutcome;
use fsw_path::{BareSlashMode, eq_ignore_case, is_valid_windows_root};
use std::env;
use std::path::PathBuf;
use std::process::Command;
use windows_reactor::*;

mod folder_picker;

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
    update_tag: Option<String>,
    distributions: Vec<String>,
    wsl_default: Option<String>,
    broker: BrokerState,
    broker_window: bool,
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
            distributions,
            wsl_default,
            // 250 ms so a wedged broker cannot stall a refresh (main.cpp:827
            // used 750; this runs off the UI thread now, but the window still
            // waits on the result to repaint).
            broker: broker_state(250),
            broker_window: broker_window_exists(),
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

    /// Verbatim from `src/settings/main.cpp:832-838`, including the hardcoded
    /// driver line — the driver is production-gated and never queried here.
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
            "Windows broker: {broker}\nFilesystem driver: not installed (production-gated)"
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
    UpdateCheckFinished(update::UpdateOutcome),
    SetAutoUpdate(bool),
    SelectDistribution(Option<usize>),
    ToggleIntegration(Integration, bool),
    OpenWslRoot,
    RefreshStatus,
    DismissNotice,
    /// Register the downloaded bundle now instead of waiting for the next logon.
    RestartToUpdate,
    /// A background `State::read()` completed. Every refresh is off-thread: the
    /// read touches the registry, the broker window and the update directory.
    StateLoaded(State),
    /// `ensure_broker_running()` finished off-thread; the broker column of the
    /// status line may have changed.
    BrokerProbed,
    /// A `fwdslash.exe` invocation finished off-thread. `action` is the
    /// `show_result` phrase the request was started with.
    ControllerFinished {
        action: &'static str,
        terminal: bool,
        succeeded: bool,
    },
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
    notice: Option<(InfoBarSeverity, &'static str, String)>,
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
        context.spawn_background(move |_| Msg::ControllerFinished {
            action,
            terminal,
            succeeded: run_controller(arguments),
        });
    }

    /// The single notification path, reproducing `ShowResult`
    /// (`src/settings/main.cpp:874-887`). Only Success and Error are ever used.
    fn show_result(&mut self, succeeded: bool, action: &str, terminal: bool) {
        let mut message = action.to_string();
        if succeeded && terminal {
            message.push_str(". Reopen affected terminals.");
        } else if !succeeded {
            message.push_str(" failed. Existing settings were left in place.");
        }
        self.notice = Some((
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
            (
                InfoBarSeverity::Error,
                "Some terminal integrations could not be updated",
                "Turn the affected integration off and on again on the Terminals page."
                    .to_string(),
            )
        } else {
            let names: Vec<&str> = upgrade
                .done
                .iter()
                .map(|integration| integration.display_name())
                .collect();
            let verb = if names.len() == 1 { "is" } else { "are" };
            (
                InfoBarSeverity::Success,
                "Terminal integrations updated",
                format!("{} {verb} now on {FSW_VERSION}", names.join(", ")),
            )
        });
        // Not the update-available notice, so dismissal must not clear the
        // persisted AvailableUpdate value.
        self.notice_is_update = false;
        Self::refresh(context);
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
            ensure_broker_running();
            Msg::BrokerProbed
        });
        // Daily GitHub update check (GitHub flavor only; the gate inside
        // `run_update_check` no-ops for the Store flavor and unpackaged
        // builds). Off the UI thread so the curl timeouts cannot stall the
        // window.
        if update::update_check_allowed(
            has_package_identity(),
            is_store_flavor(),
            update::read_auto_update_enabled(),
        ) {
            context.spawn_background(|_| Msg::UpdateCheckFinished(update::run_update_check()));
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
        };
        // An outdated adapter is repaired on sight, at the first state the
        // window ever sees.
        model.maybe_start_upgrade(context);
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
                    self.notice = Some((
                        InfoBarSeverity::Error,
                        "Could not set the folder root",
                        "Use an absolute path like C:\\code or \\\\wsl.localhost\\Ubuntu\\home\\me."
                            .to_string(),
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
                if !open_wsl_root() {
                    self.show_result(false, "Opening the WSL root", false);
                }
            }
            Msg::RefreshStatus => {
                Self::refresh(context);
                self.show_result(true, "Status refreshed", false);
            }
            Msg::UpdateCheckFinished(outcome) => {
                let UpdateOutcome::Ready(tag) = outcome else {
                    return;
                };
                let short = tag.strip_prefix('v').unwrap_or(&tag).to_string();
                self.notice = Some((
                    InfoBarSeverity::Informational,
                    "Update available",
                    format!(
                        "Version {short} was downloaded. It applies after you sign out and \
                         back in, or restart Forward Slash Windows now."
                    ),
                ));
                self.notice_is_update = true;
                // The bundle only exists once the check has downloaded it, so
                // the "Restart to update" action appears with this refresh.
                Self::refresh(context);
            }
            Msg::RestartToUpdate => {
                let Some(bundle) = update::pending_bundle_path() else {
                    self.notice = Some((
                        InfoBarSeverity::Error,
                        "Could not start the update",
                        "The update could not be started.".to_string(),
                    ));
                    return;
                };
                // Ask the broker to close first so it removes its notification
                // icon itself; a forced shutdown by the installer would leave a
                // ghost icon behind.
                close_broker_window();
                if update::restart_to_update(&bundle) {
                    // Exit through the window's own close path: the reactor's
                    // only process-exit route is WinUI's `Window.Closed`.
                    request_close();
                } else {
                    self.notice = Some((
                        InfoBarSeverity::Error,
                        "Could not start the update",
                        "The update could not be started.".to_string(),
                    ));
                }
            }
            Msg::SetAutoUpdate(enabled) => {
                if enabled == update::read_auto_update_enabled() {
                    return;
                }
                let succeeded = update::set_auto_update_enabled(enabled).is_ok();
                self.show_result(
                    succeeded,
                    if enabled {
                        "Automatic updates enabled"
                    } else {
                        "Automatic updates disabled"
                    },
                    false,
                );
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
                self.show_result(succeeded, action, terminal);
                Self::refresh(context);
            }
            Msg::StateLoaded(state) => {
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
            Some((severity, title, message)) => InfoBar::new()
                .title(*title)
                .message(message)
                .severity(*severity)
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
        // The GitHub flavor only: the Store updates through the Store.
        let restartable =
            self.state.packaged && !self.state.store_flavor && self.state.update_bundle_ready;
        if self.upgrade.is_none() && !restartable && self.pending.is_none() {
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
        let restart_action: View = if restartable {
            Button::new()
                .is_enabled(self.controls_enabled())
                .horizontal_alignment(HorizontalAlignment::Left)
                .on_click(context.message(Msg::RestartToUpdate))
                .content("Restart to update")
        } else {
            View::empty()
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
            .children((upgrade_notice, restart_action, progress))
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
                // Automatic updates: GitHub flavor only. The Store build
                // updates through the Store; its updater never runs.
                if state.packaged && !state.store_flavor {
                    toggle_card(
                        "Automatic updates",
                        "Check GitHub daily and install new versions automatically.",
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
            body(state.adapter_component_line(Integration::Cmd))
                .foreground(ThemeBrush::TextSecondary),
            body(state.adapter_component_line(Integration::WindowsPowerShell))
                .foreground(ThemeBrush::TextSecondary),
            body(state.adapter_component_line(Integration::PowerShell7))
                .foreground(ThemeBrush::TextSecondary),
            body(format!("Package: {}", Self::package_label()))
                .foreground(ThemeBrush::TextSecondary),
            body(state.flavor_component_line()).foreground(ThemeBrush::TextSecondary),
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
                body(
                    "The filesystem minifilter remains production-gated and is not installed by \
                     this app.",
                )
                .foreground(ThemeBrush::TextSecondary),
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
    let Some(controller) = controller_path() else {
        return false;
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
    command.status().is_ok_and(|status| status.success())
}

/// Opens the WSL provider root through the shell, as `main.cpp:561-563` does.
fn open_wsl_root() -> bool {
    #[cfg(windows)]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide = |value: &str| -> Vec<u16> {
            let mut buffer: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().collect();
            buffer.push(0);
            buffer
        };
        let verb = wide("open");
        let target = wide(WSL_ROOT);
        let result = ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        // ShellExecuteW returns a fake HINSTANCE; anything above 32 is success.
        result as isize > 32
    }
    #[cfg(not(windows))]
    {
        false
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
