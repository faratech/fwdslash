# Privacy Policy — fwdslash (Forward Slash Windows)

**Last updated:** 5 September 2026
**Publisher:** WindowsForum.com (the Microsoft Store publisher display name).
Developed by Mike Fara, Fara Technologies LLC.

## Summary

fwdslash collects nothing and stores no personal data. Everything it does with
your paths happens locally on your computer.

**Network.** Both builds can check for their own updates, and both do it under
the same **Automatic updates** switch in Settings — on by default in the
**GitHub** build, off by default in the **Microsoft Store** build. Which flavor
you have is decided at runtime from the package identity, never at build time.
A check runs only while the app is installed as a package, and at most once per
day.

With the switch on:

- the **GitHub** build sends a plain `GET` to `api.github.com` for the latest
  release and, when a newer version exists, downloads that release's signed
  package from `github.com`;
- the **Store** build asks Microsoft's own Store update service whether a newer
  version of this app has been published, and — when you let it install one —
  asks the same service to install it. Its only fallback, used when the Store
  declines a silent install, is Windows Package Manager (`winget`) against its
  `msstore` source, which is that same service. The Store build never downloads
  code from anywhere but the Store.

Neither build's requests carry identifiers — no account, no machine id, no
installation id, no usage data — and nothing is uploaded. With the switch off,
nothing checks on a timer and neither build makes a network connection of its
own; the only way to reach the network then is to press **Check now** in
Settings yourself.

The product is open source under the MIT License. Every claim below can be
checked against the source at <https://github.com/faratech/fwdslash>
(`crates/fsw-core/src/update.rs` and `crates/fsw-cli/src/update/` are the whole
of the network code).

## What the app does not do

- It does **not** collect, store, or transmit personal data.
- It does **not** contain analytics or telemetry of any kind.
- It does **not** send anything anywhere. The only outbound requests are the
  update checks described above.
- It does **not** log keystrokes, or record what you type.
- It does **not** show advertising, and there are no third-party SDKs.

## What the app reads, and why

**Keyboard.** A background process installs a system-wide low-level keyboard
hook. The hook examines the virtual key code only, and passes every key other
than **Enter** straight through untouched. No key is recorded anywhere.

**The focused text box, on Enter only.** When you press Enter, and only when the
active window is one of four navigation surfaces — the File Explorer address
bar, the Run dialog, Windows Search, or a classic Open/Save dialog — the app
reads the text of the focused control through Windows UI Automation. If that
text begins with `/`, it is translated into the equivalent WSL path (for
example `/etc/apt` becomes `\\wsl.localhost\Ubuntu\etc\apt`) and written back to
the control so Windows opens the right location. If the text does not begin with
`/`, the keystroke is replayed unchanged and nothing further happens. Before
any read, rewrite, replay, or Escape action, the app rechecks that both the
original foreground window and exact focused control are still current; a stale
request is dropped rather than redirected to a later window.

The text is only read at all when the focused control positively reports an
editable, non-password Edit or ComboBox with a writable ValuePattern. A failed
UI Automation property query is a rejection, not a guess. A Find box, password
field, read-only control, or unavailable UIA property is never read, and the
keystroke passes through untouched.

This text is used immediately, in memory, and is never written to disk or sent
anywhere.

**Your WSL distribution names.** The app reads the list of installed
distributions from `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Lxss`,
which is where Windows Subsystem for Linux records them. This is a read-only
lookup used to decide whether a path such as `/Ubuntu/...` names a real
distribution.

## What the app writes to your computer

All of it is local, and all of it is reversible from the settings app.

| Location | Purpose |
|---|---|
| `HKCU\Software\ForwardSlashWindows` | Your settings: enabled/disabled, bare-slash behaviour, chosen default distribution. |
| `HKCU\Software\Microsoft\Command Processor` (`AutoRun`) | Only if you enable the Command Prompt integration. Loads the `dir`/`ls` adapter in new Command Prompt sessions. The previous value is recorded first and restored exactly on removal. |
| `Documents\WindowsPowerShell\profile.ps1`, `Documents\PowerShell\profile.ps1` | Only if you enable a PowerShell integration. A marked block that imports the adapter module. The original file is snapshotted first and restored byte-for-byte on removal. |
| `%LOCALAPPDATA%\ForwardSlashWindows` | The adapter files that Command Prompt and PowerShell load. |
| `HKCU\Software\ForwardSlashWindows\Settings` (`AutoUpdate`) | Both builds. Whether the daily update check runs. Absent means on for the GitHub build and off for the Store build. |
| `HKCU\Software\ForwardSlashWindows\Settings` (`LastUpdateCheck`) | Both builds. A timestamp, so the check runs at most once a day. |
| `HKCU\Software\ForwardSlashWindows\Settings` (`AvailableUpdate`) | Both builds. The version or release tag of an update that is waiting, so the notice survives a restart. |
| `HKCU\Software\ForwardSlashWindows\Settings` (`UpdateRoute`) | Both builds. Optional and never written by the app: set it by hand to pin one install route, or to `notify` to stop the app installing anything by itself. |
| `%LOCALAPPDATA%\ForwardSlashWindows\update` | Both builds. Updater storage: a GitHub download is kept as a `*.part` file until atomically promoted, `update-attempt.lock` holds an attempt-owner token, and `fwdslash uninstall` cancels owned update tasks before sweeping this directory. |
| `%LOCALAPPDATA%\ForwardSlashWindows\update\fwdslash-helper.exe` | Only while an update installs. A byte-identical copy of the app's own command-line executable, run without package identity — the only way to ask the Store to install an update over the running app, and to register a downloaded GitHub package while it is in use. The next install overwrites it and `fwdslash uninstall` removes it. |
| `%LOCALAPPDATA%\ForwardSlashWindows\update\last-result.txt` | One word written by that helper — `completed`, `paused`, or `error:` with the Windows error code — so the app can learn how the install ended. The next check or status reads it and deletes it. It records no path and nothing you typed. |
| `%TEMP%\fwdslash-update-<pid>-<sequence>.cmd` and `.xml` | Immutable temporary sidecars for one uniquely named `fwdslash-update-watchdog-<pid>-<sequence>` task. They contain updater commands and package metadata only, never typed paths or keystrokes, and are removed with their owning task. |

**Diagnostics.** The background process writes a diagnostic log only if you set
the `FSW_DIAGNOSTIC_LOG` environment variable yourself. It is off by default.
When on, it records event categories such as `event=route_distribution` — never
the paths you typed and never the paths they resolved to.

**Crash log.** If the settings window fails to start, it writes a short error
message to `%LOCALAPPDATA%\ForwardSlashWindows\settings-crash.log`. It stays on
your computer.

## Children

The app is a developer utility. It is not directed at children and collects no
information from anyone.

## Changes

Any change to this policy will be committed to the repository, so the file
history is the changelog.

## Contact

Published on the Microsoft Store as **WindowsForum.com**; developed by Mike
Fara, Fara Technologies LLC, New York. Open an issue at
<https://github.com/faratech/fwdslash/issues>.
