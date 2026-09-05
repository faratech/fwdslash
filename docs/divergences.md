# Deliberate divergences from the C++ build

Every entry here is a place the Rust port behaves differently on purpose. Each
one has a test pinning it. Anything *not* listed here is meant to be
byte-for-byte identical, and a difference is a bug.

## Resolver rules (R1-R12)

The rule numbers are cited throughout `crates/fsw-path/src/lib.rs`,
`crates/fsw-core/src/lib.rs` and the entries below, and this is where they are
defined. They describe the shared contract both resolvers implement, in the
order the code applies them (`resolve` in `crates/fsw-path/src/lib.rs`,
mirroring the C++ `ResolveSlashPath`).

| Rule | Contract |
|---|---|
| **R1** | The input must begin with `/`. Empty or anything else is `NotASlashPath` — the CLI's "not a slash path" and the shells' exit 3. |
| **R2** | A second `/` at index 1 is `DoubleLeadingSlash`. Checked **before** R3, so `//\0` reports the double slash rather than the NUL. |
| **R3** | An embedded NUL anywhere is `EmbeddedNul`. |
| **R4** | A backslash anywhere is `BackslashNotAllowed`. This is what lets R10 derive the Linux path from the rendered UNC by swapping separators. |
| **R5** | A bare `/` is the provider root (`Resolved::WslRoot`, rendered `\\wsl.localhost`) — but only in distribution-list mode with no custom folder root; see R7-R9. |
| **R6** | A trailing `/` on an input longer than one character is *captured* as `had_trailing_separator`; whether it survives is R12. In default-distribution mode it is captured from the rewritten string, which is why bare `/` reports `true` there (Settings-independent; see Resolver §5). |
| **R7** | The leading segment is everything from index 1 up to the next `/`, or to the end. |
| **R8** | If that segment is a **registered** distribution (case-insensitively, per Resolver §1), the input is an explicit distribution path and resolves against it. A registered distribution always wins over a same-named folder. |
| **R9** | Otherwise the bare-slash mode decides: *distribution list* → bare `/` is R5 and anything else is `UnregisteredDistribution`; *default distribution* → the pinned distribution, else the WSL default, else `NoDefaultDistribution`. A configured folder root pre-empts both (Resolver §6). |
| **R10** | Components are normalized during the render: an empty component and `.` are dropped, `..` truncates back to the previous separator. No component vector is built. |
| **R11** | A `..` that would leave the distribution (or folder) root is `TraversalAboveRoot`. Traversal *to* exactly the root is allowed. |
| **R12** | The captured trailing separator (R6) is re-appended only if at least one component survived R10. `/Ubuntu/` keeps it, `/Ubuntu/.` does not, and bare `/` in default mode does not. |

Rules R1-R4 and R10-R12 apply unchanged to a custom folder root; R5 and R7-R9
are where a folder root differs (Resolver §6).

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

**Non-BMP characters.** `simple_upper` folds `char`s — Unicode scalar values —
while `CompareStringOrdinal` folds UTF-16 code *units*. For anything above
U+FFFF the two are structurally different operations: Win32 sees a surrogate
pair and case-folds neither half (no surrogate code unit has a simple uppercase
mapping), whereas Rust sees one scalar and would apply its mapping if the plane
has one — Deseret (U+10428 `𐐨` → U+10400 `𐐀`) and Adlam are the live examples.
A distribution name containing one of those characters would compare
case-insensitively in Rust and case-sensitively in Win32. No WSL distribution
name is plausibly in that set, and the C++ behaviour is the accidental one, so
this is recorded rather than "fixed".

**Follow-up:** a Windows-only differential test walking the BMP against
`CompareStringOrdinal` is the honest way to keep this table current. It cannot
run on Linux CI, so it belongs in the Windows leg. It would not cover the
non-BMP case above; that needs its own surrogate-pair cases.

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

### 6. The custom bare-slash root is a Rust-layer feature

`fwdslash bare-slash root <path>` stores an absolute Windows path (`C:\code`,
`\\wsl.localhost\Ubuntu\home\mike`) in a new `BareSlashRoot` REG_SZ under the
settings key, and the funnel in `fsw-core::resolve_user_slash_path` then routes
every input whose first segment is not a registered distribution to
`fsw_path::resolve_under_root`: a bare `/` opens the root, `/foo` resolves to
root\foo, `..` clamps at the root (`TraversalAboveRoot`), in **either**
bare-slash mode. Registered-distribution inputs keep WSL semantics — that is
the escape hatch back to `\\wsl.localhost`. No new `ResolveError` variant
exists, so the C++ wire contract is untouched.

Deliberate details:
- `BareSlashRoot` is **not** a third `BareSlashMode` value. Both resolvers read
  any nonzero `BareSlashMode` DWORD as "default distribution"
  (`fsw-core/src/lib.rs`, `src/core/wsl_registry.cpp`), so a mode value would
  make a stale C++ build disagree about what `/` means. With a separate value a
  stale C++ build ignores it and keeps today's behavior — the same thing that
  happens when the value is corrupt, because the funnel re-validates it on
  every resolve (`is_valid_windows_root`).
- `Resolved::Folder` and `Resolved::is_provider_root()` exist only in Rust; the
  broker's `event=route_folder` diagnostic category is new (category-only, per
  PRIVACY.md).
- The Explorer message-only special case keys on `is_provider_root()`, so a
  folder `/` goes through the ordinary set-focused-value path.
- `ResolveError::message()` for `TraversalAboveRoot` still says "distribution
  root", which reads slightly wrong under a folder root. Generalizing the
  sentence is a cross-tree wire-text change; accepted for now.
- `ForwardSlashWindows.psm1` no longer compares rendered paths at all. It calls
  `fwdslash shell-resolve`, which reports `kind` (`root` / `distribution` /
  `folder` / `native`) structurally, so a custom root that itself lives under
  `\\wsl.localhost` can never be misread as the provider root. The old exact
  `\\wsl.localhost` / `\\wsl.localhost\` literal comparison is gone (the second
  of those literals could never match anything the resolver rendered).
- `is_valid_windows_root` accepts only *absolute* locations, and 0.0.3 tightened
  it: a drive-**relative** root (`C:code`, `C:Users\me`) is rejected, because
  Win32 resolves it against a hidden per-drive current directory rather than a
  fixed folder; a share-less UNC root (`\\server`, `\\server\`,
  `\\server\\share`) is rejected, because it names no folder; wildcards (`*`,
  `?`) are rejected; and `\\wsl.localhost` itself is rejected in any casing and
  with any number of trailing separators, so `Resolved::unc_display` can only
  ever produce that literal for `Resolved::WslRoot`. `C:` and `C:\` stay valid.
  Pinned by `windows_root_validation_table`.
- `has_win32_normalization_hazard` inspects the **last** component only. Win32
  strips a trailing `.` or space outside the `\\?\` namespace, but only at the
  end of the string — a `.` or space followed by a separator survives, so a
  middle component and a path written with a trailing separator are not hazards,
  and `.`/`..` are normalized away by R10 rather than truncated. It is also no
  longer computed and discarded: the broker appends a trailing `\` when it is
  true, and logs `event=win32_normalization_hazard`. `FolderPath` exposes the
  same accessor, because a folder root can sit on ext4 too.

Named tests: `folder_root_*` and `windows_root_validation_table`
(`crates/fsw-path/tests/resolver.rs`), `folder_root_resolution_allocates_nothing`
(`tests/allocations.rs`), and the `bare_slash_root_*` funnel suite
(`crates/fsw-core/tests/bare_slash_root.rs`).

## Product behaviour (Rust-only): dual-track distribution + self-update

The GitHub-distributed build (Trusted Signing publisher, different package
family from the Store listing) checks `api.github.com/releases/latest` at most
daily and can atomically install its own signed MSIX bundle
(`crates/fsw-core/src/update.rs`). Gated at runtime to
`packaged && !is_store_flavor() && AutoUpdate` — the Store build never
performs the check, and an `AutoUpdate` settings value (default on) can switch
it off. The settings app surfaces a found update as an Informational InfoBar
with a "Restart to update" button beneath it (registration is deferred while
this process is part of the package, so applying now means closing the broker,
re-registering with `-ForceApplicationShutdown` from a detached PowerShell, and
relaunching), and shows an "Automatic updates" toggle only in the GitHub
flavor. There is no tray balloon: the settings app has no tray icon. The C++ tree has no counterpart; documented for certification
in docs/store-submission.md (the Store package performs no network
connections).

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

### 1b. The settings window is a plain window; only the single-instance guard is new

The settings app has **no** notification-area icon, no window subclass, and no
watchdog thread. 0.0.2 gave it a tray icon of its own (`tray.rs`) that hid the
window on close or minimize; because the broker also owns one, the product
showed **two** identical icons for the rest of the session, and the whole
watchdog / zombie-takeover / `FSW_SIMULATE_WINDOWLESS` apparatus existed only to
survive that hide-to-tray design. All of it is deleted. The product's one icon
belongs to the broker (Broker §2).

Closing the window now exits the process: `windows-reactor` routes WinUI's
`Window.Closed` through `dispatch_window_closed` → `finalize_closed_window` →
`exit_ui_thread()`, and nothing calls `DestroyWindow` directly, so a process
holding the mutex always has a window to raise.

What remains — and what the C++ app still does not have — is the single-instance
guard: a `Local\ForwardSlashWindows.Settings` mutex, and a second launch raises
the first instance's window instead of opening a duplicate. The raise matches on
the window title **and** on the owning process image being `fswsettings.exe`
(`EnumWindows` + `GetWindowThreadProcessId` + `QueryFullProcessImageNameW`); a
bare `FindWindowW(NULL, title)` used to match the broker's own never-shown
top-level window, which had the same caption, and "raise" it as a 0x0 caption
strip. The broker's window was retitled `fwdslash broker` as well, so the two
can no longer collide by name. The same current-process-only enumeration is what
supplies the folder picker's owner HWND (`folder_picker::current_process_window`).

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

### 6. Instance lifecycle and off-thread controller calls

The C++ settings app has no single-instance guard, and it runs every controller
invocation on the UI thread. Both differ here:

- **Fail closed.** Any `CreateMutexW` error other than "already exists" shows a
  message box and exits — it never falls through to "I'm the first instance".
  Silently running a second instance is the failure this guard exists to
  prevent.
- **Raise, never take over.** With the mutex held elsewhere, the relaunch polls
  for the other instance's window for 10 s (WinUI takes a beat to materialize
  it) and raises it. There is no windowless-zombie state to recover from any
  more, so there is no process termination, no packaging-identity comparison,
  and no `FSW_SIMULATE_WINDOWLESS` fixture; if no window appears, the launch
  reports it and exits rather than killing anything.
- **Controller calls run on the thread pool.** `run_controller` reaches
  `fwdslash.exe`, and `integration windows-powershell enable` loads the user's
  whole profile — up to 15 s with the window frozen in the C++ design. Every
  invocation now goes through `SettingsModel::start_controller`, which sets a
  `pending` action, spawns the work with `context.spawn_background`, and
  finishes on `Msg::ControllerFinished`. While `pending` is set every
  state-mutating control is disabled (`controls_enabled()`) and a `ProgressRing`
  is shown, so a second request cannot race the first.
- **State reads are off-thread too.** `State::read()` runs in
  `spawn_background` and arrives as `Msg::StateLoaded`; only the very first
  frame reads synchronously. `ensure_broker_running()` (which spins up to 2 s)
  moved off-thread as well and reports back with `Msg::BrokerProbed`.
  `broker_state` uses a 250 ms timeout (the C++ uses 750), `pwsh.exe` discovery
  is a process-wide `OnceLock`, and navigating to About skips the refresh
  because it shows nothing live.
- **Standing banners are Buttons, not InfoBar actions.** Reactor's `InfoBar`
  exposes no action-button slot, so the "Restart to update" action is an
  ordinary `Button` rendered directly beneath its bar, in a second fixed grid
  row that is kept out of `self.notice` — a routine "Updated" result must not
  hide a standing notice, and vice versa.
- **Outdated shell adapters are upgraded automatically, with no button to
  press.** The Rust app upgrades outdated shell adapters automatically on every
  launch (one sequential `fwdslash integration <id> enable` per adapter,
  reported by an InfoBar: "Updating terminal integrations…" → "Terminal
  integrations updated" / "Some terminal integrations could not be updated")
  and shows a Components card on About with broker state, per-adapter payload
  versions, package version/architecture and package flavor — the C++ app has
  neither. The broker does the same sweep at startup (Broker §2), so the
  settings window is the second chance, not the only one.
- **The tray tooltip is the broker's alone.** It reads
  `Forward Slash Windows — active` / `— paused` / `— hook unavailable`. The
  0.0.2 arrangement of two deliberately-different tooltips is gone with the
  second icon.
- **The filesystem-driver line is live.** `src/settings/main.cpp:832-838`
  hardcodes `Filesystem driver: not installed (production-gated)`, and the C++
  About page repeats the claim in prose. The Rust app probes instead — the
  `FswFilter` service through the SCM (read-only, never elevating) plus a
  connect to `\FswFilterPort` — and General and the About Components card both
  render `Filesystem driver:` followed by one of `not installed` /
  `installed, not loaded` / `loaded, not connected` / `connected`, the same
  four states `fwdslash driver status` prints. The About page no longer carries
  the production-gated sentence at all.

## Broker (fsw-broker)

### 1. The window is a never-shown top-level tool window, not message-only

The C++ broker creates its window with the `HWND_MESSAGE` parent
(`src/broker/main.cpp:712`). Message-only windows are skipped by `HWND_BROADCAST`,
so `TaskbarCreated` (explorer.exe restart) and `WM_QUERYENDSESSION`/`WM_ENDSESSION`
(session end) can never reach the tray-icon lifecycle. The Rust broker creates a
real top-level window with `WS_EX_TOOLWINDOW` that is never shown: same
`FindWindowW`-by-class discovery for the CLI and settings app, but the icon is
re-added after a shell restart and removed before a session-end ghost can appear.

### 2. Two windows, two threads, and a tray icon the C++ does not have

The C++ broker classifies and processes Enter on the hook thread, owns one
window, and has no notification-area presence. The Rust broker differs on all
three, and this section is the whole list.

**Windows and threads.**

- The top-level window is titled `fwdslash broker` (the C++ and 0.0.2 titled it
  `Forward Slash Windows`, which collided with the settings window's caption —
  see Settings §1b). Nothing discovers it by title; the class
  `ForwardSlashWindows.Broker` is the contract.
- A **second** window, class `ForwardSlashWindows.BrokerWorker`, lives on a
  worker thread with its own `CoInitializeEx(STA)` and its own `IUIAutomation`.
  This one *is* `HWND_MESSAGE`, which is correct precisely because it needs no
  broadcasts. The hook posts `PROCESS_ENTER` to it with the classification in
  `wParam` and the foreground HWND in `lParam`.
- Everything that can block runs there: UI Automation, the resolver,
  `ShellExecuteExW` (now with `SEE_MASK_ASYNCOK | SEE_MASK_FLAG_NO_UI`),
  `SendInput`, `Navigate2`, and `persist_disabled`. A low-level hook whose
  thread exceeds `LowLevelHooksTimeout` is removed by Windows without telling
  the process, and binding `\\wsl.localhost\<distro>` boots a stopped
  distribution — seconds, on the thread that owns every keystroke on the
  machine. Menu commands are handed to the worker the same way
  (`WORKER_OPEN_PATH`, which transfers ownership of a boxed `String`).
- The hook thread's own work is now only: class check → return `Unknown` unless
  the class is `CabinetWClass`, `ExploreWClass`, `#32770` or
  `Windows.UI.Core.CoreWindow`; only then the process image, into a 1024-unit
  buffer instead of 32768. The classification travels to the worker, which never
  re-runs it.

**Behaviour.**

- **A stale request is dropped, not replayed.** If the foreground window changed
  while the request was queued, the worker logs
  `event=enter_dropped_foreground_changed` and returns. Replaying Enter into
  whatever the user switched to would send a half-written message or run a
  half-typed command.
- **`#32770` is narrowed twice.** In the hook, a dialog outside `explorer.exe`
  qualifies only if it has a `DUIViewWndClassName` child (the modern
  common-item dialog) or a `cmb13`/`edt1` control (the classic one). In the
  worker, **every** surface additionally requires the focused element to be an
  Edit or ComboBox, `IsPassword == false`, and a non-read-only `ValuePattern`;
  otherwise it logs `event=surface_rejected` and replays untouched. The C++
  claims every `#32770` in every process, which is how it swallowed Enter in
  Find boxes and rewrote their search text. Requiring the writable pattern
  before reading also means the broker never reads text it could not have
  written back — the promise `PRIVACY.md` makes.
- **`FSW_WM_SET_PAUSED` replies the resulting `BrokerState`** (`Active` = 1,
  `Paused` = 2) or **0** when the change could not be honoured. The old
  unconditional `1` made a failed resume indistinguishable from a successful
  one. The hook is removed *before* the setting is persisted, and the write
  itself is asynchronous — `persist_disabled` shells out to `reg.exe`, and a
  process creation plus wait on the hook thread is exactly what must not happen.
  A failed write surfaces later as a balloon plus
  `event=persist_disabled_failed`; a failed `install_hook` on resume shows the
  hook balloon and answers 0. The CLI turns that 0 into a specific message by
  asking the broker what state it actually reached.
- **The tray icon and its menu.** Tooltip:
  `Forward Slash Windows — active` / `— paused` / `— hook unavailable`.
  `Shell_NotifyIconW(NIM_ADD)` is checked (it fails with `ERROR_TIMEOUT` while
  the shell is busy — exactly when the MSIX startup task runs at logon); a
  failure sets `ICON_ADDED = false`, logs `event=tray_icon_add_failed`,
  suppresses every balloon, and is retried from the health timer, with
  `NIM_SETVERSION` applied only after an add lands. The menu is **Open
  settings** (the `SetMenuDefaultItem` default, and what left click and
  double-click do) · separator · **Enabled** as an `MF_CHECKED` toggle · **Open
  WSL root** · **Open distribution ▸** (one item per registered distribution,
  capped at 64, resolved against the list the menu was built from) ·
  **Integrations ▸** · separator · a greyed version line · **Exit**. It opens at
  the point `NOTIFYICON_VERSION_4` carries in `wParam` (sign-extended per
  monitor), falling back to `GetCursorPos` on the legacy `WM_RBUTTONUP` path,
  and `PostMessageW(window, WM_NULL, 0, 0)` follows `TrackPopupMenu` — without
  it the *next* right-click flashes a menu that dismisses itself.
- **The health timer is adaptive.** 5 s only while a driver actually answers on
  the filter port; 60 s otherwise, which is the shipping configuration. The
  connect probe runs first and decides the interval; with no port the registry
  enumeration and the kernel round-trip are skipped entirely, and the attempted
  distribution list is recorded anyway (`ATTEMPTED_DISTRIBUTIONS`) so the
  compare-only path can engage with no driver present. The same tick doubles as
  the tray-icon retry and as a hook **re-arm** (`UnhookWindowsHookEx` +
  `SetWindowsHookExW`, keeping the incumbent if the replacement fails, logging
  `event=hook_rearmed`), spaced at 60 s independently of the tick interval.
  There is no way to ask whether a hook handle is still live, so re-arming on a
  slow timer is the only defence against a silent removal.
- **Trailing separator for the Win32 hazard.** When
  `Resolved::has_win32_normalization_hazard()` is true and the rendered path
  does not already end in `\`, one is appended before opening — Win32 keeps a
  trailing `.` or space when a separator follows it — and
  `event=win32_normalization_hazard` is logged.
- **Balloon text.** `"Windows could not open the location."` where the C++ says
  `"...WSL location."`: with a custom folder root the target need not be in WSL.
  The pause-write failure adds `"The pause setting could not be saved."`
- **Shell adapters are upgraded at startup.** `start_adapter_upgrade` checks
  each installed adapter's recorded payload version and, when it predates the
  running build, silently re-runs `fwdslash integration <id> enable` on a
  background thread (90 s ceiling per adapter), so terminal integrations are
  upgraded automatically after a product update. The result is reported in a
  single balloon: `"Terminal integrations were updated to <version>: <names>."`
  or `"Some terminal integrations could not be updated automatically. Open
  Settings to retry."` The settings window repeats the sweep on launch
  (Settings §6) and `fwdslash integration <id> enable` remains the manual
  fallback. The C++ broker does nothing of the kind.
- **New diagnostic categories** (category-only, per `PRIVACY.md`):
  `event=enter_dropped_foreground_changed`, `event=surface_rejected`,
  `event=hook_rearmed`, `event=persist_disabled_failed`,
  `event=win32_normalization_hazard`, `event=tray_icon_add_failed`,
  `event=worker_start_failed`, `event=worker_detached`,
  `event=debug_uia_failed`, `event=debug_hook_failed`,
  `event=adapter_upgraded`, `event=adapter_upgrade_failed`,
  `event=adapter_upgrade_skipped`. The C++'s `event=enter_handler_failed` has no
  Rust counterpart.

## CLI (fwdslash)

### 1. `fwdslash start` only ever closes the broker it spawned

On probe failure the C++ controller finds whichever window carries the broker
class and closes it — including a pre-existing healthy instance the spawn did not
create (the spawn may have lost the mutex race and exited already). The Rust CLI
compares the window's PID against the spawned `dwProcessId` before posting
`WM_CLOSE`, and reports `Resolution is paused; run "fwdslash enable" to activate.`
when the probed broker is merely paused instead of the misleading "keyboard hook
is unavailable".

### 2. Shell verbs, exit 3, and a self-upgrading adapter payload

The C++ controller has no shell verbs beyond `cmd-list`, and its adapter
payload is frozen at install time. The Rust CLI adds:

- **`fwdslash cmd-cd <input>`** — the target for the cmd `CD`/`CHDIR`/`PUSHD`
  macros. Stdout carries the Win32 path and nothing else, so the batch file can
  capture it verbatim. A bare `/` in distribution-list mode is not one
  directory, so it goes to stderr with the "`/` lists your WSL distributions…"
  message and exit 1.
- **`fwdslash shell-resolve <input>`** — one JSON line
  (`{"kind":"root|distribution|folder","target":…,"distributions":[…]}`) for the
  PowerShell module, so `ls /` costs a single spawn instead of a `resolve` plus
  a `status --json`. It answers from `Snapshot::current()` alone: no broker
  round trip, no filter-port probe.
- **The exit-3 contract.** Every shell verb returns **3** for "run your own
  command unchanged" — resolution is paused, the input is not a slash path, or
  (for `cmd-list`) the target does not exist (`ERROR_FILE_NOT_FOUND` /
  `ERROR_PATH_NOT_FOUND`, so a missing path degrades to native `DIR`). **1** is
  a resolver rejection, already explained on stderr. **0** is a target on
  stdout. All three verbs share one funnel, `shell_target`, so a `cd` and a
  `dir` can never disagree about what an input means.
- **The PowerShell `pushd` wrapper is a global function built from an unbound
  script block**, not a module function — the only wrapper that is. `Push-Location`
  run inside a module pushes onto *that module's* location stack, so the caller's
  `popd` never sees it; and `@args` re-splats named parameters faithfully only
  from a simple function's own `$args`, whereas the same array collected by an
  advanced function rebinds `-LiteralPath` as a positional value. Running in the
  caller's session state fixes both, at the cost of being able to call only
  commands the global scope can see — which is why
  `Resolve-ForwardSlashWindowsLocationTarget` is exported.
- **`pushd -StackName` is not mirrored.** The wrapper pushes onto the default
  stack; a named stack is passed through to `Push-Location` untouched only when
  the argument list carries no slash path.
- **Adapters self-upgrade.** `PAYLOAD_VERSION` derives from
  `CARGO_PKG_VERSION`, and an `installed` marker whose `Version` differs from
  it makes `fwdslash integration <name> enable` run the uninstall transaction
  for the deployed payload followed by the install transaction for this one —
  same rollback guarantees, and the PowerShell uninstall removes the module
  directory it actually created rather than the one this build would create.
  `fwdslash integrations` prints `installed (update available)` for such an
  adapter and reports it in `--json`. Nobody has to run it by hand: the broker
  sweeps at startup and the settings window sweeps on launch, and the verb is
  the manual fallback. Without this an updated product kept running a frozen
  copy of the old payload and old `fwdslash.exe` forever.

## Product behaviour (landing later — recorded here so the list stays in one place)

These are planned, not yet implemented. Each needs its own entry with a test
before the milestone that lands it can close.

- **A second rendered path form.** `unc_win32` (`\\?\UNC\wsl.localhost\…`) for raw
  Win32 file calls, alongside `unc_display` for the shell. `unc_display` is still effectively frozen: the
  provider root renders as exactly `\\wsl.localhost`, and
  `is_valid_windows_root` rejects that literal as a folder root specifically so
  the two can never be confused (Resolver §6).
- **A bounded Enter deadline.** There is none today: the hook swallows Enter,
  posts to the worker and returns immediately, and the worker takes as long as
  the surface takes. Nothing is lost or duplicated — a request whose foreground
  window changed is dropped rather than replayed (Broker §2) — but a slow
  surface still delays the *replayed* Enter it produces. A deadline after which
  the worker abandons and replays would bound that; if one lands,
  `docs/compatibility.md`'s "No lost, duplicated, or delayed Enter behavior"
  gate needs restating with the number.

- The Rust CLI decodes `REG_SZ`/`REG_EXPAND_SZ` data into UTF-16 code units *before* stripping NUL
  terminators (0.0.2 stripped zero bytes first and lost the final ASCII character of every value it
  read), and tolerates exactly one missing trailing character when comparing the live AutoRun
  against the marker's `InstalledAutoRun`, so 0.0.2-era installs can still be upgraded. The shared
  `PowerShell\<version>` module directory is keyed on the version each edition's marker records, and
  orphaned version directories are pruned on uninstall, on the `fwdslash uninstall` sweep, and after
  every successful PowerShell `enable`.

