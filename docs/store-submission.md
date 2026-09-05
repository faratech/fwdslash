# Microsoft Store submission

Product: **fwdslash** — Store ID `9P51CM0MTMK2`, publisher `windowsforum`.

## 1. Package identity

These are the defaults in both packagers, so producing an uploadable bundle is
one command once the binaries are built. **The shipping product is the Rust
tree**, so build it first and then package with `-BinarySource Rust` (the
`Cpp` default stages the C++ reference build, which is not what is on the
Store):

```powershell
cargo build --release --target aarch64-pc-windows-msvc --workspace
cargo build --release --target x86_64-pc-windows-msvc  --workspace
.\tools\Package-Msix.ps1 -BinarySource Rust
```

From WSL, `python3 tools/package_msix.py` does the same thing: it stages the
same three Rust exes and the `shell/` payload out of the repo, and shells out
to `makeappx.exe`/`makepri.exe` through `wslpath`. Both read the version from
`workspace.package.version` in the root `Cargo.toml`.

`release.yml` Authenticode-signs `shell/powershell/ForwardSlashWindows.psm1`
with the Trusted Signing kit *before* packaging, so the GitHub bundle carries a
signed module (both packagers copy that file byte-for-byte, and the Store's
re-signing of the package leaves an embedded script signature alone). A Store
bundle built by hand from an unsigned tree ships the module unsigned, which
changes nothing functional — script signing is integrity only and does not
affect the execution-policy behaviour documented in `docs/compatibility.md`.

| Field | Value |
|---|---|
| `Package/Identity/Name` | `32827MikeFara.fwdslash` |
| `Package/Identity/Publisher` | `CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4` |
| `Package/Properties/PublisherDisplayName` | `WindowsForum.com` |
| Package Family Name | `32827MikeFara.fwdslash_t6j5qexy2jpp2` |

Note the publisher is the Partner Center GUID subject, **not** `CN=WindowsForum`
(which is what the sibling wfdiag sideload package used).

`-IdentityName` is also the PRI resource-map name, so it must be set at *build*
time — it cannot be patched into a finished package.

`C:\code\wfdiag-selfsign.pfx` has the same subject as the Store publisher, so it
signs a package with the real Store identity for local install testing:

```powershell
.\tools\Package-Msix.ps1 -BinarySource Rust -CertificatePath C:\code\wfdiag-selfsign.pfx -CertificatePassword '<pw>'
Add-AppxPackage out\msix\fwdslash-<version>.msixbundle
```

Upload `out\msix\fwdslash-<version>.msixbundle` **unsigned**. The Store re-signs
it; a self-signature would be discarded.

## 1a. Automatic publishing

Since 0.0.4 a release publishes itself to the Store. Nothing here has to be
done by hand for a normal version bump — tag, and the submission goes out.

### The flow

1. `.github/workflows/release.yml` builds both architectures, packages and
   Trusted Signing-signs the **GitHub flavor**, then packages the tree a second
   time with the packager's default identity — the **Store flavor** — into
   `out\msix-store`, leaves it unsigned, and renames it
   `fwdslash-<version>-store-unsigned.msixbundle`.
2. `tools/Test-StoreBundle.ps1` validates that bundle before it is attached:
   Identity `Name`/`Publisher`/`Version`, both `x64` and `arm64` packages, the
   full runtime payload including the `shell\` adapters, `runFullTrust` and
   *not* `unvirtualizedResources`, no `AppxSignature.p7x`, and nothing with a
   `.sys`/`.pdb`/`.inf` extension. Run it locally after `Package-Msix.ps1`
   whenever the payload changes.
3. Both bundles and the ZIPs are attached to the GitHub release, and the
   release body gains a `## Downloads` table saying which is which.
4. The `publish-to-store` job dispatches `publish-to-store.yml` with the short
   version. It downloads **only** the `*-store-unsigned.msixbundle` asset,
   configures `msstore`, derives the Store "What's new" from the release body,
   uploads with `--noCommit`, sets the release notes on every listing, then
   commits and polls the submission.

The Store artifact is deliberately not installable. Both the GitHub installer
(`tools/Install-fwdslash.ps1`) and the in-app updater
(`extract_bundle_url` in `crates/fsw-core/src/update.rs`) skip any asset ending
in `-store-unsigned.msixbundle`, so the second bundle on a release cannot be
mistaken for the signed one.

A dry run (`workflow_dispatch` with `dry_run` left at its default) still builds
and validates the Store bundle and uploads it as a workflow artifact, but
creates no release and dispatches no publish — the Store job is gated on the
same condition as the release step.

### Secrets

Names only; the values live in Settings > Secrets and variables > Actions.

| Secret | Used by | What it is |
|---|---|---|
| `STORE_TENANT_ID` | publish-to-store, check-store-submission | The Azure AD tenant of an app registration **associated with the Partner Center account**. Preferred over the `AZURE_*` fallback below — optional, but see the failure signature further down for why it may be required. |
| `STORE_CLIENT_ID` | same | Same application. |
| `STORE_CLIENT_SECRET` | same | Same application. |
| `AZURE_TENANT_ID` | release, publish-to-store (fallback), check-store-submission (fallback) | The Azure AD tenant of the Trusted Signing service principal. Already present, and used by `release.yml` regardless. Both workflows fall back to this trio when `STORE_*` is not set — but it is a **different identity** than the Store one and is not guaranteed to be associated with Partner Center; see below. |
| `AZURE_CLIENT_ID` | same | Same application. |
| `AZURE_CLIENT_SECRET` | same | Same application. |
| `STORE_SELLER_ID` | publish-to-store | The Partner Center **seller ID** (Account settings > Organization profile > Legal info). Not the Store ID, and not public. |

Both workflows resolve credentials the same way, expressed once per value:
`${{ secrets.STORE_TENANT_ID || secrets.AZURE_TENANT_ID }}` (and the same
pattern for client ID / client secret). The "Check the Store credentials are
configured" step in `publish-to-store.yml`, and the equivalent check in
`check-store-submission.yml`, log which side of the `||` resolved — the literal
words `STORE_*` or `AZURE_* fallback` — never a secret value.

The Store ID `9P51CM0MTMK2` is public (it is in the README and the Store URL),
so it is a workflow `env:` constant, not a secret.

### When the upload fails with "Could not retrieve your application"

Run [33955525282](https://github.com/faratech/fwdslash/actions/runs/33955525282)
(the first automatic Store submission) failed at "Upload package to draft
submission" with `msstore publish` printing, three times:

> Could not retrieve your application. Please make sure you have the correct AppId.

The AppId (`9P51CM0MTMK2`) was correct. Reproducing the same call directly
against the Partner Center REST API (`https://manage.devcenter.microsoft.com`)
showed the actual cause: the Azure AD app in `AZURE_TENANT_ID` /
`AZURE_CLIENT_ID` — the Trusted Signing service principal — obtains an OAuth
token fine, but the submission API rejects it with **HTTP 401 "Identity cannot
be authorized"**. The app simply isn't associated with this Partner Center
account; msstore's CLI turns that 401 into the generic AppId message, which is
misleading. (The sibling repo `faratech/wfdiag` uses a *different* Azure AD app
for its Store submission under the same `AZURE_*` secret names, which is how
this went unnoticed — the fallback here mirrors that split.)

The fix is one of:

- Add the Azure AD application (the one behind whichever tenant/client ID is
  in use) under Partner Center > **Account settings > User management > Azure
  AD applications**, with the **Manager** role, in the tenant that is linked to
  the account — not just the tenant that owns Trusted Signing.
- Or configure dedicated `STORE_TENANT_ID` / `STORE_CLIENT_ID` /
  `STORE_CLIENT_SECRET` secrets for an app that is already associated, so
  publishing stops depending on the Trusted Signing app's identity at all.

Once fixed, re-run the same release rather than re-tagging:

```bash
gh workflow run publish-to-store.yml -f version=0.0.4
```

### Publishing an existing release by hand

Actions > **Publish to Microsoft Store** > Run workflow, with the short version
(`0.0.4`, no `v`). The release must already carry a
`*-store-unsigned.msixbundle`. Optionally paste `release_notes` to override the
Store "What's new"; leave it blank to derive it from the release body.

From the CLI:

```bash
gh workflow run publish-to-store.yml -f version=0.0.4
```

Re-running is safe: `msstore publish` deletes the pending submission and
creates a fresh one. What is *not* safe is re-publishing a version the Store
already has — every submission's version must be strictly higher than the last
published one, so a rerun for a shipped version is rejected by Partner Center.

### When a submission is stuck

`msstore publish` deletes any existing pending submission before creating its
own. That delete has been seen to 504 after ~80 s and return an HTML error
page, which crashes the CLI mid-parse; the workflow retries three times with a
60 s backoff and then fails with a message saying so. When that happens the
pending submission is wedged and every retry will fail the same way.

Run Actions > **Check Microsoft Store Submission** with the submission ID to
see what Partner Center believes the state is, and tick `cancel_submission` to
clear it. The cancel refuses unless Partner Center itself reports that ID as
the active *pending* submission, so it cannot touch the live listing. Cancelling
from the Partner Center portal works too. Once the pending pointer is clear,
re-run **Publish to Microsoft Store** for the same version.

## 2. Capabilities (resolved — none beyond runFullTrust)

The package previously declared the restricted `unvirtualizedResources`
capability for targeted virtualization exclusions, which required Microsoft
approval. Clean-room testing (2026-09-04, docs/compatibility.md) showed the
exclusions were solving the wrong problem: the shell-facing registry state the
adapters depend on is now written through `reg.exe` — a System32 child process
without package identity — so it lands in the real hive regardless of MSIX
virtualization. The manifest declares only `runFullTrust`, which needs no
approval, and App Installer can sideload the package without restriction.

## 3. Notes for certification

The broker installs a system-wide low-level keyboard hook. That is legitimate
here but resembles a keylogger to an automated scan, so state plainly:

> The background process installs a `WH_KEYBOARD_LL` hook that inspects **only**
> `VK_RETURN`. Every other key is passed through untouched on the first line of
> the callback. Enter is acted on only when the foreground window is one of four
> recognized navigation surfaces (File Explorer, the Run dialog, Windows Search,
> a classic Open/Save dialog) **and** the focused control's text begins with `/`.
> In every other case the keystroke is replayed unmodified.
>
> No keystroke content is recorded, stored, or transmitted. Diagnostics log event
> categories only, never user-entered or resolved paths.
>
> The package's **only** network use is an update path that is opt-in and **off
> by default** in this flavor, and it talks exclusively to Microsoft's own Store
> update service. Nothing else in the package opens a socket. The default is
> decided at runtime from the package family, so a Store install ships with the
> **Automatic updates** switch off and performs no check until the user turns it
> on.
>
> With it on, the app asks `Windows.Services.Store.StoreContext` — at most once
> a day — whether a newer version of this product has been published. It
> installs one only through the Store, and only for its own product id
> (`9P51CM0MTMK2`, a constant in the binary): first
> `AppInstallManager.StartProductInstallWithOptionsAsync`, the same sequence
> Windows Package Manager uses, called in-process from the packaged app and, if
> that call is refused, from an identity-less copy of the app's own signed
> executable; if that route is unavailable at all it degrades to `StoreContext`'s
> own silent download and install, and below that to
> `winget upgrade --id 9P51CM0MTMK2 --source msstore`, which is the same service
> again. If none of those can run, the app only tells the user there is an
> update and offers to open the Store listing.
>
> It downloads no code from anywhere but the Store, installs no third-party
> software, and carries no mechanism to install any product other than this one.
> A single registry value (`HKCU\Software\ForwardSlashWindows\Settings`,
> `UpdateRoute`) pins or disables individual routes without a rebuild, so the
> `AppInstallManager` route can be turned off in a servicing update if it is ever
> found objectionable.
>
> UI Automation is used to read and rewrite the focused address/filename control
> in those same four surfaces — this is how a typed `/etc/apt` becomes
> `\\wsl.localhost\Ubuntu\etc\apt` before Windows acts on it.
>
> Enter is processed on a dedicated worker thread, not in the hook callback, and
> only when the focused element is an editable, non-password Edit or ComboBox
> with a writable ValuePattern. A control the app could not write back to is
> never read, so a Find box or a password field in one of those windows passes
> Enter through untouched.
>
> The hook can be turned off at any time from the notification-area icon or the
> settings app, and is removed entirely when the feature is disabled.
>
> Full source: https://github.com/faratech/fwdslash (MIT).

**The update path needs the published version to increase.** The Store already
requires that of each submission, and the app's own update depends on it twice
over: `StoreContext` reports an update only for a higher published version, and
the watchdog that brings the app back after an install waits for
`Get-AppxPackage` to report a version greater than the one that was running.
**0.0.5 is the first release that carries this feature** — a 0.0.4 or earlier
Store install has no update code path at all, so the first self-update anyone
can observe is a 0.0.5 install being offered 0.0.6.

## 4. Listing requirements not satisfied by the package

- **Privacy policy URL** — required, because the app installs a keyboard hook.
  Must state that keystrokes are not collected.
- **Screenshots** — at least one 1366×768 or larger. Frames from the README demo
  under `out\readme-demo\<timestamp>\` are suitable sources.
- **Store logo** 300×300 (uploaded in Partner Center; not part of the package).
- Description, age rating questionnaire, markets, pricing.
- Support contact.

## 5. Known gaps at first submission

- **Startup task is `Enabled="true"`**, but it only fires at *logon* and MSIX
  runs nothing at install time. Opening the app therefore starts the broker if
  it is not already running (`ensure_broker_running`, called from `create()` in
  `crates/fsw-settings/src/main.rs`).
  A reviewer who installs and immediately tests a slash path without opening the
  app first will see nothing happen, so the certification notes tell them to
  open the app or run `fwdslash start`.
- **Same-version reinstall is blocked** (`0x80073CFB`). Iterating locally needs
  `Remove-AppxPackage` first, or a version bump.
- **Uninstall leaves the adapters behind.** MSIX has no uninstall hook, so
  `Remove-AppxPackage` does not run the cmd/PowerShell rollback transactions.
  The `AutoRun` value, the PowerShell profile blocks and
  `%LOCALAPPDATA%\ForwardSlashWindows` survive package removal. Disable the
  integrations from the settings app before uninstalling.
- **Version is `0.0.6.0`.** Each submission must increase it, and the fourth
  field is reserved by the Store (always `0`). It comes from
  `workspace.package.version`, and the statement above is one of the locations
  `tools/bump_version.py` rewrites — bump with `python3 tools/bump_version.py
  <x.y.z>` rather than editing it, and CI's `--check` will catch it if this
  line ever falls behind the workspace. The three-field version in the tag and
  in `Cargo.toml` always gains the reserved `.0` here.

## 6. Current verification checklist

For a candidate build, install the self-signed bundle with `Add-AppxPackage`
(`Remove-AppxPackage` first when testing the same version) and confirm the
following before upload. This is a release checklist, not evidence that any
particular host, logon path, or package install has already been verified.

- Package family name resolves to `32827MikeFara.fwdslash_t6j5qexy2jpp2`.
- `windows.appExecutionAlias` puts `fwdslash.exe` on PATH at
  `%LOCALAPPDATA%\Microsoft\WindowsApps`, and `fwdslash version` prints
  the package identity version derived from the current Cargo workspace
  version with its reserved fourth `.0` field.
- The packaged controller runs and reports broker/integration state.
- The packaged controller's writes reach the **real** hive: packaged
  `fwdslash disable` must flip `HKCU\Software\ForwardSlashWindows\Settings`
  `Disabled` as read from an *unpackaged* shell. This works because the writes
  go through `reg.exe`, not because of a virtualization exclusion — there is
  none (see §2).
- The settings app launches from `WindowsApps` with a window, confirming the
  Windows App SDK bootstrap initializer is correctly suppressed.
- `windows.protocol` activation works and `fwdslash://settings/terminals`
  selects the Terminals page, so activation still delivers the URI as a
  command-line argument (`initial_section` in `crates/fsw-settings/src/main.rs`).

- **One notification-area icon.** Launch from Start: exactly one icon. Close the
  settings window: `Get-Process fswsettings` is empty and the icon stays.
  Relaunch while the window is open: same PID, window raised.
- **Tray menu.** Open settings, the Enabled check, Open WSL root, Open
  distribution (one item per registered distribution), Integrations, Exit; a
  second right-click opens cleanly.
- **cd adapters from the packaged payload.** In a new console:
  `cd /Ubuntu`, `cd /d /Ubuntu`, `pushd /Ubuntu` + `popd`, and `dir /b` staying
  native. In a new PowerShell session: `cd /Ubuntu`, `pushd /Ubuntu` + `popd`,
  and `cd ..` / `cd C:\` untouched.
- **Packaged adapters enable.** A fresh packaged
  `fwdslash integration cmd enable` produces a real `Command Processor AutoRun`
  value, and the staged payload under `%LOCALAPPDATA%\ForwardSlashWindows\cmd`
  contains `fsw-cd.cmd` and `fsw-pushd.cmd`.
- **Adapters auto-upgrade from an older payload.** With an older adapter
  installed, starting the broker upgrades it on its own and reports the current
  package version in a balloon; opening the settings window with one still
  outdated repairs it too ("Updating terminal integrations…" → "Terminal
  integrations updated"). Neither needs a button.
  Confirm afterwards that `fwdslash integrations` no longer prints
  "(update available)" and that the payload directory is
  `%LOCALAPPDATA%\ForwardSlashWindows\cmd\<current-version>`.

### Historical evidence

The first self-update-capable Store build was version 0.0.5; an earlier Store
install therefore could not exercise this path. This is dated release-history
context only, not current verification evidence. Logon-startup and fresh
package-install behavior require their own candidate-build validation.
