# Forward Slash Windows

Forward Slash Windows makes short WSL paths useful in native Windows
navigation surfaces while preserving a clear boundary around Windows path
semantics.

```text
/                         -> \\wsl.localhost
/Ubuntu                   -> \\wsl.localhost\Ubuntu
/Ubuntu/home/alice        -> \\wsl.localhost\Ubuntu\home\alice
```

Bare `/` opens the WSL distribution list. It never silently selects a default
distribution.

## Current implementation

The per-user C++ broker supports these driver-free surfaces:

- File Explorer address bar, in the current window/tab.
- Win+R.
- Windows Search path navigation.
- Classic Open/Save dialogs that expose a standard editable path control.

The broker rewrites a recognized edit control to the validated native UNC path
and lets that surface complete its normal Enter action. Windows Search is
handled as direct path navigation. Unknown distributions and malformed slash
paths are blocked with an explanation, so they cannot fall through to Edge or
a web search.

The optional filesystem minifilter is still production-gated. Once installed
and connected, it reserves registered distro names at every drive root for the
signed-in user. A normalized `C:\Ubuntu\etc` open is then reparsed to
`\\wsl.localhost\Ubuntu\etc`. This enables explicit `/Ubuntu/...` paths in
PowerShell, .NET, Python, and other desktop applications that reach Windows
filesystem APIs.

## Settings app

`fswsettings.exe` is an unpackaged WinUI 3 desktop app. It uses the Windows 11
Mica backdrop, an app-drawn title bar with native caption buttons, and a compact
NavigationView. Every integration is independent: turning one off runs its
reversible uninstall transaction, while the General **Disable** switch pauses
resolution without forgetting the selected integrations.

The tray menu opens the app directly at General, Windows, Command Prompt,
Windows PowerShell, or PowerShell 7 through the registered
`fwdslash://settings/...` protocol. Double-clicking the tray icon opens General.

The settings app requires the Microsoft Windows App Runtime 1.8 for the target
architecture. Build output contains the bootstrap DLL used to locate that
runtime; it does not install or service the runtime itself.

## Terminal behavior

Windows command interpreters parse input before filesystem APIs see it:

- `cmd.exe` treats `/` as an option prefix, so its built-in `dir /` fails before
  a driver could inspect a file open.
- Bare `/` passed to a filesystem API means the current drive root. Redirecting
  it globally would break Windows, so generic bare-root interception is not
  provided.
- Some third-party `ls.cmd` wrappers parse POSIX syntax themselves.

Use the safe terminal commands instead:

```powershell
fswctl list /
fswctl list /Ubuntu/etc
fswctl open /
fswctl open /Ubuntu/home
fswctl resolve /Ubuntu/etc
```

### Optional Command Prompt adapter

The opt-in adapter makes the simple interactive forms `dir /`, `ls /`,
`dir /Ubuntu/path`, and `ls /Ubuntu/path` work in newly opened Command Prompt
windows. Ordinary single-argument switches such as `dir /a` keep their native
meaning. Multi-argument slash aliases should use `fswctl list` in version
`0.0.1`.

```powershell
.\tools\Install-CmdAdapter.ps1 -ControllerPath .\out\user\arm64\Release\fswctl.exe
.\tools\Uninstall-CmdAdapter.ps1
```

Installation stages a private copy under `%LOCALAPPDATA%`, preserves the exact
prior `HKCU\Software\Microsoft\Command Processor\AutoRun` value and registry
type, and records a recovery state before activation. Uninstall restores that
value only when it still matches the installed transaction; if another program
changed it, uninstall refuses to overwrite the change. Already-open Command
Prompt windows retain their in-memory DOSKEY macros until closed.

### Optional PowerShell adapters

Windows PowerShell 5.1 and PowerShell 7 can be enabled separately in Settings,
or managed directly:

```powershell
fswctl integration windows-powershell enable
fswctl integration powershell enable
fswctl integration windows-powershell disable
fswctl integration powershell disable
```

Each adapter adds a transaction-marked import to that edition's current-user
all-hosts profile. `dir /` and `ls /` list WSL distributions, while explicit
paths such as `dir /Ubuntu/etc` resolve to their `\\wsl.localhost` location.
All other `Get-ChildItem` behavior is delegated to PowerShell's real command.
Install verifies the result in a fresh process of the selected edition before
reporting success. Existing terminal processes cannot reload a changed profile,
so close and reopen the affected shell after changing its toggle.

Profile writes are atomic and uninstall removes only the exact block recorded
by the install transaction. Controlled Folder Access or another security tool
may deny a profile write. That is a supported failure: the toggle returns to
off, partial state is cleaned up, and the app does not disable or weaken the
security control.

With the production-signed filter installed, explicit aliases such as
`Get-ChildItem /Ubuntu/etc` work when the caller passes the normalized path to
the filesystem. Programs that consume the argument as a switch remain outside
the compatibility guarantee.

## Build and run

On ARM64 Windows:

```powershell
.\tools\Build-UserMode.ps1 -Architecture ARM64 -Configuration Release
.\out\user\arm64\Release\fswcore_tests.exe
.\out\user\arm64\Release\fswctl.exe install
```

On x64 or 32-bit Windows, replace `ARM64` with `x64` or `x86`. The build uses
static C++ runtime linking for the broker/controller and produces no injected
DLL.

Useful controller commands:

```text
fswctl status [--json]
fswctl resolve /Distro/path
fswctl doctor /Distro/path | --all
fswctl open /Distro/path
fswctl list /Distro/path
fswctl settings
fswctl settings windows|cmd|windows-powershell|powershell|about
fswctl integrations [--json]
fswctl integration <windows|cmd|windows-powershell|powershell> enable|disable
fswctl pause | resume
fswctl driver status
fswctl start | stop
fswctl install | uninstall
```

The driver-free install is per-user and reversible:

```powershell
.\fswctl.exe uninstall
```

That stops the broker and removes its HKCU startup registration. No Explorer
restart is required.

## Driver development gate

The `driver/` directory contains the minifilter and `test/hyperv/` contains the
VM-only install/remove workflow. The filter:

- accepts authenticated mappings per user SID and interactive session;
- supports standard and elevated interactive desktop callers;
- excludes services, session zero, AppContainers, low-integrity callers,
  paging files, volume opens, and file-ID opens;
- fails open on internal errors and clears mappings on broker disconnect;
- never implements generic bare `/` redirection.

Registered distro names are intentionally reserved at drive roots while the
filter is enabled. For example, a real `C:\Ubuntu` conflicts with the WSL alias.
Production installation must detect and require confirmation for such
collisions.

Do not load the unsigned/test-signed driver on a physical workstation. Build
and test it only in a checkpointed Hyper-V guest:

```powershell
.\tools\Build-Driver.ps1 -Architecture ARM64 -Configuration Debug
```

Public generic-filesystem support requires Microsoft signing and the complete
Driver Verifier/HLK release gates described in
[`docs/compatibility.md`](docs/compatibility.md).

## Tests

- `fswcore_tests.exe`: resolver and invalid-path contract.
- `fsw_address_bar_integration.exe Ubuntu /usr/share`: literal Explorer input
  with same-window navigation verification.
- `fsw_address_bar_integration.exe Ubuntu --root`: bare `/` to the WSL list.
- `fsw_filesystem_integration.exe Ubuntu /etc/hosts`: alias-versus-UNC
  `CreateFileW` verification, run only in the driver VM.
- `tools/Test-Sandbox.ps1`: driver-free lifecycle and package smoke test.

See [`docs/architecture.md`](docs/architecture.md) for trust boundaries and
[`docs/compatibility.md`](docs/compatibility.md) for the evidence matrix.

## Project

Forward Slash Windows is open-source software by Mike Fara,
Fara Technologies LLC, New York, United States. Source is available at
[github.com/faratech/fwdslash](https://github.com/faratech/fwdslash) under the
[MIT License](LICENSE).
