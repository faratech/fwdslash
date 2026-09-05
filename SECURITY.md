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
