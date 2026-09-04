# Deliberate divergences from the C++ build

Every entry here is a place the Rust port behaves differently on purpose. Each
one has a test pinning it. Anything *not* listed here is meant to be
byte-for-byte identical, and a difference is a bug.

## Resolver (`fsw-path`)

### 1. Case folding uses Rust's Unicode tables, not `CompareStringOrdinal`

The C++ calls `CompareStringOrdinal(.., bIgnoreCase = TRUE)`. The Rust resolver
folds in pure Rust so the crate stays dependency-free and Linux CI exercises the
*shipping* comparison rather than a stand-in.

`CompareStringOrdinal` folds through the **simple** uppercase table, which is 1:1
and never changes a string's length. Rust's `char::to_uppercase` is the **full**
mapping and expands some characters (`ß` → `SS`, `ﬁ` → `FI`). `eq_ignore_case`
therefore takes only the single-character mappings — see `simple_upper` — which
reproduces the simple table.

Verified to **agree** with Win32:

| Pair | Result | Why |
|---|---|---|
| `İ` (U+0130) vs `i` | not equal | U+0130 has no simple mapping to `i` |
| `ı` (U+0131) vs `I` | equal | U+0131 simple-uppercases to `I` |
| `ß` vs `SS` | not equal | the expansion is suppressed |
| `ﬁ` (U+FB01) vs `FI` | not equal | ditto |
| `ünicode` vs `ÜNICODE` | equal | ordinary 1:1 cased mapping |

**What still diverges:** the Unicode version. Rust's tables are pinned by the
toolchain; Win32's are pinned by the user's Windows build. Two characters added
or recased between those versions can disagree. WSL distribution names are
overwhelmingly ASCII (`Ubuntu`, `Debian`, `kali-linux`, `openSUSE-Tumbleweed`),
and the ASCII fast path is exact, so the exposure is theoretical.

**Follow-up:** a Windows-only differential test walking the BMP against
`CompareStringOrdinal` is the honest way to keep this table current. It cannot
run on Linux CI, so it belongs in the Windows leg.

Pinned by `case_folding_matches_the_win32_simple_uppercase_table`.

### 2. Failure returns `Err`, not a struct with partial state

The C++ `ResolveResult` carries an error field alongside populated data, and its
failure paths leave that data inconsistent: `distribution` is populated on
`unregistered_distribution`, `target` is still `distribution` on
`traversal_above_root`, and `had_trailing_separator` is stale on
`no_default_distribution`. Nothing in the tree reads those fields after a
failure, so `Result<Resolved, ResolveError>` is a safe tightening.

### 3. `ResolveError::MissingDistribution` is unreachable, and always was

An empty distribution segment requires either `input == "/"` (returned earlier as
the provider root) or `input[1] == '/'` (rejected earlier as
`DoubleLeadingSlash`). The variant is retained because its name is a diagnostics
wire value (`reason=missing_distribution`) that the C++ build can still emit, and
a `debug_assert!` documents the unreachability at the one call site.

### 4. Malformed distribution names are dropped, not half-registered

`is_valid_distribution_name` rejects names that are empty, `.`, `..`, or contain
`/`, `\`, `:` or a code unit below U+0020. The C++ registers whatever the registry
holds and then produces an unusable UNC — `\\wsl.localhost\a:b` — which the
redirector fails opaquely.

Dropping them at cache-build time is what lets the bare-slash rewrite pass the
distribution out-of-band instead of concatenating and re-parsing (a name
containing `/` would turn the concatenation into a double leading slash), and it
keeps the resolver in agreement with the minifilter's
`FswIsValidDistributionName`.

The driver's 127-UTF-16-unit cap is deliberately **not** applied here: enforcing
it would stop routing a long-named distribution that works today. Truncation
stays where it already is, in the filter-message builder.

### 5. The bare-slash rewrite is structural, not textual

The C++ builds `"/" + target + input` and re-parses it. The Rust resolver passes
the distribution out-of-band and scans `input` from index 1, which removes one
allocation and one full re-parse per rewritten keystroke — and in the C++ also
removed a second full registry enumeration, because the re-parse called the
`is_registered` predicate again.

This is an optimization, not a behaviour change, and it is not asserted on
faith: `rewrite_equivalence` runs the concatenate-and-reparse reference
implementation and the direct path over a 912-case corpus and requires byte
equality.

The one observable difference is `had_trailing_separator` for `input == "/"` in
default-distribution mode. The C++ computes it from the *rewritten* string
(`"/Ubuntu/"`, so `true`); the direct path reproduces that rather than computing
it from the input. Rule R12 discards it either way because no component survives,
so `unc_display` and `linux_path` are unaffected. Pinned by
`bare_slash_in_default_mode_reports_a_trailing_separator`.

## Settings window (`fsw-settings`)

The Rust settings app is built on `windows-reactor` rather than WinUI 3 XAML
interop, so a few things the C++ gets from the framework have no direct
equivalent. State reads are *not* on this list: `fsw-settings` reads HKCU and the
broker window in-process exactly as `RefreshState()` does, so every value shown
comes from the same source as the C++.

### 1. No icon in the title bar

`src/settings/main.cpp:387-392` sets `TitleBar.IconSource` to an
`ImageIconSource` over `ms-appx:///Assets/fwdslash-titlebar.png`. reactor models
no `IconSource` type at all — `PropertyId::ImageIconSource` is the *source*
property of the `ImageIcon` **element**, which is an `IconElement` and not
assignable to `IconSource`.

Binding `Microsoft.UI.Xaml.Controls.ImageIconSource` by hand was tried and
rejected: the IID and vtable layout match the SDK headers, `ITitleBar`'s
`SetIconSource` slot is in the right position, and everything up to the
activation succeeds — but `ImageIconSource::new()` fail-fasts (`0xC0000409`)
under the unpackaged Windows App SDK. The `TitleBar` `Content` slot is not a
substitute either: it centres its child, so the icon lands mid-bar instead of at
the leading edge.

The caption is therefore drawn by `TitleBar` itself, which puts it in exactly the
C++ position. The product icon still reaches the taskbar, Alt-Tab and Explorer
through the `IDI_FSW_APP` resource in `crates/fsw-settings/app.rc`. Closing this
needs an upstream `TitleBar.IconSource` in `windows-reactor`.

### 2. The navigation pane pushes content instead of overlaying it

`src/settings/main.cpp:396` sets `PaneDisplayMode = LeftCompact`. That pins WinUI's
`DisplayMode` to `Compact`, which hosts the pane in a `SplitView` set to
`CompactOverlay` — so opening the pane draws it *on top of* the page. The content
does not reflow, and the headings and body text are clipped mid-word behind it.
This is reproducible in the shipped C++ build (`out/package/.../fswsettings.exe`),
so it is a defect the port inherited rather than one it introduced.

The Rust app sets `PaneDisplayMode = Left`, which forces `DisplayMode = Expanded`
and therefore `SplitView CompactInline`. Closed, that renders the identical 48px
icon rail — the collapsed window is pixel-identical to the C++ one. Opened, the
pane expands inline to `OpenPaneLength` and the content shifts aside instead of
being covered.

Reverting to strict parity is a one-word change back to `LeftCompact`.

### 3. No refresh when the window is activated

The C++ hooks `window_.Activated` (`main.cpp:443-446`) so a change made by
`fwdslash` in a terminal shows up on alt-tab. reactor exposes no activation
observation — `HostEvent` carries only `WindowSize`, `ColorScheme` and errors.
The Rust app refreshes on every mutation, on navigation, and on the explicit
"Refresh status" button, which covers everything except an external change made
while the window is already open and untouched.

### 4. Deep links select the page but do not focus the control

`ShowSection` (`main.cpp:855-871`) calls `Focus(FocusState::Programmatic)` on the
toggle named by `fwdslash://settings/cmd` and friends. reactor exposes no
programmatic focus API, so the Rust app selects the right page and stops there.

### 5. Guards replace the `loading_` flag

The C++ suppresses handler re-entry with a `loading_` flag around the imperative
`RefreshState()` (`main.cpp:330`, `755`, `840`). A declarative view re-applies
every value on each mount, and the mount echo for `RadioButtons`/`ComboBox`
arrives *after* reactor's synchronous feedback-suppression window closes, so a
time-window flag cannot work. Every handler instead compares the requested value
against current state and returns early when they agree — see
`SettingsModel::update`. This is why the bare-slash controls are two
`RadioButton`s sharing a `GroupName`, matching `main.cpp:490-517`, rather than
the items-source `RadioButtons` control: `RadioButton.IsChecked` echoes
synchronously and is suppressed by the framework.

## Product behaviour (landing later — recorded here so the list stays in one place)

These are planned, not yet implemented. Each needs its own entry with a test
before the milestone that lands it can close.

- **Surface classification narrows.** The C++ treats every `#32770` window in
  every process as an editable file dialog. The Rust broker requires
  `!IsPassword`, an Edit or ComboBox control type, a writable ValuePattern, and a
  `SHELLDLL_DefView` ancestor for the generic-dialog candidate. This changes what
  `PRIVACY.md` and `docs/store-submission.md` must say, and it can lose a surface
  that currently works — calibrate against `docs/compatibility.md` first.
- **A second rendered path form.** `unc_win32` (`\\?\UNC\wsl.localhost\…`) for raw
  Win32 file calls, alongside `unc_display` for the shell. `unc_display` is frozen:
  `ForwardSlashWindows.psm1` compares `fwdslash resolve /` against the literal
  `\\wsl.localhost`.
- **Enter latency is bounded at 400 ms** by the abandon-and-replay deadline. This
  contradicts `docs/compatibility.md`'s "No lost, duplicated, or delayed Enter
  behavior" gate as currently worded, which must be restated.
