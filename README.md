# fwdslash

**Type Linux paths in Windows.** `/etc/apt` in the File Explorer address bar opens `\\wsl.localhost\Ubuntu\etc\apt`.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)
![Platform](https://img.shields.io/badge/platform-Windows%2011-0078D6?style=flat-square&logo=windows)
![C++](https://img.shields.io/badge/C%2B%2B-20-00599C?style=flat-square&logo=cplusplus)
![WSL](https://img.shields.io/badge/WSL-2-E95420?style=flat-square&logo=linux&logoColor=white)
[![GitHub stars](https://img.shields.io/github/stars/faratech/fwdslash?style=flat-square)](https://github.com/faratech/fwdslash/stargazers)

<p align="center">
  <img src="docs/readme-demo.gif" alt="fwdslash in the Run dialog, File Explorer, Windows Search, an Open dialog, Command Prompt, PowerShell, and the settings app" width="100%">
</p>

---

## Why fwdslash?

You already know where the file is. You just can't type it.

WSL files live at `\\wsl.localhost\Ubuntu\...` — fine in a script, miserable to type into an address bar. So type the path you were already thinking of:

- **Works where you already navigate** — File Explorer, Win+R, Windows Search, Open/Save dialogs
- **No driver, no admin** — per-user, reversible, and no Explorer restart
- **Terminals too, if you want** — `dir` and `ls` keep working in Command Prompt and PowerShell
- **Fails safely** — a typo is blocked with an explanation instead of turning into a web search
- **Stays out of the way** — pause it from the tray without uninstalling anything
- **No network, no telemetry, no account**

---

## Where it works

| Surface | Type this |
|---------|-----------|
| File Explorer address bar | `/etc/apt` |
| Run (Win+R) | `/usr/share` |
| Windows Search | `/etc` |
| Open / Save dialogs | `/home/alice` |
| Command Prompt&nbsp;* | `dir /etc/apt` |
| PowerShell 5.1 / 7&nbsp;* | `ls /usr` |

<sub>* optional adapters, off by default</sub>

---

## Paths

```text
/Ubuntu                 →  \\wsl.localhost\Ubuntu
/Ubuntu/home/alice      →  \\wsl.localhost\Ubuntu\home\alice
```

Bare `/` lists your distributions — it never silently picks one. Prefer a default? Turn on default-distribution mode and plain Linux paths work unprefixed:

```powershell
fwdslash bare-slash default          # follow `wsl --set-default`
fwdslash bare-slash default Ubuntu   # or pin one
```

```text
/etc/apt                →  \\wsl.localhost\Ubuntu\etc\apt
/tmp/build/log.txt      →  \\wsl.localhost\Ubuntu\tmp\build\log.txt
```

A registered distribution always wins over a same-named folder, so `/Ubuntu/home` keeps meaning the distribution.

---

## Install

**Requires** Windows 11 and WSL with at least one distribution installed. With no distribution registered there is nothing for a slash path to open.

```powershell
.\tools\Build-UserMode.ps1 -Architecture ARM64 -Configuration Release
.\out\user\arm64\Release\fwdslash.exe install
```

Replace `ARM64` with `x64` or `x86` as needed. Uninstalling is one command and leaves nothing behind:

```powershell
fwdslash uninstall
```

---

## Terminals

Command interpreters parse `/` before the filesystem ever sees it — `cmd.exe` reads it as a switch, and to a filesystem API a bare `/` means the current drive root. So terminal support is an opt-in adapter rather than a global hook.

```powershell
fwdslash integration cmd enable
fwdslash integration windows-powershell enable
fwdslash integration powershell enable
```

Each adapter records exactly what it replaced and restores it byte-for-byte when you turn it off, and refuses to overwrite anything another program changed in the meantime. Open a **new** shell afterwards — running ones can't reload their profile.

Prefer not to touch your shell? These always work:

```powershell
fwdslash list /Ubuntu/etc
fwdslash open /Ubuntu/home
fwdslash resolve /etc/apt
```

---

## Commands

```text
fwdslash status [--json]          Broker, distributions, and driver state
fwdslash resolve /Distro/path     Print the resolved UNC path
fwdslash open|list /Distro/path   Open in Explorer, or list to stdout
fwdslash doctor /path | --all     Diagnose a path
fwdslash bare-slash [list|default [Distro]]
fwdslash integrations [--json]
fwdslash integration <name> enable|disable
fwdslash pause | resume           Pause resolution, keep integrations
fwdslash settings [section]       Open the settings app
fwdslash start | stop | install | uninstall
```

---

## Settings

A WinUI 3 app with a tray icon. Every integration is independent, and turning one off runs its reversible uninstall. The General **Disable** switch pauses resolution without forgetting what you installed.

Needs the [Windows App Runtime 1.8](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads) for your architecture.

---

## Tests

```powershell
.\out\user\arm64\Release\fswcore_tests.exe                        # resolver contract
.\out\user\arm64\Release\fsw_address_bar_integration.exe Ubuntu /usr/share
.\tools\Test-Sandbox.ps1                                          # lifecycle smoke test
```

---

## Filesystem driver

`driver/` holds an optional minifilter that would extend explicit `/Ubuntu/...` paths to PowerShell, .NET, Python and anything else reaching Windows filesystem APIs. It is **production-gated and not part of any release.**

> **Do not load the unsigned driver on a physical machine.** Build and test it only in a checkpointed Hyper-V guest. See [`SECURITY.md`](SECURITY.md) and [`docs/compatibility.md`](docs/compatibility.md).

---

## Docs

- [`docs/architecture.md`](docs/architecture.md) — trust boundaries and design
- [`docs/compatibility.md`](docs/compatibility.md) — what's verified, and what isn't
- [`PRIVACY.md`](PRIVACY.md) — no data collected, no network calls

---

## License

MIT — see [LICENSE](LICENSE). By Mike Fara, Fara Technologies LLC, New York.

<p align="center">
  <b>Star this repo if you find it useful!</b><br>
  <a href="https://github.com/faratech/fwdslash">https://github.com/faratech/fwdslash</a>
</p>
