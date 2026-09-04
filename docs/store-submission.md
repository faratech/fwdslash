# Microsoft Store submission

Product: **fwdslash** — Store ID `9P51CM0MTMK2`, publisher `windowsforum`.

## 1. Package identity

These are baked into `tools/Package-Msix.ps1` as the defaults, so a plain
`.\tools\Package-Msix.ps1` produces an uploadable bundle:

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
.\tools\Package-Msix.ps1 -CertificatePath C:\code\wfdiag-selfsign.pfx -CertificatePassword '<pw>'
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
> categories only, never user-entered or resolved paths. The product makes no
> network connections.
>
> UI Automation is used to read and rewrite the focused address/filename control
> in those same four surfaces — this is how a typed `/etc/apt` becomes
> `\\wsl.localhost\Ubuntu\etc\apt` before Windows acts on it.
>
> The hook can be turned off at any time from the tray icon or the settings app,
> and is removed entirely when the feature is disabled.
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
  it is not already running (`EnsureBrokerRunning` in `src/settings/main.cpp`).
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
- **Version is `0.0.1.0`.** Each submission must increase it, and the fourth
  field is reserved by the Store (always `0`).
## 6. Verified locally (0.0.1.0, ARM64, Windows 11 26200)

Installed from a self-signed bundle with `Add-AppxPackage` and exercised:

- Package family name resolves to `32827MikeFara.fwdslash_t6j5qexy2jpp2`.
- `windows.appExecutionAlias` puts `fwdslash.exe` on PATH at
  `%LOCALAPPDATA%\Microsoft\WindowsApps`.
- The packaged controller runs and reports broker/integration state.
- **Virtualization exclusion confirmed**: the packaged controller wrote
  `Disabled` under `HKCU\Software\ForwardSlashWindows\Settings` and an
  unpackaged `reg.exe` read the new value back. This is the behaviour the
  restricted capability exists for.
- The WinUI settings app launches from `WindowsApps` with a window, confirming
  the bootstrap initializer is correctly suppressed and the package resource map
  resolves `ms-appx:///` lookups.
- `windows.protocol` activation works, and `fwdslash://settings/terminals`
  selects the Terminals page — so protocol activation still delivers the URI as
  a command-line argument, which `InitialSection` in `src/settings/main.cpp`
  depends on.

Not yet verified: the startup task firing at logon (needs a sign-out), and the
cmd/PowerShell adapters reinstalled from the packaged payload.
