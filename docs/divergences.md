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

The caption is therefore drawn by `TitleBar` itself, and the leading-edge icon
goes in the TitleBar's `LeftHeader` slot (added to `windows-reactor` for this):
same position the C++ `IconSource` occupies, automatic drag regions, and
`ImageIcon` + `EncodedImage::from_static` over `assets/fwdslash-titlebar.png`
decodes in place via `BitmapImage.SetSourceAsync`, never constructing the
fail-fasting `ImageIconSource`. The taskbar/Alt-Tab icon is not part of this
divergence either: `WindowVisuals::icon_resource(IDI_FSW_APP)` loads the
`app.rc` resource and applies it with `WM_SETICON`, the same thing
`src/settings/main.cpp:354-366` does against the raw HWND. (An `.rc` icon alone
only becomes the exe's file icon — it never reaches the taskbar on its own.)
The only remaining caption gap versus the C++ is that the PNG sits in
`LeftHeader` rather than `IconSource` — functionally equivalent at the leading
edge.

### 1b. The settings window has tray + close/minimize-to-tray behavior the C++ lacks

`crates/fsw-settings/src/tray.rs` subclasses the window (`SetWindowSubclass`)
and adds a notification-area icon: minimizing or closing hides the window to
the tray, left click restores, right click offers Show/Exit, and
`TaskbarCreated` re-adds the icon after a shell restart. The C++ settings app
has no tray surface at all. This is the wfdiag `reactor-spike/window_support.rs`
solution ported onto `windows-sys`; it also pairs with the single-instance
mutex in `activate_existing_instance` — a second launch raises the existing
window instead of starting a duplicate, which the C++ app does not guard
against either.

### 2. The navigation pane pushes content instead of overlaying it

Also against the Fluent audit (2026-09): the Rust app deviates from the C++
*product* on purpose in three places to match the published WinUI guidance —
`OpenPaneLength` at the documented 320 default (the C++ pins a custom value),
24px content padding (the C++ uses 32/24/32/28), and secondary text drawn with
`TextFillColorSecondaryBrush` via `ThemeBrush::TextSecondary` instead of the
C++'s `Opacity(0.7x)` dimming, which does not survive high-contrast themes.
These are Fluent-conformance fixes, not porting drift; the C++ app was left as
shipped.

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

### 6. Instance lifecycle: takeover, watchdog, fail-closed guard

The C++ settings app has no single-instance guard and no tray, so it has no
zombie failure mode. The Rust app's guard (added with the tray) initially had
three holes, all now closed; the fixes are behavior the C++ does not have:

- **Fail closed.** Any `CreateMutexW` error other than "already exists" shows a
  message box and exits — it never falls through to "I'm the first instance",
  which is how a duplicate tray icon could appear.
- **Zombie takeover.** A prior instance can outlive its own window (a direct
  `DestroyWindow` or a session end skips the reactor's only exit path, the
  `Window.Closed` event), leaving a process that holds the single-instance mutex
  with nothing to raise; every later launch would then be a silent no-op. When a
  relaunch finds no window for 10 s, it terminates a *qualifying* peer and takes
  over. Qualifying means: `fswsettings.exe`, same packaging context (same MSIX
  package family, or both unpackaged — a packaged and an unpackaged instance
  never kill each other), older than 15 s (a young peer is a concurrent launch
  still materializing), and owning no window at all. Anything ambiguous shows an
  error box instead of killing.
- **Watchdog.** `watchdog.rs` posts `WM_QUIT` to the UI thread (and exits the
  process as a last resort) if the window the discovery poller found ever
  disappears without an exit having been requested. `WM_DESTROY` in the tray
  subclass posts the quit directly as the first line of defense.
- **Exit routes through the close pipeline.** The tray Exit item posts
  `WM_CLOSE` with `FORCE_CLOSE` armed rather than calling `DestroyWindow`
  directly, so the reactor's `Window.Closed` exit path actually runs.
- **`FSW_SIMULATE_WINDOWLESS`** (any value) makes the process hold the mutex
  forever with no window — the test fixture for the takeover path.
- **Tray tooltips are distinct**: the broker reads `"fwdslash broker"` (plus
  `" (paused)"` while resolution is disabled) and the settings app reads
  `"fwdslash settings"`. Both used to read `"Forward Slash Windows"`, making the
  two icons indistinguishable.

## Broker (fsw-broker)

### 1. The window is a never-shown top-level tool window, not message-only

The C++ broker creates its window with the `HWND_MESSAGE` parent
(`src/broker/main.cpp:712`). Message-only windows are skipped by `HWND_BROADCAST`,
so `TaskbarCreated` (explorer.exe restart) and `WM_QUERYENDSESSION`/`WM_ENDSESSION`
(session end) can never reach the tray-icon lifecycle. The Rust broker creates a
real top-level window with `WS_EX_TOOLWINDOW` that is never shown: same
`FindWindowW`-by-class discovery for the CLI and settings app, but the icon is
re-added after a shell restart and removed before a session-end ghost can appear.

## CLI (fwdslash)

### 1. `fwdslash start` only ever closes the broker it spawned

On probe failure the C++ controller finds whichever window carries the broker
class and closes it — including a pre-existing healthy instance the spawn did not
create (the spawn may have lost the mutex race and exited already). The Rust CLI
compares the window's PID against the spawned `dwProcessId` before posting
`WM_CLOSE`, and reports `Resolution is paused; run "fwdslash enable" to activate.`
when the probed broker is merely paused instead of the misleading "keyboard hook
is unavailable".

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
