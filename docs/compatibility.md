# Compatibility and release gates

Blank or pending entries are unverified and must not be advertised as working.

| Surface | Mechanism | Current evidence |
|---|---|---|
| Resolver `/` | Shared resolver (`fsw-path`) | Automated: `\\wsl.localhost` |
| Resolver `/Distro/path` | Shared resolver (`fsw-path`) | Automated |
| Explorer current tab | UIA rewrite/replay | Automated on ARM64 host |
| Explorer bare `/` | COM provider-root navigation | Automated on ARM64 host |
| Win+R | UIA rewrite/replay | Manual gate required |
| Windows Search navigation | Direct shell open | Manual gate required |
| Classic Open/Save dialog | UIA rewrite/replay | Re-verify: detection narrowed in 0.0.3 |
| Modern app-specific picker | Adapter or minifilter | Per-application gate |
| Win32 `CreateFileW` | Minifilter | VM integration gate pending |
| .NET `System.IO` | Minifilter | VM integration gate pending |
| PowerShell provider | Minifilter after normalization | VM gate pending |
| Python filesystem APIs | Minifilter | VM gate pending |
| `cmd.exe dir /` | Optional DOSKEY adapter | Installed adapter; manual new-session gate |
| `cmd.exe cd /Distro`, `chdir`, `pushd` | Optional DOSKEY adapter (enters via `pushd`) | New in 0.0.3; manual new-session gate |
| Windows PowerShell `dir /`, `ls /Distro` | Optional profile adapter | Automated fresh-process host test |
| PowerShell 7 `dir /`, `ls /Distro` | Optional profile adapter | Automated fresh-process host test |
| PowerShell `cd /Distro`, `chdir`/`sl`/`pushd` | Optional profile adapter | New in 0.0.3; manual fresh-session gate |
| Settings app | Windows App Runtime **2.x** (`Microsoft.WindowsAppRuntime.2`) | ARM64 host launch and responsiveness verified |
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

**Controlled Folder Access.** With CFA in Block mode the adapters' writes into
`Documents` are refused, and the block does **not** always arrive as
"access denied" — on the 0.0.3 dev host it surfaced as `ERROR_FILE_NOT_FOUND`.
`fwdslash integration <id> enable` reports the CFA guidance for either shape
(the failure rolls back cleanly, leaving no `prepared` marker), and the same
text reaches the settings InfoBar and the `fwdslash doctor` /
`fwdslash integrations` health lines. Two consequences to know about:

- Enabling a PowerShell adapter needs the executable that performs the write —
  the packaged `fwdslash.exe`, or the unpackaged one — allowed through
  *Allow an app through Controlled folder access*.
- **After an uninstall**, the deferred self-clean runs from the *staged*
  controller (`%LOCALAPPDATA%\ForwardSlashWindows\PowerShell\<version>\fwdslash.exe`),
  so it can only remove the profile blocks if that copy is allowed through CFA
  too. If it is not, the guarded block stays in place — silently and
  harmlessly, since it either imports a module that is still there or does
  nothing — and has to be removed by hand. Turning the integrations off before
  uninstalling avoids it entirely.
- No lost, duplicated, or delayed Enter behavior in unrelated Explorer views.
- One notification-area icon, owned by the broker; closing the settings window
  ends `fswsettings.exe` and leaves the icon in place.

**Open/Save detection, re-verify pending.** 0.0.3 narrowed the `#32770` rule:
a dialog now qualifies only if it hosts a `DUIViewWndClassName` child (the
modern common-item dialog) or the classic `cmb13`/`edt1` controls, **and** the
focused element is an editable, non-password Edit or ComboBox with a
non-read-only ValuePattern. That fixed Enter being swallowed in Find and
property dialogs, but it also means the Open/Save row above has to be
re-verified against real applications before it can be advertised: a picker
that satisfies neither test now passes Enter through untouched.

## Driver release gate

The gate is executed by `tools/Test-Driver.ps1`, inside the checkpointed
Hyper-V guest that `tools/New-DriverLabVm.ps1` creates and
`tools/Bootstrap-DriverLabGuest.ps1` prepares. Each row names the harness step
that produces its evidence. `docs/driver-lab.md` is the operator runbook.

| Gate | Harness step | Evidence |
|---|---|---|
| Unsigned/test-signed installation only in a checkpointed Hyper-V guest | a | VM gate pending |
| Alias-versus-UNC parity for create, read, write, enumerate, metadata, rename, delete, long and Unicode paths | c | VM gate pending |
| Standard and elevated callers redirected; AppContainer and SYSTEM not | d | VM gate pending |
| ARM64 native, x64 emulated and x86 emulated callers; native x64 in an x64 VM | c (re-run per lab) | VM gate pending; x64 lab not stood up |
| Two concurrent users and sessions with different WSL registrations | not automated | VM gate pending |
| Broker disconnect, crash, restart and mapping refresh | e | VM gate pending |
| Logoff, WSL shutdown/restart | not automated | VM gate pending |
| Malformed messages rejected without a bugcheck; slot accounting | e | VM gate pending |
| Allocation failures | not automated (fail-open by design; see the Verifier note) | VM gate pending |
| Sleep/resume | manual, reported `[SKIPPED]` by step f | VM gate pending |
| Unload under load, then reload | f | VM gate pending |
| Driver Verifier: Special Pool, pool tracking, force IRQL checking, I/O verification, deadlock detection, security checks, miscellaneous checks (mask `0x93B`) | a asserts it is active for the whole run | VM gate pending |
| Create-rate cost on non-matching paths | g (informational, no threshold) | VM gate pending |
| Transactional install, unload and driver-store removal | a and h | VM gate pending |
| Collision warning for every registered distro name on every mounted drive | not automated | VM gate pending |
| Applicable HLK filter/filesystem playlists and Microsoft production signing | out of scope of the lab | Deferred (Tier 3: altitude allocation, Partner Center, attestation signing) |

Verifier runs with mask `0x93B` and deliberately **without** low-resource
simulation (`0x0004`): the filter is fail-open on every allocation failure by
design, so randomized failures would turn a real bug into a silent pass.

A `[SKIPPED]` step is not evidence. A row stays "VM gate pending" until the
harness step that covers it passes in a run with Driver Verifier active, and a
`-FakeShare` run never clears a row on its own — it proves the reparse
mechanism, not WSL semantics (`docs/driver-lab.md`, "The FakeShare trick, and
its limits").

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
