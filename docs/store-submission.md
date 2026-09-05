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
> categories only, never user-entered or resolved paths. The Store package makes
> no network connections: its self-update code path is gated to the
> GitHub-distributed flavor at runtime (package-family comparison) and cannot
> execute in the Store package.
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
- **Version is `0.0.3.0`.** Each submission must increase it, and the fourth
  field is reserved by the Store (always `0`). It comes from
  `workspace.package.version`; see the version-copy list in CLAUDE.md.

## 6. Verification checklist for the 0.0.3 submission

Install the self-signed bundle with `Add-AppxPackage` (`Remove-AppxPackage`
first — a same-version reinstall is blocked) and confirm each of these before
uploading. Carried over from the 0.0.2 pass and still expected to hold:

- Package family name resolves to `32827MikeFara.fwdslash_t6j5qexy2jpp2`.
- `windows.appExecutionAlias` puts `fwdslash.exe` on PATH at
  `%LOCALAPPDATA%\Microsoft\WindowsApps`, and `fwdslash version` prints
  `0.0.3.0` (the packaged identity version).
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

New in 0.0.3, and the reason this section is a checklist rather than a record:

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
- **Adapters auto-upgrade from a 0.0.1/0.0.2 payload.** With an older adapter
  installed, starting the broker upgrades it on its own and reports
  "Terminal integrations were updated to 0.0.3: …" in a balloon; opening the
  settings window with one still outdated repairs it too ("Updating terminal
  integrations…" → "Terminal integrations updated"). Neither needs a button.
  Confirm afterwards that `fwdslash integrations` no longer prints
  "(update available)" and that the payload directory is
  `%LOCALAPPDATA%\ForwardSlashWindows\cmd\0.0.3`.

Still unverified: the startup task firing at logon (needs a sign-out).
