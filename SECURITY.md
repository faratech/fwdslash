# Security policy

## Supported version

The project is pre-release. Only the current `main` branch and version `0.0.3`
receive security fixes.

## Reporting a vulnerability

Do not open a public issue for a vulnerability that could enable privilege
escalation, kernel compromise, unintended path redirection, or disclosure of
local data. Use GitHub's private vulnerability reporting feature for this
repository. Include the affected commit, Windows build and architecture,
reproduction steps, and whether the filesystem filter was loaded.

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
