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
- Execution-policy preflight: a Restricted or AllSigned edition refuses before
  any write and names the fix.

**PowerShell execution policy.** The adapters install a guarded block into
`profile.ps1`, so the edition's *effective* policy decides whether they do
anything at all. Windows PowerShell 5.1 ships **Restricted** on Windows client
editions, which is the default most users have.

| Effective policy | PowerShell adapter | 0.0.3 behaviour |
|---|---|---|
| `RemoteSigned`, `Unrestricted`, `Bypass` | Works | Installs and verifies normally |
| `Restricted` | Blocked | Refused **before any write** with the one-line fix |
| `Undefined` | Blocked (Windows PowerShell treats it as Restricted) | Same refusal, wording says so |
| `AllSigned` | Unsupported | Refused; the user's own `profile.ps1` would need a signature |
| Anything else | Treated as allowing scripts | Never refused on a parse failure; reported with a note |

The fix is `Set-ExecutionPolicy -Scope CurrentUser RemoteSigned`, run in the
edition being enabled — the policy is per edition, so fixing Windows PowerShell
from a `pwsh` window changes the wrong one. The preflight runs the edition's own
shell with `-NoProfile -NonInteractive -Command Get-ExecutionPolicy` and **no**
`-ExecutionPolicy` flag, so a process-scope `PSExecutionPolicyPreference` is
honoured exactly as the alias verification honours it; a shell that cannot be
started falls through to the pre-existing behaviour rather than adding a failure
mode. The same text reaches the settings InfoBar through the controller's
stderr, and `fwdslash doctor` / `fwdslash integrations` report each edition's
effective policy (`integrations --json` adds
`windowsPowerShellExecutionPolicy` / `PolicyBlocked` / `PolicyRemedy` and the
`powerShell7` equivalents without renaming any existing field).

**Signing `ForwardSlashWindows.psm1` does not change any of the above.**
`signtool` does sign `.psm1` through the PowerShell SIP — verified locally with
a self-signed certificate on the 0.0.3 dev host: `signtool sign /fd SHA256`
appended a `# SIG # Begin signature block` comment and `signtool verify /pa`
reported it valid — so `release.yml` signs the module with the Trusted Signing
kit before packaging, and the signature travels byte-for-byte into the MSIX
bundle, the ZIPs and the deployed copy. It is integrity only: under `Restricted`
nothing runs at all, and under `AllSigned` the user's own `profile.ps1` remains
unsigned. The Store bundle, which is built locally from the same repo tree,
carries the signed module only when it is built from a tree where that step ran;
an unsigned module there changes nothing functional.

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

## Automatic updates gate

The self-update path (`fwdslash update check|install|status`,
`crates/fsw-cli/src/update/`) ships in **0.0.5**, which is the first release
that carries it. A full end-to-end run therefore cannot be observed until a
*newer* version than the installed one is published: the rows below that need
one are marked pending, and the first observable Store update is a 0.0.5
install being offered 0.0.6. Blank or pending means unverified and must not be
advertised as working.

Store-identity rows are produced with

```powershell
Invoke-CommandInDesktopPackage -PackageFamilyName 32827MikeFara.fwdslash_t6j5qexy2jpp2 `
  -AppId App -Command <dev>\fwdslash.exe -Args 'update check --json'
```

— and a `cmd.exe` wrapper needs `-PreventBreakaway`, or the System32 child runs
without the identity (the same fact the adapters rely on for `reg.exe`).

| Gate | How | Evidence |
|---|---|---|
| `update check` under Store package identity reports the flavor and the true state, and never fails closed | `update check --json` through `Invoke-CommandInDesktopPackage` | ARM64 host, Windows 11 26200, Store package 0.0.4, 2026-09-05: `flavor` `store`, `state` `upToDate`, `autoUpdate` `false`, exit 0 — **PASS** |
| `update install` with nothing to install answers "nothing", not "deferred" | `update install --json` on a current install | Same host, 2026-09-05: exit **12**, `state` `upToDate`; answered before any route probe or moment gate — **PASS** |
| A helper-only verb refuses to run with package identity | `update apply-store` through `Invoke-CommandInDesktopPackage` | Same host, 2026-09-05: exit **20**, no task registered, no helper staged — **PASS** |
| The Store accepts `StartProductInstallWithOptionsAsync` from an identity-less process (route 1b is real) | `update apply-store --product 9P51CM0MTMK2` from a plain console | Same host, 2026-09-05: the call was accepted and completed as a no-op on the current version; `last-result.txt` = `completed`; the running app was not terminated — **PASS** |
| `update check` without package identity reports `disabled` and makes no network call | `update check --json` from an unpackaged build | Pending |
| Route 1 end to end: Store installs, package force-closed, version advanced, broker back through the alias, task and script gone, `last-result.txt` = `completed` | `update install --route appinstall` with a newer version published | Pending — needs a published version above the installed one |
| Route 2 (`StoreContext` silent), route 3 (`winget upgrade --source msstore`), route 4 (notify) | `update install --route store|winget|notify` | Pending — needs a published version above the installed one |
| A metered connection suppresses route 3, and the Store's `Paused*` states map to exit 10 rather than an error | Metered Wi-Fi profile, `update install --route winget` | Pending |
| Watchdog alone: `--previous 0.0.1` relaunches at once, `--previous 99.0.0` waits the ceiling and relaunches only when no broker is running; no task or script left behind; exactly one broker and one tray icon afterwards | `update install` with each `--previous`, then `schtasks /query /tn fwdslash-update` | Pending |
| GitHub flavor end to end through the same watchdog (download, `-ForceApplicationShutdown` from the helper, relaunch) | A bumped tag installed over an older GitHub build | Pending |
| Broker cadence: no check within 5 minutes of logon, none while the Enter worker is busy, at most one check a day | `FSW_DIAGNOSTIC_LOG` over a logon and an idle day | Pending |
| The Automatic updates switch is honoured in both flavors, and an explicit "off" recorded by an older build still reads as off | Settings toggle plus a hand-written `AutoUpdate` DWORD | Pending |

## Driver release gate

The gate is executed by `tools/Test-Driver.ps1`, inside the checkpointed
Hyper-V guest that `tools/New-DriverLabVm.ps1` creates and
`tools/Bootstrap-DriverLabGuest.ps1` prepares. Each row names the harness step
that produces its evidence. `docs/driver-lab.md` is the operator runbook.

| Gate | Harness step | Evidence |
|---|---|---|
| Unsigned/test-signed installation only in a checkpointed Hyper-V guest | a | ARM64 guest, Win 26200.9278, FakeShare, Verifier `0x93B`, 2026-09-05: PASS |
| Alias-versus-UNC parity for create, read, write, enumerate, metadata, long and Unicode paths | c | ARM64 guest, FakeShare, Verifier `0x93B`, 2026-09-05 (post-#39 fix, reproduced across two clean runs): PASS for create, read, write, enumerate, metadata, long paths (up to 276/289 characters), Unicode names, trailing-dot/trailing-space names, and PowerShell/.NET/Win32/cmd access — alias and UNC agree in every case |
| Delete through the alias | c | ARM64 guest, FakeShare, Verifier `0x93B`, 2026-09-05 (post-#40 fix): **PASS.** `Remove-Item` through `C:\Ubuntu` removes the file and the UNC path agrees. It had been recorded as a failure only because it shared a try block with the rename below; each mutation now has its own block and its own file |
| Rename, move and hard link through the alias | c | **FAIL — issue #49** (issue #40's specific defect is fixed). With `SL_OPEN_TARGET_DIRECTORY` creates now reparsed like any other create, the target-directory open resolves onto the redirected volume — `Rename-Item`'s `DirectoryNotFoundException` became a real filesystem status — but the operation still does not complete: rename returns `NTSTATUS 0xC0000022 STATUS_ACCESS_DENIED` (surfacing as `UnauthorizedAccessException`, `ERROR_ACCESS_DENIED` from `MoveFileExW`, `IOException` from `File.Move`) and hard link returns `0xC0000035 STATUS_OBJECT_NAME_COLLISION` (`ERROR_ALREADY_EXISTS` from `CreateHardLinkW`/`New-Item -ItemType HardLink`). **The UNC control passes**: the harness now runs the identical user-mode strategy (`NtSetInformationFile` with `FileRenameInformation`/`FileLinkInformation`, null `RootDirectory`, full NT target path — what `MoveFileEx` and `CreateHardLink` issue) against `\\wsl.localhost\Ubuntu\...` and gets `nt=0x00000000` for both, so the gap is the filter's reparse of that create, not the backing share. Use the UNC path for rename, move and hard link |
| Standard and elevated callers redirected; AppContainer and SYSTEM not | d | ARM64 guest, FakeShare, 2026-09-05: PASS for elevated, standard-integrity and SYSTEM (all three correctly redirected/not-redirected, each via a nested nested `schtasks` probe). AppContainer still pending: `Invoke-CommandInDesktopPackage` times out after 20 s when the harness itself runs inside the interactive `FswGate` scheduled task (nested-task-inside-a-task; a harness/launch-path limitation, not evidence about the driver) — re-run this one case from a plain elevated console to clear the cell |
| ARM64 native, x64 emulated and x86 emulated callers; native x64 in an x64 VM | c (re-run per lab) | VM gate pending; x64 lab not stood up |
| Two concurrent users and sessions with different WSL registrations | not automated | VM gate pending |
| Broker disconnect, crash, restart and mapping refresh | e | ARM64 guest, FakeShare, 2026-09-05: PASS — `fwdslash stop`/`start` round-trips the mapping, a killed broker clears its slot (disconnect, not a graceful clear), and the 17th sequential connection still succeeds (no slot leak) |
| Logoff, WSL shutdown/restart | not automated | VM gate pending |
| Malformed messages rejected without a bugcheck; slot accounting | e | ARM64 guest, FakeShare, Verifier `0x93B`, 2026-09-05: PASS — wrong input length, wrong protocol version, non-zero `Reserved`, `DistributionCount > 32`, and a backslash in a name are each rejected; the machine survives the whole set |
| Allocation failures | not automated (fail-open by design; see the Verifier note) | VM gate pending |
| Sleep/resume | manual, reported `[SKIPPED]` by step f | VM gate pending |
| Unload under load, then reload | f | ARM64 guest, Win 26200.9278, Verifier `0x93B`, 2026-09-05: PASS (unload during a create loop, no bugcheck, filter reloads) |
| Driver Verifier: Special Pool, pool tracking, force IRQL checking, I/O verification, deadlock detection, security checks, miscellaneous checks (mask `0x93B`) | a asserts it is active for the whole run | ARM64 guest, Win 26200.9278, 2026-09-05: PASS — active for a full run that reached teardown with no bugcheck |
| Create-rate cost on non-matching paths | g (informational, no threshold) | ARM64 guest, 2026-09-05, 20 000 iterations, latest clean run: loaded 65,548 opens/s vs unloaded 66,791 opens/s, −1.86 % (noisy on this guest across five runs so far, from −7.5 % to +16 %; informational only, no threshold) |
| Transactional install, unload and driver-store removal | a and h | ARM64 guest, Win 26200.9278, Verifier `0x93B`, 2026-09-05: PASS (`pnputil /add-driver /install`, `fltmc unload`, `pnputil /delete-driver oem1.inf /uninstall /force`) |
| Collision warning for every registered distro name on every mounted drive | not automated | VM gate pending |
| Applicable HLK filter/filesystem playlists and Microsoft production signing | out of scope of the lab | Deferred (Tier 3: altitude allocation, Partner Center, attestation signing) |

**Driver gate status 2026-09-05 (ARM64 guest, Win 26200.9278, FakeShare,
Driver Verifier `0x93B`).** Eight runs total, two kernel faults found and
fixed, one lab-plumbing defect found and fixed, one driver limitation found,
partly fixed and re-scoped. The harness runs the whole matrix through teardown
with the port connected on every run since the #39 fix. Latest:
`64 passed / 7 failed / 4 skipped`, no bugcheck — the failures are the
rename/move/hard-link family tracked by #49, and the count rose because the
run before it (`63 passed / 2 failed`) had only two of these rows at all.

- **Fixed (issue #36).** The guest bugchecked `0x0000003B`
  `SYSTEM_SERVICE_EXCEPTION` (param 1 `0xC0000005`) on the broker's first
  `\FswFilterPort` connect. `SeQueryInformationToken(token, TokenSessionId,
  ...)` returns the session id **by value** — the documented class table says
  the out-parameter receives "a **DWORD** value (not a pointer to it)" — and
  `FswQueryRequestorIdentity` dereferenced it as a pointer, so a session-0
  requestor faulted at address 0. The read now uses the scalar
  `SeQuerySessionIdToken`. Dump: `out/lab/crash/090426-4750-01.dmp`.

- **Fixed (issue #38), commit `1e97b24`.** With #36 fixed the guest bugchecked
  `0x000000C2` `BAD_POOL_CALLER` `(0x99, 0x3000, 0, 0)`. For arg1 `0x99`,
  **arg2 is the address being freed**, and `0x3000` is
  `SECURITY_MANDATORY_HIGH_RID` — so `ExFreePool()` was handed the integrity
  RID itself. Same systematic mistake as #36 seen from the free side:
  `TokenIntegrityLevel` also returns a DWORD by value, and
  `TokenIsAppContainer` is not a supported class at all (it returned
  `STATUS_INVALID_INFO_CLASS`, which short-circuited the `&&` chain and is why
  the fault was a bad free rather than a bad dereference). `TokenUser` is now
  the only class whose result is dereferenced or freed; the integrity RID is
  read as a value and range-checked fail-closed; the AppContainer query is
  gone, since an app container's integrity level is always low and the
  existing `>= SECURITY_MANDATORY_MEDIUM_RID` test already excludes it. Dump:
  `out/lab/crash/090426-4562-01.dmp`. Verified by rerun: no bugcheck, no
  `LastBootUpTime` jump, `[PASS] h. no unexpected-shutdown (Kernel-Power 41)
  event during the run`.

- **Fixed (issue #39) — was never a driver fault.** The prior 28-failure run
  had one cause: PowerShell Direct's `Invoke-Command -VMName` lands in the
  guest's **session 0**, so the detached `Test-Driver.ps1`, the CLI and
  `fswbroker.exe` all ran there, and `FswPortConnect` refuses session 0 by
  design — no mapping was ever published. Two independent fixes, both in the
  harness: (1) `tools/Test-Driver.ps1` now asserts its own session id is
  non-zero at preflight and fails fast with an explanation instead of
  producing a mass of failures that read like a driver regression; (2) the
  gate's in-guest launch now registers an **interactive scheduled task**
  (`schtasks /it /rl HIGHEST`, run as the console user) instead of a bare
  `Start-Process` from the PowerShell Direct session — verified session id 1
  and an elevated token before committing to the full run. A second,
  independent defect surfaced once the port actually connected:
  `-FakeShare` provisions the SMB share and shadow directory but never seeds
  `HKCU\...\Lxss`, so the broker had nothing to publish even with the launch
  fixed. `Test-Driver.ps1`'s `-FakeShare` preflight now seeds a synthetic
  Lxss registration (`DistributionName` = the distro, tagged with a private
  `FswLabFakeDistribution` marker) and removes it again in step h. A third,
  harness-only defect (a nested `schtasks /run` from inside the outer
  interactive task blocking indefinitely instead of returning) was found
  live and fixed: every native call in the identity-rules step now goes
  through a bounded (job + `Wait-Job -Timeout`) wrapper, and the `/st` value
  is computed in the future instead of a fixed `00:00`.

- **Fixed (issue #40), partly — remainder is issue #49.** `FswPreCreate` had
  `SL_OPEN_TARGET_DIRECTORY` in its fail-open list, so the create the I/O
  manager issues for a rename or hard-link target's **parent directory** was
  never reparsed: it resolved against the native `C:\<distro>` while the source
  file object lived on the redirected volume, and every rename through the
  alias failed with `DirectoryNotFoundException`. That exclusion is gone; the
  create now runs the ordinary pipeline (the name on it is the full target
  path, and the parent-open semantics live in the flag, which survives the
  reparse). It cannot cause a cross-volume rename — source and target pass the
  same first-component/identity/eligibility gate, so they redirect together,
  and a non-matching target correctly yields `STATUS_NOT_SAME_DEVICE` — and it
  cannot loop, because the replacement name lands on `\Device\Mup` and
  `FswInstanceSetup` attaches only to `FILE_DEVICE_DISK_FILE_SYSTEM` volumes
  (step a asserts exactly that). Verified: `Remove-Item` through the alias
  passes and the rename's error moved from path resolution to the filesystem.
  The operation still does not complete — `STATUS_ACCESS_DENIED` for rename,
  `STATUS_OBJECT_NAME_COLLISION` for hard link, while the new UNC control
  succeeds — which is **issue #49**.

- **New harness coverage for this family.** Every mutation row now has its own
  try block and its own file (a rename failure used to abort the sequence and
  report the delete as a failure that was never attempted), and rename/move/
  hard link are each measured through the UNC path first as a **control**,
  using `NtSetInformationFile` directly so the transcript carries the raw
  NTSTATUS instead of the Win32 error the shell APIs collapse it into. A
  control that fails skips its alias row with the reason instead of blaming
  the driver for a limitation of the backing filesystem.

Rows covered by steps that do not need a published mapping — a, f, g, h — were
already recorded from the 2026-09-05 post-#38 run and are unchanged. Steps b
through e now have real evidence for the first time.

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
