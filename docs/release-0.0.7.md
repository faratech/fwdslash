# 0.0.7 candidate

The browser implementation is recovered from the working changes in
`C:\code\fwdslash-audit-20260905`, based on commit
`712f619f21cd5563713404196e9437a8e6104705` plus uncommitted changes.
The commit alone does not contain this implementation.

## Browser behavior

- `/mnt/c/...` resolves to a local Windows path without requiring the target to exist.
- Browser addresses use correctly escaped `file:` URIs, including spaces, Unicode,
  percent signs, and fragment characters in filenames.
- Stable input capture handles text still queued when Enter is intercepted.
- Replay tolerates Chromium replacing its UIA address control after a write while
  retaining the same browser, address profile, and translated value.
- Ordinary URLs, webpage fields, and password fields retain native behavior.
- Native surfaces handle translated paths themselves, including normal file
  execution. The broker has no shell-execution fallback for failed UIA writes.

## Update changes

The candidate includes strict release tags and newer-version checks, native
Windows download/deployment paths, precise package-verification failures, and
retained handles against replacement of verified downloads. It also centralizes
Windows child-tool resolution through `SystemBinary`.

## Evidence and release boundary

Historical saved runs show nine passing cases each for Edge (`a558338f...`),
Chrome (`39d6acda...`), and Brave (`c748d013...`). These checked committed URLs,
distinct loaded documents, bare-slash directory navigation, and webpage/password
decoys. They establish the recovery baseline, not verification of this candidate.

The recovered broker was restored on the user's Windows host and the user
confirmed that it works. The installed Store package remains 0.0.6.0.

Candidate validation completed so far:

- Windows ARM64: 185 core/updater tests passed; one interactive Task Scheduler
  test remains ignored. Logs: `out/0.0.7-validation/windows-tests.log` and
  `out/0.0.7-validation/core-extra-tests.log`.
- Windows x64: 13 restored path compatibility/contract tests passed.
- Bootstrap static contracts and embedded C# compilation passed. This does not
  prove execution of a downloaded, signed runtime installer.
- Edge and Chrome candidate runs passed native-file baseline, `/mnt/c` loaded
  document navigation, ordinary URL, and password-decoy cases.
- Brave candidate navigation remains unproven: additional keystrokes invalidated
  the test transaction before translation. Historical Brave passes and the
  user's recovery confirmation are not substitutes for candidate validation.

- Final formatting and full-workspace Clippy with warnings denied passed for
  ARM64 and x64. Both release builds passed.
- Final Windows ARM64 broker unit tests: 27 passed, three explicit GUI tests
  ignored. Build, lint, package and test logs are in `out/0.0.7-validation`.
- Unsigned bundle: `out/msix-store/fwdslash-0.0.7.0.msixbundle`, 12,965,627 bytes.
  SHA-256: `6eeee075b066dec837f8946909e368942dabc1aa7d80699145c739b50fd51345`.
  Bundle validation reports expected Store identity, publisher, version, both
  architectures, complete payloads and the full-trust capability.

Remaining candidate checks include bare-slash and webpage-decoy browser cases,
and a clean Brave run. Firefox support remains unverified. The low-integrity spoof harness
compiled but its live proof failed foreground setup, not the security assertion.

This is a candidate, not a completed release approval. No publication is
authorized by this document. The driver remains a separate lab-only component.

## Component currency check

The host has Rust 1.98.1 and stable Windows App Runtime 2.4.0 installed, including
ARM64 and x64 runtime packages. The candidate retains Windows SDK NuGet
10.0.28000.2526; the registry's current stable SDK is 10.0.28000.2705. This
read-only check did not upgrade the SDK or change the working host runtime.
