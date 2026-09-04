# Compatibility and release gates

Blank or pending entries are unverified and must not be advertised as working.

| Surface | Mechanism | Current evidence |
|---|---|---|
| Resolver `/` | Shared C++ core | Automated: `\\wsl.localhost` |
| Resolver `/Distro/path` | Shared C++ core | Automated |
| Explorer current tab | UIA rewrite/replay | Automated on ARM64 host |
| Explorer bare `/` | COM provider-root navigation | Automated on ARM64 host |
| Win+R | UIA rewrite/replay | Manual gate required |
| Windows Search navigation | Direct shell open | Manual gate required |
| Classic Open/Save dialog | UIA rewrite/replay | Integration gate pending |
| Modern app-specific picker | Adapter or minifilter | Per-application gate |
| Win32 `CreateFileW` | Minifilter | VM integration gate pending |
| .NET `System.IO` | Minifilter | VM integration gate pending |
| PowerShell provider | Minifilter after normalization | VM gate pending |
| Python filesystem APIs | Minifilter | VM gate pending |
| `cmd.exe dir /` | Optional DOSKEY adapter | Installed adapter; manual new-session gate |
| Windows PowerShell `dir /`, `ls /Distro` | Optional profile adapter | Automated fresh-process host test |
| PowerShell 7 `dir /`, `ls /Distro` | Optional profile adapter | Automated fresh-process host test |
| WinUI 3 settings | Windows App SDK 1.8 | ARM64 host launch and responsiveness verified |
| Bare `/` in generic APIs | Windows drive-root semantics | Intentionally unsupported |
| Elevated desktop app | Per-user driver mapping | VM gate pending |
| Service/AppContainer/SYSTEM | Excluded by policy | Intentionally unsupported |

## User-mode release gate

- Warning-as-error Debug and Release builds for ARM64 and x64.
- Resolver tests, invalid path tests, and machine-readable controller output.
- Same-window Explorer tests for provider root, distro root, and nested path.
- Manual Win+R, Search, and common-dialog checks with confirmation that Edge is
  not launched for malformed or unknown slash paths.
- Sandbox install/start/pause/resume/stop/uninstall verification.
- Cmd adapter functional, native-switch pass-through, exact registry rollback,
  interrupted-transaction recovery, and external-AutoRun-change refusal tests.
- Each PowerShell profile adapter: fresh-process alias/path tests, byte-exact
  rollback, interrupted-transaction recovery, external-change refusal, and
  graceful Controlled Folder Access failure.
- No lost, duplicated, or delayed Enter behavior in unrelated Explorer views.

## Driver release gate

- Unsigned/test-signed installation only in a checkpointed Hyper-V guest.
- Alias-versus-UNC tests for create, read, write, enumerate, metadata, rename,
  delete, and long/Unicode paths.
- Standard and elevated callers across ARM64 native, x64 emulated, and x86
  emulated processes; native x64 coverage in an x64 VM.
- Two concurrent users and sessions with different WSL registrations.
- Broker disconnect/crash, logoff, WSL shutdown/restart, mapping refresh,
  malformed messages, allocation failures, sleep/resume, and unload under load.
- Driver Verifier with Special Pool, pool tracking, force IRQL checking, I/O
  verification, deadlock detection, security checks, and miscellaneous checks.
- Applicable HLK filter/filesystem playlists and Microsoft production signing.
- Verified transactional install, rollback, unload, driver-store removal, and
  collision warnings for every registered distro name on every mounted drive.

The driver is not included in normal packages until all driver gates pass.

## MSIX virtualization and the shell adapters (verified 2026-09-04, one ARM64
## host, Windows 11 26200; re-verify on new Windows builds)

Clean-room findings that shaped the design:

- A packaged process's **registry** writes to `HKCU\Software\...` land in the
  package's private hive — invisible to unpackaged shells. Proven: packaged
  `fwdslash disable` (exit 0) left the real `Settings\Disabled` value at `0`.
- A packaged process's **file** writes to `%LOCALAPPDATA%\ForwardSlashWindows`
  and `Documents` land in the real file system (those paths are not
  redirected for this package).
- A `powershell.exe` child spawned by the packaged app also writes real —
  which is why the retired PowerShell helper scripts appeared to work.

The adapters are therefore installed natively (`crates/fsw-cli/src/adapters/`)
with every registry write routed through `reg.exe` — a System32 child without
package identity, so its writes land in the real hive. Reads use the merged
view and are always correct. With this design the manifest needs **no**
virtualization exclusions and **no** `unvirtualizedResources` capability;
both were removed, removing the Microsoft-approval requirement for the Store
submission.

Verify on every new Windows build before a Store submission: packaged
`fwdslash disable` must flip the real `HKCU\...\Settings\Disabled` value
(read it from an unpackaged PowerShell), and a fresh packaged
`fwdslash integration cmd enable` must produce a real `Command Processor
AutoRun` value.
