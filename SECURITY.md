# Security policy

## Supported version

The project is pre-release. Only the current `main` branch and version `0.0.5`
receive security fixes.

## Reporting a vulnerability

Do not open a public issue for a vulnerability that could enable privilege
escalation, kernel compromise, unintended path redirection, or disclosure of
local data. Use GitHub's private vulnerability reporting feature for this
repository. Include the affected commit, Windows build and architecture,
reproduction steps, and whether the filesystem filter was loaded.

## Updates

Both flavors can install their own updates, under the **Automatic updates**
switch in Settings — on by default in the GitHub build, off by default in the
Store build.

**Where a payload can come from.** A Store install updates only through the
Microsoft Store: `AppInstallManager` or `StoreContext` for this product's own
Store id, or `winget upgrade --source msstore`, which is the same service. A
GitHub install updates only from a release of this repository, as an
`.msixbundle` signed through Azure Trusted Signing — `Add-AppxPackage` is what
verifies that signature, and refuses a bundle it does not trust. There is no
third origin, no plain-HTTP download, and no way to make either mechanism
install a different product: the Store product id is a constant in the binary
and the bundle URL comes from the release the check just read.

**The helper.** Installing an update requires a process *without* package
identity — the Store will not replace the package that is asking, and
`Add-AppxPackage` cannot register over the package it is running inside. So
`%LOCALAPPDATA%\ForwardSlashWindows\update\fwdslash-helper.exe` is a
byte-identical copy of the app's own `fwdslash.exe`, carrying the same
signature, created only when an install actually runs and removed by
`fwdslash uninstall`. It writes no registry value at all; it reports how the
install ended in `last-result.txt` beside it. Copying an executable into
`%LOCALAPPDATA%`, registering a scheduled task and driving an installer is a
shape antivirus heuristics do look at — if a scanner flags it, that is worth
reporting here.

**The scheduled task.** One per-user task named `fwdslash-update`, registered
with `schtasks` in the user's own context, never elevated and never in
`HKLM`; it deletes itself and its script when it finishes. Nothing in this
product requires administrator rights.

**Turning it off.**

- Switch **Automatic updates** off. Nothing is then checked or installed by the
  app in either flavor.
- Or set `UpdateRoute` (`REG_SZ`) under
  `HKCU\Software\ForwardSlashWindows\Settings` to `notify`: a **Store**
  install then only tells you an update exists and offers the Store listing,
  and installs nothing itself.
- `UpdateRoute=store` pins the sanctioned `StoreContext` path for a Store
  install, so the identity-less helper is never staged. That is the one-value
  answer if the `AppInstallManager` route is ever found objectionable — it needs
  no rebuild and no new release.
- The route override applies to the **Store** flavor only. A GitHub install has
  a single route (register the bundle it downloaded), so the switch above is how
  you stop it.

## Driver status

The filter under `driver/` is development code. It must not be installed on a
physical workstation or distributed as a production driver until it has passed
the documented Driver Verifier and HLK gates and has a Microsoft production
signature. Test-signed packages belong only in checkpointed virtual machines.

`docs/driver-lab.md` is the runbook for that machine: a Hyper-V Generation 2
guest with Secure Boot off and test signing on, created by
`tools/New-DriverLabVm.ps1`, prepared by `tools/Bootstrap-DriverLabGuest.ps1`
and driven by `tools/Test-Driver.ps1`, with a `clean-os` checkpoint restored
after every run. Windows Sandbox is not a substitute — it shares the host's
kernel image and boot policy and cannot enable test signing, reboot, run Driver
Verifier or load a kernel driver. The gate itself, and which of its rows are
still unverified, is in `docs/compatibility.md`.
