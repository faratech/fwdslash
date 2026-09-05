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

/// Whether one ` & `-joined AutoRun segment is a fwdslash hook — a
/// `call "…ForwardSlashWindows…fsw-autorun.cmd"`. Recognised case-insensitively
/// by the two path markers rather than an exact path, so a hook a *different*
/// install left behind is still ours (#37).
#[must_use]
pub fn is_fwdslash_autorun_segment(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    lower.trim_start().starts_with("call ")
        && lower.contains("forwardslashwindows")
        && lower.contains("fsw-autorun.cmd")
}

/// The observed AutoRun with every fwdslash hook segment removed, so the true
/// third-party value is recovered even when a prior install's marker was lost
/// but its `call "…fsw-autorun.cmd"` hook persisted (exactly the MSIX-uninstall
/// leftover). Empty when fwdslash's hook was the only content. Segments are the
/// ` & ` groups the installer itself composes, so a genuine third-party value —
/// including one that already contains ` & ` — rejoins byte-for-byte (#37).
#[must_use]
pub fn strip_fwdslash_autorun(current: &str) -> String {
    if current.is_empty() {
        return String::new();
    }
    let kept: Vec<&str> = current
        .split(" & ")
        .filter(|segment| !is_fwdslash_autorun_segment(segment))
        .collect();
    kept.join(" & ")
}

/// Existence and registry kind survive even when the original value is empty.
pub fn original_autorun_present(present: bool, raw: &str, cleaned: &str) -> bool {
    present && (raw.is_empty() || !cleaned.is_empty())
}

/// Whether an AutoRun value routes through a fwdslash hook at all — the cheap
/// classifier `fwdslash doctor` and the self-clean probe use.
#[must_use]
pub fn autorun_references_fwdslash(current: &str) -> bool {
    current.split(" & ").any(is_fwdslash_autorun_segment)
}

/// The quoted path out of the first fwdslash hook segment, so its existence can
/// be tested on disk (a missing target is the cmd orphan). `None` when the
/// value carries no fwdslash hook or the segment is not quoted.
#[must_use]
pub fn fwdslash_autorun_path(current: &str) -> Option<String> {
    let segment = current
        .split(" & ")
        .find(|segment| is_fwdslash_autorun_segment(segment))?;
    let open = segment.find('"')?;
    let rest = segment.get(open + 1..)?;
    let close = rest.find('"')?;
    rest.get(..close).map(str::to_owned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutorunVerdict {
    /// Current value equals what we installed (or what was there before us).
    Matches,
    /// Changed after installation; uninstall must refuse.
    Changed,
}

/// The **0.0.2 truncation compatibility rule**. 0.0.2's registry reader
/// stripped trailing zero *bytes* before pairing them into UTF-16 code units,
/// so every value it read came back one character short whenever the last
/// character was ASCII (see `reg::decode_reg_string`). A hive/marker pair left
/// in that shape must still be upgradable, so `current` also counts as
/// matching `installed` when `installed` is `current` plus **exactly one**
/// more trailing character. One character and no more: any longer divergence
/// is a genuine third-party edit and is still refused. `original` is compared
/// exactly — the tolerance exists only for the value we wrote ourselves.
fn matches_installed(current: &str, installed: &str) -> bool {
    if current == installed {
        return true;
    }
    if current.is_empty() {
        return false;
    }
    installed
        .strip_prefix(current)
        .is_some_and(|tail| tail.chars().count() == 1)
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
    if matches_installed(current, installed) || current == original {
        AutorunVerdict::Matches
    } else {
        AutorunVerdict::Changed
    }
}

/// The product-presence decision for the orphan self-clean (#37 addendum).
///
/// The cheap check (a `Test-Path` on the probe recorded at install time) runs
/// on every shell start and must stay negligible; the slow confirm
/// (`Get-AppxPackage` / a recorded install directory) runs only when the cheap
/// check has already failed, so a transient alias blip during an in-flight
/// update never triggers cleanup. The product is confirmed gone only when both
/// say so.
#[must_use]
pub fn product_confirmed_gone(cheap_probe_present: bool, slow_confirm_present: bool) -> bool {
    !cheap_probe_present && !slow_confirm_present
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

/// Whether the shared `PowerShell\<version>` module directory that *this*
/// marker deployed should be removed.
///
/// The unit of sharing is a **version directory**, not the tree: two editions
/// only collide when both markers name the same version. Keying the decision
/// on "the other edition has a marker at all" (what 0.0.2 did) meant that
/// upgrading both editions one after the other left each old directory pinned
/// by the edition that had not been upgraded yet, and nothing ever came back
/// for it — hence the observed `PowerShell\0.0.1`, `0.0.2` and `0.0.3` all
/// living side by side. `other_marker_version` is `None` when the other
/// edition has no marker.
pub fn remove_shared_module(other_marker_version: Option<&str>, this_version: &str) -> bool {
    other_marker_version != Some(this_version)
}

// ---------------------------------------------------------------------------
// PowerShell execution policy (#45)
// ---------------------------------------------------------------------------

/// An execution policy as `Get-ExecutionPolicy` names it.
///
/// Windows PowerShell 5.1 ships **Restricted** on client editions, so the
/// guarded block the adapter appends to `profile.ps1` can never load there
/// until the user changes it — the most common reason an install appears to
/// succeed and then does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Restricted,
    /// No policy configured in any scope. Windows PowerShell treats this as
    /// `Restricted`; `pwsh` reports `RemoteSigned` instead and so never
    /// surfaces it in practice.
    Undefined,
    /// Blocking for a different reason: every script *including the user's own
    /// `profile.ps1`* would need an Authenticode signature, and no installer
    /// can sign a file the user owns. Signing the shipped module does not
    /// help.
    AllSigned,
    RemoteSigned,
    Unrestricted,
    Bypass,
    /// Anything else the shell printed. Never blocking — a parse failure must
    /// not refuse an install that would have worked.
    Unrecognized,
}

/// Parses one `Get-ExecutionPolicy` line. Case-insensitive and
/// whitespace-tolerant, because the value arrives as raw console output.
#[must_use]
pub fn parse_execution_policy(reported: &str) -> ExecutionPolicy {
    let trimmed = reported.trim();
    if trimmed.eq_ignore_ascii_case("restricted") {
        ExecutionPolicy::Restricted
    } else if trimmed.eq_ignore_ascii_case("undefined") {
        ExecutionPolicy::Undefined
    } else if trimmed.eq_ignore_ascii_case("allsigned") {
        ExecutionPolicy::AllSigned
    } else if trimmed.eq_ignore_ascii_case("remotesigned") {
        ExecutionPolicy::RemoteSigned
    } else if trimmed.eq_ignore_ascii_case("unrestricted") {
        ExecutionPolicy::Unrestricted
    } else if trimmed.eq_ignore_ascii_case("bypass") {
        ExecutionPolicy::Bypass
    } else {
        ExecutionPolicy::Unrecognized
    }
}

/// Why a policy blocks the adapter, and the one line that fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyBlock {
    /// A complete sentence naming the edition and the policy.
    pub reason: String,
    /// A complete sentence ending in the command to run, with no trailing
    /// punctuation — it is meant to be copied.
    pub remedy: String,
}

/// The verdict on one edition's effective execution policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// Scripts load. `note` is `Some` only for a policy string this build does
    /// not know, which is reported but never refused.
    Allowed { note: Option<String> },
    Blocked(PolicyBlock),
}

impl PolicyVerdict {
    #[must_use]
    pub fn blocked(&self) -> Option<&PolicyBlock> {
        match self {
            Self::Blocked(block) => Some(block),
            Self::Allowed { .. } => None,
        }
    }

    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }
}

/// The command that fixes a Restricted machine. Named once so the CLI text,
/// the docs and the tests cannot drift apart.
pub const REMOTE_SIGNED_COMMAND: &str = "Set-ExecutionPolicy -Scope CurrentUser RemoteSigned";

/// Where the user has to run [`REMOTE_SIGNED_COMMAND`]: the policy is per
/// edition, so fixing Windows PowerShell from a `pwsh` window changes the
/// wrong one.
fn shell_to_run_in(edition: Edition) -> &'static str {
    match edition {
        Edition::WindowsPowerShell => "Windows PowerShell",
        Edition::PowerShell => "PowerShell 7 (pwsh)",
    }
}

/// Classifies `reported` for `edition`. Pure: the caller supplies whatever the
/// edition's own shell printed for `Get-ExecutionPolicy`.
///
/// Blocking policies are `Restricted`, `Undefined` and `AllSigned`. Everything
/// else — including a string this build has never heard of — is allowed, so a
/// future policy name or a localized shell can never refuse an install that
/// would have worked.
#[must_use]
pub fn classify_execution_policy(edition: Edition, reported: &str) -> PolicyVerdict {
    let policy = parse_execution_policy(reported);
    let edition_name = edition.display_name();
    let where_to_run = shell_to_run_in(edition);
    let remedy = format!(
        "Run this in {where_to_run}, then enable the adapter again: {REMOTE_SIGNED_COMMAND}"
    );
    match policy {
        ExecutionPolicy::Restricted => PolicyVerdict::Blocked(PolicyBlock {
            reason: format!(
                "{edition_name}'s execution policy is Restricted, so the profile the adapter installs can never load."
            ),
            remedy,
        }),
        ExecutionPolicy::Undefined => PolicyVerdict::Blocked(PolicyBlock {
            reason: format!(
                "{edition_name}'s execution policy is Undefined, which is treated as Restricted, so the profile the adapter installs can never load."
            ),
            remedy,
        }),
        ExecutionPolicy::AllSigned => PolicyVerdict::Blocked(PolicyBlock {
            reason: format!(
                "{edition_name}'s execution policy is AllSigned, which the adapter does not support: under AllSigned your own profile.ps1 would need an Authenticode signature that Forward Slash Windows cannot provide."
            ),
            remedy: format!(
                "Run this in {where_to_run} to use the adapter, then enable it again: {REMOTE_SIGNED_COMMAND}"
            ),
        }),
        ExecutionPolicy::RemoteSigned | ExecutionPolicy::Unrestricted | ExecutionPolicy::Bypass => {
            PolicyVerdict::Allowed { note: None }
        }
        ExecutionPolicy::Unrecognized => PolicyVerdict::Allowed {
            note: Some(format!(
                "unrecognized execution policy '{}'; treating it as allowing scripts",
                reported.trim()
            )),
        },
    }
}

/// The message `fwdslash integration <id> enable` prints when the preflight
/// refuses. Nothing has been written at that point, which is the part a user
/// needs to know before re-running anything.
#[must_use]
pub fn policy_install_error(block: &PolicyBlock) -> String {
    format!("{} Nothing was changed. {}", block.reason, block.remedy)
}

/// The message the alias verification prints when the child shell refused to
/// run the profile (exit 42) and the policy explains why. The writes are
/// already undone by the transaction's rollback, which is the only difference
/// from [`policy_install_error`].
#[must_use]
pub fn policy_verify_error(block: &PolicyBlock) -> String {
    format!(
        "{} The installation was rolled back. {}",
        block.reason, block.remedy
    )
}

/// The `fwdslash doctor` / `fwdslash integrations` line for one edition's
/// policy: the policy the shell reported, plus the remedy when it blocks.
#[must_use]
pub fn policy_health_status(reported: &str, verdict: &PolicyVerdict) -> String {
    let policy = reported.trim();
    match verdict {
        PolicyVerdict::Blocked(block) => {
            format!("{policy} — blocked by execution policy: {}", block.remedy)
        }
        PolicyVerdict::Allowed { note: Some(note) } => format!("{policy} ({note})"),
        PolicyVerdict::Allowed { note: None } => policy.to_string(),
    }
}
