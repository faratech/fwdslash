//! Marker-state and decision logic for the adapter installers, extracted
//! verbatim from the retired `tools/Install-*.ps1` / `Uninstall-*.ps1`
//! scripts. Pure functions over plain data — every branch here is one that
//! the scripts expressed as `if`/`throw`, so the tests in `tests.rs` pin the
//! port against the original control flow.

/// The `State` value of an adapter marker key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerState {
    Prepared,
    Installed,
    Removing,
    /// Missing, empty, or not one of the three transaction states.
    Unknown,
}

/// `State` strings are a hive contract shared with the retired scripts;
/// existing installs carry them verbatim.
pub fn classify(state: &str) -> MarkerState {
    match state {
        "prepared" => MarkerState::Prepared,
        "installed" => MarkerState::Installed,
        "removing" => MarkerState::Removing,
        _ => MarkerState::Unknown,
    }
}

/// What `enable` should do given the current marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDecision {
    Proceed,
    /// Already installed: a friendly no-op (PowerShell editions) or an error
    /// (cmd — the scripts' exact behaviors differ).
    AlreadyInstalled,
    /// An interrupted transaction exists; the uninstall must recover it first.
    RecoverRequired,
}

pub fn decide_cmd_install(marker_present: bool, state: MarkerState) -> InstallDecision {
    if !marker_present {
        return InstallDecision::Proceed;
    }
    match state {
        MarkerState::Installed => InstallDecision::AlreadyInstalled,
        _ => InstallDecision::RecoverRequired,
    }
}

pub fn decide_ps_install(marker_present: bool, state: MarkerState) -> InstallDecision {
    if !marker_present {
        return InstallDecision::Proceed;
    }
    match state {
        MarkerState::Installed => InstallDecision::AlreadyInstalled,
        _ => InstallDecision::RecoverRequired,
    }
}

/// What `disable` should do given the current marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallDecision {
    /// No marker: report "not installed" and exit 0.
    NotInstalled,
    /// State value is not one of the three transaction states.
    UnknownState,
    /// The shell's own value changed after we installed it; refuse.
    AutoRunChanged,
    Proceed,
}

pub fn decide_cmd_uninstall(
    marker_present: bool,
    state: MarkerState,
    autorun: AutorunVerdict,
) -> UninstallDecision {
    if !marker_present {
        return UninstallDecision::NotInstalled;
    }
    if state == MarkerState::Unknown {
        return UninstallDecision::UnknownState;
    }
    if autorun == AutorunVerdict::Changed {
        return UninstallDecision::AutoRunChanged;
    }
    UninstallDecision::Proceed
}

pub fn decide_ps_uninstall(marker_present: bool, state: MarkerState) -> UninstallDecision {
    if !marker_present {
        return UninstallDecision::NotInstalled;
    }
    match state {
        MarkerState::Unknown => UninstallDecision::UnknownState,
        _ => UninstallDecision::Proceed,
    }
}

/// The AutoRun value the installer writes: the marker alone, or the user's
/// existing value appended verbatim with ` & `.
pub fn installed_autorun(original: &str, marker: &str) -> String {
    if original.trim().is_empty() {
        marker.to_string()
    } else {
        format!("{original} & {marker}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutorunVerdict {
    /// Current value equals what we installed (or what was there before us).
    Matches,
    /// Changed after installation; uninstall must refuse.
    Changed,
}

pub fn judge_autorun(
    current_present: bool,
    current: &str,
    installed: &str,
    original: &str,
) -> AutorunVerdict {
    if !current_present {
        return AutorunVerdict::Matches;
    }
    if current == installed || current == original {
        AutorunVerdict::Matches
    } else {
        AutorunVerdict::Changed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    WindowsPowerShell,
    PowerShell,
}

impl Edition {
    /// Documents subfolder and registry leaf for the edition.
    pub fn folder_name(self) -> &'static str {
        match self {
            Self::WindowsPowerShell => "WindowsPowerShell",
            Self::PowerShell => "PowerShell",
        }
    }

    pub fn registry_leaf(self) -> &'static str {
        self.folder_name()
    }

    /// Human name used in user-facing messages.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::WindowsPowerShell => "Windows PowerShell",
            Self::PowerShell => "PowerShell",
        }
    }

    /// The `fwdslash integration <id>` identifier.
    pub fn cli_id(self) -> &'static str {
        match self {
            Self::WindowsPowerShell => "windows-powershell",
            Self::PowerShell => "powershell",
        }
    }
}

pub fn other_edition(edition: Edition) -> Edition {
    match edition {
        Edition::WindowsPowerShell => Edition::PowerShell,
        Edition::PowerShell => Edition::WindowsPowerShell,
    }
}

/// Whether the shared `PowerShell\<version>` module directory should be
/// removed: only when the other edition has no marker key.
pub fn remove_shared_module(other_marker_present: bool) -> bool {
    !other_marker_present
}
