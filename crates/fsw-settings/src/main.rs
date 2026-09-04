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
    BrokerState, CMD_ADAPTER_KEY, POWERSHELL_ADAPTER_ROOT, adapter_installed, broker_state,
    broker_window_exists, ensure_broker_running, executable_available, executable_directory,
    get_bare_slash_mode, get_bare_slash_override, get_bare_slash_root, get_default_distribution,
    has_package_identity, is_disabled, list_registered_distributions,
    windows_integration_installed,
};
use fsw_path::{BareSlashMode, eq_ignore_case, is_valid_windows_root};
use std::env;
use std::path::PathBuf;
use std::process::Command;
use windows_reactor::*;

mod folder_picker;
mod tray;
mod watchdog;

/// The WSL provider root. Bare `/` may resolve elsewhere depending on bare-slash mode,
/// so "Open WSL root" targets this literally, as `src/settings/main.cpp:562` does.
const WSL_ROOT: &str = r"\\wsl.localhost";

/// Icon resource id from app.rc, kept in step with `include/fsw_resources.h`.
const IDI_FSW_APP: u16 = 101;

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
            Self::Cmd => state.cmd,
            Self::WindowsPowerShell => state.windows_powershell,
            Self::PowerShell7 => state.powershell7,
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
    cmd: bool,
    windows_powershell: bool,
    powershell7: bool,
    powershell7_available: bool,
    bare_mode: BareSlashMode,
    pinned: String,
    root: Option<String>,
    distributions: Vec<String>,
    wsl_default: Option<String>,
    broker: BrokerState,
    broker_window: bool,
}

impl State {
    fn read() -> Self {
        let distributions = list_registered_distributions();
        let wsl_default = get_default_distribution(&distributions);
        Self {
            disabled: is_disabled(),
            packaged: has_package_identity(),
            windows: windows_integration_installed(),
            cmd: adapter_installed(CMD_ADAPTER_KEY),
            windows_powershell: adapter_installed(&format!(
                "{POWERSHELL_ADAPTER_ROOT}WindowsPowerShell"
            )),
            powershell7: adapter_installed(&format!("{POWERSHELL_ADAPTER_ROOT}PowerShell")),
            powershell7_available: executable_available("pwsh.exe"),
            bare_mode: get_bare_slash_mode(),
            pinned: get_bare_slash_override(),
            root: get_bare_slash_root(),
            distributions,
            wsl_default,
            // 750 ms so a wedged broker cannot stall a refresh (main.cpp:827).
            broker: broker_state(750),
            broker_window: broker_window_exists(),
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
    SelectDistribution(Option<usize>),
    ToggleIntegration(Integration, bool),
    OpenWslRoot,
    RefreshStatus,
    DismissNotice,
    /// The settings HWND exists; install the tray/lifecycle subclass on the UI
    /// thread. The payload is the window discovered by the background poll.
    WindowHookReady(isize),
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
    notice: Option<(InfoBarSeverity, &'static str, String)>,
}

impl SettingsModel {
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

    fn refresh(&mut self) {
        self.state = State::read();
    }
}

impl Component for SettingsModel {
    type Input = Section;
    type Message = Msg;

    fn create(input: &Self::Input, context: &ComponentContext<Self>) -> Self {
        // A packaged build runs nothing at install time and its startup task only
        // fires at logon, so opening this window is the first chance to arm the
        // broker. Without this a Store install does nothing at all.
        ensure_broker_running();
        // Reactor materializes the HWND after `create` returns, so discovery
        // runs off-thread with bounded polling; `WindowHookReady` hands the
        // window back for the (UI-thread-only) subclass installation.
        context.spawn_background(|_| {
            for _ in 0..100 {
                let window = tray::discover_window();
                if window != 0 {
                    return Msg::WindowHookReady(window);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Msg::WindowHookReady(0)
        });
        Self {
            section: *input,
            pane_open: false,
            color_scheme: ColorScheme::Dark,
            state: State::read(),
            root_draft: String::new(),
            folder_selected: false,
            notice: None,
        }
    }

    fn update(&mut self, message: Self::Message, _context: &ComponentContext<Self>) {
        match message {
            Msg::Navigate(Some(tag)) => {
                let section = Section::from_tag(&tag);
                if section == self.section {
                    return;
                }
                self.section = section;
                // Stands in for the C++ `window_.Activated` refresh, which reactor
                // has no equivalent for. See docs/divergences.md.
                self.refresh();
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
                let succeeded = run_controller([if enabled { "enable" } else { "disable" }]);
                self.show_result(
                    succeeded,
                    if enabled {
                        "Resolution enabled"
                    } else {
                        "Resolution disabled"
                    },
                    false,
                );
                self.refresh();
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
                let succeeded = run_controller(["bare-slash", "list"]);
                self.show_result(succeeded, "Bare slash shows all distributions", false);
                self.refresh();
            }
            Msg::BareSlashDefaultChecked(checked) => {
                if !checked
                    || (!self.state.is_list_mode() && self.state.root.is_none() && !self.folder_selected)
                {
                    return;
                }
                self.folder_selected = false;
                let succeeded = run_controller(["bare-slash", "default"]);
                self.show_result(succeeded, "Bare slash opens the default distribution", false);
                self.refresh();
            }
            Msg::FolderRadioChecked(checked) => {
                // Selecting the radio only reveals the folder controls; the
                // Apply button validates and commits. Unchecking is the WinUI
                // echo of another radio winning.
                if !checked || self.folder_selected || self.state.root.is_some() {
                    return;
                }
                self.folder_selected = true;
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
                let parent = crate::tray::discover_window();
                let Some(path) = folder_picker::pick_folder(parent as _) else {
                    return; // cancelled
                };
                self.root_draft = path.clone();
                self.folder_selected = true;
                if !is_valid_windows_root(&path) || Some(path.as_str()) == self.state.root.as_deref()
                {
                    return;
                }
                let succeeded = run_controller(["bare-slash", "root", &path]);
                if succeeded {
                    self.folder_selected = false;
                }
                self.show_result(succeeded, "Bare slash opens the chosen folder", false);
                self.refresh();
            }
            Msg::ApplyRoot => {
                // Echo guard: re-pressing Apply with an unchanged draft must
                // not re-invoke the controller (divergences #5).
                if Some(self.root_draft.as_str()) == self.state.root.as_deref() {
                    return;
                }
                if !is_valid_windows_root(&self.root_draft) {
                    self.notice = Some((
                        InfoBarSeverity::Error,
                        "Could not set the folder root",
                        "Use an absolute path like C:\\code or \\\\wsl.localhost\\Ubuntu\\home\\me."
                            .to_string(),
                    ));
                    return;
                }
                let succeeded = run_controller(["bare-slash", "root", &self.root_draft]);
                if succeeded {
                    self.folder_selected = false;
                }
                self.show_result(succeeded, "Bare slash opens the chosen folder", false);
                self.refresh();
            }

            Msg::SelectDistribution(index) => {
                // `None` means the items source was replaced and WinUI reset the
                // selection to -1. Acting on it would clear the user's pin.
                let Some(index) = index else { return };
                if index == self.state.distribution_index() {
                    return;
                }
                let succeeded = if index == 0 {
                    run_controller(["bare-slash", "default"])
                } else if let Some(distribution) = self.state.distributions.get(index - 1) {
                    run_controller(["bare-slash", "default", distribution.as_str()])
                } else {
                    return;
                };
                self.show_result(succeeded, "Bare slash default updated", false);
                self.refresh();
            }

            Msg::ToggleIntegration(integration, enabled) => {
                if enabled == integration.installed(&self.state) {
                    return;
                }
                let succeeded = run_controller([
                    "integration",
                    integration.id(),
                    if enabled { "enable" } else { "disable" },
                ]);
                self.show_result(
                    succeeded,
                    integration.action(enabled),
                    integration.is_terminal(),
                );
                self.refresh();
            }

            Msg::OpenWslRoot => {
                if !open_wsl_root() {
                    self.show_result(false, "Opening the WSL root", false);
                }
            }
            Msg::RefreshStatus => {
                self.refresh();
                self.show_result(true, "Status refreshed", false);
            }
            Msg::DismissNotice => self.notice = None,
            Msg::WindowHookReady(window) => {
                // Subclassing requires the window's own thread; `update` runs
                // on it. A 0 payload means discovery timed out -- tray and
                // close-to-tray would silently degrade to default behavior,
                // leaving an unquittable window; exit loudly instead.
                if window == 0 {
                    log_crash("settings window discovery failed; exiting");
                    show_startup_error(
                        "Forward Slash Windows could not attach to its own window \
                         and will now exit.",
                    );
                    watchdog::note_exit_requested();
                    watchdog::quit_ui_thread();
                } else {
                    watchdog::note_window(window);
                    if let Err(code) = tray::install(window) {
                        log_crash(&format!(
                            "tray subclass installation failed: Win32 error {code}"
                        ));
                    }
                }
            }
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
            Section::About => Self::view_about(),
        };

        Border::new()
            .padding(Thickness::uniform(24.0))
            .content(
                Grid::new()
                    .rows([GridLength::Auto, GridLength::Star(1.0)])
                    .children((
                        notice,
                        ScrollViewer::new()
                            .grid_row(1)
                            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                            .content(page),
                    )),
            )
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
                                .on_text_changed(context.callback(Msg::RootTextChanged)),
                            Button::new()
                                .on_click(context.message(Msg::BrowseRoot))
                                .content("Browse\\u{2026}"),
                            Button::new()
                                .on_click(context.message(Msg::ApplyRoot))
                                .content("Apply folder"),
                        )),
                    body("Typing / opens this folder; /name goes inside it.")
                        .foreground(ThemeBrush::TextSecondary),
                ))
        };

        let picker: View = if state.is_list_mode() {
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
                        .is_checked(state.is_list_mode() && state.root.is_none() && !self.folder_selected)
                        .on_checked(context.callback(Msg::BareSlashListChecked))
                        .content("Show all distributions"),
                    RadioButton::new()
                        .group_name("BareSlashMode")
                        .automation_name("Open my default distribution")
                        .is_checked(!state.is_list_mode() && state.root.is_none() && !self.folder_selected)
                        .on_checked(context.callback(Msg::BareSlashDefaultChecked))
                        .content("Open my default distribution"),
                    RadioButton::new()
                        .group_name("BareSlashMode")
                        .automation_name("Open a folder I choose")
                        .is_checked(state.is_folder_mode() || self.folder_selected)
                        .on_checked(context.callback(Msg::FolderRadioChecked))
                        .content("Open a folder I choose"),
                    picker,
                    folder_picker,
                ))),
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
                    integration_toggle(Integration::Windows, state, context)
                        .is_enabled(!state.packaged)
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
                toggle_card(
                    "Command Prompt",
                    "Adds reversible dir and ls DOSKEY adapters for new cmd.exe sessions.",
                    integration_toggle(Integration::Cmd, state, context)
                        .automation_name("Install Command Prompt integration"),
                ),
                toggle_card(
                    "Windows PowerShell 5.1",
                    "Adds a guarded profile import and preserves normal Get-ChildItem behavior.",
                    integration_toggle(Integration::WindowsPowerShell, state, context)
                        .automation_name("Install Windows PowerShell integration"),
                ),
                toggle_card(
                    "PowerShell 7",
                    if state.powershell7_available {
                        "Adds the same reversible adapter to the PowerShell 7 profile."
                    } else {
                        "PowerShell 7 is not installed on this computer."
                    },
                    integration_toggle(Integration::PowerShell7, state, context)
                        .is_enabled(state.powershell7_available || state.powershell7)
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

    // The only `expect`s in the crate: `navigate_uri` on a compile-time
    // constant string is infallible by construction.
    #[allow(clippy::expect_used)]
    fn view_about() -> View {
        page_stack(16.0)
            .children((
                page_header("About", "Forward Slash Windows 0.0.1"),
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
    card(
        Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(20.0)
            .children((
                StackPanel::new()
                    .grid_column(0)
                    .spacing(3.0)
                    .vertical_alignment(VerticalAlignment::Center)
                    .children((strong(title), body(description).foreground(ThemeBrush::TextSecondary))),
                toggle,
            )),
    )
}

fn integration_toggle(
    integration: Integration,
    state: &State,
    context: &mut ViewContext<SettingsModel>,
) -> ToggleSwitch {
    ToggleSwitch::new()
        .is_on(integration.installed(state))
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
    #[cfg(windows)]
    if std::env::var_os("FSW_SIMULATE_WINDOWLESS").is_some() {
        // Test hook for the takeover path in `activate_existing_instance`:
        // hold the single-instance mutex forever with no window, exactly the
        // state a direct `DestroyWindow` (or a session end) can leave behind.
        // See docs/divergences.md. Set the variable to any value to enable.
        simulate_windowless();
        return;
    }
    watchdog::note_ui_thread();
    watchdog::spawn();
    if activate_existing_instance() {
        return;
    }
    if let Err(error) = App::run_component::<SettingsModel>(initial_section()) {
        log_crash(&format!("App::run_component error: {error:?}"));
    }
}

const SETTINGS_MUTEX_NAME: &str = "Local\\ForwardSlashWindows.Settings";
const WINDOW_TITLE: &str = "Forward Slash Windows";

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Keeps the settings window single-instance, the way the broker is kept
/// single-instance by its well-known window class: a named mutex decides
/// ownership (`Local\ForwardSlashWindows.Settings`, following the
/// `Local\ForwardSlashWindows.Broker` convention), and a second launch raises
/// the existing window instead of opening a duplicate.
///
/// A prior instance can outlive its own window (a direct `DestroyWindow` or a
/// session end skips the reactor's `Window.Closed` exit path), leaving a
/// process that holds the mutex but has nothing to raise -- every future
/// launch would then be a silent no-op. That zombie is terminated and the
/// launching instance takes over.
///
/// The mutex handle is intentionally leaked so it lives until process exit;
/// the kernel releases it when the owning process terminates.
fn activate_existing_instance() -> bool {
    #[cfg(windows)]
    unsafe {
        use std::thread::sleep;
        use std::time::Duration;
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };

        let name = to_wide(SETTINGS_MUTEX_NAME);
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if mutex.is_null() {
            // Fail closed: an error here means ownership cannot be decided,
            // and silently running a second instance is exactly the
            // duplicate-tray-icon failure this guard exists to prevent.
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
        let title = to_wide(WINDOW_TITLE);
        for _ in 0..200 {
            let window = FindWindowW(std::ptr::null(), title.as_ptr());
            if !window.is_null() {
                ShowWindow(window, SW_RESTORE);
                SetForegroundWindow(window);
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        // No window anywhere. Take over from a windowless holder, if one is
        // there to take over from.
        if terminate_windowless_holder() {
            // WAIT_ABANDONED is success: the holder died holding the mutex.
            let wait = WaitForSingleObject(mutex, 5000);
            if wait == 0 || wait == 0x0000_0080 {
                return false;
            }
        }
        show_startup_error(
            "Forward Slash Windows is already running, but its window could not be \
             restored. Quit it from the notification area (or Task Manager) and try \
             again.",
        );
        true
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Kills an `fswsettings.exe` peer that holds the single-instance mutex but
/// owns no window: the windowless zombie. Only a same-identity peer older than
/// 15 s qualifies, and the kill is skipped whenever anything is ambiguous -- a
/// killed healthy instance is worse than a failed launch.
#[cfg(windows)]
unsafe fn terminate_windowless_holder() -> bool {
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, TerminateProcess, GetCurrentProcessId,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    // 15 s in 100 ns units. A younger peer is a legitimate concurrent launch
    // still materializing its window.
    const MINIMUM_AGE_100NS: u64 = 15 * 10_000_000;
    // FILETIME epoch offset: 100 ns intervals between 1601-01-01 and 1970-01-01.
    const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() {
        return false;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        unsafe { CloseHandle(snapshot) };
        return false;
    }
    let mine = unsafe { GetCurrentProcessId() };
    let now = UNIX_EPOCH_FILETIME
        + u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() / 100)
                .unwrap_or(0),
        )
        .unwrap_or(0);
    let mut killed = false;
    loop {
        if entry.th32ProcessID != mine && eq_wide(&entry.szExeFile, "fswsettings.exe") {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                    0,
                    entry.th32ProcessID,
                )
            };
            if !handle.is_null() {
                let mut creation: FILETIME = unsafe { std::mem::zeroed() };
                let mut exit_time: FILETIME = unsafe { std::mem::zeroed() };
                let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
                let mut user: FILETIME = unsafe { std::mem::zeroed() };
                if unsafe {
                    GetProcessTimes(handle, &mut creation, &mut exit_time, &mut kernel, &mut user)
                } != 0 {
                    let created = (creation.dwLowDateTime as u64)
                        | ((creation.dwHighDateTime as u64) << 32);
                    let qualifies = now.saturating_sub(created) >= MINIMUM_AGE_100NS
                        && unsafe {
                            same_package_identity(entry.th32ProcessID)
                                && !window_exists(entry.th32ProcessID)
                        };
                    if qualifies {
                        unsafe { TerminateProcess(handle, 1) };
                        killed = true;
                    }
                }
                unsafe { CloseHandle(handle) };
            }
        }
        if killed || unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break;
        }
    }
    unsafe { CloseHandle(snapshot) };
    killed
}

/// True only when the peer lives in the same packaging context as us: two
/// unpackaged builds, or the same MSIX package family. A packaged instance and
/// an unpackaged dev build use different named-object namespaces and must
/// never kill each other.
#[cfg(windows)]
unsafe fn same_package_identity(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let Ok(mine) = env::current_exe() else {
        return false;
    };
    let handle = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
    };
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
    let theirs = String::from_utf16_lossy(&image[..length as usize]).to_ascii_lowercase();
    let mine = mine.to_string_lossy().to_ascii_lowercase();
    const WINDOWS_APPS: &str = r"\windowsapps\";
    let mine_packaged = mine.contains(WINDOWS_APPS);
    let theirs_packaged = theirs.contains(WINDOWS_APPS);
    if mine_packaged != theirs_packaged {
        return false;
    }
    if !mine_packaged {
        return true;
    }
    // Same package family: the segment right after windowsapps\ is
    // <family>_<version>_<arch>_<publisherhash>; compare up to the first '_'.
    let family = |path: &str| {
        path.split(WINDOWS_APPS)
            .nth(1)
            .unwrap_or("")
            .split('_')
            .next()
            .unwrap_or("")
            .to_string()
    };
    family(&mine) == family(&theirs)
}

/// Whether `pid` owns any top-level window at all. The zombie is defined by
/// owning none; any window disqualifies the kill, whatever its title.
#[cfg(windows)]
unsafe fn window_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};

    struct Match {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn on_window(window: HWND, lparam: isize) -> i32 {
        unsafe {
            let state = &mut *(lparam as *mut Match);
            let mut owner = 0u32;
            GetWindowThreadProcessId(window, &mut owner);
            if owner == state.pid {
                state.found = true;
                return 0;
            }
            1
        }
    }
    let mut state = Match { pid, found: false };
    unsafe { EnumWindows(Some(on_window), &mut state as *mut Match as isize) };
    state.found
}

/// Wide-string comparison against a NUL-terminated fixed buffer.
#[cfg(windows)]
fn eq_wide(buffer: &[u16], value: &str) -> bool {
    let expected: Vec<u16> = value.encode_utf16().collect();
    buffer.len() >= expected.len()
        && buffer[..expected.len()] == expected[..]
        && buffer.iter().nth(expected.len()) == Some(&0)
}

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

/// Holds the single-instance mutex forever with no window, for testing the
/// takeover path. Enabled by setting `FSW_SIMULATE_WINDOWLESS`.
#[cfg(windows)]
fn simulate_windowless() {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name = to_wide(SETTINGS_MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() || unsafe { GetLastError() } == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS {
        eprintln!("another instance already holds the mutex");
        return;
    }
    // The handle must outlive everything this process does; the loop below
    // keeps it alive, and the kernel reaps it at process exit.
    let _ = mutex;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
