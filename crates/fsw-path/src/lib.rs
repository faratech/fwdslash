//! Pure resolver for Linux-style WSL paths typed into Windows navigation surfaces.
//!
//! `/etc/apt` becomes `\\wsl.localhost\Ubuntu\etc\apt`. This crate is the single
//! funnel every surface resolves through — Explorer, Run, Search, the shell
//! adapters and the CLI — so a change here reaches all of them at once.
//!
//! It has no dependencies and no `unsafe`, and it is the only crate shared
//! across both windows-rs version islands. Keep it that way: see
//! `docs/dependencies.md`.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

/// The UNC prefix every resolved path is built on.
pub const WSL_ROOT_UNC: &str = r"\\wsl.localhost";

/// Why an input is not a usable forward-slash WSL path.
///
/// Deliberately **not** `#[non_exhaustive]`: adding a variant must break the
/// name and message tables below at compile time, because those strings are a
/// wire contract (`event=path_rejected reason=<name>` in broker diagnostics,
/// and user-facing text in the CLI and the tray).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolveError {
    NotASlashPath,
    DoubleLeadingSlash,
    /// Unreachable in practice — see [`ResolveError::MissingDistribution`] notes
    /// on [`resolve_strict`]. Retained because the name is a diagnostics wire
    /// value that the C++ build can still emit.
    MissingDistribution,
    UnregisteredDistribution,
    BackslashNotAllowed,
    EmbeddedNul,
    TraversalAboveRoot,
    NoDefaultDistribution,
}

impl ResolveError {
    /// Stable category name. Mirrors the C++ `ResolveErrorName` exactly, because
    /// it is emitted as `reason=<name>` into the diagnostic log.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotASlashPath => "not_a_slash_path",
            Self::DoubleLeadingSlash => "double_leading_slash",
            Self::MissingDistribution => "missing_distribution",
            Self::UnregisteredDistribution => "unregistered_distribution",
            Self::BackslashNotAllowed => "backslash_not_allowed",
            Self::EmbeddedNul => "embedded_nul",
            Self::TraversalAboveRoot => "traversal_above_root",
            Self::NoDefaultDistribution => "no_default_distribution",
        }
    }

    /// The sentence shown to the user. Callers append a "Try /Ubuntu, /Debian."
    /// hint for [`Self::UnregisteredDistribution`]; see `hint_lists_distributions`.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotASlashPath => "This is not a forward-slash WSL path.",
            Self::DoubleLeadingSlash => {
                "Use one leading slash. Two leading slashes are not a WSL alias."
            }
            Self::MissingDistribution => {
                "Enter / to list WSL distributions, or /Distro/path to open one."
            }
            Self::UnregisteredDistribution => "That WSL distribution is not registered.",
            Self::BackslashNotAllowed => {
                "Use forward slashes in aliases, for example /Ubuntu/home."
            }
            Self::EmbeddedNul => "The path contains an invalid character.",
            Self::TraversalAboveRoot => "The path cannot traverse above the distribution root.",
            Self::NoDefaultDistribution => {
                "No default WSL distribution is available. Choose one in Forward Slash Windows \
                 settings, or use /Distro/path."
            }
        }
    }

    /// Whether the C++ appends the " Try /Ubuntu, ..." suffix for this error.
    #[must_use]
    pub const fn hint_lists_distributions(self) -> bool {
        matches!(self, Self::UnregisteredDistribution)
    }
}

/// How a leading segment that is not a registered distribution is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareSlashMode {
    /// `/` lists distributions; `/tmp` is an error. The default.
    DistributionList,
    /// `/` and `/tmp` both resolve inside the default distribution.
    DefaultDistribution,
}

/// The set of WSL distributions currently registered for this user.
pub trait Registry {
    /// Ordinal, case-insensitive membership test.
    fn is_registered(&self, name: &str) -> bool;
}

impl Registry for [&str] {
    fn is_registered(&self, name: &str) -> bool {
        self.iter().any(|candidate| eq_ignore_case(candidate, name))
    }
}

impl Registry for [String] {
    fn is_registered(&self, name: &str) -> bool {
        self.iter().any(|candidate| eq_ignore_case(candidate, name))
    }
}

impl<T: Registry + ?Sized> Registry for &T {
    fn is_registered(&self, name: &str) -> bool {
        (**self).is_registered(name)
    }
}

/// Everything resolution needs besides the input itself.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a, R: Registry + ?Sized> {
    pub registry: &'a R,
    pub mode: BareSlashMode,
    /// The user's pinned distribution, if any (`BareSlashDistribution`).
    pub preferred: Option<&'a str>,
    /// WSL's own default distribution, if one could be determined.
    pub wsl_default: Option<&'a str>,
}

impl<'a, R: Registry + ?Sized> Context<'a, R> {
    /// A context that only accepts explicit `/Distro/path` inputs.
    #[must_use]
    pub const fn list_mode(registry: &'a R) -> Self {
        Self {
            registry,
            mode: BareSlashMode::DistributionList,
            preferred: None,
            wsl_default: None,
        }
    }
}

/// Scratch space the resolver renders into, reused across calls so the hot path
/// does not allocate. One buffer per thread is the intended usage.
#[derive(Debug, Default)]
pub struct RenderBuf {
    unc: String,
    linux: String,
}

impl RenderBuf {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            unc: String::new(),
            linux: String::new(),
        }
    }

    /// Pre-reserve so steady-state resolution never reallocates.
    #[must_use]
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            unc: String::with_capacity(bytes),
            linux: String::with_capacity(bytes),
        }
    }
}

/// A successfully resolved input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved<'r> {
    /// A bare `/` in distribution-list mode: the provider root that lists distros.
    WslRoot,
    /// A path inside one distribution.
    Distribution(DistributionPath<'r>),
    /// A path under the user's custom bare-slash root (Rust-layer feature; the
    /// C++ resolver has no counterpart — see docs/divergences.md).
    Folder(FolderPath<'r>),
}

impl<'r> Resolved<'r> {
    /// The form written into the Explorer address bar, handed to `ShellExecuteEx`
    /// and `Navigate2`, and printed by `fwdslash resolve`.
    ///
    /// This is a wire contract: `ForwardSlashWindows.psm1` compares the output of
    /// `fwdslash resolve /` against the literal `\\wsl.localhost` (and its
    /// trailing-separator spelling). A custom root resolves to a longer UNC or
    /// to a drive path — never to those literals.
    #[must_use]
    pub const fn unc_display(&self) -> &'r str {
        match self {
            Self::WslRoot => WSL_ROOT_UNC,
            Self::Distribution(path) => path.unc_display,
            Self::Folder(path) => path.display,
        }
    }

    /// The Linux path the user meant, normalized.
    ///
    /// For [`Resolved::Folder`] this is the path *under* the chosen root
    /// (`/` when the input is bare), since a Win32 root has no Linux path.
    #[must_use]
    pub const fn linux_path(&self) -> &'r str {
        match self {
            Self::WslRoot => "/",
            Self::Distribution(path) => path.linux_path,
            Self::Folder(path) => path.under_root,
        }
    }

    /// The distribution this resolved into, or `None` for the provider root
    /// or a folder root.
    #[must_use]
    pub const fn distribution(&self) -> Option<&'r str> {
        match self {
            Self::WslRoot | Self::Folder(_) => None,
            Self::Distribution(path) => Some(path.distribution),
        }
    }

    #[must_use]
    pub const fn is_wsl_root(&self) -> bool {
        matches!(self, Self::WslRoot)
    }

    /// True only for the provider root that lists distributions. Tests and the
    /// broker key on this rather than `distribution().is_none()`, because a
    /// folder root also has no distribution.
    #[must_use]
    pub const fn is_provider_root(&self) -> bool {
        matches!(self, Self::WslRoot)
    }
}

/// A path inside a single distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributionPath<'r> {
    distribution: &'r str,
    unc_display: &'r str,
    linux_path: &'r str,
    had_trailing_separator: bool,
}

impl<'r> DistributionPath<'r> {
    /// Preserves the casing the user typed, not the registry's canonical
    /// spelling — `/ubuntu/home` yields `\\wsl.localhost\ubuntu\home`.
    #[must_use]
    pub const fn distribution(&self) -> &'r str {
        self.distribution
    }

    #[must_use]
    pub const fn unc_display(&self) -> &'r str {
        self.unc_display
    }

    #[must_use]
    pub const fn linux_path(&self) -> &'r str {
        self.linux_path
    }

    #[must_use]
    pub const fn had_trailing_separator(&self) -> bool {
        self.had_trailing_separator
    }

    /// True when Win32 path normalization would open a *different* file than
    /// [`Self::linux_path`] names, because a component ends in `.` or a space
    /// and Win32 strips those outside the `\\?\` namespace. On ext4 `secret `
    /// and `secret` are distinct files.
    #[must_use]
    pub fn has_win32_normalization_hazard(&self) -> bool {
        self.linux_path
            .split('/')
            .any(|component| component.ends_with('.') || component.ends_with(' '))
    }
}

/// A path under the user's custom bare-slash root — any Win32 location, a
/// drive path (`C:\code`) or a UNC (`\\wsl.localhost\Ubuntu\home\mike`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderPath<'r> {
    display: &'r str,
    under_root: &'r str,
    had_trailing_separator: bool,
}

impl<'r> FolderPath<'r> {
    /// The Win32 path to open: the string handed to `ShellExecuteEx` and
    /// written into the address bar.
    #[must_use]
    pub const fn display(&self) -> &'r str {
        self.display
    }

    /// The `/`-separated path below the chosen root — `/` when the input is
    /// bare. A Win32 root has no Linux path, so this is the *relative* tail.
    #[must_use]
    pub const fn under_root(&self) -> &'r str {
        self.under_root
    }

    #[must_use]
    pub const fn had_trailing_separator(&self) -> bool {
        self.had_trailing_separator
    }
}

/// Resolve an input against a user-chosen folder root: `/` is the root and
/// everything below it joins onto it lexically, exactly the way `render`
/// joins components onto a distribution. Pure string work — no filesystem
/// access, no canonicalization — matching the resolver's contract.
///
/// The input shape rules R1-R4 apply unchanged, including `..` clamping:
/// traversal past the root is [`ResolveError::TraversalAboveRoot`], because
/// the root is the folder the user chose, not a suggestion.
pub fn resolve_under_root<'r>(
    input: &str,
    root: &str,
    buf: &'r mut RenderBuf,
) -> Result<Resolved<'r>, ResolveError> {
    if input.is_empty() || !input.starts_with('/') {
        return Err(ResolveError::NotASlashPath);
    }
    if input.as_bytes().get(1) == Some(&b'/') {
        return Err(ResolveError::DoubleLeadingSlash);
    }
    if input.contains('\0') {
        return Err(ResolveError::EmbeddedNul);
    }
    if input.contains('\\') {
        return Err(ResolveError::BackslashNotAllowed);
    }

    let RenderBuf { unc, linux } = buf;
    unc.clear();
    linux.clear();

    // The root without trailing separators. A bare drive keeps exactly one:
    // `C:` alone is a drive-relative path in Win32, not a folder.
    unc.push_str(root);
    while unc.ends_with('\\') {
        unc.pop();
    }
    if !unc.contains('\\') {
        unc.push('\\');
    }
    let base_end = unc.len();

    let had_trailing_separator = input.ends_with('/');
    for component in input[1..].split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if unc.len() == base_end {
                return Err(ResolveError::TraversalAboveRoot);
            }
            let cut = unc
                .get(base_end..)
                .and_then(|rest| rest.rfind('\\'))
                .unwrap_or(0);
            unc.truncate(base_end + cut);
            continue;
        }
        // A drive-root base (`C:\`) already ends in the separator; appending
        // another would produce `C:\\tmp`, which Win32 reads as an UNC-style
        // path prefix. Every other base needs one added.
        if !unc.ends_with('\\') {
            unc.push('\\');
        }
        unc.push_str(component);
    }

    let has_components = unc.len() > base_end;
    if had_trailing_separator && has_components {
        unc.push('\\');
    }

    if has_components {
        // Mirror the UNC tail below the root, with `/` separators. When the
        // base is a drive root the tail does not start with a separator (it
        // is already part of the base), so the leading `/` is explicit.
        let tail = unc[base_end..].strip_prefix('\\').unwrap_or(&unc[base_end..]);
        linux.push('/');
        for (index, part) in tail.split('\\').enumerate() {
            if index > 0 {
                linux.push('/');
            }
            linux.push_str(part);
        }
    } else {
        linux.push('/');
    }

    Ok(Resolved::Folder(FolderPath {
        display: unc.as_str(),
        under_root: linux.as_str(),
        had_trailing_separator,
    }))
}

/// Whether `root` is a usable custom-root value: an absolute drive path
/// (`C:`, `C:\Users\me`) or UNC (`\\server\share`, `\\wsl.localhost\Ubuntu\…`).
/// Existence is deliberately not checked — a `\\wsl.localhost` root may be
/// offline at set time.
#[must_use]
pub fn is_valid_windows_root(root: &str) -> bool {
    if root.is_empty() || root.contains('/') || root.contains('\0') {
        return false;
    }
    // Device and NT namespaces are kernel paths, not user folders.
    for namespace in [r"\\.\", r"\\?\", r"\??\"] {
        if root.starts_with(namespace) {
            return false;
        }
    }
    let forbidden = |byte: u8| matches!(byte, b'"' | b'<' | b'>' | b'|' | 0..=0x1F);

    let bytes = root.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        // Drive form; a second `:` (`C:\a:b`) would be a stream, not a folder.
        return root[2..].bytes().all(|byte| byte != b':' && !forbidden(byte));
    }
    if let Some(rest) = root.strip_prefix(r"\\") {
        // UNC form: `\\server\share[\dir …]` — a bare `\\server` is not a
        // folder, and UNC paths never carry a colon.
        return match rest.find('\\') {
            Some(share_start) if share_start > 0 => {
                rest.bytes().all(|byte| byte != b':' && !forbidden(byte))
            }
            _ => false,
        };
    }
    false
}

/// Resolve an explicit `/Distro/path` input. Mirrors the C++ `ResolveSlashPath`.
///
/// Note that `ResolveError::MissingDistribution` is unreachable here, and always
/// was: an empty distribution segment requires either `input == "/"` (returned
/// earlier as [`Resolved::WslRoot`]) or `input[1] == '/'` (rejected earlier as
/// [`ResolveError::DoubleLeadingSlash`]).
pub fn resolve_strict<'r, R: Registry + ?Sized>(
    input: &str,
    registry: &R,
    buf: &'r mut RenderBuf,
) -> Result<Resolved<'r>, ResolveError> {
    resolve(input, &Context::list_mode(registry), buf)
}

/// Resolve an input under the user's bare-slash mode.
pub fn resolve<'r, R: Registry + ?Sized>(
    input: &str,
    ctx: &Context<'_, R>,
    buf: &'r mut RenderBuf,
) -> Result<Resolved<'r>, ResolveError> {
    // Rules R1-R5, in the C++'s order. R2 deliberately precedes R3, so `//\0`
    // reports DoubleLeadingSlash rather than EmbeddedNul.
    if input.is_empty() || !input.starts_with('/') {
        return Err(ResolveError::NotASlashPath);
    }
    if input.as_bytes().get(1) == Some(&b'/') {
        return Err(ResolveError::DoubleLeadingSlash);
    }
    if input.contains('\0') {
        return Err(ResolveError::EmbeddedNul);
    }
    if input.contains('\\') {
        return Err(ResolveError::BackslashNotAllowed);
    }

    // R7-R9: split the leading segment and decide whether it names a distro.
    let after_root = &input[1..];
    let (segment, rest_from) = match after_root.find('/') {
        Some(offset) => (&after_root[..offset], 1 + offset + 1),
        None => (after_root, input.len()),
    };

    let explicit = !segment.is_empty() && ctx.registry.is_registered(segment);

    if explicit {
        // R6 on the input itself.
        let trailing = input.len() > 1 && input.ends_with('/');
        return render(input, segment, rest_from, trailing, buf).map(Resolved::Distribution);
    }

    // Not an explicit distribution. In list mode that is either the provider
    // root or an error; in default mode both fall through to the pinned distro.
    let is_bare_root = input == "/";
    if ctx.mode == BareSlashMode::DistributionList {
        return if is_bare_root {
            Ok(Resolved::WslRoot)
        } else {
            debug_assert!(!segment.is_empty(), "MissingDistribution is unreachable");
            Err(ResolveError::UnregisteredDistribution)
        };
    }

    let target = ctx
        .preferred
        .filter(|name| !name.is_empty() && ctx.registry.is_registered(name))
        .or_else(|| {
            ctx.wsl_default
                .filter(|name| !name.is_empty() && ctx.registry.is_registered(name))
        })
        .ok_or(ResolveError::NoDefaultDistribution)?;

    // The C++ builds `"/" + target + input` and re-parses. Because `input`
    // always begins with `/` and a validated distribution name contains no `/`,
    // the component scan starts at exactly the same offset either way — so we
    // pass the distribution out-of-band and scan `input` from index 1 instead.
    // `rewrite_equivalence` in the tests proves the two agree.
    //
    // R6 applies to the *rewritten* string, whose length is always > 1, so the
    // `len() > 1` guard the explicit path needs does not apply here. This is the
    // only observable difference, and only for `input == "/"`.
    render(input, target, 1, input.ends_with('/'), buf).map(Resolved::Distribution)
}

/// Render `\\wsl.localhost\<distribution>\<components>` followed by the Linux
/// path into `buf`, normalizing `.` and `..` by truncation.
///
/// `component_start` is the byte offset in `input` at which component scanning
/// begins; everything before it has already been consumed as the distribution.
fn render<'r>(
    input: &str,
    distribution: &str,
    component_start: usize,
    had_trailing_separator: bool,
    buf: &'r mut RenderBuf,
) -> Result<DistributionPath<'r>, ResolveError> {
    // Split the &mut into two disjoint field borrows so the Linux path can be
    // built from the UNC tail without self-borrowing one buffer.
    let RenderBuf { unc, linux } = buf;
    unc.clear();
    linux.clear();

    unc.push_str(WSL_ROOT_UNC);
    unc.push('\\');
    let distro_start = unc.len();
    unc.push_str(distribution);
    let distro_end = unc.len();

    // R10: normalize components. `..` truncates back to the previous separator,
    // which is why no component vector is needed.
    let tail = input.get(component_start..).unwrap_or_default();
    for component in tail.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if unc.len() == distro_end {
                return Err(ResolveError::TraversalAboveRoot);
            }
            // The `==` check above guarantees a separator exists behind the
            // distro root; the fallback keeps the resolver total (a panic
            // under panic = "abort" would take down the resident broker).
            let cut = unc
                .get(distro_end..)
                .and_then(|rest| rest.rfind('\\'))
                .unwrap_or(0);
            unc.truncate(distro_end + cut);
            continue;
        }
        unc.push('\\');
        unc.push_str(component);
    }

    // R12: a trailing separator survives only if a component did.
    let has_components = unc.len() > distro_end;
    if had_trailing_separator && has_components {
        unc.push('\\');
    }

    // The Linux path is *derived* from the UNC tail rather than rendered
    // independently, so the two can never drift. Components contain neither `\`
    // (rejected by R4) nor `/` (they are the delimiters), so swapping separators
    // is exactly the inverse of the render.
    if has_components {
        for (index, part) in unc[distro_end..].split('\\').enumerate() {
            if index > 0 {
                linux.push('/');
            }
            linux.push_str(part);
        }
    } else {
        linux.push('/');
    }

    Ok(DistributionPath {
        distribution: &unc[distro_start..distro_end],
        unc_display: unc.as_str(),
        linux_path: linux.as_str(),
        had_trailing_separator,
    })
}

/// Windows' *simple* uppercase mapping: the 1:1 half of `char::to_uppercase`.
///
/// `CompareStringOrdinal(.., bIgnoreCase = TRUE)` folds through the simple
/// uppercase table, which never changes a string's length. Rust's
/// `to_uppercase` is the *full* mapping and expands some characters — `ß` to
/// `SS`, `ﬁ` to `FI`. Taking only the single-character mappings reproduces the
/// simple table, which is what keeps `ß` distinct from `SS` the way Win32 does.
fn simple_upper(c: char) -> char {
    let mut mapped = c.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(upper), None) => upper,
        _ => c,
    }
}

/// Ordinal, case-insensitive equality.
///
/// Matches `CompareStringOrdinal(.., TRUE)` for every case the product can
/// plausibly meet, including the Turkish dotted/dotless I pair
/// (`İ` U+0130 stays distinct from `i`; `ı` U+0131 folds to `I`, as Win32 does).
/// Remaining divergences are recorded in `docs/divergences.md`; a Windows-only
/// differential test against `CompareStringOrdinal` over the BMP is the way to
/// keep that list honest.
#[must_use]
pub fn eq_ignore_case(left: &str, right: &str) -> bool {
    if left.is_ascii() && right.is_ascii() {
        return left.eq_ignore_ascii_case(right);
    }
    // The simple mapping is 1:1, so a lockstep character compare is equivalent
    // to the C++'s length-check-then-compare and needs no separate short circuit.
    let mut left_folded = left.chars().map(simple_upper);
    let mut right_folded = right.chars().map(simple_upper);
    loop {
        match (left_folded.next(), right_folded.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

/// Whether a name read from the WSL registry is usable as a distribution.
///
/// Rejecting these at cache-build time is what lets [`render`] pass a
/// distribution out-of-band instead of concatenating and re-parsing, and it
/// keeps this resolver in agreement with the minifilter's
/// `FswIsValidDistributionName`. The driver's 127-unit cap is deliberately not
/// applied here — enforcing it would stop routing a long-named distro that
/// works today, so truncation stays in the filter-message builder.
#[must_use]
pub fn is_valid_distribution_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || (c as u32) < 0x20)
}
