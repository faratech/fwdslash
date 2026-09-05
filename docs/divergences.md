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

The C++ tree has no counterpart to any of this: it neither checks for updates
nor installs them. What follows is the whole of the Rust product's update
behaviour, in one place.

**Both flavors update themselves, under one switch.** The GitHub-distributed
build (Trusted Signing publisher, different package family from the Store
listing) checks `api.github.com/releases/latest`; the Store build asks the Store.
The gate is `fsw_core::update::update_check_allowed(packaged, auto_update)` —
two arguments, no flavor — and the flavor decides only the switch's **default**:
`default_auto_update(store_flavor) = !store_flavor`, so `AutoUpdate` absent means
on for the GitHub build and off for the Store build. The stored encoding is the
inverted DWORD it always was (`1` = auto-update off), so an explicit "off"
recorded by an older build still reads as off. Cadence is unchanged at
`CHECK_CADENCE_SECS` = 24 h, bypassed by `--force`.

**All of it lives in the CLI**, `crates/fsw-cli/src/update/`, as
`fwdslash update check|install|status` plus two helper-only verbs
(`apply-store`, `apply-bundle`) that are absent from `usage()` and exit **20**
when run with package identity. The broker and the settings window own no update
logic: they run `fwdslash update` and read its exit code — `0` up to date or
install started, `10` update available or deferred, `11` needs the user, `12`
nothing to install, `1` error, `2` usage, `20` wrong execution context — or its
one hand-rolled JSON line
(`{"flavor","state","available","autoUpdate","lastUpdateCheck","route","action","detail"}`,
golden-tested; there is no serde in this workspace). COM is initialised in this
module and nowhere else in the CLI, so the `cd /` hot path pays nothing for it.

**The install ladder (Store flavor).** `route_for` is a pure function of five
inputs and the single definition of precedence; the probes below it are lazy, so
a rung is only asked about once the rung above is out.

| # | Route | Precondition | Runs in | Terminates the app |
|---|---|---|---|---|
| 1a | `AppInstallManager.StartProductInstallWithOptionsAsync` (winget's own sequence: `AllowForcedAppRestart`, both toast modes `NoToast`) | always attempted first when packaged | the packaged CLI, in-process | yes, by the Store |
| 1b | the same call from the staged helper | 1a failed before an item was queued (`E_ACCESSDENIED` above all) | the identity-less helper, from the scheduled task | yes |
| 2 | `StoreContext` silent download + install | route 1 unavailable and `CanSilentlyDownloadStorePackageUpdates` | the packaged CLI | yes, when deployment lands |
| 3 | `winget upgrade --id … --source msstore --silent --force` | winget present and the network unmetered | the scheduled task | yes |
| 4 | notify | otherwise | the packaged CLI | no (exit 11) |

Two orderings inside `install` are load-bearing and each fixed a shipped bug:
*nothing to install* (exit 12) is answered **before** the moment gate, because
exit 10 promises there is something to come back for; and availability outranks
`--route`, because a forced route says how to install, never whether there is
anything to. `install_moment_ok(forced, settings_window_open, worker_busy)` is
the moment gate — an explicit request always wins, otherwise an open settings
window or a busy Enter worker defers. Only the broker knows `worker_busy`, so it
gates before it invokes the CLI at all. Route 1's phase-1a call exists because
the spike found `AppInstallManager` activates and answers queries *inside* the
package; whether the install itself is allowed there is only knowable at
runtime, so it is tried and the identity-less path is the fallback, not the
default.

**The GitHub flavor** keeps its two-phase shape — `run_update_check` downloads
the signed bundle and registers it with
`-DeferRegistrationWhenPackagesAreInUse` — but no longer applies it from a
detached PowerShell. `install` hands the downloaded bundle to the same helper,
which registers it with `-ForceApplicationShutdown` (the broker is resident, so
a deferred registration would never land), behind the same watchdog. No bundle
is exit 12.

**The helper** is `%LOCALAPPDATA%\ForwardSlashWindows\update\fwdslash-helper.exe`:
a byte-identical copy of the running `fwdslash.exe`, staged through
`adapters::real_copy_file` (a `cmd.exe` child, because the source is in
`WindowsApps`) and named distinctly so a user or an antivirus report can
identify it. It exists for the one thing package identity forbids — asking the
Store to replace the package that is asking, and `Add-AppxPackage` against the
package it is running inside. Its hard rule: **it never writes HKCU.** An
identity-less write lands in the real hive while the packaged app reads the
virtualized one, which would be invisible on a dev build where the two views are
the same. It reports through `last-result.txt` in the same directory —
`completed`, `paused`, or `error:<hex>` — and the next packaged
`update check`/`update status` folds that file into the registry and **deletes
it**, so one helper run is folded exactly once. Only `completed` clears the
cached `AvailableUpdate` notice; a pause or an error leaves it standing.

**The watchdog** is one per-user scheduled task, `fwdslash-update`, registered
with `/f` (so retries overwrite rather than accumulate) **before** the install
runs, because a process the Store has just force-closed cannot relaunch itself.
Its `.cmd` runs the optional lead command (the helper, or `winget`), then an
inline `powershell.exe -Command` watchdog that polls `Get-AppxPackage` every 5 s
until the installed version is greater than the one that was running, ceiling 45
minutes, then relaunches — `--relaunch broker` (the default) starts the broker
through the app-execution alias and only when none is running, `app` starts the
package's `App` entry point, `none` skips. Then the task deletes itself and its
script. The script text obeys two rules the tests assert: **no `%`** (`cmd.exe`
would expand it silently — hence `$env:LOCALAPPDATA`) and **no `"`** (it would
end the argument `cmd.exe` is building), which is also why the comparisons are
`-lt`/`-gt`/`-not`. Every literal spliced in is checked by
`is_safe_task_literal` first, and an unsafe one produces **no script at all**
rather than a mangled one.

**`UpdateRoute`** (`REG_SZ` under the settings key, values `auto`, `appinstall`,
`store`, `winget`, `notify`) pins one rung without a rebuild — the escape hatch
if the Store ever objects to route 1, and the way a user keeps the check while
refusing unattended installs (`notify`). It applies to the **Store** ladder
only: the GitHub path has a single route and never consults it. It is read-only
to the product — nothing writes it — and `--route <name>` is the same override
for one invocation.

Certification wording for all of the above is in `docs/store-submission.md` §3;
what it sends and stores is in `PRIVACY.md`.

## Product behaviour (Rust-only): every settings write reaches both hives (#52)

The C++ tree routes only `Disabled` through `reg.exe`; `BareSlashMode`,
`BareSlashDistribution` and `BareSlashRoot` are written with the in-process
registry API, so a packaged build files them in the package's private hive and
the unpackaged shell adapters — which read the real hive — never see them. The
measured symptom on a Store 0.0.3 install: the settings app said *default
distribution* while `cd /` in PowerShell still listed the distributions.

The Rust tree has one writer for that key, `fsw_core::settings_write`
(`set_setting_u32` / `set_setting_u64` / `set_setting_string` /
`delete_setting`), and nothing else may write it. Its decision is
`write_plan(packaged)`: unpackaged, the in-process API *is* the real hive and
one write is the whole job; packaged, the value goes to the real hive through a
`reg.exe` child **and** to the package hive in-process — the second half
matters because a stale private-hive copy shadows the real one for every
packaged reader, so a real-hive-only write would simply invert the split.
`sync_settings_to_real_hive()` repairs installs that already carry it: a
packaged process compares its merged view (authoritative) against a child
`reg.exe query` of the real hive and mirrors what differs, never deleting. It
runs from the broker's startup sweep, the settings window's launch sweep and
`fwdslash repair-adapters`, and logs `event=settings_synced` — category only.

## Product behaviour (Rust-only): a state-changed broadcast (#55)

The C++ tree has no cross-component change notification at all: the settings
window catches an external change only on `window_.Activated`, and the broker
catches one never. Both keep rendering what they read at launch.

The Rust tree adds one registered window message,
`fsw_core::FSW_STATE_CHANGED_MESSAGE` = `ForwardSlashWindows.StateChanged`,
registered per session with `RegisterWindowMessageW` and posted to
`HWND_BROADCAST` by whoever changed something, *after* the change lands:
`fsw_core::settings_write` announces every successful settings write (so the
bare-slash values, `Disabled` and the update values are covered wherever they
are written from), and `fwdslash` announces the verbs whose state lives
elsewhere — `integration … enable|disable|repair`, `repair-adapters`,
`install`/`uninstall`, `start`/`stop`, `pause`/`resume` — once per invocation
and only on exit 0. It carries no payload: every listener re-reads what it
needs, so nothing about what changed travels between processes (`PRIVACY.md`).

The broker listens on its existing top-level window and re-reads the settings
(Broker §2). The settings window listens on a hidden top-level window of its
own, on its own thread, because it has no window procedure it can reach
otherwise — `crates/fsw-settings/src/state_watch.rs`, class
`ForwardSlashWindows.SettingsWatcher`, and a real top-level window because
`HWND_BROADCAST` skips message-only ones (the same reason the broker's window is
one). Both are unchanged by the message itself; the re-read is what applies it.

`RegNotifyChangeKeyValue` was considered and rejected as the primary mechanism:
under MSIX registry virtualization it is not clear which hive layer the
notification tracks, and writes now go to both (#52). A broadcast plus a poll is
deterministic in a way that does not depend on the answer.

`Invoke-ForwardSlashWindowsSetLocation` also answers `cd ..` at a distribution's
share root (`\\wsl.localhost\<Distro>`, or the `\\wsl$` spelling) with one
line naming the distribution, instead of PowerShell's `Cannot find path
'\\wsl.localhost'`. Every other path, and a paused product, keeps the native
behaviour.

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

### 3. A change watch instead of a refresh on activation

The C++ hooks `window_.Activated` (`main.cpp:443-446`) so a change made by
`fwdslash` in a terminal shows up on alt-tab. reactor exposes no activation
observation — `HostEvent` carries only `WindowSize`, `ColorScheme` and errors —
and the Rust app used to refresh only on its own mutations, on navigation and on
the "Refresh status" button, which left exactly the case the C++ covered: an
external change while the window sits open and untouched (#55).

It now watches instead, which covers more than alt-tab did — the window follows
a change it never touched, without being touched itself.
`crates/fsw-settings/src/state_watch.rs` owns both halves:

- **The broadcast.** A hidden top-level window on its own thread receives
  `FSW_STATE_CHANGED_MESSAGE` and signals a manual-reset event. `wait()` — one
  turn, run as background work and re-armed by handling the message it produces
  — waits on that event, and on a signal sleeps 250 ms and clears the event
  before returning, so a multi-value write (`bare-slash default` writes three)
  costs one read, and a signal raised after the clear wakes the next turn.
- **The poll.** The same wait times out after 5 s and reports `Wake::Poll`,
  which covers writers that do not broadcast — an older staged `fwdslash.exe`,
  a hand `reg.exe` edit. It is skipped while the window is minimized or hidden
  (`should_read`); a broadcast never is, so a restored window is right the
  moment it appears.

`ReadCoalescer` keeps it to one `State::read()` at a time, remembering at most
one owed read, and `Msg::StateRefreshed` compares before assigning: an equal
`State` — which is what the poll finds almost every time — touches nothing. All
of it is off the UI thread; the UI thread only swaps the value in.

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
- **Update controls, for both flavors.** The C++ app has no update surface at
  all; the Rust app's used to be GitHub-only. Now, for any packaged build:
  - The **Automatic updates** switch is shown for both flavors (`state.packaged`
    alone). The Store text is "Let fwdslash install Store updates in the
    background. Off by default; the Store still updates the app on its own
    schedule."; the GitHub text is unchanged. The default the switch reads
    when nothing is stored is the flavor's (`default_auto_update`), and the
    stored inverted DWORD is untouched, so nobody's recorded "off" flips.
  - A **Check now** button on General runs `fwdslash update check --force
    --json` off the UI thread and always answers on screen ("Up to date",
    "Update available", or "Could not check for updates"), while the launch
    check — the same verb without `--force`, gated by
    `update_check_allowed(has_package_identity(), read_auto_update_enabled())` —
    stays silent unless it found something. Both go through the CLI now rather
    than calling `fsw_core::update::run_update_check` in-process, because the
    CLI is the only component that knows the Store routes.
  - The install banner appears when `packaged && (update_bundle_ready ||
    update_available.is_some())`, labelled **Install now** for the Store flavor
    and **Restart to update** for the GitHub one. It runs `update install
    --force --relaunch app --json`, after `close_broker_window()` so the broker
    removes its own notification icon rather than leaving a ghost. Exit 0 closes
    the window (the install is about to force the package down and the CLI's
    watchdog brings it back); 10, 11 and 12 each leave a bar, and 11 — "the
    Store has to finish this" — is the one notice with an action button,
    **Open Microsoft Store**, on `ms-windows-store://pdp/?productid=<STORE_PRODUCT_ID>`.
  - The About Components card gains `Last update check: <never | just now |
    N minutes/hours/days ago>` and, when one is recorded, `Update available:
    <version>`.
  - A **Repair integrations** button on the Terminals page runs `fwdslash
    repair-adapters` (#56). It exists because the broker's failure balloon told
    the user to "Open Settings to retry" when there was nothing to press — the
    retry only happened by accident, in the launch sweep. It takes the same
    cross-process sweep lock the launch sweep does and reports "Integrations are
    already being updated" rather than fighting the broker for the payload tree.

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
  itself is asynchronous — a packaged `persist_disabled` shells out to `reg.exe`
  (`fsw_core::settings_write`, issue #52), and a process creation plus wait on
  the hook thread is exactly what must not happen.
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
  Settings and choose Repair integrations."` The settings window repeats the
  sweep on launch (Settings §6) and `fwdslash integration <id> enable` remains
  the manual fallback. The C++ broker does nothing of the kind.
- **A failed adapter upgrade is retried once, and a transient one is never
  announced** (#56). The first failure is followed by a 5 s pause and one
  retry. `adapter_outcome(first, retry)` then classifies: success either time is
  `Upgraded`; two attempts that never produced an exit code at all — the child
  could not be spawned, or blew the 90 s budget and was killed — are `Deferred`,
  logged `event=adapter_upgrade_deferred` and **silent**, because the marker key
  still reads the old version and the next broker start or settings launch tries
  again; only a retry that *ran and refused* is `NeedsUser` and earns the
  warning balloon. The whole sweep is serialised against the settings window's
  launch sweep by the named mutex `Local\ForwardSlashWindows.AdapterSweep`
  (`fsw_core::FSW_ADAPTER_SWEEP_MUTEX`, held for existence rather than
  ownership, exactly like the two singleton mutexes): whoever finds it already
  held logs `event=adapter_sweep_busy` and stands down, because the holder is
  running the identical work. Before this, an update that restarted the app
  started both sweeps within seconds and the loser deleted a payload tree the
  winner's child was running out of, which is the transient failure the balloon
  was reporting as terminal.
- **The broker drives the self-update.** The C++ has no updater at all, and
  before this the check only ran when the settings window opened. `health_tick`
  now calls `maybe_start_update_cycle()` once a minute, which starts a cycle
  only when all four of `!UPDATE_RUNNING`, an age of at least
  `UPDATE_CONSIDER_INTERVAL_MS` (6 h; the CLI enforces the real 24 h cadence),
  `!WORKER_BUSY` and
  `fsw_core::update::update_check_allowed(has_package_identity(),
  read_auto_update_enabled())` hold — `update_cycle_due`, a pure function, is
  the whole truth table. The first cycle of a process is held off for
  `UPDATE_FIRST_DELAY_MS` (5 min) after startup: logon is the busiest moment on
  the machine, the adapter sweep is already running, and an update that
  force-closes the package seconds after the user signed in is the worst
  possible one. `WORKER_BUSY` is raised around **both** worker messages
  (`PROCESS_ENTER` and `WORKER_OPEN_PATH`) by an RAII guard, so an install can
  never terminate the process mid-rewrite of an address bar.
  The cycle itself always runs on a thread named `fsw-update` — never inline,
  the same rule as the adapter sweep, because this thread owns the low-level
  keyboard hook; a spawn failure logs `event=update_cycle_skipped` and waits for
  the next tick, and a `Drop` guard clears `UPDATE_RUNNING` however the cycle
  ends. It runs `fwdslash update check --json` (120 s ceiling) against the
  `fwdslash.exe` **beside the broker**, never one from PATH, and on exit 10 goes
  on to `fwdslash update install --relaunch broker --json` (300 s) — no
  `--force`, so the CLI's own moment gate still declines while a settings window
  is open, and `broker` because what has to come back afterwards is the resident
  daemon, not a window nobody asked for. `run_cli_bounded` is the shared child
  runner underneath this and both adapter verbs; it returns `Option<i32>`, where
  `None` means "never answered" (spawn failure, wait error, or killed at the
  deadline) rather than "failed".
  Balloons are rationed: install exit 11 → `NIIF_INFO` "An update to fwdslash is
  available in the Microsoft Store. Open Settings to install it."; two
  consecutive install exit 1 **on the Store flavor only** → `NIIF_WARNING`
  "fwdslash could not update itself automatically." (the GitHub flavor's failed
  install leaves the downloaded bundle in place and applies it at the next
  logon, so there is nothing to ask the user for); exits 0, 10 and 12 are
  silent. Both go through `notify_when_icon_ready` and are deduplicated against
  `cached_update_tag()`, so one available version produces one balloon however
  many six-hour cycles see it.
- **New diagnostic categories** (category-only, per `PRIVACY.md`):
  `event=enter_dropped_foreground_changed`, `event=surface_rejected`,
  `event=hook_rearmed`, `event=persist_disabled_failed`,
  `event=win32_normalization_hazard`, `event=tray_icon_add_failed`,
  `event=worker_start_failed`, `event=worker_detached`,
  `event=debug_uia_failed`, `event=debug_hook_failed`,
  `event=adapter_upgraded`, `event=adapter_upgrade_failed`,
  `event=adapter_upgrade_skipped`, `event=settings_synced`,
  `event=state_changed`, `event=hook_unavailable`, and — new with the
  self-update and #56 — `event=adapter_upgrade_retry`,
  `event=adapter_upgrade_deferred`, `event=adapter_sweep_busy`,
  `event=update_cycle_started`, `event=update_available`,
  `event=update_installing`, `event=update_cycle_failed`,
  `event=update_cycle_skipped`. The C++'s `event=enter_handler_failed` has no
  Rust counterpart. None of them carries a version, a path or anything the user
  typed.
- **It re-reads the settings on a state-changed broadcast** (#55). The tray
  tooltip, the keyboard hook and the published mapping all derive from state
  another process can change; before this they caught up at the next health
  tick, or — for the pause flag, which the broker held only in memory — never.
  `reload_settings` compares the stored `Disabled` against `PAUSED` and, when
  they differ, applies the pause exactly as the tray toggle does minus the write
  (`apply_paused`), then refreshes the tooltip and republishes. It skips the
  comparison entirely while one of its own pause writes is in flight
  (`PERSIST_IN_FLIGHT`): the tray toggle changes `PAUSED` first and persists
  off-thread, so a broadcast arriving in between would otherwise be read as an
  external change and revert it. The tray menus need nothing — they are built
  from live state when the menu opens. The C++ broker has no equivalent.

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

### 3. Self-healing shell integrations (#37)

The C++ adapter writes a bare `Import-Module` into the profile, snapshots the
profile verbatim, and never revisits either. The Rust adapter hardens the whole
lifecycle so an upgrade or an MSIX uninstall can never leave a broken shell:

- **The profile block is guarded, fenced, and self-cleaning.** `block_text`
  emits `$m`/`$p`/`$a`/`$c` (module, product-presence probe, app-execution
  alias, staged controller) and
  `if ((Test-Path $p) -or (Test-Path $a)) { if (Test-Path $m) { Import-Module … } }
  elseif (Test-Path $c) { Start-Process $c uninstall --orphaned }`. A pruned
  module directory can no longer throw the red `no valid module file` error,
  and a product that was uninstalled with no code run (MSIX) is cleaned up by
  the leftover hook on the next shell start — launched detached, so a shell
  never blocks on it. The region is delimited by
  `# >>> Forward Slash Windows <ver> <id> >>>` / `# <<< … <<<` fence lines.
- **The probe is the package's app-data folder, not the alias.** A packaged
  install records `%LOCALAPPDATA%\Packages\<family>` (from the actual
  `fsw_core::package_family()` at install time, so either flavor works); an
  unpackaged one records the controller's directory. The app-execution alias is
  only ever an additional OR, because a user can switch it off under
  Settings > Apps > App execution aliases without uninstalling anything — using
  it as *the* probe would silently disable the integration and spawn a
  self-clean on every shell start. The package folder also survives an update,
  which closes the in-flight-update race.
- **Install/enable is replace-not-append and idempotent.** `commit_install`
  computes the *true* original — the current profile with **every** fwdslash
  fence stripped (`strip_fwdslash_blocks`, encoding-aware over UTF-8/16/32) —
  snapshots that, and writes it plus exactly one current block. A repeated
  enable, or an upgrade over an older block, can never accumulate duplicates or
  strand a stale block, and uninstall restores the genuine pre-fwdslash profile.
  `OriginalPresent` now tracks whether that true original is non-empty, so a
  profile that was purely our own block is deleted on removal.
- **Detect-and-repair.** `fwdslash repair-adapters` (run by the broker startup
  sweep and the settings launch sweep) and the per-adapter
  `fwdslash integration <id> repair` classify each profile — orphaned (missing
  module), stale (wrong version), duplicated — and repair to exactly one current
  block when the adapter should be installed, or strip it out when it should
  not. `fwdslash doctor` and `fwdslash integrations` print a
  `shell integration health:` line per adapter.
- **cmd never snapshots its own hook.** `begin_install` strips any
  `call "…ForwardSlashWindows…fsw-autorun.cmd"` segment from the observed
  `AutoRun` before recording the original, so an MSIX-leftover hook is not
  mistaken for a third-party value and `installed_autorun` never composes
  `call fsw & call fsw`. `fsw-autorun.cmd` is generated at install time with the
  probe baked in: it installs the doskey macros only while the product is
  present, and otherwise runs the self-clean instead of routing through an
  orphaned controller copy.
- **`fwdslash uninstall --orphaned`** is the deferred self-clean. It confirms
  the product is really gone (cheap file-system probes, then a
  `Get-AppxPackage` slow confirm only if those fail, so an in-flight update is
  safe), runs the transactional sweep (restoring profiles/AutoRun byte-exact,
  cmd still refusing a third-party change), then belt-and-braces strips what a
  refusal left: any fwdslash profile fence, and — the cmd analogue — fwdslash's
  own `call` segment out of `AutoRun`, keeping every third-party segment
  byte-for-byte and deleting the value only if nothing else remains. It removes
  `HKCU\Software\ForwardSlashWindows` and the unpackaged Run value, deletes the
  protocol key **only when its `shell\open\command` is still ours** (the normal
  uninstall's refusal, kept), and schedules deletion of
  `%LOCALAPPDATA%\ForwardSlashWindows` — including the directory it is running
  from — after it exits, but **never while `AutoRun` still references the
  payload**. Idempotent and safe to run twice.
- **The deferred delete is a scheduled task, not a detached child.** A
  `DETACHED_PROCESS` `cmd.exe` is killed with the rest of the tree when the
  launching shell lives inside a job object — measured on the dev host, where a
  WSL-interop-launched shell left the payload directory behind on 2/2 uninstall
  cycles even though nothing held the file open. `CREATE_BREAKAWAY_FROM_JOB` is
  not a fix either: that job forbids breakaway, so `CreateProcess` fails
  outright. The self-clean therefore writes
  `%LOCALAPPDATA%\Temp\fwdslash-orphan-cleanup.cmd` and registers a one-shot
  per-user task (`schtasks /create /sc once /st <now+1min> /f /tr <script>`,
  no elevation), then runs it immediately — the Task Scheduler service starts
  the script in its own session, outside any job we are in. The script waits
  ~2 s, removes the tree, then deletes the task and itself, so nothing
  accumulates; the one-minute trigger is only a backstop. The task name is
  fixed, so `/f` overwrites rather than piling up one task per run, and the
  detached child remains as the fallback when schtasks is unavailable.
  Belt and braces: `enable` and `repair-adapters` drop a payload tree that no
  adapter marker names before staging into it.
- **Controlled Folder Access is recognised through `ERROR_FILE_NOT_FOUND`.**
  CFA does not always block with `ERROR_ACCESS_DENIED`: on the dev host the
  blocked temp-file create inside a protected `Documents` subfolder surfaced as
  `os error 2`, and the user got "The system cannot find the file specified"
  instead of the product's guidance. `looks_like_blocked_write` now treats a
  "not found" as a block **when the containing folder exists**, keeps the
  access-denied case unconditional, and the message says "…or the folder is
  otherwise not writable". The same explanation reaches the settings InfoBar —
  `run_controller` captures the controller's stderr — and `doctor` /
  `integrations` report an installed adapter whose profile cannot be written.

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

