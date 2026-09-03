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

**A running broker or settings window will fail the link** — `link.exe` cannot overwrite a
loaded image. Before rebuilding:

```powershell
.\out\user\arm64\Release\fwdslash.exe stop
Get-Process fswsettings -ErrorAction SilentlyContinue | Stop-Process
```

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

`tools/Test-Sandbox.ps1` drives a Windows Sandbox install/start/pause/uninstall cycle;
`tools/Test-Prerequisites.ps1 [-RequireWdk]` checks the toolchain.

## Package

```powershell
.\tools\Package.ps1 -Architecture ARM64            # ZIP, the sideload SKU
.\tools\Package-Msix.ps1                           # x64 + ARM64 .msixbundle for the Store
```

See `docs/store-submission.md` for identity values and Store constraints.

## Architecture

Three user-mode binaries share one static core and cooperate at runtime:

- **`fswbroker.exe`** — resident tray + `HWND_MESSAGE` daemon. Installs a system-wide
  `WH_KEYBOARD_LL` hook that inspects **only `VK_RETURN`** and passes everything else through.
  On Enter, if the foreground window is a recognized surface, it reads the focused control's
  text via UI Automation, and if it starts with `/` rewrites it to the resolved UNC path and
  replays Enter (tagged with a private `dwExtraInfo` marker so the hook ignores its own input).
  Otherwise it replays the keystroke untouched. This swallow-inspect-rewrite-replay cycle is
  the heart of the product; read `ProcessEnterRequest` in `src/broker/main.cpp` first.
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

### Shell adapters

`cmd` and PowerShell support are optional adapters installed by `fwdslash` shelling out to
`tools/Install-*.ps1`. They are **transactional**: the previous `AutoRun` value and the original
PowerShell profile bytes are snapshotted before modification and restored byte-exact on removal,
and uninstall refuses to touch anything a third party has since changed. Preserve that property.

The cmd adapter works by `doskey` macros, so **it only takes effect in interactive consoles** —
`cmd /c "dir /etc"` will always fail with "Invalid switch". Test it in a real console window.

### Driver

`driver/fswfilter` is a production-gated kernel minifilter, excluded from every normal build
(only `Build.ps1 -Driver` touches it, gated again by `FSWDriverProject` in
`Directory.Build.props`). It must never enter a package. Per `SECURITY.md` it is only ever
loaded in a checkpointed VM.

## Conventions

- C++20, `/W4 /WX /permissive- /utf-8`. Native binaries use `/MT`; the MSBuild settings app
  uses `/MD`. Warnings are errors — a new warning fails the build.
- **Per-user only.** Everything is HKCU and `asInvoker`; there are no HKLM writes and nothing
  requires elevation. Keep it that way.
- Version `0.0.1` is hardcoded in `assets/fwdslash.rc`, `src/settings/app.manifest`,
  `CMakeLists.txt`, `tools/Package.ps1` and the adapter payload directory name. Changing it
  means changing all of them.
- `docs/compatibility.md` lists release gates; blank entries mean unverified and must not be
  advertised as working.

### PowerShell gotchas in `tools/`

Two non-obvious failures have already been hit here:

- `$PSScriptRoot` is **empty while parameter defaults are evaluated** under `[CmdletBinding()]`.
  Resolve repo-relative defaults in the body, not in the `param()` block.
- Variable names are case-insensitive, so `foreach ($architecture in $Architecture)` over a
  `[string[]]` parameter re-wraps each value back into an array via the parameter's type
  constraint. Name loop variables distinctly.
- `Copy-Item -LiteralPath` does not expand wildcards; `-LiteralPath (Join-Path $dir '*')`
  silently copies nothing.
