# Driver validation lab

The `fswfilter` minifilter is kernel code with no Microsoft signature. It is
loaded in exactly one place: a disposable, checkpointed Hyper-V guest with
Secure Boot off and test signing on. This is the runbook for building that
guest, running the release gate in it, and rolling it back afterwards.

Nothing here should ever be run on a physical workstation. Every script in the
lab set refuses to run on hardware that does not look like a virtual machine
unless `-Force` is passed, and every script's header says so.

## Why not Windows Sandbox

`tools/Test-Sandbox.ps1` exists and works, but it tests *user-mode* binaries.
Windows Sandbox cannot host this driver:

- **It shares the host's kernel image and boot configuration.** There is no
  separate BCD store to write, so `bcdedit /set testsigning on` has nothing to
  change for the container; code integrity policy comes from the host, which has
  Secure Boot on.
- **It cannot reboot.** Test signing, Driver Verifier and a driver service with
  `StartType = 3` all need a boot to take effect. Restarting a Sandbox discards
  it.
- **It cannot run Driver Verifier.** Verifier is a kernel feature configured
  per-machine and applied at boot; a container has neither.
- **It refuses kernel-driver installation.** `pnputil /add-driver` and
  `fltmc load` need a real kernel to load into.
- **A bugcheck in a Sandbox is a bugcheck on the host.** The whole point of the
  lab is to survive one.

A Hyper-V Generation 2 guest has its own kernel, its own BCD, its own Secure
Boot policy, and a checkpoint to restore after a crash.

## Why Secure Boot must be off in the guest

The boot loader refuses `bcdedit /set testsigning on` while Secure Boot is
enforcing — test signing is exactly the policy Secure Boot exists to prevent.
Without test signing, Windows will not load a driver that lacks a Microsoft
signature, and this driver does not have one. `tools/New-DriverLabVm.ps1`
therefore runs `Set-VMFirmware -EnableSecureBoot Off` at creation time.

That is also the reason the guest must stay disposable: a machine with Secure
Boot off and test signing on will load any unsigned kernel code presented to it.

## What "lab-only" means

- **No Microsoft signature.** The lab package is either unsigned or signed with
  a self-signed certificate created by `tools/Package-Driver.ps1 -Lab`. It loads
  only on a machine that has been deliberately weakened.
- **Never on a physical machine.** Not "not yet" — the gate in
  `docs/compatibility.md` is unfinished, and kernel code that misbehaves takes
  the machine with it.
- **The shipping product contains no driver.** The Store MSIX cannot carry one,
  and the GitHub release does not. Nothing a user installs puts a `.sys` on
  their machine.
- **The altitude is a placeholder.** `371120` is squatted in the FSFilter
  Activity Monitor range. A production altitude has to be allocated by
  Microsoft.
- **Signing kits are not interchangeable.** `signing/` holds the Azure Trusted
  Signing kit for the user-mode binaries and the MSIX. It cannot make kernel
  code loadable. `Package-Driver.ps1 -Production` refuses for that reason.

## The command sequence

### Host, elevated, once

```powershell
.\tools\New-DriverLabVm.ps1 -IsoPath D:\iso\Win11_ARM64.iso -ExposeVirtualization
```

There is no interactive Setup/OOBE step and `vmconnect` is never needed: the
script builds the VHDX directly (applies `install.wim`, runs `bcdboot`) and
stamps it with an unattend.xml before the VM ever boots, so first boot goes
straight to a ready desktop. The account it provisions (`fswlab` by default,
`-GuestAccountName` to change it), its Administrators membership, the local
account token-filter policy, and autologon are all set imperatively in the
unattend's **specialize** pass — the equivalent declarative elements
(`<UserAccounts>`/`<Group>`/`<AdministratorPassword>`/`<FirstLogonCommands>`
in `oobeSystem`) silently no-op on this DISM-apply + bcdboot boot path; the
script's own header comment has the full story. Once the VM is created and
started, the script itself polls PowerShell Direct until the unattended first
boot succeeds and then takes the `clean-os` checkpoint — there is no OOBE step
left for an operator to finish first. Credentials land in
`out\lab\guest-credentials.txt`.

`-DryRunUnattend` generates the same unattend.xml (with a real, freshly
generated password, so the quoting/escaping path is exercised for real),
validates that it parses, and writes it to `out\lab\preview-unattend.xml` —
without touching Hyper-V, DISM, any disk, or requiring elevation or
`-IsoPath`. Use it to audit the provisioning commands or as a smoke test after
editing the script.

`clean-os` is the baseline every run starts from.

### Host, per package

```powershell
.\tools\Build-Driver.ps1 -Architecture ARM64 -Configuration Release
.\tools\Package-Driver.ps1 -Architecture ARM64 -Lab
```

`-Lab` creates `out\driver\lab\fwdslash-lab.cer` once (with `makecert`, or
`New-SelfSignedCertificate` when the SDK no longer ships `makecert`), signs
`fswfilter.sys` and `fswfilter.cat` with `signtool /fd sha256`, and puts the
`.cer` in the zip. Add `-NoTimestamp` when the machine has no network.

Copy the package and the two guest scripts in. Guest Service Interface is
enabled by `New-DriverLabVm.ps1`, so no network share is needed:

```powershell
$vm = 'fswlab-arm64'
Copy-VMFile -Name $vm -SourcePath .\out\driver\arm64\fwdslash-filter-0.0.3.0-arm64.zip `
    -DestinationPath C:\FswLab\fwdslash-filter.zip -CreateFullPath -FileSource Host
Copy-VMFile -Name $vm -SourcePath .\tools\Bootstrap-DriverLabGuest.ps1 `
    -DestinationPath C:\FswLab\Bootstrap-DriverLabGuest.ps1 -CreateFullPath -FileSource Host
Copy-VMFile -Name $vm -SourcePath .\tools\Test-Driver.ps1 `
    -DestinationPath C:\FswLab\Test-Driver.ps1 -CreateFullPath -FileSource Host
```

The harness also wants `fwdslash.exe` (and the broker next to it) for steps b
and e; copy the user-mode build in the same way, or the run degrades those steps
to `[SKIPPED]`.

The lab `.cer` is inside the zip. Unpack it in the guest, or copy it separately.

### Guest, elevated, once per checkpoint restore

```powershell
C:\FswLab\Bootstrap-DriverLabGuest.ps1 `
    -CertificatePath C:\FswLab\fwdslash-lab.cer -FakeShare -Reboot
```

This turns on test signing, imports the certificate into `LocalMachine\Root` and
`LocalMachine\TrustedPublisher`, enables Driver Verifier for `fswfilter.sys`
with flag mask `0x93B`, and (with `-FakeShare`) stands up the loopback share.
Add `-InstallWsl` instead of `-FakeShare` when the guest has nested
virtualization, `-KernelDebug` to attach a debugger, `-NoVerifier` for a quick
smoke run that is not gate evidence.

The Verifier mask is the set `docs/compatibility.md` names:

| Bit | Check |
|---|---|
| `0x0001` | Special Pool |
| `0x0002` | Force IRQL Checking |
| `0x0008` | Pool Tracking |
| `0x0010` | I/O Verification |
| `0x0020` | Deadlock Detection |
| `0x0100` | Security Checks |
| `0x0800` | Miscellaneous Checks |

Low-resource simulation (`0x0004`) is deliberately left out: the filter is
fail-open on every allocation failure by design, so randomized failures would
turn a real bug into a silent pass. The failure paths are covered by the
malformed-message and unload-under-load cases instead.

After the reboot, confirm the reparse target before anything else:

```powershell
Test-Path \\wsl.localhost\Ubuntu     # must be True
```

If it is False the harness has nothing to redirect *to*, and every parity case
would fail for the wrong reason. `Test-Driver.ps1` stops at step a rather than
produce that noise.

### Guest, elevated, per run

```powershell
C:\FswLab\Test-Driver.ps1 -PackageZip C:\FswLab\fwdslash-filter.zip -FakeShare
```

Steps, in order, each line prefixed `[PASS]`, `[FAIL]` or `[SKIPPED]`:

| Step | What it proves |
|---|---|
| a | test signing on, UNC target reachable, `pnputil /add-driver /install`, `fltmc load`, altitude `371120`, instances on disk volumes only |
| b | `fwdslash start`, `driverConnected` true within 10 s, alias root resolves, ping reports the loaded protocol version |
| c | alias-versus-UNC parity for Test-Path, enumerate, read, write, create, delete, rename, metadata, long paths, Unicode and trailing-character names — through the PowerShell provider, .NET, Win32 `CreateFileW`, `cmd.exe` and python |
| d | standard and elevated callers redirected; AppContainer and SYSTEM not redirected |
| e | broker stop, crash and restart; malformed port messages rejected without a bugcheck; a closed connection frees its slot |
| f | `fltmc unload` under a create loop, then reload |
| g | create-rate benchmark on a non-matching path, filter loaded versus unloaded (informational, no threshold) |
| h | unload, `pnputil /delete-driver /uninstall /force`, summary |

Exit code 0 when nothing failed. `[SKIPPED]` does not fail the run, but a
skipped step is not evidence: a `docs/compatibility.md` row stays pending until
its step actually passes.

### Guest teardown

Restore the checkpoint. That is the only complete undo — step h removes the
driver from the driver store, but the test-signing flag, the trusted publisher,
the Verifier settings and the fake share all remain.

```powershell
Restore-VMSnapshot -VMName fswlab-arm64 -Name clean-os -Confirm:$false
```

## The FakeShare trick, and its limits

WSL2 inside a Hyper-V guest needs nested virtualization
(`Set-VMProcessor -ExposeVirtualizationExtensions`), which x64 hosts have
supported for years and ARM64 hosts support only on recent hardware and recent
Windows builds. When it is unavailable, `-FakeShare` gives the driver something
to reparse to without WSL:

- `127.0.0.1 wsl.localhost` in the hosts file, so the name resolves.
- `C:\FswLab\Ubuntu` populated with a small corpus — nested directories, a
  Unicode name, a path longer than 260 characters, and names with a trailing dot
  and a trailing space.
- `New-SmbShare -Name Ubuntu -Path C:\FswLab\Ubuntu -FullAccess Everyone`, so
  `\\wsl.localhost\Ubuntu` serves that directory.
- `HKLM\SYSTEM\CurrentControlSet\Services\LanmanServer\Parameters`
  `DisableStrictNameChecking = 1` (DWORD), so the server answers to a name that
  is neither its computer name nor a registered alias.
- `HKLM\SYSTEM\CurrentControlSet\Control\Lsa\MSV1_0`
  `BackConnectionHostNames = wsl.localhost` (REG_MULTI_SZ), so the loopback-check
  mitigation stops treating an alias-addressed loopback SMB connection as a
  reflection attack.

Both registry values are read at service start, so the reboot matters.

**What this proves:** the driver rewrites `C:\Ubuntu\...` to
`\\wsl.localhost\Ubuntu\...`, the reparse is accepted, the redirector serves the
result, and every Win32 file API sees the same object through both names. That
is the mechanism.

**What it does not prove:** anything about WSL. The fake share is NTFS behind
SMB. There is no 9P/plan9 server, no WSL metadata, no case-sensitive directory
behaviour, no Linux permissions, no symlink semantics, and no `\\wsl.localhost`
provider quirks. A green `-FakeShare` run is necessary but not sufficient — the
harness prints that in its summary, and the driver rows in
`docs/compatibility.md` stay pending until a real distribution passes the same
matrix.

## Two labs

This host is ARM64 and cannot run an x64 guest; Hyper-V guests run the host's
architecture. The x64 half of the gate needs a separate x64 machine (or a cloud
VM with nested virtualization enabled) running the same three scripts with an
x64 ISO and `-Architecture x64`. Record both results separately in
`docs/compatibility.md`; an ARM64 pass says nothing about x64 emulation paths.

## Deferred: Tier 3, production signing

None of this is started. It is the checklist for making the driver loadable on
a normal machine, and every item has external lead time.

- [ ] **Request a production altitude** from Microsoft for the FSFilter Activity
      Monitor group (or FSFilter Virtualization, if Microsoft prefers that group
      for a name-redirecting filter). Replace `371120` in
      `driver/fswfilter/fswfilter.inf` when it is allocated.
- [ ] **Register a Partner Center Hardware Program account** against the Azure
      Trusted Signing identity in `signing/`. Microsoft has accepted Trusted
      Signing in place of an EV certificate since 2024; verify at registration
      time, and fall back to an EV certificate if it is refused.
- [ ] **Build the submission cab** — `makecab` over `fswfilter.sys`, `.inf` and
      the `Inf2Cat` catalog, per architecture — and sign the cab with the
      Trusted Signing kit.
- [ ] **Submit for attestation signing** (Windows 10/11 client, x64 and ARM64).
      No HLK run is required for client SKUs.
- [ ] **Ship the bytes Microsoft returns.** Never rebuild after signing; the
      signature covers the exact binary.
- [ ] **Verify** with `signtool verify /kp /v fswfilter.sys` — the chain must
      show the Microsoft Windows Hardware Compatibility Publisher — then install
      on a stock Windows 11 machine with Secure Boot on and test signing off.
- [ ] **Distribution** stays GitHub-only and optional: the Store package cannot
      contain a kernel driver, and installing one needs administrator rights,
      the single documented exception to the product's per-user rule.

## Files

| Path | Runs on | Purpose |
|---|---|---|
| `tools/New-DriverLabVm.ps1` | host, elevated (`-DryRunUnattend`: unprivileged) | builds the VHDX unattended and creates the Gen 2 guest |
| `tools/Package-Driver.ps1` | host | zips and optionally test-signs the package |
| `tools/Bootstrap-DriverLabGuest.ps1` | guest, elevated | test signing, certificate, Verifier, fake share |
| `tools/Test-Driver.ps1` | guest, elevated | the release-gate harness |
| `docs/compatibility.md` | — | the gate this lab exists to satisfy |
