# Update test run: 0.1.0

Successor to `docs/update-test-0.0.7.md`. Records what the 0.1.0 self-update
work (issue #140, PR #141) can and cannot be verified against today, and the
results already obtained. Fold outcomes back into the matrix in
`docs/compatibility.md` (§ Automatic updates gate).

## Store channel state (2026-09-07)

- Installed on the dev host: **0.0.8.0**, `SignatureKind Store`, family
  `32827MikeFara.fwdslash_t6j5qexy2jpp2`.
- **0.0.9 was never published.** It was submitted at 05:11 and reached
  `Certification` at 05:17. The 0.1.0 publish dispatched at 05:53 found that
  same *pending* submission (`msstore` logs `Found Pending Submission`),
  committed 0.1.0 into it, and reached `Certification` at 06:00 with
  `fwdslash-0.1.0.0-store-unsigned.msixbundle` as the only package. 0.1.0
  superseded 0.0.9 in place rather than conflicting with it.
- **Consequence for testing:** the next Store delivery onto this host is
  0.0.8.0 → 0.1.0.0, not 0.0.9.

## The structural limit: which updater drives the test

The same trap as the 0.0.7 run (its finding §2). A route test exercises **the
updater inside the installed package**, not the working tree. So:

- The shipped **0.0.8** updater is the *pre-fix* code: route 1a polls the
  queued Store item in-process for up to 45 minutes while the caller waits.
  Driving `update install` from it reproduces the reported hang. That is
  useful as a witness for the bug, and proves nothing about the fix.
- The fix becomes observable only once **0.1.0 is installed and a newer
  version is offered** — i.e. 0.1.0 → 0.1.1, which needs a flight or a
  published bump.

Everything below is sorted by that constraint.

## Verified today, on 0.1.0 code, no newer version required

Driven with the release build at `target\aarch64-pc-windows-msvc\release`,
inside the package via `Invoke-CommandInDesktopPackage … -PreventBreakaway`.
The probe script is `out/update-debug/gc-probe.ps1` (not committed; it seeds
sidecars in `%LOCALAPPDATA%\Temp` and removes them again).

| Property | Method | Result |
|---|---|---|
| `update status` collects nothing | Seed a stale owned sidecar, a fresh owned one and a foreign one, run `update status --json` | All three survive — **PASS**. `status` is read-only again |
| `update check` collects only stale *owned* leftovers | Same three files, then `update check --json` | Stale owned removed; fresh owned and foreign both survive — **PASS**. Age rule and ownership grammar both hold outside the unit tests |
| No package identity reports `disabled` and makes no network call | `update check --json` / `update status --json` from a plain console | `flavor` `unpackaged`, `state` `disabled`, exit **0** for both — **PASS** (closes a row that had been pending since 0.0.5) |
| A helper-only verb refuses under package identity | `update apply-store --product 9P51CM0MTMK2` through `Invoke-CommandInDesktopPackage` | Refusal message, exit **20** — **PASS** (re-confirmed after the `Verb` changes) |

Note on measuring exit codes through the wrapper: `%errorlevel%` inside an
`&`-chained `cmd` line is expanded when the line is *parsed*, so it always
reads 0. Use `cmd /v:on` and `!errorlevel!`.

## Still pending — needs 0.1.0 installed plus a newer offer

Reset the baseline between routes: uninstall, install 0.1.0.0 from the Store,
publish 0.1.1 to a flight.

- [ ] **Queued hand-off (the headline fix).** `update install --force --json`
      from the settings window's own path returns within the three-minute
      admission window with exit 0 and `action: "queued"`; the window stays
      open with the background-install bar; the broker is restored rather than
      left closed. Result: ____________
- [ ] **Route 1 to completion.** The Store force-closes the package, the
      version advances, the watchdog brings the product back through the
      alias, task and sidecars gone, `last-result.txt` = `completed`.
      Result: ____________
- [ ] **Route 2 `store`, route 3 `winget`, route 4 `notify`** via `--route`.
      Result: ____________
- [ ] **Watchdog timeout relaunches the broker.** `--previous 99.0.0` so the
      version never appears to advance: after the ceiling the broker must be
      running again and `last-result.txt` must read `error:0x800705B4`.
      Shorten the ceiling or accept a 45-minute run. Result: ____________
- [ ] **Orphaned-lock reclaim.** Write an `update-attempt.lock` naming a task
      that does not exist, then `update install` — it must acquire rather than
      report that the watchdog could not be registered. Result: ____________
- [ ] **Scan-cooldown hold.** Two forced checks inside half an hour: the
      second must keep the offer and report the cache detail, not clear it.
      Result: ____________
- [ ] **Metered network suppresses route 3.** Result: ____________
- [ ] **Precedence:** `--route` beats the `UpdateRoute` registry override.
      Result: ____________

## Wrap-up

- [ ] Flip the corresponding rows in `docs/compatibility.md`.
- [ ] Record the outcome here and commit.
