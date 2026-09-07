# Update test run: 0.0.6 → 0.0.7 via the Internal flight

Checklist for executing the 0.0.7 self-update test. Fill in results and fold the
outcome rows back into the matrix in `docs/compatibility.md` (~line 118).

Baseline facts:

- Candidate: `out/msix-store/fwdslash-0.0.7.0.msixbundle`
  SHA-256 `6eeee075b066dec837f8946909e368942dabc1aa7d80699145c739b50fd51345`
  (verified 2026-09-06 against `docs/release-0.0.7.md`).
- Flight: **Internal** in Partner Center — submit the bundle manually. Do **not**
  run `publish-to-store.yml`; it targets the public listing.
- Update flow under test: `fwdslash update check` →
  `StoreContext.GetAppAndOptionalStorePackageUpdatesAsync`
  (`crates/fsw-cli/src/update/store.rs:98`), install ladder
  app-install → store → winget → notify (`route_for`,
  `crates/fsw-cli/src/update/mod.rs:229`), staged helper + watchdog
  (`crates/fsw-cli/src/update/relaunch.rs`), result folded from
  `%LOCALAPPDATA%\ForwardSlashWindows\update\last-result.txt`.

## Findings (2026-09-06 flight test)

1. **Flight delivery Store→device works end to end.** Internal flight
   (0.0.7.0) published → the Store app delivered it onto a Store-signed
   0.0.6.0 install without any in-app updater involvement. Post-update state
   clean: `status` = `upToDate`, no `last-result.txt`, no leftover
   `AvailableUpdate` registry value.
2. **Shipped 0.0.6.0 cannot meaningfully self-update; do not route-test from
   it.** Evidence: with a Store-family 0.0.6.0 baseline installed, the shipped
   0.0.6 updater reports `state: available, available: "0.0.6.0"` — it accepts
   a **non-newer** candidate (the Store's signature-repair offer for a
   Developer-signed rebuild). The strict strictly-newer filtering present in
   the 0.0.7 working tree is not in the shipped 0.0.6 code. A route-1 run from
   this baseline would force-close, never see the version advance, and time
   the watchdog out. Migration of real 0.0.6 users rides **Store-driven
   delivery**, which works (finding 1) and needs no app-side code.
3. **The 0.0.7 updater (working tree) is verified against the real Store:**
   `flavor: store`, forced checks round-trip, `upToDate`/`available` states
   correct. The in-app update path is testable from 0.0.7.0 forward — first
   real matrix: 0.0.7.0 → next flight (0.0.7.1+).
4. **Packaging trap:** `tools/Package-Msix.ps1` defaults to
   `-BinarySource Cpp` and will silently stage stale committed binaries from
   `out\user\arm64\Release\` (help text with no `update`/`version` lines, exit
   2 on unknown commands). Always pass `-BinarySource Rust` and verify the
   staged exe hash matches the cargo build before signing. Consider making
   Rust the default or failing when the C++ tree is stale.
5. **Baseline recipes that work** (see Phase 1): Store-signed 0.0.6.0 via
   `winget` (only while the public listing serves it); locally-signed
   Store-identity builds via the v0.0.6 worktree + `-BinarySource Rust` +
   signtool with the `CN=ABDB6B3F…` cert (`6D0BD446…`, trusted). GitHub-release
   bundles are personal-cert signed (GitHub flavor) and unusable for Store
   flavor tests.

## Phase 1 — Baseline (0.0.6, Store-signed)

**Gotcha (found 2026-09-06):** the signed GitHub-release bundle
(`fwdslash-0.0.6.0.msixbundle` from release `v0.0.6`) is signed with the personal
cert — its package family is `32827MikeFara.fwdslash_twbfdd23yjahj`, **not** the
Store family `..._t6j5qexy2jpp2`. Installing it yields GitHub flavor, and its
update checks never see the Store flight. Do not use it as a Store-flavor
baseline. `-LocalBeta` builds and Store installs keep the Store publisher.

Working procedure:

- [x] Uninstall any existing package: `Get-AppxPackage *fwdslash* | Remove-AppxPackage`
- [x] Install the Store-signed build: `winget install --id 9P51CM0MTMK2 --source
      msstore --accept-package-agreements --accept-source-agreements` — this also
      establishes Store entitlement for the machine's Microsoft account.
- [x] Verify: `SignatureKind Store`, family `32827MikeFara.fwdslash_t6j5qexy2jpp2`,
      version `0.0.6.0` (public listing version — if the listing ever moves past
      the flight version, fall back to the self-signed full-identity recipe in
      `docs/store-submission.md` §1 at a version below the flight).
- [x] Machinery check: `update check --json --force` in package context →
      `flavor: store`, exit 0 (2026-09-06: `state: upToDate` while the flight was
      still in certification — expected; re-run once certification completes).

**Go/no-go:** after certification, re-run the forced check. `available` showing
the flight version proves entitlement + flight membership. If still `upToDate`
hours after certification passes, check Internal flight-group membership for this
MSA in Partner Center.

Working invocation (stdout is not relayed by `Invoke-CommandInDesktopPackage`;
redirect to a file, and note `-Args` takes a single string):

```powershell
Invoke-CommandInDesktopPackage -PackageFamilyName 32827MikeFara.fwdslash_t6j5qexy2jpp2 `
  -AppId App -Command C:\Windows\System32\cmd.exe `
  -Args '/c <repo>\target\aarch64-pc-windows-msvc\release\fwdslash.exe update check --json --force > <repo>\out\update-test-0.0.7\check.json 2>&1' `
  -PreventBreakaway
```

**Two drivers, two assertions** (both checks verified working 2026-09-06,
pre-offer, on the 0.0.6.0 Store baseline):

- **Packaged 0.0.6 exe** (the release-gating assertion — this is the code real
  0.0.6 users will run): `out/update-test-0.0.7/check06.cmd` wraps the exe at
  `C:\Program Files\WindowsApps\32827MikeFara.fwdslash_0.0.6.0_arm64__t6j5qexy2jpp2\fwdslash.exe`
  (path is version-pinned; it disappears after the update installs). Result file:
  `check-0.0.6.json`. Route tests must be **driven by the packaged exe of the
  installed version**, not the dev build, or you are testing the wrong
  updater/helper/watchdog code.
- **Dev 0.0.7 exe** (asserts the new update code against the real Store):
  `<repo>\target\aarch64-pc-windows-msvc\release\fwdslash.exe` inside the package
  context. Result file: `check.json`. Secondary — nice to know, not
  release-gating.

Note: `autoUpdate` is currently `false` on this machine, so the Store will not
auto-deliver the flight — route tests are driven manually via `update install`,
which is what we want.

## Phase 2 — Publish to the Internal flight (manual, Partner Center)

- [ ] Open Internal flight → add `out/msix-store/fwdslash-0.0.7.0.msixbundle`
      (hash verified above).
- [ ] Rollout 100%, complete submission, wait for certification and propagation
      (can take a few hours).

## Phase 3 — Update matrix

Reset the baseline between routes: uninstall, re-sideload 0.0.6.0.

- [ ] **Check:** `Invoke-CommandInDesktopPackage ... fwdslash.exe update check
      --json --force` → offered `0.0.7.0`; `AvailableUpdate` set under
      `HKCU\Software\ForwardSlashWindows\Settings`.
- [ ] **Route 1 app-install:** `update install --route app-install` → forced
      package close, helper task runs, watchdog waits for version > `0.0.6.0`,
      `last-result.txt` = `completed`, final version `0.0.7.0`.
      Result: ____________
- [ ] **Route 2 store:** `update install --route store` (requires
      `CanSilentlyDownloadStorePackageUpdates`). Result: ____________
- [ ] **Route 3 winget:** `update install --route winget`; also verify suppression
      on a metered network. Result: ____________
- [ ] **Route 4 notify:** `update install --route notify` → user told, nothing
      installed. Result: ____________
- [ ] **Precedence:** `--route` flag beats `UpdateRoute` registry override.
- [ ] **Result folding:** `update status` folds `last-result.txt` into registry.
- [ ] **Cadence:** without `--force`, a second check within 24h is skipped.

Capture exit codes (contract `0/10/11/12`) and `last-result.txt` for any failure.

## Phase 4 — Wrap-up

- [ ] Flip the corresponding Pending rows in `docs/compatibility.md`.
- [ ] Note the flight test result in `docs/release-0.0.7.md`.
- [ ] Commit doc updates.
