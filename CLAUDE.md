# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Forward Slash Windows (`fwdslash`) makes Linux-style WSL paths work in native Windows
navigation surfaces. Typing `/etc/apt` in the File Explorer address bar, Run, Windows Search
or a classic Open/Save dialog opens `\\wsl.localhost\Ubuntu\etc\apt`.

## Build

`tools/Build-UserMode.ps1` is authoritative for shippable artifacts. It drives `cl.exe`/`link.exe`
directly for the native binaries and MSBuild for the WinUI 3 settings app.

```powershell
.\tools\Build-UserMode.ps1 -Architecture ARM64 -Configuration Release
.\tools\Build.ps1 -Architecture x64 -Configuration Release -Driver   # adds the kernel driver
```

Output: `out\user\<arch>\<config>\`. Architectures are `x86`, `x64`, `ARM64`.

CMake (`CMakePresets.json`, Debug-only presets) is a **partial parallel path used by CI for
compile/test coverage**. It does not build the settings app, the driver, or stage the shell
payloads. Do not assume a CMake build produces a runnable product.

`.github/workflows/build.yml` runs three independent jobs: CMake presets `x64-debug` /
`arm64-debug` / `x86-debug` (ctest only on `x64-debug`), MSBuild invoked directly against
`ForwardSlashWindows.Settings.vcxproj` for `Win32`/`x64`/`ARM64` (not via `Build-UserMode.ps1`),
and a driver-compile job gated to `workflow_dispatch` only — hosted runners ship no WDK
(`stampinf.exe` is missing), so it needs a self-hosted runner.

**A running broker or settings window will fail the link** — `link.exe` cannot overwrite a
loaded image. Before rebuilding:

```powershell
.\out\user\arm64\Release\fwdslash.exe stop
Get-Process fswsettings -ErrorAction SilentlyContinue | Stop-Process
```

### Rust port (parallel, not yet wired into the build)

A full Rust rewrite lives in `crates/` (workspace root `Cargo.toml`). It is committed as of
v0.0.1, but it is **not wired into the build or release pipeline** — `build.yml`,
`Build-UserMode.ps1`, `Package.ps1` and the README all still describe only the C++ product
above, and nothing in CI compiles or tests the Rust tree yet. See [Rust port](#rust-port) under Architecture for the crate map and the
version-island rule before touching any crate. From WSL/Linux, only the two library crates
build without an MSVC linker:

```bash
cargo test -p fsw-path                                    # runs directly, no target needed
cargo check --target x86_64-pc-windows-msvc -p fsw-core   # type-checks only, no linker
```

The three `[[bin]]` crates (`fsw-cli`, `fsw-broker`, `fsw-settings`) link against `windows`-crate
COM/UI Automation surfaces and require `link.exe` — Windows only, and subject to the same
running-process-blocks-the-linker rule as the C++ binaries above.

## Test

```powershell
.\out\user\<arch>\<config>\fswcore_tests.exe    # resolver unit tests; the only automated suite
cmake --preset x64-debug; ctest --preset x64-debug
```

`fswcore_tests.exe` is a single binary with no filtering — there is no "run one test"; add a
case to `tests/core_tests.cpp` and run the whole thing.

The other two test binaries are **manual harnesses that need a live WSL install**, not part of
any suite:

```powershell
.\fsw_address_bar_integration.exe <distribution> [/path | --root]
.\fsw_filesystem_integration.exe <distribution> /path
```

`cargo test -p fsw-path` is the Rust resolver's equivalent of `fswcore_tests.exe` (47 cases) and
runs directly on Linux/WSL with no target flag — it's the fastest way to validate a resolver
change before touching the C++ side. It is currently the only crate in the Rust workspace with
tests.

`tools/Test-Sandbox.ps1` drives a Windows Sandbox install/start/pause/uninstall cycle;
`tools/Test-Prerequisites.ps1 [-RequireWdk]` checks the toolchain.

## Package

```powershell
.\tools\Package.ps1 -Architecture ARM64            # ZIP, the sideload SKU
.\tools\Package-Msix.ps1                           # x64 + ARM64 .msixbundle for the Store
```

See `docs/store-submission.md` for identity values and Store constraints.

`tools/package_msix.py` is a WSL-runnable equivalent to `Package-Msix.ps1`: it shells out to
`makeappx.exe`/`makepri.exe` under `packages/` via `wslpath`, so MSIX packaging doesn't require
leaving WSL for native PowerShell. It is part of the Rust-port work (see below) and currently
hardcodes its own `VERSION` constant rather than reading the workspace version.

**Local MSIX test loop (Rust binaries):** `cargo build --release --target <rust-triple>
--workspace` on the Windows side (both `aarch64-pc-windows-msvc` and `x86_64-pc-windows-msvc`
are installed), then `python3 tools/package_msix.py` from WSL — it stages the three Rust exes
from `target/<triple>/release`, so the MSIX is the *Rust* product even though nothing else
consumes it yet. Sign with the publisher-matching self-sign cert
(`C:\code\wfdiag-selfsign.pfx`, password recorded in the wfdiag repo's `build-cross.py`; the
cert must also be in the machine's trusted root — it already is on the dev host). Same-version
reinstall is blocked with 0x80073CFB: `Get-AppxPackage 32827MikeFara.fwdslash | Remove-AppxPackage`
before each `Add-AppxPackage`, or bump `VERSION` in `package_msix.py`. Launch the packaged
app with `explorer.exe 'shell:AppsFolder\32827MikeFara.fwdslash!App'`, and remember the
running-process rule above applies to the packaged copy too — a running settings window
(often the leftover unpackaged dev build in `target\release`) blocks relinking.

## Architecture

Three user-mode binaries share one static core and cooperate at runtime:

- **`fswbroker.exe`** — resident tray + `HWND_MESSAGE` daemon. Installs a system-wide
  `WH_KEYBOARD_LL` hook that inspects **only `VK_RETURN`** and passes everything else through.
  On Enter, if the foreground window is a recognized surface, it reads the focused control's
  text via UI Automation, and if it starts with `/` rewrites it to the resolved UNC path and
  replays Enter (tagged with a private `dwExtraInfo` marker so the hook ignores its own input).
  Otherwise it replays the keystroke untouched. This swallow-inspect-rewrite-replay cycle is
  the heart of the product; read `ProcessEnterRequest` in `src/broker/main.cpp` first.
  `docs/architecture.md` has the ASCII data-flow diagrams for this path and for the
  (excluded-from-build) filesystem-routing path through the driver.
- **`fwdslash.exe`** — the CLI and the only component that mutates install state. The settings
  app never writes integration state itself; it shells out to this.
- **`fswsettings.exe`** — unpackaged WinUI 3 desktop app (Windows App SDK 1.8).

They find each other by well-known names in `include/fsw_user_protocol.h`: window class
`ForwardSlashWindows.Broker`, mutex `Local\ForwardSlashWindows.Broker`, and `WM_APP+10..12`
messages. There is no pipe or RPC.

`src/core/` holds the pure resolver (`path_resolver.cpp`) and WSL registry reads
(`wsl_registry.cpp`). **All resolution flows through `ResolveUserSlashPath`**, so a change
there reaches Explorer, Run, Search and both shell adapters at once. Bare-slash behaviour is
opt-in: in `default_distribution` mode a leading segment that is not a registered distribution
resolves against the default distro, so `/tmp/build` works unprefixed. A registered
distribution always wins over a same-named directory.

### Settings persistence

Runtime settings live under `HKCU\Software\ForwardSlashWindows\Settings` — `Disabled` (DWORD,
global pause), `BareSlashMode` (DWORD, 0 = distribution list / 1 = default distribution), and
`BareSlashDistribution` (string, the pin). The key path and value names are defined once in
`include/fsw_user_protocol.h` and shared by the core, broker and controller — **except**
`shell/powershell/ForwardSlashWindows.psm1`, which hardcodes the same key path as a literal
string (`Test-ForwardSlashWindowsDisabled`) because it can't include a C++ header. Renaming a
setting means updating the module too. Each optional integration also has its own
install-state marker, `HKCU\Software\ForwardSlashWindows\CmdAdapter` and
`...\PowerShellAdapter\WindowsPowerShell` / `...\PowerShellAdapter\PowerShell`, a `State` value
of `installed` — this is separate from the transactional snapshot (previous `AutoRun`/profile
bytes) and is only what `fwdslash integration <name> enable|disable` and
`fwdslash integrations` check for idempotency.

### Packaged vs unpackaged duality

The product ships both as a ZIP and as an MSIX, and behaves differently in each.
`fsw::HasPackageIdentity()` (`src/core/package_identity.cpp`) is the switch:

| Concern | Unpackaged | Packaged (MSIX) |
|---|---|---|
| Logon start | HKCU `...\CurrentVersion\Run`, written by `fwdslash install` | `windows.startupTask` in the manifest |
| `fwdslash://` protocol | HKCU `Software\Classes\fwdslash` | `windows.protocol` in the manifest |
| CLI on PATH | user adds the folder | `windows.appExecutionAlias` |

When adding anything that writes install state, branch on `HasPackageIdentity()`. Writing the
Run key or protocol registration from a packaged build leaves **orphaned entries pointing into
a deleted `WindowsApps` directory** — those locations are deliberately un-virtualized.

MSIX virtualizes HKCU and `%APPDATA%`, which would hide the adapter payload and the cmd
`AutoRun` value from unpackaged shells. `packaging/AppxManifest.xml` names three targeted
virtualization exclusions and declares the `unvirtualizedResources` restricted capability.
Do not widen those exclusions casually — the narrow scope is what makes Store approval
plausible.

The startup task only fires at **logon** and MSIX runs nothing at install time, so the settings
app calls `EnsureBrokerRunning()` on launch. Without it a Store install does nothing at all.

The settings app must be built with `-Packaged` for MSIX: otherwise the Windows App SDK
compiles in a bootstrap initializer that calls `exit()` when it finds package identity.
`WindowsPackageType=MSIX` is *not* usable — it makes the SDK demand an `AppxManifest` item on
the vcxproj; the gate is `WindowsAppSdkBootstrapInitialize=false`.

### Rust port

`crates/` holds a full Rust rewrite of the entire product, developed in parallel with the C++
tree described above and shipped alongside it since v0.0.1. Five crates map onto the three C++ binaries plus
the shared core:

| Crate | Binary / role | Mirrors |
|---|---|---|
| `fsw-path` | library, zero dependencies | `src/core/path_resolver.cpp` |
| `fsw-core` | library, registry reads + settings persistence + broker probes | `src/core/wsl_registry.cpp` + broker settings glue |
| `fsw-cli` | `fwdslash.exe` | `src/controller/` |
| `fsw-broker` | `fswbroker.exe` | `src/broker/main.cpp` |
| `fsw-settings` | `fswsettings.exe`, built on the vendored `windows-reactor` crate in `crates/windows-reactor/` | `src/settings/` (WinUI 3) |

`fsw-core` is `#[cfg(windows)]`-gated with non-Windows fallbacks (registry reads return empty,
writes no-op), which is why it type-checks on Linux even though it ultimately reads/writes HKCU.

**`fsw-settings` depends on `fsw-core`**, so the settings window reads state in-process the
way `RefreshState()` in `src/settings/main.cpp` does rather than parsing `fwdslash --json`.
That is sound across the version islands because `fsw-core`'s dependency closure contains no
`windows-core` at all — `windows-registry` 0.6 and `windows-sys` 0.61 both stop at
`windows-link`. The argument and the CI gate are in `docs/dependencies.md`; do not add a
crate to `fsw-core` that pulls `windows-core`.

**The version-island rule** (full detail in `docs/dependencies.md`) is the load-bearing
constraint on this workspace: `windows-reactor` requires `windows-core` 0.100, while everything
else uses the `windows`/`windows-sys` 0.62/0.61 generation, and the two `windows-core` majors are
incompatible COM/WinRT type systems that must never appear in the same binary. `fsw-path` and
`fsw-core` are therefore the only crates shared across both islands, which is only sound because
`fsw-path`'s `[dependencies]` table is intentionally empty — CI asserts `cargo tree -p fsw-path`
is exactly one line, and a PR that needs to add a dependency there must justify it in
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

`crates/fsw-settings/src/tray.rs` is the wfdiag `reactor-spike/window_support.rs` solution
ported onto `windows-sys`: the window is subclassed with `SetWindowSubclass` and that one file
owns tray icon, minimize/close-to-tray, and the Show/Exit menu. Window discovery enumerates
**current-process** windows only — a bare `FindWindowW(NULL, title)` matches another
instance's window and `SetWindowSubclass` fails cross-process. The app is also
single-instance (`activate_existing_instance` + `Local\ForwardSlashWindows.Settings` mutex):
a second launch raises the existing window. The C++ settings app has neither guard.

`docs/divergences.md` pins every place the Rust resolver is deliberately not byte-identical to
the C++ one (case-folding table, error-shape tightening, an unreachable error variant, malformed
distribution names, and the structural vs. textual bare-slash rewrite) — and, further down,
every settings-app divergence (caption icon route, pane display mode, Fluent-conformance
departures from the C++ such as 24px padding / 320 pane length / semantic secondary-text
brush, tray + single-instance behavior the C++ lacks), each backed by a named
test in `crates/fsw-path/tests/resolver.rs`. A difference not listed there is a bug, not a
feature. `docs/size-baseline.md` records the measured C++ binary sizes (e.g. `fswbroker.exe`
~161–173 KB of code) as the budget the Rust binaries have to clear, and the codegen policy
(`crt-static`, fat LTO, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"`) that makes a
comparable size plausible — `panic = "abort"` in particular is not negotiable, since unwinding
out of a `WH_KEYBOARD_LL` callback or a COM vtable is undefined behavior. The flags live in
`.cargo/config.toml` (per-target `rustflags`; never `[build] rustflags`) and the toolchain is
pinned by `rust-toolchain.toml` — install the pinned version on **both** the Windows host and
WSL, and re-measure `docs/size-baseline.md` whenever either changes. `cargo tree -p fsw-path`
counts dev-dependencies too, so `fsw-path` may not gain even a dev-dep: the shared test corpus
is `crates/fsw-path/tests/common/mod.rs`, the zero-allocation contract is `tests/allocations.rs`
(always on), and the timing smoke is `tests/perf.rs` (`--release -- --ignored`). Runtime
comparison against the C++ binaries is `tools/Measure-Runtime.ps1`, which builds the C++ side
into `out\user\<arch>\ReleaseCpp` (`Build-UserMode.ps1 -Configuration ReleaseCpp`) so it never
clobbers the Rust exes the MSIX loop stages into `...Release`; `tools/update-deps.py` is the
workspace-aware dependency updater and enforces the island pins listed in its `ISLAND_PINNED`.

### Dual-track distribution

Two package flavors ship from the same tree: the **Store** build (Identity
publisher `CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4`, family
`32827MikeFara.fwdslash_t6j5qexy2jpp2`, updated by the Store) and the
**GitHub** build (publisher `CN=Mike Fara, O=Mike Fara, …`, Trusted
Signing-signed by `.github/workflows/release.yml`, self-updating via
`crates/fsw-core/src/update.rs` — daily GitHub check, `Add-AppxPackage` of the
downloaded bundle, gated by `packaged && !is_store_flavor() && AutoUpdate`).
Flavor is always detected at runtime by package family (`is_store_flavor`),
never at build time. `signing/` holds the Azure Trusted Signing kit
(credentials live in GitHub Secrets); `tools/Install-fwdslash.ps1` refuses to
install the GitHub build over a Store install unless `-Force`.

### Shell adapters

`cmd` and PowerShell support are optional adapters installed natively by `fwdslash`
(`crates/fsw-cli/src/adapters/`; the retired `tools/Install-*.ps1` helpers are gone). They are
**transactional**: the previous `AutoRun` value and the original PowerShell profile bytes are
snapshotted before modification and restored byte-exact on removal, and uninstall refuses to touch
anything a third party has since changed. Preserve that property. `fwdslash uninstall` sweeps all
installed adapters before removing the Run key and protocol.

The cmd adapter works by `doskey` macros, so **it only takes effect in interactive consoles** —
`cmd /c "dir /etc"` will always fail with "Invalid switch". Test it in a real console window.

### Driver

`driver/fswfilter` is a production-gated kernel minifilter, excluded from every normal build
(only `Build.ps1 -Driver` touches it, gated again by `FSWDriverProject` in
`Directory.Build.props`). It must never enter a package. Per `SECURITY.md` it is only ever
loaded in a checkpointed VM. The only code the broker and driver share is the IPC contract in
`include/fsw_filter_protocol.h`: the broker connects to a Filter Manager port
(`FSW_FILTER_PORT_NAME`) and publishes a versioned distro-name mapping
(`PublishFilterMappings` in `src/broker/main.cpp`, resent on a 5s health timer and on any state
change) whether or not the driver is actually loaded.

## Conventions

- C++20, `/W4 /WX /permissive- /utf-8`. Native binaries use `/MT`; the MSBuild settings app
  uses `/MD`. Warnings are errors — a new warning fails the build.
- **Per-user only.** Everything is HKCU and `asInvoker`; there are no HKLM writes and nothing
  requires elevation. Keep it that way.
- Version `0.0.1` is hardcoded in `assets/fwdslash.rc`, `src/settings/app.manifest`,
  `CMakeLists.txt`, `tools/Package.ps1` and the adapter payload directory name. Changing it
  means changing all of them. The Rust port adds its own set of copies: root `Cargo.toml`
  (`workspace.package.version`, inherited by every crate), `crates/fsw-settings/app.manifest`,
  `crates/fsw-settings/app.rc`, `crates/fsw-broker/app.rc`, and `tools/package_msix.py`'s
  hardcoded `VERSION` constant.
- The icon resource id `IDI_FSW_APP = 101` also has copies: `include/fsw_resources.h`
  for the C++ tree, and `crates/fsw-broker/app.rc` plus a `const` in
  `crates/fsw-broker/src/main.rs` for the Rust broker. Each Rust binary links a
  different icon on purpose — none for the CLI, a 16-48px `assets/fwdslash-tray.ico`
  for the broker, the full `assets/fwdslash.ico` for the settings app. See
  `docs/size-baseline.md`.
- `docs/compatibility.md` lists release gates; blank entries mean unverified and must not be
  advertised as working.
- **Never log a path.** `Diagnostic()` in `src/broker/main.cpp` (opt-in only, via the
  `FSW_DIAGNOSTIC_LOG` env var) writes fixed event/reason category strings such as
  `event=route_distribution` — never the text the user typed or the UNC path it resolved to.
  This is a commitment made in `PRIVACY.md`; keep new diagnostic calls category-only.

### PowerShell gotchas in `tools/`

Two non-obvious failures have already been hit here:

- `$PSScriptRoot` is **empty while parameter defaults are evaluated** under `[CmdletBinding()]`.
  Resolve repo-relative defaults in the body, not in the `param()` block.
- Variable names are case-insensitive, so `foreach ($architecture in $Architecture)` over a
  `[string[]]` parameter re-wraps each value back into an array via the parameter's type
  constraint. Name loop variables distinctly.
- `Copy-Item -LiteralPath` does not expand wildcards; `-LiteralPath (Join-Path $dir '*')`
  silently copies nothing.
