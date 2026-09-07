# Behaviour notes and resolver rules

This is the behaviour specification for the shipping product — the Rust tree in
`crates/`. It records the resolver's rule set, the decisions each component
makes and why, and the guarantees other code and tests depend on. The resolver
entries are each pinned by a named test in `crates/fsw-path/tests/resolver.rs`;
the rest describe behaviour the broker, CLI and settings app are required to
keep.

Section names and numbers here are cited from source comments and tests. Keep
them stable.

## Resolver rules (R1-R12)

The rule numbers are cited throughout `crates/fsw-path/src/lib.rs`,
`crates/fsw-core/src/lib.rs` and the entries below, and this is where they are
defined. They describe the resolver contract, in the order the code applies
them (`resolve` in `crates/fsw-path/src/lib.rs`).

| Rule | Contract |
|---|---|
| **R1** | The input must begin with `/`. Empty or anything else is `NotASlashPath` — the CLI's "not a slash path" and the shells' exit 3. |
| **R2** | A second `/` at index 1 is `DoubleLeadingSlash`. Checked **before** R3, so `//\0` reports the double slash rather than the NUL. |
| **R3** | An embedded NUL anywhere is `EmbeddedNul`. |
| **R4** | A backslash anywhere is `BackslashNotAllowed`. This is what lets R10 derive the Linux path from the rendered UNC by swapping separators. |
| **R5** | A bare `/` is the provider root (`Resolved::WslRoot`, rendered `\\wsl.localhost`) — but only in distribution-list mode with no custom folder root; see R7-R9. |
| **R6** | A trailing `/` on an input longer than one character is *captured* as `had_trailing_separator`; whether it survives is R12. In default-distribution mode it is captured from the rewritten string, which is why bare `/` reports `true` there (Settings-independent; see Resolver §5). |
| **R7** | The leading segment is everything from index 1 up to the next `/`, or to the end. |
| **R8** | If that segment is a **registered** distribution (case-insensitively, per Resolver §1), the input is an explicit distribution path and resolves against it — **but only in distribution-list mode with no configured folder root** (R9). Where the default distribution or a chosen folder root owns `/`, the first segment is filesystem content of that root: a folder named like any installed distribution resolves under the root instead of being shadowed by it (2026-09-06). |
| **R9** | Otherwise the bare-slash mode decides: *distribution list* → bare `/` is R5 and anything else is `UnregisteredDistribution`; *default distribution* → the pinned distribution, else the WSL default, else `NoDefaultDistribution`. A configured folder root pre-empts both (Resolver §6). |
| **R10** | Components are normalized during the render: an empty component and `.` are dropped, `..` truncates back to the previous separator. No component vector is built. |
| **R11** | A `..` that would leave the distribution (or folder) root is `TraversalAboveRoot`. Traversal *to* exactly the root is allowed. |
| **R12** | The captured trailing separator (R6) is re-appended only if at least one component survived R10. `/Ubuntu/` keeps it, `/Ubuntu/.` does not, and bare `/` in default mode does not. |

Rules R1-R4 and R10-R12 apply unchanged to a custom folder root; R5 and R7-R9
are where a folder root differs (Resolver §6).

## Resolver (`fsw-path`)

### 1. Case folding uses Rust's Unicode tables, not `CompareStringOrdinal`

The resolver folds case in pure Rust so the crate stays dependency-free and
Linux CI exercises the *shipping* comparison rather than a stand-in. The
behaviour it has to agree with is Win32's `CompareStringOrdinal(..,
bIgnoreCase = TRUE)`, because that is what the shell applies to the same names.

`CompareStringOrdinal` folds through the **simple** uppercase table, which is 1:1
and never changes a string's length. Rust's `char::to_uppercase` is the **full**
mapping and expands some characters (`ß` → `SS`, `ﬁ` → `FI`). `eq_ignore_case`
therefore takes only the single-character mappings — see `simple_upper` — which
reproduces the simple table.

Verified to **agree** with Win32:

| Pair | Result | Why |
|---|---|---|
| `İ` (U+0130) vs `i` | not equal | U+0130 has no simple mapping to `i` |
| `ı` (U+0131) vs `I` | equal | U+0131 simple-uppercases to `I` |
| `ß` vs `SS` | not equal | the expansion is suppressed |
| `ﬁ` (U+FB01) vs `FI` | not equal | ditto |
| `ünicode` vs `ÜNICODE` | equal | ordinary 1:1 cased mapping |

**What can still disagree:** the Unicode version. Rust's tables are pinned by the
toolchain; Win32's are pinned by the user's Windows build. Two characters added
or recased between those versions can disagree. WSL distribution names are
overwhelmingly ASCII (`Ubuntu`, `Debian`, `kali-linux`, `openSUSE-Tumbleweed`),
and the ASCII fast path is exact, so the exposure is theoretical.

**Non-BMP characters.** `simple_upper` folds `char`s — Unicode scalar values —
while `CompareStringOrdinal` folds UTF-16 code *units*. For anything above
U+FFFF the two are structurally different operations: Win32 sees a surrogate
pair and case-folds neither half (no surrogate code unit has a simple uppercase
mapping), whereas Rust sees one scalar and applies its mapping if the plane
has one — Deseret (U+10428 `𐐨` → U+10400 `𐐀`) and Adlam are the live examples.
A distribution name containing one of those characters compares
case-insensitively here and case-sensitively in Win32. No WSL distribution
name is plausibly in that set, so this is recorded rather than "fixed".

**Follow-up:** a Windows-only differential test walking the BMP against
`CompareStringOrdinal` is the honest way to keep this table current. It cannot
run on Linux CI, so it belongs in the Windows leg. It would not cover the
non-BMP case above; that needs its own surrogate-pair cases.

Pinned by `case_folding_matches_the_win32_simple_uppercase_table`.

### 2. Failure returns `Err`, never a struct with partial state

`resolve` returns `Result<Resolved, ResolveError>`. There is no success-shaped
value carrying an error field, so no caller can read a half-populated
`distribution`, `target` or `had_trailing_separator` off a failed resolve. A
failure carries the reason and nothing else.

### 3. `ResolveError::MissingDistribution` is unreachable, and always was

An empty distribution segment requires either `input == "/"` (returned earlier as
the provider root) or `input[1] == '/'` (rejected earlier as
`DoubleLeadingSlash`). The variant is retained because its name is a diagnostics
wire value (`reason=missing_distribution`), and a `debug_assert!` documents the
unreachability at the one call site.

### 4. Malformed distribution names are dropped, not half-registered

`is_valid_distribution_name` rejects names that are empty, `.`, `..`, or contain
`/`, `\`, `:` or a code unit below U+0020. Registering whatever the registry
holds would produce an unusable UNC — `\\wsl.localhost\a:b` — which the
redirector fails opaquely.

Dropping them at cache-build time is what lets the bare-slash rewrite pass the
distribution out-of-band instead of concatenating and re-parsing (a name
containing `/` would turn the concatenation into a double leading slash), and it
keeps the resolver in agreement with the minifilter's
`FswIsValidDistributionName`.

The driver's 127-UTF-16-unit cap is deliberately **not** applied here: enforcing
it would stop routing a long-named distribution that works today. Truncation
stays where it already is, in the filter-message builder.

### 5. The bare-slash rewrite is structural, not textual

In default-distribution mode the obvious implementation is to build
`"/" + target + input` and re-parse it. The resolver instead passes the
distribution out-of-band and scans `input` from index 1, which removes one
allocation and one full re-parse per rewritten keystroke — and one repeated call
to the `is_registered` predicate, which the re-parse would trigger.

This is an optimization, not a behaviour change, and it is not asserted on
faith: `rewrite_equivalence` runs the concatenate-and-reparse reference
implementation and the direct path over a 912-case corpus and requires byte
equality.

The one visible consequence is `had_trailing_separator` for `input == "/"` in
default-distribution mode. It is defined as the value the *rewritten* string
would produce (`"/Ubuntu/"`, so `true`), and the direct path reproduces that
rather than computing it from the input. Rule R12 discards it either way because
no component survives, so `unc_display` and `linux_path` are unaffected. Pinned
by `bare_slash_in_default_mode_reports_a_trailing_separator`.

### 6. The custom bare-slash root

`fwdslash bare-slash root <path>` stores an absolute Windows path (`C:\code`,
`\\wsl.localhost\Ubuntu\home\mike`) in a `BareSlashRoot` REG_SZ under the
settings key, and the funnel in `fsw-core::resolve_user_slash_path` then routes
every input to `fsw_path::resolve_under_root`: a bare `/` opens the root, `/foo`
resolves to root\foo, `..` clamps at the root (`TraversalAboveRoot`), in
**either** bare-slash mode. Since 2026-09-06 registered-distribution inputs
resolve under the root like everything else — the root owns `/` completely, so a
folder that shares its name with an installed distribution is reachable, and
cross-distro access is the full `\\wsl.localhost\Distro\path` spelling. No new
`ResolveError` variant exists.

Deliberate details:
- `BareSlashRoot` is **not** a third `BareSlashMode` value. Any nonzero
  `BareSlashMode` DWORD reads as "default distribution"
  (`fsw-core/src/lib.rs`), so a mode value would make an older build disagree
  about what `/` means. With a separate value an older build ignores it and
  keeps the previous behaviour — the same thing that happens when the value is
  corrupt, because the funnel re-validates it on every resolve
  (`is_valid_windows_root`).
- The Explorer message-only special case keys on `Resolved::is_provider_root()`,
  so a folder `/` goes through the ordinary set-focused-value path.
- `ResolveError::message()` for `TraversalAboveRoot` says "distribution root",
  which reads slightly wrong under a folder root. Generalizing the sentence is a
  wire-text change; accepted for now.
- `ForwardSlashWindows.psm1` does not compare rendered paths at all. It calls
  `fwdslash shell-resolve`, which reports `kind` (`root` / `distribution` /
  `folder` / `native`) structurally, so a custom root that itself lives under
  `\\wsl.localhost` can never be misread as the provider root.
- `is_valid_windows_root` accepts only *absolute* locations, and 0.0.3 tightened
  it: a drive-**relative** root (`C:code`, `C:Users\me`) is rejected, because
  Win32 resolves it against a hidden per-drive current directory rather than a
  fixed folder; a share-less UNC root (`\\server`, `\\server\`,
  `\\server\\share`) is rejected, because it names no folder; wildcards (`*`,
  `?`) are rejected; and `\\wsl.localhost` itself is rejected in any casing and
  with any number of trailing separators, so `Resolved::unc_display` can only
  ever produce that literal for `Resolved::WslRoot`. `C:` and `C:\` stay valid.
  Pinned by `windows_root_validation_table`.
- `has_win32_normalization_hazard` inspects the **last** component only. Win32
  strips a trailing `.` or space outside the `\\?\` namespace, but only at the
  end of the string — a `.` or space followed by a separator survives, so a
  middle component and a path written with a trailing separator are not hazards,
  and `.`/`..` are normalized away by R10 rather than truncated. It is not
  computed and discarded: the broker appends a trailing `\` when it is true, and
  logs `event=win32_normalization_hazard`. `FolderPath` exposes the same
  accessor, because a folder root can sit on ext4 too.

Named tests: `folder_root_*` and `windows_root_validation_table`
(`crates/fsw-path/tests/resolver.rs`), `folder_root_resolution_allocates_nothing`
(`tests/allocations.rs`), and the `bare_slash_root_*` funnel suite
(`crates/fsw-core/tests/bare_slash_root.rs`).

## Dual-track distribution + self-update

**Both flavors update themselves, under one switch.** The GitHub-distributed
build (Trusted Signing publisher, different package family from the Store
listing) checks `api.github.com/releases/latest`; the Store build asks the Store.
The gate is `fsw_core::update::update_check_allowed(packaged, auto_update)` —
two arguments, no flavor — and the flavor decides only the switch's **default**:
`default_auto_update(store_flavor) = !store_flavor`, so `AutoUpdate` absent means
on for the GitHub build and off for the Store build. The stored encoding is an
inverted DWORD (`1` = auto-update off), so an explicit "off" recorded by an
older build still reads as off. Cadence is `CHECK_CADENCE_SECS` = 24 h, bypassed
by `--force`.

**All of it lives in the CLI**, `crates/fsw-cli/src/update/`, as
`fwdslash update check|install|status` plus two helper-only verbs
(`apply-store`, `apply-bundle`) that are absent from `usage()` and exit **20**
when run with package identity. The broker and the settings window own no update
logic: they run `fwdslash update` and read its exit code — `0` up to date or
install started, `10` update available or deferred, `11` needs the user, `12`
nothing to install, `1` error, `2` usage, `20` wrong execution context — or its
one hand-rolled JSON line
(`{"flavor","state","available","autoUpdate","lastUpdateCheck","route","action","detail"}`,
golden-tested; there is no serde in this workspace). COM is initialised in this
module and nowhere else in the CLI, so the `cd /` hot path pays nothing for it.

**What "an update is available" means (issues #90 and #97).** The presence of an
offer and its version are separate facts, and only the first is authoritative.
`StoreUpdatePending` records that the last successful Store query listed a
pending update for this package; `AvailableUpdate` records a version *only* when
one passed the strictly-newer filter. `fsw_core::update::offer_from_state` turns
the pair into an `Offer::Named(tag)` or `Offer::Unnamed`, and everything reads
through it — a label that is no longer newer than the running version is spent
and yields no offer at all, which is how a notice left by an older build stops
being shown without a migration. `store_offer_from_entries` does the same job for
a live query, and is deliberately agnostic about whether
`StorePackageUpdate.Package.Id.Version` names the catalog's version or echoes the
installed one: under the first reading it yields `Named`, under the second
`Unnamed`, and under neither does it advertise the installed version as a target.

An unnamed offer is fully installable — every route installs by product id, not
by version — but bounded. It may be a same-version repair offer that will never
advance the version, and each attempt costs a force-close plus a watchdog
timeout, so `unnamed_offer_actionable` gives it one attempt per
`UNNAMED_RETRY_BACKOFF_SECS` (24 h) rather than one per cycle. A named offer is
never subject to that: its version is the proof that something will change.

`install`'s whole gate is the pure `install_answer`, whose third arm is issue
#90: a Store query that **failed** is not a Store that answered "nothing". It
reports `needsUser` / exit 11 and starts no installer. Not exit 12, which would
claim there is nothing to install; and not exit 0, which the settings window
reads as "about to be force-closed" — it would show no message and leave the
broker down. On the wire an unnamed offer is the already-legal shape `state:
"available"` with `available: null`, so no JSON field and no exit code changed.

**The install ladder (Store flavor).** `route_for` is a pure function of five
inputs and the single definition of precedence; the probes below it are lazy, so
a rung is only asked about once the rung above is out.

| # | Route | Precondition | Runs in | Terminates the app |
|---|---|---|---|---|
| 1a | `AppInstallManager.StartProductInstallWithOptionsAsync` (winget's own sequence: `AllowForcedAppRestart`, both toast modes `NoToast`), watched for at most `ADMISSION_WINDOW` (3 min) | **last rung**, after both sanctioned routes decline (issue #98) | the packaged CLI, in-process | yes, by the Store |
| 1b | the same call from the staged helper | 1a failed before an item was queued (`E_ACCESSDENIED` above all) | the identity-less helper, from the scheduled task | yes |
| 2 | `StoreContext` silent download + install | route 1 unavailable and `CanSilentlyDownloadStorePackageUpdates` | the packaged CLI | yes, when deployment lands |
| 3 | `winget upgrade --id … --source msstore --silent --force` | winget present and the network unmetered | the scheduled task | yes |
| 4 | notify | otherwise | the packaged CLI | no (exit 11) |

Two orderings inside `install` are load-bearing and each fixed a shipped bug:
*nothing to install* (exit 12) is answered **before** the moment gate, because
exit 10 promises there is something to come back for; and availability outranks
`--route`, because a forced route says how to install, never whether there is
anything to. `install_moment_ok(forced, settings_window_open, worker_busy)` is
the moment gate — an explicit request always wins, otherwise an open settings
window or a busy Enter worker defers. Only the broker knows `worker_busy`, so it
gates before it invokes the CLI at all. Route 1's phase-1a call exists because
`AppInstallManager` activates and answers queries *inside* the package; whether
the install itself is allowed there is only knowable at runtime, so it is tried
and the identity-less path is the fallback, not the default.

**The routes are now genuine fallbacks for each other, and the private API is
last (issue #98).** `auto_ladder` returns *every* rung an unforced install may
try, in order, and `install_via_ladder` walks it: a rung that declines before
queueing anything falls through to the next. `Rung::Declined` is the only
outcome that licenses continuing — once a rung has queued work, deployment may
already be under way and a second installer would race it, so everything else
stops the walk.

The order is sanctioned APIs first. `StoreContext` is the documented way for an
app to install its own Store update, `winget` is the same service again, and
`AppInstallManager` — which Microsoft documents as gated by a private
capability restricted to its own apps — is the rung before giving up rather
than the default. It is kept because a user who turned automatic updates on
would rather have the update than a notification.

Before this, route 1 was *first* and its probe was
`has_package_identity() || helper_path().is_some()`, true on every real
install, so it was always selected and neither sanctioned rung was ever
evaluated once in the product's life. `route_for` is gone: `auto_ladder`
supersedes it and says strictly more, since the ladder is the whole precedence
rather than only its head. A forced `--route` or `UpdateRoute` still runs
exactly one rung with no failover, which is what makes the escape hatch useful
for diagnosis.

**The hand-off bound belongs to every route that can block, not just route 1
(issue #140).** `WaitPolicy`, `Verdict` and `verdict` live in the update module
root and both Store routes use them. This matters because route 2 carried the
identical defect: `silent_download_and_install` blocked up to 45 minutes on
`TrySilentDownloadAndInstallStorePackageUpdatesAsync` from the packaged CLI,
with the settings window or the broker waiting on it, so promoting route 2 to
the default without bounding it would have moved the hang rather than fixed it.
Route 2 has no pollable progress signal — progress on an
`IAsyncOperationWithProgress` arrives through a handler, and `GetResults` on a
still-`Started` operation is invalid — so its foreground wait is bounded by the
admission window alone, and it reports the same `installing` / exit 0 /
`action: "queued"` on hand-off.

**Route 1 hands off instead of waiting (issue #140).** The packaged CLI is a
child of the settings window or the broker, so it never polls a queued Store
item to a conclusion. `appinstall::WaitPolicy::Foreground` leaves as soon as an
item shows progress (a state past the download's start, a byte, a percent) or
when the three-minute admission window runs out with the item still waiting
its turn, and reports `installing`, exit 0, `action: "queued"`. The Store
owns the item from there and the watchdog owns the comeback. The helper
(`apply-store`, from the task, nobody waiting) keeps the 45-minute
`Background` ceiling and the `paused` verdict. Before queueing, the Store's
own `AppInstallItems` queue is reconciled: a live item for this product is
adopted (also `queued`), a terminal leftover is cancelled so it cannot shadow
the new request. The settings window and the broker bound the child (10 min)
and kill it past that; a killed child leaves its queued item and its watchdog
standing. On `queued` the window stays open with an informational bar, brings
the broker back, and the watchdog relaunches the app when the version advances.

**The Store's scan cooldown.** The install service refuses a second online
scan for the same package family within roughly half an hour and answers from
its cache — `Microsoft-Windows-Store/Operational` says "Online scan not
allowed due to cooldown period", zero applicable. `install` therefore trusts a
cached newer `AvailableUpdate` before it asks the Store at all, and `check`
keeps a cached newer offer standing when the previous check was less than
`STORE_SCAN_COOLDOWN_SECS` ago (`keep_cached_offer`, reported with a detail
line). Only a check outside that window clears it.

**The GitHub flavor** has a two-phase shape — `run_update_check` downloads the
signed bundle and registers it with `-DeferRegistrationWhenPackagesAreInUse`.
`install` hands the downloaded bundle to the same helper, which registers it with
`-ForceApplicationShutdown` (the broker is resident, so a deferred registration
would never land), behind the same watchdog. No bundle is exit 12.

**The helper** is `%LOCALAPPDATA%\ForwardSlashWindows\update\fwdslash-helper.exe`:
a byte-identical copy of the running `fwdslash.exe`, staged through
`adapters::real_copy_file` (a `cmd.exe` child, because the source is in
`WindowsApps`) and named distinctly so a user or an antivirus report can
identify it. It exists for the one thing package identity forbids — asking the
Store to replace the package that is asking, and `Add-AppxPackage` against the
package it is running inside. Its hard rule: **it never writes HKCU.** An
identity-less write lands in the real hive while the packaged app reads the
virtualized one, which would be invisible on a dev build where the two views are
the same. It reports through `last-result.txt` in the same directory —
`completed`, `paused`, or `error:<hex>` — and the next packaged
`update check`/`update status` folds that file into the registry and **deletes
it**, so one helper run is folded exactly once. Only `completed` clears the
cached `AvailableUpdate` notice; a pause or an error leaves it standing.

**The watchdog** is a unique per-user task named
`fwdslash-update-watchdog-<pid>-<sequence>`, registered before the install
runs because a force-closed package cannot relaunch itself. Each attempt owns
immutable temporary `.cmd` and `.xml` sidecars rather than rewriting a shared
script. The command runs the optional helper or `winget`, then polls
`Get-AppxPackage` every 5 s for a **newer version of the exact package family**.
Only that condition permits `--relaunch broker` (the default) or `app`; `none`
skips it. A 45-minute timeout reports a failed handoff (`error:0x800705B4`,
unless the helper already wrote a verdict) and then starts the **broker** if
none is running, in every mode: the old package is still the installed
product, and the caller closed the broker to make room for an install that did
not land (issue #140). The task removes only its own task and sidecars. Script
literals are validated before they are written.

Each attempt owns an `update-attempt.lock` token in the updater directory. A
token older than 65 minutes is stale; so is a younger one whose named task is
no longer registered (`scheduled_task::task_exists`) — an attempt killed
between registering and running left exactly that behind and used to fail
every install for an hour with "the watchdog could not be registered".

**Garbage collection** (`update::gc::collect`, issue #140) runs on every
packaged `check` — so from the broker's update cycle and the settings window's
launch — and removes what an attempt could not clean up
after itself: every owned task (the generated grammar and the legacy fixed
name) whose `.cmd` sidecar is missing or older than `STALE_AFTER` (70 min),
orphaned `.cmd`/`.xml` sidecars of the same age, and an attempt token that is
older than 65 minutes or names a task that is no longer registered. Age is
the whole rule, deliberately: each task's XML limits it to an hour and its
trigger is at most five minutes out, so nothing that old can be a live
install, and the scheduler's status column is localized and not consulted.
`status` deliberately does **not** collect: it reports what the registry
already knows, and a person or script asking what the updater thinks must not
thereby delete a scheduled task out from under a live install. Nothing is
missed by that, because every caller that reaches `status` reaches `check`
too. `Verb::collects_garbage` is the pure decision and `collect_for` its
single call site, so the read-only contract is a test rather than a
convention.
GitHub downloads remain `*.part` files until atomic promotion, and
`last-result.txt` contains only the compact completed/paused/error outcome.
`fwdslash uninstall` cancels owned tasks before sweeping updater storage, so it
does not delete another attempt's live files.

**`UpdateRoute`** (`REG_SZ` under the settings key, values `auto`, `appinstall`,
`store`, `winget`, `notify`) pins one rung without a rebuild — the escape hatch
if the Store ever objects to route 1, and the way a user keeps the check while
refusing unattended installs (`notify`). It applies to the **Store** ladder
only: the GitHub path has a single route and never consults it. It is read-only
to the product — nothing writes it — and `--route <name>` is the same override
for one invocation.

Certification wording for all of the above is in `docs/store-submission.md` §3;
what it sends and stores is in `PRIVACY.md`.

## Every settings write reaches both hives (#52)

There is exactly one writer for `HKCU\Software\ForwardSlashWindows\Settings`,
`fsw_core::settings_write` (`set_setting_u32` / `set_setting_u64` /
`set_setting_string` / `delete_setting`), and nothing else may write it. Its
decision is `write_plan(packaged)`: unpackaged, the in-process API *is* the real
hive and one write is the whole job; packaged, the value goes to the real hive
through a `reg.exe` child **and** to the package hive in-process — the second
half matters because a stale private-hive copy shadows the real one for every
packaged reader, so a real-hive-only write would simply invert the split.

The failure this prevents is measurable: a packaged build that writes
`BareSlashMode` with the in-process API files it in the package's private hive,
where the unpackaged shell adapters — which read the real hive — never see it.
The symptom on a Store 0.0.3 install was the settings app saying *default
distribution* while `cd /` in PowerShell still listed the distributions.

`sync_settings_to_real_hive()` repairs installs that already carry that split: a
packaged process compares its merged view (authoritative) against a child
`reg.exe query` of the real hive and mirrors what differs, never deleting. It
runs from the broker's startup sweep, the settings window's launch sweep and
`fwdslash repair-adapters`, and logs `event=settings_synced` — category only.

## The state-changed broadcast (#55)

One registered window message, `fsw_core::FSW_STATE_CHANGED_MESSAGE` =
`ForwardSlashWindows.StateChanged`, registered per session with
`RegisterWindowMessageW` and posted to `HWND_BROADCAST` by whoever changed
something, *after* the change lands: `fsw_core::settings_write` announces every
successful settings write (so the bare-slash values, `Disabled` and the update
values are covered wherever they are written from), and `fwdslash` announces the
verbs whose state lives elsewhere — `integration … enable|disable|repair`,
`repair-adapters`, `install`/`uninstall`, `start`/`stop`, `pause`/`resume` —
once per invocation and only on exit 0. It carries no payload: every listener
re-reads what it needs, so nothing about what changed travels between processes
(`PRIVACY.md`).

Without it, a settings window and a broker both keep rendering what they read at
launch while the other changes it.

The broker listens on its existing top-level window and re-reads the settings
(Broker §2). The settings window listens on a hidden top-level window of its
own, on its own thread, because it has no window procedure it can reach
otherwise — `crates/fsw-settings/src/state_watch.rs`, class
`ForwardSlashWindows.SettingsWatcher`, and a real top-level window because
`HWND_BROADCAST` skips message-only ones (the same reason the broker's window is
one). Both are unchanged by the message itself; the re-read is what applies it.

`RegNotifyChangeKeyValue` was considered and rejected as the primary mechanism:
under MSIX registry virtualization it is not clear which hive layer the
notification tracks, and writes go to both (#52). A broadcast plus a poll is
deterministic in a way that does not depend on the answer.

`Invoke-ForwardSlashWindowsSetLocation` also answers `cd ..` at a distribution's
share root (`\\wsl.localhost\<Distro>`, or the `\\wsl$` spelling) with one
line naming the distribution, instead of PowerShell's `Cannot find path
'\\wsl.localhost'`. Every other path, and a paused product, keeps the native
behaviour.

## Settings window (`fsw-settings`)

The settings app is built on the vendored `windows-reactor` crate rather than
WinUI 3 XAML interop, which constrains a few things below. It reads HKCU and
probes the broker window in-process; every value shown comes from `fsw-core`,
not from parsing CLI output.

### 1. No icon in the title bar

reactor models no `IconSource` type at all — `PropertyId::ImageIconSource` is
the *source* property of the `ImageIcon` **element**, which is an `IconElement`
and not assignable to `TitleBar.IconSource`.

Binding `Microsoft.UI.Xaml.Controls.ImageIconSource` by hand was tried and
rejected: the IID and vtable layout match the SDK headers, `ITitleBar`'s
`SetIconSource` slot is in the right position, and everything up to the
activation succeeds — but `ImageIconSource::new()` fail-fasts (`0xC0000409`)
under the unpackaged Windows App SDK. The `TitleBar` `Content` slot is not a
substitute either: it centres its child, so the icon lands mid-bar instead of at
the leading edge.

The caption is therefore drawn by `TitleBar` itself, and the leading-edge icon
goes in the TitleBar's `LeftHeader` slot (added to `windows-reactor` for this):
the same position `IconSource` would occupy, automatic drag regions, and
`ImageIcon` + `EncodedImage::from_static` over `assets/fwdslash-titlebar.png`
decodes in place via `BitmapImage.SetSourceAsync`, never constructing the
fail-fasting `ImageIconSource`. The taskbar/Alt-Tab icon is separate:
`WindowVisuals::icon_resource(IDI_FSW_APP)` loads the `app.rc` resource and
applies it with `WM_SETICON` against the raw HWND. (An `.rc` icon alone only
becomes the exe's file icon — it never reaches the taskbar on its own.)

### 1b. The settings window is a plain window with a single-instance guard

The settings app has **no** notification-area icon, no window subclass, and no
watchdog thread. 0.0.2 gave it a tray icon of its own (`tray.rs`) that hid the
window on close or minimize; because the broker also owns one, the product
showed **two** identical icons for the rest of the session, and the whole
watchdog / zombie-takeover / `FSW_SIMULATE_WINDOWLESS` apparatus existed only to
survive that hide-to-tray design. All of it is deleted. The product's one icon
belongs to the broker (Broker §2).

Closing the window exits the process: `windows-reactor` routes WinUI's
`Window.Closed` through `dispatch_window_closed` → `finalize_closed_window` →
`exit_ui_thread()`, and nothing calls `DestroyWindow` directly, so a process
holding the mutex always has a window to raise.

The single-instance guard is a `Local\ForwardSlashWindows.Settings` mutex, and a
second launch raises the first instance's window instead of opening a duplicate.
The raise matches on the window title **and** on the owning process image being
`fswsettings.exe` (`EnumWindows` + `GetWindowThreadProcessId` +
`QueryFullProcessImageNameW`); a bare `FindWindowW(NULL, title)` used to match
the broker's own never-shown top-level window, which had the same caption, and
"raise" it as a 0x0 caption strip. The broker's window was retitled
`fwdslash broker` as well, so the two can no longer collide by name. The same
current-process-only enumeration supplies the folder picker's owner HWND
(`folder_picker::current_process_window`).

### 2. The navigation pane pushes content instead of overlaying it

`PaneDisplayMode = LeftCompact` pins WinUI's `DisplayMode` to `Compact`, which
hosts the pane in a `SplitView` set to `CompactOverlay` — so opening the pane
draws it *on top of* the page. The content does not reflow, and the headings and
body text are clipped mid-word behind it.

The app therefore sets `PaneDisplayMode = Left`, which forces
`DisplayMode = Expanded` and therefore `SplitView CompactInline`. Closed, that
renders the same 48px icon rail. Opened, the pane expands inline to
`OpenPaneLength` and the content shifts aside instead of being covered.

Three further choices come from the Fluent audit (2026-09) and follow the
published WinUI guidance: `OpenPaneLength` at the documented 320 default, 24px
content padding, and secondary text drawn with `TextFillColorSecondaryBrush` via
`ThemeBrush::TextSecondary` rather than opacity dimming, which does not survive
high-contrast themes.

### 3. A change watch, not a refresh on activation

reactor exposes no activation observation — `HostEvent` carries only
`WindowSize`, `ColorScheme` and errors — so "refresh when the window is
alt-tabbed back" is not available. Refreshing only on the app's own mutations,
on navigation and on the "Refresh status" button left exactly one case
uncovered: an external change while the window sits open and untouched (#55).

The app watches instead, which covers more than an activation hook would: the
window follows a change it never touched, without being touched itself.
`crates/fsw-settings/src/state_watch.rs` owns both halves:

- **The broadcast.** A hidden top-level window on its own thread receives
  `FSW_STATE_CHANGED_MESSAGE` and signals a manual-reset event. `wait()` — one
  turn, run as background work and re-armed by handling the message it produces
  — waits on that event, and on a signal sleeps 250 ms and clears the event
  before returning, so a multi-value write (`bare-slash default` writes three)
  costs one read, and a signal raised after the clear wakes the next turn.
- **The poll.** The same wait times out after 5 s and reports `Wake::Poll`,
  which covers writers that do not broadcast — an older staged `fwdslash.exe`,
  a hand `reg.exe` edit. It is skipped while the window is minimized or hidden
  (`should_read`); a broadcast never is, so a restored window is right the
  moment it appears.

`ReadCoalescer` keeps it to one `State::read()` at a time, remembering at most
one owed read, and `Msg::StateRefreshed` compares before assigning: an equal
`State` — which is what the poll finds almost every time — touches nothing. All
of it is off the UI thread; the UI thread only swaps the value in.

### 4. Deep links select the page but do not focus the control

reactor exposes no programmatic focus API, so `fwdslash://settings/cmd` and
friends select the right page and stop there rather than moving focus to the
named toggle.

### 5. Handlers guard by comparing values, not by a re-entry flag

A declarative view re-applies every value on each mount, and the mount echo for
`RadioButtons`/`ComboBox` arrives *after* reactor's synchronous
feedback-suppression window closes, so a time-window "currently loading" flag
cannot suppress the echo. Every handler instead compares the requested value
against current state and returns early when they agree — see
`SettingsModel::update`. This is why the bare-slash controls are two
`RadioButton`s sharing a `GroupName` rather than the items-source `RadioButtons`
control: `RadioButton.IsChecked` echoes synchronously and is suppressed by the
framework.

### 6. Instance lifecycle and off-thread controller calls

- **Fail closed.** Any `CreateMutexW` error other than "already exists" shows a
  message box and exits — it never falls through to "I'm the first instance".
  Silently running a second instance is the failure this guard exists to
  prevent.
- **Raise, never take over.** With the mutex held elsewhere, the relaunch polls
  for the other instance's window for 10 s (WinUI takes a beat to materialize
  it) and raises it. There is no windowless-zombie state to recover from, so
  there is no process termination, no packaging-identity comparison, and no
  `FSW_SIMULATE_WINDOWLESS` fixture; if no window appears, the launch reports it
  and exits rather than killing anything.
- **Controller calls run on the thread pool.** `run_controller` reaches
  `fwdslash.exe`, and `integration windows-powershell enable` loads the user's
  whole profile — up to 15 s, which on the UI thread would freeze the window.
  Every invocation goes through `SettingsModel::start_controller`, which sets a
  `pending` action, spawns the work with `context.spawn_background`, and
  finishes on `Msg::ControllerFinished`. While `pending` is set every
  state-mutating control is disabled (`controls_enabled()`) and a `ProgressRing`
  is shown, so a second request cannot race the first.
- **State reads are off-thread too.** `State::read()` runs in
  `spawn_background` and arrives as `Msg::StateLoaded`; only the very first
  frame reads synchronously. `ensure_broker_running()` (which spins up to 2 s)
  is off-thread as well and reports back with `Msg::BrokerProbed`.
  `broker_state` uses a 250 ms timeout, `pwsh.exe` discovery is a process-wide
  `OnceLock`, and every page refreshes on navigation, About included — its
  Components and Updates cards are live state.
- **All app-update UI lives on the About page.** The Automatic updates toggle,
  the last-check line, "Check now", the install button and the progress ring
  with its caption are one card there, beside the version and the offer the
  Components card already shows — an install button only means something next
  to what it would replace. The card renders nothing at all on an unpackaged
  build, which has no package to replace. `banners()` therefore keeps only the
  terminal-integration upgrade bar, which belongs to Terminals rather than to
  the app's own updates and is progress the user did not ask for and cannot act
  on, the one thing that earns a standing row on every page. The dismissible
  result bar stays shared: every feature reports through it. Note `self.pending`
  is still a single global, so a check started from About disables the
  bare-slash and integration controls on the other pages while it runs.
- **Standing banners are Buttons, not InfoBar actions.** Reactor's `InfoBar`
  exposes no action-button slot, so the "Restart to update" action is an
  ordinary `Button` rendered directly beneath its bar, in a second fixed grid
  row that is kept out of `self.notice` — a routine "Updated" result must not
  hide a standing notice, and vice versa.
- **Outdated shell adapters are upgraded automatically, with no button to
  press.** The app upgrades outdated shell adapters on every launch (one
  sequential `fwdslash integration <id> enable` per adapter, reported by an
  InfoBar: "Updating terminal integrations…" → "Terminal integrations updated" /
  "Some terminal integrations could not be updated") and shows a Components card
  on About with broker state, per-adapter payload versions, package
  version/architecture and package flavor. The broker does the same sweep at
  startup (Broker §2), so the settings window is the second chance, not the only
  one.
- **The tray tooltip is the broker's alone.** It reads
  `Forward Slash Windows — active` / `— paused` / `— hook unavailable`. The
  0.0.2 arrangement of two deliberately-different tooltips is gone with the
  second icon.
- **The filesystem-driver line is live, not hardcoded.** The app probes the
  `FswFilter` service through the SCM (read-only, never elevating) plus a
  connect to `\FswFilterPort`, and General and the About Components card both
  render `Filesystem driver:` followed by one of `not installed` /
  `installed, not loaded` / `loaded, not connected` / `connected`, the same
  four states `fwdslash driver status` prints. The About page carries no
  production-gated sentence.
- **Update controls, for both flavors.** For any packaged build:
  - The **Automatic updates** switch is shown for both flavors (`state.packaged`
    alone). The Store text is "Let fwdslash install Store updates in the
    background. Off by default; the Store still updates the app on its own
    schedule." The default the switch reads when nothing is stored is the
    flavor's (`default_auto_update`), and the stored inverted DWORD is
    untouched, so nobody's recorded "off" flips.
  - A **Check now** button on General runs `fwdslash update check --force
    --json` off the UI thread and always answers on screen ("Up to date",
    "Update available", or "Could not check for updates"), while the launch
    check — the same verb without `--force`, gated by
    `update_check_allowed(has_package_identity(), read_auto_update_enabled())` —
    stays silent unless it found something. Both go through the CLI rather than
    calling `fsw_core::update::run_update_check` in-process, because the CLI is
    the only component that knows the Store routes.
  - The install banner appears when `packaged && (update_bundle_ready ||
    update_available.is_some())`, labelled **Install now** for the Store flavor
    and **Restart to update** for the GitHub one. It runs `update install
    --force --relaunch app --json`, after `close_broker_window()` so the broker
    removes its own notification icon rather than leaving a ghost. Exit 0 closes
    the window (the install is about to force the package down and the CLI's
    watchdog brings it back); 10, 11 and 12 each leave a bar, and 11 — "the
    Store has to finish this" — is the one notice with an action button,
    **Open Microsoft Store**, on `ms-windows-store://pdp/?productid=<STORE_PRODUCT_ID>`.
  - The About Components card carries `Last update check: <never | just now |
    N minutes/hours/days ago>` and, when one is recorded, `Update available:
    <version>`.
  - A **Repair integrations** button on the Terminals page runs `fwdslash
    repair-adapters` (#56). It exists because the broker's failure balloon told
    the user to "Open Settings to retry" when there was nothing to press — the
    retry only happened by accident, in the launch sweep. It takes the same
    cross-process sweep lock the launch sweep does and reports "Integrations are
    already being updated" rather than fighting the broker for the payload tree.

## Broker (fsw-broker)

### 1. The window is a never-shown top-level tool window, not message-only

Message-only windows (the `HWND_MESSAGE` parent) are skipped by
`HWND_BROADCAST`, so `TaskbarCreated` (explorer.exe restart) and
`WM_QUERYENDSESSION`/`WM_ENDSESSION` (session end) could never reach the
tray-icon lifecycle. The broker therefore creates a real top-level window with
`WS_EX_TOOLWINDOW` that is never shown: the same `FindWindowW`-by-class
discovery for the CLI and settings app, and the icon is re-added after a shell
restart and removed before a session-end ghost can appear.

### 2. Two windows, two threads, and the tray icon

The broker owns the product's single notification-area icon, and it classifies
Enter on the hook thread but processes it on a worker. This section is the whole
list of its behaviour.

**Windows and threads.**

- The top-level window is titled `fwdslash broker` (0.0.2 titled it
  `Forward Slash Windows`, which collided with the settings window's caption —
  see Settings §1b). Nothing discovers it by title; the class
  `ForwardSlashWindows.Broker` is the contract.
- A **second** window, class `ForwardSlashWindows.BrokerWorker`, lives on a
  worker thread with its own `CoInitializeEx(STA)` and its own `IUIAutomation`.
  This one *is* `HWND_MESSAGE`, which is correct precisely because it needs no
  broadcasts. The hook posts `PROCESS_ENTER` to it with the classification in
  `wParam` and the foreground HWND in `lParam`.
- Everything that can block runs there: UI Automation, the resolver,
  `ShellExecuteExW` (with `SEE_MASK_ASYNCOK | SEE_MASK_FLAG_NO_UI`),
  `SendInput`, and `Navigate2`. Pause persistence has its own FIFO background
  queue. A low-level hook whose thread exceeds `LowLevelHooksTimeout` is removed
  by Windows without telling the process, and binding
  `\\wsl.localhost\<distro>` boots a stopped distribution — seconds, on the
  thread that owns every keystroke on the machine. Menu commands are handed to
  the worker the same way (`WORKER_OPEN_PATH`, which transfers ownership of a
  boxed `String`).
- The hook thread's own work is only: class check → return `Unknown` unless
  the class is `CabinetWClass`, `ExploreWClass`, `#32770` or
  `Windows.UI.Core.CoreWindow`; only then the process image, into a 1024-unit
  buffer instead of 32768. The classification travels to the worker, which never
  re-runs it.

**Behaviour.**

- **Worker delivery never becomes hook-thread work.** A missing worker window
  passes Enter through natively; a failed menu-path post discards the request
  and reports it. Neither path falls back to inline `ShellExecuteExW` or a
  synchronous persistence write on the hook-owning thread.
- **A stale request is dropped, not replayed.** If the foreground window or the
  exact focused control changed while the request was queued or a blocking UIA/
  COM call was in progress, the worker logs
  `event=enter_dropped_foreground_changed` and returns. Replaying Enter into
  whatever the user switched to would send a half-written message or run a
  half-typed command.
- **`#32770` is narrowed twice.** In the hook, a dialog outside `explorer.exe`
  qualifies only if it has a `DUIViewWndClassName` child (the modern
  common-item dialog) or a `cmb13`/`edt1` control (the classic one). In the
  worker, **every** surface additionally requires the focused element to
  positively report Edit or ComboBox, `IsPassword == false`, and a non-read-only
  `ValuePattern`; an unavailable property is a rejection, not a false value;
  otherwise it logs `event=surface_rejected` and replays untouched. Claiming
  every `#32770` in every process is how an earlier design swallowed Enter in
  Find boxes and rewrote their search text. Requiring the writable pattern
  before reading also means the broker never reads text it could not have
  written back — the promise `PRIVACY.md` makes.
- **`FSW_WM_SET_PAUSED` replies the resulting `BrokerState`** (`Active` = 1,
  `Paused` = 2) or **0** when the change could not be honoured; it is not a
  boolean ack, because an unconditional `1` makes a failed resume
  indistinguishable from a successful one. The hook is removed *before* the
  setting is persisted, and the write itself is asynchronous — a packaged
  `persist_disabled` shells out to `reg.exe` (`fsw_core::settings_write`, issue
  #52), and a process creation plus wait on the hook thread is exactly what must
  not happen. A failed write surfaces later as a balloon plus
  `event=persist_disabled_failed`. Persistence is one FIFO background queue,
  so rapid toggles preserve submission order; a queue/start failure is reported
  asynchronously and never falls back to an inline registry write. A failed
  `install_hook` on resume shows the hook balloon and answers 0. The CLI turns
  that 0 into a specific message by asking the broker what state it actually
  reached.
- **The tray icon and its menu.** Tooltip:
  `Forward Slash Windows — active` / `— paused` / `— processing unavailable`.
  `Shell_NotifyIconW(NIM_ADD)` is checked (it fails with `ERROR_TIMEOUT` while
  the shell is busy — exactly when the MSIX startup task runs at logon); a
  failure sets `ICON_ADDED = false`, logs `event=tray_icon_add_failed`,
  suppresses every balloon, and is retried from the health timer, with
  `NIM_SETVERSION` applied only after an add lands. The menu is **Open
  settings** (the `SetMenuDefaultItem` default, and what left click and
  double-click do) · separator · **Enabled** as an `MF_CHECKED` toggle · **Open
  WSL root** · **Open distribution ▸** (one item per registered distribution,
  capped at 64, resolved against the list the menu was built from) ·
  **Integrations ▸** · separator · a greyed version line · **Exit**. It opens at
  the point `NOTIFYICON_VERSION_4` carries in `wParam` (sign-extended per
  monitor), falling back to `GetCursorPos` on the legacy `WM_RBUTTONUP` path,
  and `PostMessageW(window, WM_NULL, 0, 0)` follows `TrackPopupMenu` — without
  it the *next* right-click flashes a menu that dismisses itself.
- **The health timer is adaptive.** 5 s only while a driver actually answers on
  the filter port; 60 s otherwise, which is the shipping configuration. The
  connect probe runs first and decides the interval; with no port the registry
  enumeration and the kernel round-trip are skipped entirely, and the attempted
  distribution list is recorded anyway (`ATTEMPTED_DISTRIBUTIONS`) so the
  compare-only path can engage with no driver present. The same tick doubles as
  the tray-icon retry and as a hook **re-arm** (`UnhookWindowsHookEx` +
  `SetWindowsHookExW`, keeping the incumbent if the replacement fails, logging
  `event=hook_rearmed`), spaced at 60 s independently of the tick interval.
  There is no way to ask whether a hook handle is still live, so re-arming on a
  slow timer is the only defence against a silent removal.
- **Trailing separator for the Win32 hazard.** When
  `Resolved::has_win32_normalization_hazard()` is true and the rendered path
  does not already end in `\`, one is appended before opening — Win32 keeps a
  trailing `.` or space when a separator follows it — and
  `event=win32_normalization_hazard` is logged.
- **Balloon text.** `"Windows could not open the location."` — deliberately not
  "WSL location", because with a custom folder root the target need not be in
  WSL. The pause-write failure adds `"The pause setting could not be saved."`
- **Shell adapters are upgraded at startup.** `start_adapter_upgrade` checks
  each installed adapter's recorded payload version and, when it predates the
  running build, silently re-runs `fwdslash integration <id> enable` on a
  background thread (90 s ceiling per adapter), so terminal integrations are
  upgraded automatically after a product update. The result is reported in a
  single balloon: `"Terminal integrations were updated to <version>: <names>."`
  or `"Some terminal integrations could not be updated automatically. Open
  Settings and choose Repair integrations."` The settings window repeats the
  sweep on launch (Settings §6) and `fwdslash integration <id> enable` remains
  the manual fallback.
- **A failed adapter upgrade is retried once, and a transient one is never
  announced** (#56). The first failure is followed by a 5 s pause and one
  retry. `adapter_outcome(first, retry)` then classifies: success either time is
  `Upgraded`; two attempts that never produced an exit code at all — the child
  could not be spawned, or blew the 90 s budget and was killed — are `Deferred`,
  logged `event=adapter_upgrade_deferred` and **silent**, because the marker key
  still reads the old version and the next broker start or settings launch tries
  again; only a retry that *ran and refused* is `NeedsUser` and earns the
  warning balloon. Two refusals are answered on the **first** attempt, because
  retrying them cannot change anything (#127): exit 4 is `NeedsConfirmation` —
  the payload is current but the profile carries a block only the user may
  authorise rewriting, logged `event=adapter_upgrade_needs_confirmation` and
  ballooned as information, not a failure — and exit 5 is `Blocked`, a profile
  write Controlled Folder Access refused, logged
  `event=adapter_upgrade_blocked` and ballooned naming CFA and what to do. The
  whole sweep is serialised against the settings window's launch sweep by the
  named mutex `Local\ForwardSlashWindows.AdapterSweep`
  (`fsw_core::FSW_ADAPTER_SWEEP_MUTEX`, held for existence rather than
  ownership, exactly like the two singleton mutexes): whoever finds it already
  held logs `event=adapter_sweep_busy` and stands down, because the holder is
  running the identical work. Before this, an update that restarted the app
  started both sweeps within seconds and the loser deleted a payload tree the
  winner's child was running out of, which is the transient failure the balloon
  was reporting as terminal.
- **The broker drives the self-update.** `health_tick` calls
  `maybe_start_update_cycle()` once a minute, which starts a cycle only when all
  four of `!UPDATE_RUNNING`, an age of at least `UPDATE_CONSIDER_INTERVAL_MS`
  (6 h; the CLI enforces the real 24 h cadence), `!WORKER_BUSY` and
  `fsw_core::update::update_check_allowed(has_package_identity(),
  read_auto_update_enabled())` hold — `update_cycle_due`, a pure function, is
  the whole truth table. The first cycle of a process is held off for
  `UPDATE_FIRST_DELAY_MS` (5 min) after startup: logon is the busiest moment on
  the machine, the adapter sweep is already running, and an update that
  force-closes the package seconds after the user signed in is the worst
  possible one. `WORKER_BUSY` is raised around **both** worker messages
  (`PROCESS_ENTER` and `WORKER_OPEN_PATH`) by an RAII guard, so an install can
  never terminate the process mid-rewrite of an address bar.
  The cycle itself always runs on a thread named `fsw-update` — never inline,
  the same rule as the adapter sweep, because this thread owns the low-level
  keyboard hook; a spawn failure logs `event=update_cycle_skipped` and waits for
  the next tick, and a `Drop` guard clears `UPDATE_RUNNING` however the cycle
  ends. It runs `fwdslash update check --json` (180 s ceiling) against the
  `fwdslash.exe` **beside the broker**, never one from PATH, and on exit 10 goes
  on to `fwdslash update install --relaunch broker --json` (300 s) — no
  `--force`, so the CLI's own moment gate still declines while a settings window
  is open, and `broker` because what has to come back afterwards is the resident
  daemon, not a window nobody asked for. `run_cli_bounded` is the shared child
  runner underneath this and both adapter verbs; it returns `Option<i32>`, where
  `None` means "never answered" (spawn failure, wait error, or killed at the
  deadline) rather than "failed".
  Balloons are rationed: install exit 11 → `NIIF_INFO` "An update to fwdslash is
  available in the Microsoft Store. Open Settings to install it."; two
  consecutive install exit 1 **on the Store flavor only** → `NIIF_WARNING`
  "fwdslash could not update itself automatically." (the GitHub flavor's failed
  install leaves the downloaded bundle in place and applies it at the next
  logon, so there is nothing to ask the user for); exits 0, 10 and 12 are
  silent. Both go through `notify_when_icon_ready` and are deduplicated against
  `cached_update_tag()`, so one available version produces one balloon however
  many six-hour cycles see it.
- **Diagnostic categories** (category-only, per `PRIVACY.md`):
  `event=enter_dropped_foreground_changed`, `event=surface_rejected`,
  `event=hook_rearmed`, `event=persist_disabled_failed`,
  `event=win32_normalization_hazard`, `event=tray_icon_add_failed`,
  `event=worker_start_failed`, `event=worker_detached`,
  `event=debug_uia_failed`, `event=debug_hook_failed`,
  `event=adapter_upgraded`, `event=adapter_upgrade_failed`,
  `event=adapter_upgrade_skipped`, `event=settings_synced`,
  `event=state_changed`, `event=hook_unavailable`, and — new with the
  self-update and #56 — `event=adapter_upgrade_retry`,
  `event=adapter_upgrade_deferred`, `event=adapter_sweep_busy`,
  — new with #127 — `event=adapter_upgrade_needs_confirmation` and
  `event=adapter_upgrade_blocked`,
  `event=update_cycle_started`, `event=update_available`,
  `event=update_installing`, `event=update_cycle_failed`,
  `event=update_cycle_skipped`, and — new with the #121/#122 hook-thread work —
  `event=enter_replayed_unattested` (the worker, not the hook, attests, so a
  window that fails attestation has its swallowed Enter replayed),
  `event=enter_replayed_paused` (renamed from `event=enter_dropped_paused`: a
  pause landing after the hook swallowed the key now replays it),
  `event=enter_replayed_focus_unavailable`,
  `event=browser_enter_replayed_focus_unavailable` (renamed from
  `event=browser_enter_dropped_focus_unavailable`, same reason), and
  `event=driver_namespace_rejected` (once per process; it replaced an
  `eprintln!` that a GUI-subsystem process sent nowhere and the health timer
  repeated every tick). Also `event=route_distribution`, `event=route_folder`
  and the other routing categories. None of them carries a version, a path or
  anything the user typed.
- **It re-reads the settings on a state-changed broadcast** (#55). The tray
  tooltip, the keyboard hook and the published mapping all derive from state
  another process can change; otherwise they would catch up at the next health
  tick, or — for the pause flag, which the broker holds only in memory — never.
  `reload_settings` compares the stored `Disabled` against `PAUSED` and, when
  they differ, applies the pause exactly as the tray toggle does minus the write
  (`apply_paused`), then refreshes the tooltip and republishes. It skips the
  comparison entirely while one of its own pause writes is in flight
  (`PERSIST_IN_FLIGHT`): the tray toggle changes `PAUSED` first and persists
  off-thread, so a broadcast arriving in between would otherwise be read as an
  external change and revert it. The tray menus need nothing — they are built
  from live state when the menu opens.

## CLI (fwdslash)

### 1. `fwdslash start` only ever closes the broker it spawned

On probe failure the CLI compares the broker window's PID against the spawned
`dwProcessId` before posting `WM_CLOSE`, so a pre-existing healthy instance the
spawn did not create — the spawn may have lost the mutex race and exited already
— is never closed. It reports `Resolution is paused; run "fwdslash enable" to
activate.` when the probed broker is merely paused, rather than the misleading
"keyboard hook is unavailable".

### 2. Shell verbs, exit 3, and a self-upgrading adapter payload

- **`fwdslash cmd-cd <input>`** — the target for the cmd `CD`/`CHDIR`/`PUSHD`
  macros. Stdout carries the Win32 path and nothing else, so the batch file can
  capture it verbatim. A bare `/` in distribution-list mode is not one
  directory, so it goes to stderr with the "`/` lists your WSL distributions…"
  message and exit 1.
- **`fwdslash shell-resolve <input>`** — one JSON line
  (`{"kind":"root|distribution|folder","target":…,"distributions":[…]}`) for the
  PowerShell module, so `ls /` costs a single spawn instead of a `resolve` plus
  a `status --json`. It answers from `Snapshot::current()` alone: no broker
  round trip, no filter-port probe. That snapshot includes the **global pause**,
  so a paused product comes back as `kind:"native"` (#135) and the module no
  longer opens `HKCU\Software\ForwardSlashWindows\Settings` itself — the key
  path is now defined only in `include/fsw_user_protocol.h` and
  `crates/fsw-core/src/lib.rs`. The trade-off: while the product is paused a
  slash argument costs one spawn rather than a registry read. Only slash
  arguments pay it, because the argument scan gates first.
- **`fwdslash cmd-list <input>`** — the target for the cmd `DIR`/`LS` macros.
- **The exit-3 contract.** Every shell verb returns **3** for "run your own
  command unchanged" — resolution is paused, the input is not a slash path, or
  (for `cmd-list`) the target does not exist (`ERROR_FILE_NOT_FOUND` /
  `ERROR_PATH_NOT_FOUND`, so a missing path degrades to native `DIR`). **1** is
  a resolver rejection, already explained on stderr. **0** is a target on
  stdout. All three verbs share one funnel, `shell_target`, so a `cd` and a
  `dir` can never disagree about what an input means.
- **The PowerShell `pushd` wrapper is a global function built from an unbound
  script block**, not a module function — the only wrapper that is. `Push-Location`
  run inside a module pushes onto *that module's* location stack, so the caller's
  `popd` never sees it; and `@args` re-splats named parameters faithfully only
  from a simple function's own `$args`, whereas the same array collected by an
  advanced function rebinds `-LiteralPath` as a positional value. Running in the
  caller's session state fixes both, at the cost of being able to call only
  commands the global scope can see — which is why
  `Resolve-ForwardSlashWindowsLocationTarget` is exported.
- **`pushd -StackName` is not mirrored.** The wrapper pushes onto the default
  stack; a named stack is passed through to `Push-Location` untouched only when
  the argument list carries no slash path.
- **Adapters self-upgrade.** `PAYLOAD_VERSION` derives from
  `CARGO_PKG_VERSION`, and an `installed` marker whose `Version` differs from
  it makes `fwdslash integration <name> enable` bring the payload up to date.
  For cmd that is the uninstall transaction for the deployed payload followed by
  the install transaction for this one — same rollback guarantees. For
  PowerShell it is a **payload swap alone** since #127: the block is
  byte-identical across releases, so `powershell::upgrade` renames the new
  `payload` directory into place, refreshes the `profile.block` recovery copy in
  `%LOCALAPPDATA%`, records the new `Version`, and never opens the profile.
  `fwdslash integrations` prints `installed (update available)` for such an
  adapter and reports it in `--json`. Nobody has to run it by hand: the broker
  sweeps at startup and the settings window sweeps on launch, and the verb is
  the manual fallback. Without this an updated product would keep running a
  frozen copy of the old payload and old `fwdslash.exe` forever.

### 3. Self-healing shell integrations (#37)

The adapter hardens the whole lifecycle so an upgrade or an MSIX uninstall can
never leave a broken shell:

- **The profile block is guarded, fenced, self-cleaning — and lazy.**
  `block_text` emits `$m`/`$p`/`$a`/`$c` (module, product-presence probe,
  app-execution alias, staged controller) and
  `if ((Test-Path $p) -or (Test-Path $a)) { if (Test-Path $m) { <stubs> } }
  elseif (Test-Path $c) { Start-Process $c uninstall --orphaned }`. Since #134
  `<stubs>` is `STUB_BODY`, **not** an `Import-Module`: see §3a below. A pruned
  module directory can no longer throw the red `no valid module file` error,
  and a product that was uninstalled with no code run (MSIX) is cleaned up by
  the leftover hook on the next shell start — launched detached, so a shell
  never blocks on it. The region is delimited by the **constant**
  `# >>> Forward Slash Windows >>>` / `# <<< Forward Slash Windows <<<` fence
  lines (#127); the parser still recognises the legacy
  `# >>> Forward Slash Windows <ver> <id> >>>` form so an existing block can be
  found, migrated and removed.
- **The probe is the package's app-data folder, not the alias.** A packaged
  install records `%LOCALAPPDATA%\Packages\<family>` (from the actual
  `fsw_core::package_family()` at install time, so either flavor works); an
  unpackaged one records the controller's directory. The app-execution alias is
  only ever an additional OR, because a user can switch it off under
  Settings > Apps > App execution aliases without uninstalling anything — using
  it as *the* probe would silently disable the integration and spawn a
  self-clean on every shell start. The package folder also survives an update,
  which closes the in-flight-update race.
- **Install/enable is replace-not-append and idempotent.** `commit_install`
  computes the *true* original — the current profile with **every** fwdslash
  fence stripped (`strip_fwdslash_blocks`, encoding-aware over UTF-8/16/32) —
  snapshots that, and writes it plus exactly one current block. A repeated
  enable, or an upgrade over an older block, can never accumulate duplicates or
  strand a stale block, and uninstall restores the genuine pre-fwdslash profile.
  `OriginalPresent` tracks whether that true original is non-empty, so a
  profile that was purely our own block is deleted on removal.
- **Detect-and-repair.** `fwdslash repair-adapters` (run by the broker startup
  sweep and the settings launch sweep) and the per-adapter
  `fwdslash integration <id> repair` classify each profile — orphaned (missing
  module), migration-pending (a legacy versioned block), duplicated — and repair
  to exactly one current block when the adapter should be installed, or strip it
  out when it should not. `fwdslash doctor` and `fwdslash integrations` print a
  `shell integration health:` line per adapter. There is no `Stale` state
  (#127): a constant block cannot go stale, and the payload version of record is
  the registry marker's `Version`, never the fence text.
- **A background sweep never writes a file under `Documents` (#127).**
  `decide_profile_repair` takes a `user_initiated` flag, and every verdict that
  would write the profile becomes `NeedsConfirmation` when it is false. The
  broker's startup sweep and the settings window's launch sweep pass
  `--background` on `fwdslash integration <id> enable`, so they may swap the
  `%LOCALAPPDATA%` payload — which is the whole upgrade — but a legacy block, a
  duplicate or an orphan is *reported* (exit 4, the explanation on stderr, an
  information balloon, an `InfoBar`) and left working. The one remaining
  `Documents` write runs only from an explicit
  `fwdslash integration <id> enable|repair` or the settings toggle, and it goes
  through the ordinary uninstall+install transaction, so the byte-exact
  snapshot, the byte-exact restore, and the refusal to touch a third-party
  change are all preserved.
- **cmd never snapshots its own hook.** `begin_install` strips any
  `call "…ForwardSlashWindows…fsw-autorun.cmd"` segment from the observed
  `AutoRun` before recording the original, so an MSIX-leftover hook is not
  mistaken for a third-party value and `installed_autorun` never composes
  `call fsw & call fsw`. `fsw-autorun.cmd` is generated at install time with the
  probe baked in: it installs the doskey macros only while the product is
  present, and otherwise runs the self-clean instead of routing through an
  orphaned controller copy.
- **`fwdslash uninstall --orphaned`** is the deferred self-clean. It confirms
  the product is really gone (cheap file-system probes, then a
  `Get-AppxPackage` slow confirm only if those fail, so an in-flight update is
  safe), runs the transactional sweep (restoring profiles/AutoRun byte-exact,
  cmd still refusing a third-party change), then belt-and-braces strips what a
  refusal left: any fwdslash profile fence, and — the cmd analogue — fwdslash's
  own `call` segment out of `AutoRun`, keeping every third-party segment
  byte-for-byte and deleting the value only if nothing else remains. It removes
  `HKCU\Software\ForwardSlashWindows` and the unpackaged Run value, deletes the
  protocol key **only when its `shell\open\command` is still ours** (the normal
  uninstall's refusal, kept), and schedules deletion of
  `%LOCALAPPDATA%\ForwardSlashWindows` — including the directory it is running
  from — after it exits, but **never while `AutoRun` still references the
  payload**. Idempotent and safe to run twice.
- **The deferred delete is a scheduled task, not a detached child.** A
  `DETACHED_PROCESS` `cmd.exe` is killed with the rest of the tree when the
  launching shell lives inside a job object — measured on the dev host, where a
  WSL-interop-launched shell left the payload directory behind on 2/2 uninstall
  cycles even though nothing held the file open. `CREATE_BREAKAWAY_FROM_JOB` is
  not a fix either: that job forbids breakaway, so `CreateProcess` fails
  outright. The self-clean therefore writes
  `%LOCALAPPDATA%\Temp\fwdslash-orphan-cleanup.cmd` and registers a one-shot
  per-user task (`schtasks /create /sc once /st <now+1min> /f /tr <script>`,
  no elevation), then runs it immediately — the Task Scheduler service starts
  the script in its own session, outside any job we are in. The script waits
  ~2 s, removes the tree, then deletes the task and itself, so nothing
  accumulates; the one-minute trigger is only a backstop. The task name is
  fixed, so `/f` overwrites rather than piling up one task per run, and the
  detached child remains as the fallback when schtasks is unavailable.
  Belt and braces: `enable` and `repair-adapters` drop a payload tree that no
  adapter marker names before staging into it.
- **Controlled Folder Access is recognised through `ERROR_FILE_NOT_FOUND`.**
  CFA does not always block with `ERROR_ACCESS_DENIED`: on the dev host the
  blocked temp-file create inside a protected `Documents` subfolder surfaced as
  `os error 2`, and the user got "The system cannot find the file specified"
  instead of the product's guidance. `looks_like_blocked_write` treats a
  "not found" as a block **when the containing folder exists**, keeps the
  access-denied case unconditional, and the message says "…or the folder is
  otherwise not writable". The same explanation reaches the settings InfoBar —
  `run_controller` captures the controller's stderr — and `doctor` /
  `integrations` report an installed adapter whose profile cannot be written.
  Since #127 the failure is also **typed**: `AdapterError::blocked` marks it, a
  bare `io::ErrorKind::PermissionDenied` is mapped to it automatically, and
  `fwdslash integration <id> enable` exits **5** for it rather than the generic
  1, which is what lets the broker balloon name Controlled Folder Access
  instead of saying "could not be updated".
- **Version-free payload directories, swapped by rename-aside (#127).** The
  PowerShell module and its controller copy live in
  `%LOCALAPPDATA%\ForwardSlashWindows\PowerShell\payload`, not
  `…\PowerShell\<version>`, so `$m` and `$c` in the block never change. The
  swap is the cmd adapter's mechanism: stage into `payload.staging-<id>`, rename
  the live directory to `payload.removing-<id>`, rename staging in, drop the old
  one — with a rollback that puts the previous directory back. A payload whose
  two files already match their sources by size is left alone, so enabling the
  second edition never renames a directory the first is loading. Both adapters
  prune their own `*.removing-*` / `*.staging-*` / `*.rollback-*` leftovers
  (`is_prunable_leftover`) after every successful enable, on uninstall and from
  `repair-adapters` — two stranded `cmd.removing-*` directories were found on a
  live host — skipping only the directory an in-flight cmd uninstall recorded in
  its `RemovalPath`.

### 3a. The profile block loads the module lazily (#134)

Importing `ForwardSlashWindows.psm1` from the profile cost **~140 ms on every
PowerShell session on the machine** that does not pass `-NoProfile` — measured
on ARM64 against the packaged 0.0.8 install: `powershell -NoProfile -Command
exit` 167.8 ms, the same with the module imported 307.3 ms. Roughly half of it
is the parser walking the 39 KB signed module, about half of *that* the
Authenticode block, which is the module's only integrity evidence and cannot be
removed. The overwhelming majority of those sessions never type a slash path.

Moving the module's logic into Rust was rejected on measurement, not taste:
`fwdslash shell-resolve /etc` costs 20.9 ms against 21.4 ms for a bare
`fwdslash --version`, so the resolution work is already free and every piece
moved to the CLI would **add** a process spawn.

So the block imports nothing. `profile::STUB_BODY` installs six global
functions and re-points the same six aliases:

| Stub | Aliases |
|---|---|
| `Invoke-FswStubChildItem` | `dir`, `ls` |
| `Invoke-FswStubSetLocation` | `cd`, `chdir`, `sl` |
| `Invoke-FswStubPushLocation` | `pushd` |

plus the three helpers they share (`Import-FswStubModule`, `Test-FswStubSlash`,
`Test-FswStubParent`). Each stub scans its own arguments for a leading-`/`
string; finding none it splats straight to the matching
`Microsoft.PowerShell.Management\*` cmdlet — **no import, no registry read, no
spawn**. The first slash argument runs `Import-Module -Name $m -Global -Force`,
whose `Set-Alias -Force` re-points every alias onto the real wrapper, and the
stub re-dispatches by name; every later call in that session reaches the real
wrapper directly.

Three properties are load-bearing and pinned by tests:

- **The `cd` and `pushd` stubs are global advanced functions** carrying the same
  `[CmdletBinding()]` / `param(…)` / `process{}` shape as the module's wrappers,
  for the two reasons already documented for `pushd` — `Push-Location` inside a
  module pushes onto *that module's* stack, and the proxy parameter metadata
  (`ValueFromPipeline` on `Path`, `-StackName`, `-PassThru`, `-UseTransaction`)
  has to survive so binding is identical before and after the flip.
- **`Test-FswStubParent` reproduces the `cd ..`-at-a-distribution-root trigger**
  (#132) from `Get-Location` alone — pure string work, no import — so that case
  still reaches the module without the module deciding it.
- **The orphan self-clean arm survives into the stub block.** Without it an
  uninstalled product's stub would itself become the orphan.

**The aliases go through the `alias:` provider, not `Set-Alias`.** Same
definition, same `AllScope` option, same global scope — but `Set-Alias` lives in
`Microsoft.PowerShell.Utility`, and the first Utility cmdlet of a session costs
~70 ms of module load all by itself (`Get-Date` alone measures the same). The
stubs reach only for `Microsoft.PowerShell.Management`, which the native
passthrough needs anyway. Do not "simplify" this back to `Set-Alias`.

Measured on the same host, x64, 15 runs, median, dot-sourcing a scratch block
against a module padded to the installed 39 KB (an unsigned copy — the harness
runs under `-ExecutionPolicy Bypass`, so this counts parse cost only, not
signature verification):

| | median |
|---|---|
| bare session, no profile | 163.7 ms |
| harness floor (dot-source an empty file) | 163.0 ms |
| **old block** (`Import-Module`) | **290.2 ms** |
| **new stub block** | **214.3 ms** |
| new stub block + a native `cd <dir>` | 227.6 ms |
| old block + first `cd /etc` | 368.4 ms |
| new stub block + first `cd /etc` | 378.8 ms |

So the per-session cost drops from ~127 ms to ~51 ms — a ~76 ms saving on every
session, not the whole ~140 ms. The residual is **not** the stub text: six
global function definitions measure at the noise floor (+1 ms). It is the
Management module load that the `alias:` writes trigger, which any actual `dir`
or `cd` in that session would pay regardless. Removing it would mean not
installing aliases at all.

The first slash path costs ~10 ms more than before (the import now happens
mid-command instead of at startup) and every later one is free.

The block is still byte-identical across releases (#127): no version, no
transaction id, and the paths point at the version-free `payload` directory.
It is ~4.3 KB of PowerShell against the module's 39 KB.

`VERIFY_SCRIPT` therefore accepts **either** name for the `dir`/`ls` alias —
the stub in a fresh session, the real wrapper once the module has loaded.

`test/powershell/ForwardSlashWindows.LocationRegression.ps1` extracts
`STUB_BODY` out of `crates/fsw-cli/src/adapters/profile.rs` rather than
duplicating it, and covers the pre-load native passthrough, the stub metadata,
the first-slash flip, and the metadata after the flip.

**Content drift, and why the classifier had to learn about it.** #127 took the
version out of the fence so an upgrade would stop rewriting `Documents`. The
cost is that a stable fence can no longer say anything about the block's body:
`classify_profile` called every non-legacy, module-present block `Healthy`, and
nothing anywhere compared the deployed block's text against `block_text`. A
machine already carrying the eager-import block on this same version would have
been classified healthy forever and never rewritten — the lazy-stub change would
have shipped and reached only fresh installs.

So `ParsedBlock` now carries `text`, the fence-to-fence region (no blank-line
prefix, so it is directly comparable to `block_text(params)` with
`original_non_empty: false`), `BlockPresence` carries `matches_current`, and a
single stable, module-present block whose body differs is
`ProfileHealth::UpdatePending`. The ranking is unchanged above it: an orphan
outranks it (the only state that throws), a duplicate outranks it, and a legacy
fence outranks it because migration names the more specific repair.

`UpdatePending` is routed through the **existing** confirmation rule, not around
it: `decide_profile_repair` still downgrades every profile-writing verdict to
`NeedsConfirmation` for a background sweep, so the broker's startup sweep and
the settings window's launch sweep report it and write nothing. It is applied
only by an explicit `fwdslash integration <id> enable` or the settings toggle —
`current_version_noop` treats it exactly like a pending migration, which is what
makes a same-version deploy reach an existing install at all.
`fwdslash integrations` reports it as *"installed, but the profile block needs
an update"*, deliberately not as a legacy block.

The #127 property survives intact and is pinned by
`an_unchanged_block_text_writes_nothing`: an upgrade that does not change the
block text leaves every deployed block matching, so the health is `Healthy` and
the action is `Nothing` — for a user action and a sweep alike. Drift detection
adds a comparison, never a write.

`upgrade()` already did a content comparison of its own
(`find_subslice(&current, &desired)`), but only on the version-bump path, so it
could never fire for a same-version change and never reported anything on the
sweep path. It is unchanged; the classifier is now the general answer.

### 4. Registry string decoding and 0.0.2-era upgrades

The CLI decodes `REG_SZ`/`REG_EXPAND_SZ` data into UTF-16 code units *before*
stripping NUL terminators (0.0.2 stripped zero bytes first and lost the final
ASCII character of every value it read), and tolerates exactly one missing
trailing character when comparing the live `AutoRun` against the marker's
`InstalledAutoRun`, so 0.0.2-era installs can still be upgraded. Orphaned
`PowerShell\<version>` module directories left by that era are pruned on
uninstall, on the `fwdslash uninstall` sweep, and after every successful
PowerShell `enable`.

## Planned, not yet implemented

Each of these needs its own entry with a test before the milestone that lands it
can close.

- **A second rendered path form.** `unc_win32` (`\\?\UNC\wsl.localhost\…`) for raw
  Win32 file calls, alongside `unc_display` for the shell. `unc_display` is
  effectively frozen: the provider root renders as exactly `\\wsl.localhost`,
  and `is_valid_windows_root` rejects that literal as a folder root specifically
  so the two can never be confused (Resolver §6).
- **A bounded Enter deadline.** There is none today: the hook swallows Enter,
  posts to the worker and returns immediately, and the worker takes as long as
  the surface takes. Nothing is lost or duplicated — a request whose foreground
  window changed is dropped rather than replayed (Broker §2) — but a slow
  surface still delays the *replayed* Enter it produces. A deadline after which
  the worker abandons and replays would bound that; if one lands,
  `docs/compatibility.md`'s "No lost, duplicated, or delayed Enter behavior"
  gate needs restating with the number.
