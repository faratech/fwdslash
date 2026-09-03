# Privacy Policy — fwdslash (Forward Slash Windows)

**Last updated:** 3 September 2026
**Publisher:** Fara Technologies LLC

## Summary

fwdslash collects nothing, stores no personal data, and makes no network
connections. Everything it does happens locally on your computer.

The product is open source under the MIT License. Every claim below can be
checked against the source at <https://github.com/faratech/fwdslash>.

## What the app does not do

- It does **not** collect, store, or transmit personal data.
- It does **not** contain analytics or telemetry of any kind.
- It does **not** make network connections. None of the shipped executables
  (`fwdslash.exe`, `fswbroker.exe`, `fswsettings.exe`) import any networking
  library.
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
`/`, the keystroke is replayed unchanged and nothing further happens.

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

Open an issue at <https://github.com/faratech/fwdslash/issues>.
