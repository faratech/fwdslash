# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Forward Slash Windows (`fwdslash`) makes Linux-style WSL paths work in native Windows
navigation surfaces. Typing `/etc/apt` in the File Explorer address bar, Run, Windows Search
or a classic Open/Save dialog opens `\\wsl.localhost\Ubuntu\etc\apt`.

## Build

**The product is the Rust workspace in `crates/`.** It is what is on the Microsoft Store, what
`release.yml` signs, and what both packagers stage. There is no other implementation. Build it
from Windows:

```powershell
cargo build --release --target aarch64-pc-windows-msvc --workspace
cargo build --release --target x86_64-pc-windows-msvc  --workspace
```

Output: `target\<rust-triple>\release\` — `fwdslash.exe`, `fswbroker.exe`, `fswsettings.exe`.
Package it with `python3 tools/package_msix.py` (from WSL) or `.\tools\Package-Msix.ps1` (from
Windows); both read the version from `workspace.package.version`. See
[Crate map](#crate-map) under Architecture for the crate layout and the version-island rule
before touching any crate.

From WSL/Linux, only the two library crates build without an MSVC linker:

```bash
cargo test -p fsw-path                                    # runs directly, no target needed
cargo check --target x86_64-pc-windows-msvc -p fsw-core   # type-checks only, no linker
```

The three `[[bin]]` crates (`fsw-cli`, `fsw-broker`, `fsw-settings`) link against `windows`-crate
COM/UI Automation surfaces and require `link.exe` — Windows only.

**A running broker or settings window will fail the link** — the linker cannot overwrite a
loaded image, and this applies to the packaged copy too. Before rebuilding:

```powershell
.\target\aarch64-pc-windows-msvc\release\fwdslash.exe stop
Get-Process fswsettings -ErrorAction SilentlyContinue | Stop-Process
```

### The kernel driver is the one thing that is not Rust

`driver/fswfilter/` is **C**, not C++, and is excluded from every normal build — see
[Driver](#driver). `Directory.Build.props` exists only to gate it (`FSWDriverProject`), and
`include/` survives only to hold `include/fsw_filter_protocol.h`, the broker/driver IPC
contract that `driver/fswfilter/fswfilter.h` includes. Build it with:

```powershell
.\tools\Build-Driver.ps1 -Architecture x64 -Configuration Release
```

`.github/workflows/build.yml` runs four jobs: `rust` and `rust-windows` (described under Test),
`powershell-regressions` (the adapter location-wrapper and installer regressions), and
`driver-compile`, gated to `workflow_dispatch` only — hosted runners ship no WDK
(`stampinf.exe` is missing), so it needs a self-hosted runner.

## Test

```bash
cargo test -p fsw-path -p fsw-core                              # resolver + funnel; runs on Linux/WSL
cargo test --workspace --all-targets --target x86_64-pc-windows-msvc   # everything, on Windows
```

`cargo test -p fsw-path` needs no target flag and is the fastest way to validate a resolver
change. `fsw-core` adds the funnel and update-check tests. **`fwdslash` has no lib target** —
the adapter and update unit tests live in the bin, so reaching them alone is
`cargo test -p fwdslash --bins --target x86_64-pc-windows-msvc`, and they need a Windows target
(and a host that can run the resulting exe).

**CI runs all of it** (`.github/workflows/build.yml`): the `rust` job on ubuntu-latest runs
`python3 tools/bump_version.py --check` and its unit tests (`python3 -m unittest
tools.test_bump_version`, no toolchain needed, so they go first), the Store-submission HTTP
verifier test, `cargo fmt --all -- --check`, the two portable crates, the `cargo tree -p
fsw-path` single-line assertion, the `windows-core` single-version assertions for `fswsettings`
and `fwdslash`, and **`cargo clippy --workspace --all-targets -- -D warnings` for both MSVC
targets**; the `rust-windows` job on windows-latest checks the vendored InstallControl bindings
are current, runs the full Windows workspace test suite, does a release build and prints a
binary-size report (reported, not gated).

`tools/Test-Sandbox.ps1` drives a Windows Sandbox install/start/pause/uninstall cycle;
`tools/Test-Prerequisites.ps1 [-RequireWdk]` checks the toolchain.

## Package

```powershell
.\tools\Package-Msix.ps1                          # x64 + ARM64 .msixbundle — the shipping path
.\tools\Build-ReleaseZips.ps1 -Version <version>  # per-architecture ZIPs of the loose exes
```

`Package-Msix.ps1` stages `target\<triple>\release`, takes the `shell/` payload from the repo,
and defaults `-Version` from `cargo metadata`.

See `docs/store-submission.md` for identity values and Store constraints.

`tools/package_msix.py` is the WSL-runnable equivalent: it shells out to
`makeappx.exe`/`makepri.exe` under `packages/` via `wslpath`, so MSIX packaging doesn't require
leaving WSL for native PowerShell. It reads the version from `workspace.package.version`, stages
the `shell/` payload from the repo tree and fails if any adapter file is missing, and picks the
SDK tool architecture from `platform.machine()` (override with `FSW_SDK_TOOL_ARCH`).

**Local MSIX test loop (Rust binaries):** `cargo build --release --target <rust-triple>
--workspace` on the Windows side (both `aarch64-pc-windows-msvc` and `x86_64-pc-windows-msvc`
are installed), then `python3 tools/package_msix.py` from WSL — it stages the three Rust exes
from `target/<triple>/release`. Sign with the publisher-matching self-sign cert
(`C:\code\wfdiag-selfsign.pfx`, password recorded in the wfdiag repo's `build-cross.py`; the
cert must also be in the machine's trusted root — it already is on the dev host). Same-version
reinstall is blocked with 0x80073CFB: `Get-AppxPackage 32827MikeFara.fwdslash | Remove-AppxPackage`
before each `Add-AppxPackage`, or bump `workspace.package.version`. Launch the packaged
app with `explorer.exe 'shell:AppsFolder\32827MikeFara.fwdslash!App'`, and remember the
running-process rule above applies to the packaged copy too — a running settings window
(often the leftover unpackaged dev build in `target\release`) blocks relinking.

## Release

A release is a version bump, a release-notes file and a tag; everything after the tag is
automated.

```bash
python3 tools/bump_version.py 0.0.4     # rewrites every registered literal
python3 tools/bump_version.py --check   # what CI will assert
```

**Write `docs/release-notes/<version>.md` in the same PR.** `release.yml` refuses to publish
without it, and it is the only thing Store customers read: `publish-to-store.yml` derives the
Store "What's new" by truncating the release body at the `## Downloads` marker, and this file
is everything above that marker. Write it in plain language for someone who has never seen the
code — one short sentence per user-visible change, no file or function names, no issue numbers,
no internals. The generated "What's Changed" commit list still lands in the GitHub release for
developers, but it sits *below* the marker and never reaches the Store.

Open a PR with that bump, merge it, then tag the merge commit `v0.0.4` and push the tag
(`tools/bump_version.py --commit --tag` will make the commit and the annotated tag for you;
it never pushes). The tag push starts `.github/workflows/release.yml`, which asserts the tag
matches `workspace.package.version` — this is why the bump must be merged first — then builds
x64 + ARM64, signs the GitHub flavor through Azure Trusted Signing, packages the tree a second
time under the Partner Center identity (deliberately unsigned, since the Store re-signs),
creates the GitHub release with both bundles and the sideload ZIPs, and dispatches
`publish-to-store.yml` to submit the `*-store-unsigned.msixbundle` to the Store. A
`workflow_dispatch` run is a dry run unless `dry_run` is explicitly false: it builds and signs
into workflow artifacts and creates no release. Nothing needs to be built or signed locally.

## Architecture

Three user-mode binaries share one static core and cooperate at runtime:

- **`fswbroker.exe`** — the resident daemon, and the owner of the product's **single**
  notification-area icon. Installs a system-wide `WH_KEYBOARD_LL` hook that inspects **only
  `VK_RETURN`** and passes everything else through. On Enter the hook does nothing but
  classify the foreground window (class first, then process image) and post to a **worker
  thread**; the worker — its own STA, its own `IUIAutomation`, its own message-only window
  `ForwardSlashWindows.BrokerWorker` — reads the focused control's text via UI Automation and,
  if it starts with `/`, rewrites it to the resolved UNC path and replays Enter (tagged with a
  private `dwExtraInfo` marker so the hook ignores its own input). Otherwise it replays the
  keystroke untouched. This swallow-inspect-rewrite-replay cycle is the heart of the product;
  read `process_enter_request` in `crates/fsw-broker/src/main.rs` first. Nothing that can block
  belongs on the hook thread: Windows silently removes a low-level hook that exceeds `LowLevelHooksTimeout`.
  `docs/architecture.md` has the ASCII data-flow diagrams for this path and for the
  (excluded-from-build) filesystem-routing path through the driver; `docs/divergences.md`
  Broker §2 is the full list of broker behaviour (tray menu, tooltip, health timer, the
  `#32770` narrowing, the diagnostic categories).
- **`fwdslash.exe`** — the CLI and the only component that mutates install state. The settings
  app never writes integration state itself; it shells out to this. Beyond the user-facing
  verbs it carries the shell-adapter contract: `cmd-list`, `cmd-cd` and `shell-resolve`, all
  three sharing one `shell_target` funnel, with **exit 3 meaning "run your own command
  unchanged"**. Installing an adapter whose recorded `Version` is older than `PAYLOAD_VERSION`
  upgrades it in place (uninstall transaction, then install transaction) — and that upgrade is
  **automatic**: the broker sweeps every installed adapter at startup and the settings window
  sweeps again on launch, so `fwdslash integration <id> enable` is only the manual fallback.
  It also owns the **whole** self-update pipeline: `update check|install|status`, plus the two
  helper-only verbs (`apply-store`, `apply-bundle`) that are absent from `usage()` and exit 20
  under package identity. The broker and the settings window contain no update logic — they run
  `fwdslash update` and key on its exit code (`0` up to date / install started, `10` available
  or deferred, `11` needs the user, `12` nothing to install, `1` error, `2` usage, `20` wrong
  context) or its one JSON line. See Dual-track distribution below.
- **`fswsettings.exe`** — the desktop settings app, built on the vendored `windows-reactor` crate (Windows App
  Runtime **2.x**). It has **no tray icon and no watchdog**: closing the window exits the
  process, and the broker keeps the icon. Controller calls and state reads run on the thread
  pool with the affected controls disabled and a `ProgressRing` shown.

They find each other by well-known names defined once in `crates/fsw-core/src/lib.rs`:
window class `ForwardSlashWindows.Broker`, mutex
`Local\ForwardSlashWindows.Broker`, and `WM_APP+10..12` messages. There is no pipe or RPC.
The broker window is titled `fwdslash broker` — never discovered by title, but the settings
window's caption is `Forward Slash Windows` and the two used to collide.
`FSW_WM_SET_PAUSED` replies with the resulting `BrokerState` (`Active` = 1, `Paused` = 2) or
**0** when the change could not be applied; it is not a boolean ack.

One more message is not addressed to anyone: `FSW_STATE_CHANGED_MESSAGE`
(`ForwardSlashWindows.StateChanged`, a `RegisterWindowMessageW` id) is posted to
`HWND_BROADCAST` after a state change lands, so a running settings window and the broker stop
rendering what they read at launch (issue #55). `fsw_core::broadcast_state_changed()` is the
only way to send it: `settings_write` calls it after every successful write, and the CLI once
per mutating verb that exited 0. **Never broadcast before the write lands or after one that
failed** — every listener re-reads, so an early notice just re-renders the old value. The
broker handles it in its window proc (`reload_settings`); the settings window listens on a
hidden top-level window of its own (`crates/fsw-settings/src/state_watch.rs` — `HWND_BROADCAST`
skips message-only windows) and backs it with a 5 s poll for writers that say nothing.

`crates/fsw-path` holds the pure resolver and `crates/fsw-core` the registry reads and the
funnel. **All resolution flows through `resolve_user_slash_path`**, so a change there reaches
Explorer, Run, Search and both shell adapters at once. The rule numbers R1-R12 cited
throughout the resolver are enumerated in `docs/divergences.md`. Bare-slash behaviour is
opt-in: in `default_distribution` mode a leading segment that is not a registered distribution
resolves against the default distro, so `/tmp/build` works unprefixed. A registered
distribution wins over a same-named directory only in distribution-list mode
with no configured folder root (2026-09-06); under a default distribution or
folder root the first segment is filesystem content of that root.

### Settings persistence

Runtime settings live under `HKCU\Software\ForwardSlashWindows\Settings` — `Disabled` (DWORD,
global pause), `BareSlashMode` (DWORD, 0 = distribution list / 1 = default distribution),
`BareSlashDistribution` (string, the pin), `BareSlashRoot` (string, the custom folder root),
and — in **both** flavors — `AutoUpdate` / `LastUpdateCheck` / `AvailableUpdate`,
plus the read-only `UpdateRoute` override (`auto|appinstall|store|winget|notify`, never written
by the product), plus two Store-only values: `StoreUpdatePending` (DWORD) and
`StoreUpdateAttempt` (QWORD). **`StoreUpdatePending` is the availability truth and
`AvailableUpdate` is only its label** — the Store can offer an update whose version is not
a trustworthy target, and the installed version must never be advertised as one (issue #97).
Read the pair through `fsw_core::update::cached_offer()`, which returns an
`Offer::Named(tag)` or `Offer::Unnamed`, and never read `AvailableUpdate` raw: a persisted
label that is no longer newer than what runs is spent, not an offer. `StoreUpdateAttempt`
stamps an install started from an unnamed offer, so a same-version repair offer gets one
attempt a day rather than one per cycle. `AutoUpdate` is the one value whose *absent* meaning depends on the flavor:
`fsw_core::update::default_auto_update(store_flavor) = !store_flavor`, so nothing stored means
on for the GitHub build and off for the Store build, while the stored encoding stays the
inverted DWORD it always was (`1` = off) so an explicit "off" never flips. The gate itself,
`update_check_allowed(packaged, auto_update)`, no longer knows about flavors at all.
Anything reading two or more of them should take
`fsw_core::SettingsValues::read()`, which opens the key once; the single-value getters delegate
to it.

**Every write to that key goes through `fsw_core::settings_write`** —
`set_setting_u32` / `set_setting_u64` / `set_setting_string` / `delete_setting` — and nothing
else may call `windows_registry`'s `set_*`/`remove_value` on it (issue #52). The rule is the
adapters' rule, for the same reason: a packaged process's own registry write is virtualized
into the package's private hive, which the shell adapters — an *unpackaged* `fwdslash.exe` —
cannot see, so the settings app could switch the bare-slash mode while `cd /` in PowerShell
kept answering with the old one. `write_plan(packaged)` is the decision: unpackaged, the
in-process API already *is* the real hive and one write is the whole job; packaged, the value
goes to the real hive through `reg.exe` **and** to the package hive in-process, because a stale
private-hive copy shadows the real one for every packaged reader and a real-hive-only write
would just invert the split. `fsw_core::sync_settings_to_real_hive()` is the self-heal for
installs that already carry it — the packaged process mirrors its merged view into the real
hive (never deleting) — and it runs from the broker's startup sweep, the settings window's
launch sweep and `fwdslash repair-adapters`, before the adapter work in each.

The key path and value names are defined once in `crates/fsw-core/src/lib.rs` and shared by the
core, broker and CLI — **except** `shell/powershell/ForwardSlashWindows.psm1`, which hardcodes
the same key path as a literal string (`Test-ForwardSlashWindowsDisabled`) because a PowerShell
module cannot read a Rust constant. Renaming a setting means updating the module too. Each optional integration also has its own
install-state marker, `HKCU\Software\ForwardSlashWindows\CmdAdapter` and
`...\PowerShellAdapter\WindowsPowerShell` / `...\PowerShellAdapter\PowerShell`, a `State` value
of `installed` — this is separate from the transactional snapshot (previous `AutoRun`/profile
bytes) and is only what `fwdslash integration <name> enable|disable` and
`fwdslash integrations` check for idempotency.

### Packaged vs unpackaged duality

The product ships both as a ZIP and as an MSIX, and behaves differently in each.
`fsw_core::has_package_identity()` is the switch:

| Concern | Unpackaged | Packaged (MSIX) |
|---|---|---|
| Logon start | HKCU `...\CurrentVersion\Run`, written by `fwdslash install` | `windows.startupTask` in the manifest |
| `fwdslash://` protocol | HKCU `Software\Classes\fwdslash` | `windows.protocol` in the manifest |
| CLI on PATH | user adds the folder | `windows.appExecutionAlias` |

When adding anything that writes install state, branch on package identity. Writing the
Run key or protocol registration from a packaged build leaves **orphaned entries pointing into
a deleted `WindowsApps` directory** — those locations are deliberately un-virtualized.

MSIX virtualizes HKCU, which would hide the cmd `AutoRun` value from unpackaged shells. The
manifest declares **no** virtualization exclusions and **no** `unvirtualizedResources`
capability: the adapters route every registry write through `reg.exe`, a System32 child with no
package identity, so the writes land in the real hive regardless. Reads use the merged view and
are always correct. Do not reintroduce the exclusions — dropping them is what removed the
Microsoft-approval requirement from the Store submission (`docs/compatibility.md` has the
clean-room result). File writes to `%LOCALAPPDATA%\ForwardSlashWindows` and `Documents` are
already real for this package.

The startup task only fires at **logon** and MSIX runs nothing at install time, so the settings
app calls `ensure_broker_running()` on launch (off the UI thread). Without it a Store install
does nothing at all. The settings app needs no build-time packaged/unpackaged switch:
`windows-reactor`'s bootstrap detects package identity at runtime, so one binary serves both.

### Crate map

`crates/` is the whole product. Five crates map onto the three binaries plus the shared core:

| Crate | Binary / role |
|---|---|
| `fsw-path` | library, zero dependencies — the pure resolver |
| `fsw-core` | library, registry reads + settings persistence + broker probes |
| `fsw-cli` | `fwdslash.exe` |
| `fsw-broker` | `fswbroker.exe` |
| `fsw-settings` | `fswsettings.exe`, built on the vendored `windows-reactor` crate in `crates/windows-reactor/` |

`fsw-core` is `#[cfg(windows)]`-gated with non-Windows fallbacks (registry reads return empty,
writes no-op), which is why it type-checks on Linux even though it ultimately reads/writes HKCU.

**`fsw-settings` depends on `fsw-core`**, so the settings window reads state in-process rather
than parsing `fwdslash --json`. That is sound across the version islands because `fsw-core`'s dependency closure contains no
`windows-core` at all — `windows-registry` 0.6 and `windows-sys` 0.61 both stop at
`windows-link`. The argument and the CI gate are in `docs/dependencies.md`; do not add a
crate to `fsw-core` that pulls `windows-core`.

**The version-island rule** (full detail in `docs/dependencies.md`) is the load-bearing
constraint on this workspace: `windows-reactor` requires `windows-core` 0.100, while everything
else uses the `windows`/`windows-sys` 0.62/0.61 generation, and the two `windows-core` majors are
incompatible COM/WinRT type systems that must never appear in the same binary. `fsw-path` and
`fsw-core` are therefore the only crates shared across both islands, which is only sound because
`fsw-path`'s `[dependencies]` table is intentionally empty — the `rust` job in
`.github/workflows/build.yml` asserts `cargo tree -p fsw-path` is exactly one line, and a PR that needs to add a dependency there must justify it in
`docs/dependencies.md` first. `crates/windows-reactor/` is Microsoft's own crate (MIT/Apache-2.0,
from microsoft/windows-rs), vendored and pulled in via `[patch.crates-io]` in the root
`Cargo.toml` so it can be patched locally — treat it as a dependency to patch, not code owned by
this project. It has already absorbed local patches this way: `WindowVisuals::icon_resource`
(WM_SETICON from the exe's `.rc` resource — the taskbar icon the `.rc` file alone never
provides), `ThemeBrush::TextSecondary` (`TextFillColorSecondaryBrush`), and the
`TitleBar.LeftHeader` slot (binding + slot plumbing; the caption icon goes there because
`ImageIconSource` fail-fasts under the unpackaged SDK — see `docs/divergences.md` §1). When a
WinRT API slot is missing, the vtable in `native/winui/bindings.rs` usually reserves its
position as a `usize` placeholder — implement the fn pointer + wrapper method, then wire the
slot/property through `generated.rs` and `native/winui/generated.rs`.

The settings app has no tray icon and no window subclass (an earlier `tray.rs` was removed);
its only Win32 surfaces are `folder_picker.rs`, the single-instance guard, and
`state_watch.rs`. Window discovery in all of them enumerates **current-process** windows only —
a bare `FindWindowW(NULL, title)` matches another instance's window, and the broker's own
window has carried the same caption. The app is single-instance
(`activate_existing_instance` + `Local\ForwardSlashWindows.Settings` mutex): a second launch
raises the existing window.

`docs/divergences.md` is the behaviour specification: the enumerated resolver rules R1-R12
(each backed by a named test in `crates/fsw-path/tests/resolver.rs`), the deliberate resolver
choices (case-folding table, error shapes, malformed distribution names, the structural
bare-slash rewrite), the settings-app section (caption icon route, pane display mode, Fluent
conformance, the single-instance guard), the broker's own section (worker thread, tray menu,
`#32770` narrowing, diagnostic categories) and the CLI's shell verbs. Behaviour that contradicts
it is a bug, not a feature. `docs/size-baseline.md` records the current measured binary sizes
as the baseline a change is compared against, and the codegen policy
(`crt-static`, fat LTO, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"`) that makes a
comparable size plausible — `panic = "abort"` in particular is not negotiable, since unwinding
out of a `WH_KEYBOARD_LL` callback or a COM vtable is undefined behavior. The flags live in
`.cargo/config.toml` (per-target `rustflags`; never `[build] rustflags`) and the toolchain is
pinned by `rust-toolchain.toml` — install the pinned version on **both** the Windows host and
WSL, and re-measure `docs/size-baseline.md` whenever either changes. `cargo tree -p fsw-path`
counts dev-dependencies too, so `fsw-path` may not gain even a dev-dep: the shared test corpus
is `crates/fsw-path/tests/common/mod.rs`, the zero-allocation contract is `tests/allocations.rs`
(always on), and the timing smoke is `tests/perf.rs` (`--release -- --ignored`).
`tools/update-deps.py` is the
workspace-aware dependency updater and enforces the island pins listed in its `ISLAND_PINNED`.

### Dual-track distribution

Two package flavors ship from the same tree: the **Store** build (Identity
publisher `CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4`, family
`32827MikeFara.fwdslash_t6j5qexy2jpp2`) and the **GitHub** build (publisher
`CN=Mike Fara, O=Mike Fara, …`, Trusted Signing-signed by
`.github/workflows/release.yml`). Flavor is always detected at runtime by
package family (`is_store_flavor`), never at build time. `signing/` holds the
Azure Trusted Signing kit (credentials live in GitHub Secrets);
`tools/Install-fwdslash.ps1` refuses to install the GitHub build over a Store
install unless `-Force`.

**Both flavors now update themselves**, under one `AutoUpdate` switch — on by
default for GitHub, **off by default for the Store** (opt-in by decision; the
Store keeps updating a Store install on its own schedule regardless). The GitHub
route is unchanged in shape (`crates/fsw-core/src/update.rs`: daily
`api.github.com` check, download, `Add-AppxPackage`); the Store route asks the
Store itself. **All of it is in the CLI**, `crates/fsw-cli/src/update/` — the
broker calls `fwdslash update` from its health timer and the settings window
from its button, and neither carries update logic of its own. The install ladder
is `appinstall` (winget's `AppInstallManager` sequence, tried in-process first
and then from the helper) → `store` (`StoreContext` silent install) → `winget`
(`--source msstore`, skipped on a metered network) → `notify`; `route_for` is
the pure function that picks, and the `UpdateRoute` value or `--route` pins one
rung without a rebuild.

Two pieces of that are easy to break. The **helper**,
`%LOCALAPPDATA%\ForwardSlashWindows\update\fwdslash-helper.exe`, is a
byte-identical copy of `fwdslash.exe` run **without** package identity, because
that is the only context in which the Store will replace the calling package and
`Add-AppxPackage` can register over a running one — and it **never writes
HKCU** (its writes would land in the real hive that the packaged app does not
read; it reports through `update\last-result.txt`, which the next packaged
`check`/`status` folds in and deletes). The **watchdog** is one per-user
scheduled task, `fwdslash-update`, registered *before* the install because a
force-closed process cannot relaunch itself; it polls `Get-AppxPackage` until
the version advances and then starts the broker through the app-execution alias
(`--relaunch app|broker|none`). Its script must contain no `%` and no `"` —
`cmd.exe` would eat both — and every literal spliced into it is checked by
`is_safe_task_literal` first. `docs/divergences.md` has the full section,
`docs/store-submission.md` §3 the certification wording, `PRIVACY.md` what
leaves the machine.

One `release.yml` run produces **both**: it signs the GitHub flavor, then
packages the tree again with the packager's default (Partner Center) identity
into `out\msix-store` — deliberately unsigned, because the Store re-signs —
validates it with `tools/Test-StoreBundle.ps1`, attaches it as
`fwdslash-<version>-store-unsigned.msixbundle`, and dispatches
`.github/workflows/publish-to-store.yml` to upload it through `msstore`. That
suffix is load-bearing: `Install-fwdslash.ps1` and `extract_bundle_url` in
`crates/fsw-core/src/update.rs` both skip it so the unsigned bundle is never
mistaken for the installable one. `check-store-submission.yml` inspects or
cancels a stuck Partner Center submission. `docs/store-submission.md` §1a has
the flow, the secret names (`STORE_SELLER_ID` is the one not yet set) and the
manual-publish recipe.

### Shell adapters

`cmd` and PowerShell support are optional adapters installed natively by `fwdslash`
(`crates/fsw-cli/src/adapters/`; the retired `tools/Install-*.ps1` helpers are gone). They are
**transactional**: the previous `AutoRun` value and the original PowerShell profile bytes are
snapshotted before modification and restored byte-exact on removal, and uninstall refuses to touch
anything a third party has since changed. Preserve that property. `fwdslash uninstall` sweeps all
installed adapters before removing the Run key and protocol.

The cmd adapter works by `doskey` macros, so **it only takes effect in interactive consoles** —
`cmd /c "dir /etc"` will always fail with "Invalid switch". Test it in a real console window.
The macros cover `dir`/`ls` (`fsw-dir.cmd`) and `cd`/`chdir`/`pushd` (`fsw-cd.cmd`,
`fsw-pushd.cmd`); `cmd.exe` cannot make a UNC path current, so the CD adapter enters the target
with `pushd` on an `endlocal &` line — never inside `setlocal`, or the directory change dies
with the script. The PowerShell module aliases `cd`/`chdir`/`sl`/`pushd` as well, and its
`pushd` wrapper is a *global* function built from an unbound script block (a module function
would push onto the module's own location stack). A payload list lives in three places —
`crates/fsw-cli/src/adapters/cmd.rs`, `tools/Package-Msix.ps1` and `tools/package_msix.py` —
and adding a file means editing all three.

The installed payload directory is named for `PAYLOAD_VERSION`, which now derives from
`CARGO_PKG_VERSION`. A version bump therefore marks every deployed adapter outdated, and the
upgrade runs itself: `start_adapter_upgrade` in `crates/fsw-broker/src/main.rs` re-runs
`fwdslash integration <id> enable` on a background thread at broker startup (90 s per adapter,
one summary balloon), and the settings window repeats the sweep on launch, reporting through an
InfoBar. Running `fwdslash integration <name> enable` by hand is the fallback, not the
expected path, and the settings app has no button for it.

### Driver

`driver/fswfilter` is a production-gated kernel minifilter, excluded from every normal build
(only `tools/Build-Driver.ps1` touches it, gated again by `FSWDriverProject` in
`Directory.Build.props`). It must never enter a package. Per `SECURITY.md` it is only ever
loaded in a checkpointed VM. The only code the broker and driver share is the IPC contract in
`include/fsw_filter_protocol.h`: the broker connects to a Filter Manager port
(`FSW_FILTER_PORT_NAME`) and publishes a versioned distro-name mapping
(`publish_filter_mappings` in `crates/fsw-broker/src/main.rs`, resent on a 5 s health timer and
on any state change) whether or not the driver is actually loaded. The driver itself is C, and
`include/` and `Directory.Build.props` survive only for it.

## Conventions

- Rust 2024, toolchain pinned by `rust-toolchain.toml`; workspace lints deny `unwrap_used`,
  `expect_used` and `panic` (see the reasoning in the root `Cargo.toml` — `panic = "abort"`
  makes any of them an instant process death that skips `WM_DESTROY`). `cargo fmt --all --
  --check` and `cargo clippy --workspace --all-targets -- -D warnings` for both MSVC targets
  are CI gates — keep the tree warning-clean.
- The kernel minifilter under `driver/` is C (C17, WDK), built only by `tools/Build-Driver.ps1` and
  never part of a normal build. Warnings are errors there too.
- **Per-user only.** Everything is HKCU and `asInvoker`; there are no HKLM writes and nothing
  requires elevation. Keep it that way.
- **Version `0.1.0`.** The Rust tree has one source of truth — `workspace.package.version` in
  the root `Cargo.toml` — and everything downstream of it derives:
  - each `build.rs` (`fsw-broker`, `fsw-settings`, `fsw-cli`) passes `FSW_VER_COMMAS` /
    `FSW_VER_STR` defines to `embed_resource::compile`, so both the numeric
    `FILEVERSION`/`PRODUCTVERSION` fields **and** the string block in `crates/*/app.rc` come
    from `CARGO_PKG_VERSION` (the `#ifndef` fallbacks in those `.rc` files are a last-resort
    literal, not the source — the bump script below is what keeps them in step);
  - `adapters::PAYLOAD_VERSION` is `env!("CARGO_PKG_VERSION")`;
  - `tools/package_msix.py` and `Package-Msix.ps1` read it (from the TOML and from
    `cargo metadata` respectively);
  - `release.yml` fails the run when the tag disagrees with it.

  Everything else that carries the version as a literal — the `#ifndef` fallbacks in
  `crates/*/app.rc`, `crates/fsw-settings/app.manifest`, the About header, `SECURITY.md`,
  `docs/store-submission.md` §5, this bullet, and the workspace members' `Cargo.lock`
  entries — **is never edited by hand**. Run:

  ```bash
  python3 tools/bump_version.py --check       # CI mode: do all the copies agree?
  python3 tools/bump_version.py 0.0.4         # the bump; add --dry-run to preview
  ```

  (`.\tools\Bump-Version.ps1` is the Windows wrapper; it forwards to the same script.)
  Each location is a `Site` in that script — an anchored pattern with an expected match
  count — so a literal that moves fails the run loudly instead of being skipped, and the
  whole rewrite is all-or-nothing. The `rust` job in `.github/workflows/build.yml` runs
  `--check` and `tools/test_bump_version.py` on every push, so a stale copy cannot merge.
  **Adding a new version literal to the tree means adding a `Site`**; there is deliberately
  no global search-and-replace fallback. The script's docstring also lists what is
  *deliberately* excluded (historical "new in 0.0.x" mentions, the Store's
  last-published-version comment, version-comparison test fixtures).
- The icon resource id `IDI_FSW_APP = 101` has two copies: `crates/fsw-broker/app.rc` and a
  `const` in `crates/fsw-broker/src/main.rs`. Each binary links a
  different icon on purpose — a 16-48px `assets/fwdslash-tray.ico` for the broker, the full
  `assets/fwdslash.ico` for the settings app, and **none** for the CLI, whose
  `crates/fsw-cli/app.rc` carries VERSIONINFO only. See `docs/size-baseline.md`.
- `docs/compatibility.md` lists release gates; blank entries mean unverified and must not be
  advertised as working.
- **Never log a path.** `log_diagnostic()` in `crates/fsw-broker/src/main.rs`, opt-in only via the `FSW_DIAGNOSTIC_LOG` env var, writes fixed
  event/reason category strings such as `event=route_distribution` — never the text the user
  typed or the UNC path it resolved to. This is a commitment made in `PRIVACY.md`; keep new
  diagnostic calls category-only. The current category list is in `docs/divergences.md`
  Broker §2.

### PowerShell gotchas in `tools/`

Three non-obvious failures have already been hit here:

- `$PSScriptRoot` is **empty while parameter defaults are evaluated** under `[CmdletBinding()]`.
  Resolve repo-relative defaults in the body, not in the `param()` block.
- Variable names are case-insensitive, so `foreach ($architecture in $Architecture)` over a
  `[string[]]` parameter re-wraps each value back into an array via the parameter's type
  constraint. Name loop variables distinctly.
- `Copy-Item -LiteralPath` does not expand wildcards; `-LiteralPath (Join-Path $dir '*')`
  silently copies nothing.
