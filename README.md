# fwdslash

**Type Linux paths in Windows.** `/etc/apt` in the File Explorer address bar opens `\\wsl.localhost\Ubuntu\etc\apt`.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)
![Platform](https://img.shields.io/badge/platform-Windows%2011-0078D6?style=flat-square&logo=windows)
![Rust](https://img.shields.io/badge/Rust-1.98-000000?style=flat-square&logo=rust)
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
- **Stays out of the way** — pause it from the one tray icon without uninstalling anything
- **No telemetry, no account** — the Store build makes no network connections at all

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

Want `/` to open something else entirely? Point it at any folder on your machine — a real directory or one inside a distro:

```powershell
fwdslash bare-slash root C:\code                            # / becomes C:\code
fwdslash bare-slash root \\wsl.localhost\Ubuntu\home\me   # or a folder inside a distro
fwdslash bare-slash root                                    # clear it
```

```text
/                →  C:\code
/proj/build      →  C:\code\proj\build
/Ubuntu/home     →  still the distribution — distro paths always win
```

---

## Install

**Requires** Windows 11 and WSL with at least one distribution installed. With no distribution registered there is nothing for a slash path to open.

### Microsoft Store (recommended)

[**Get it from the Microsoft Store**](https://apps.microsoft.com/detail/9P51CM0MTMK2) — Store ID `9P51CM0MTMK2`. The Store carries the Windows App Runtime dependency and keeps the app updated for you.

### GitHub

One line, no administrator rights — the release is signed with a publicly trusted certificate:

```powershell
powershell -ExecutionPolicy Bypass -File Install-fwdslash.ps1
```

The script downloads the latest signed `.msixbundle`, installs the [Windows App Runtime 2.x](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads) first if it is missing, and registers the package for the current user. This build updates itself from GitHub (daily check, switched off in Settings).

Install one flavor or the other. Both register the same startup task and the same `fwdslash` alias, so with both installed only one broker survives the logon race; `Install-fwdslash.ps1` refuses to install over a Store install unless you pass `-Force`.

### Build from source

```powershell
cargo build --release --target aarch64-pc-windows-msvc --workspace
.\target\aarch64-pc-windows-msvc\release\fwdslash.exe install
```

Use `x86_64-pc-windows-msvc` on an Intel/AMD machine. An unpackaged build needs the [Windows App Runtime 2.x](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads) for your architecture installed separately; the Store and GitHub packages declare it as a dependency.

Uninstalling from the command line is one command:

```powershell
fwdslash uninstall
```

Removing the **package** (Store or Settings > Apps) does not sweep the shell adapters, because an uninstalling MSIX runs no code. Turn the Command Prompt and PowerShell integrations off in the settings app before you uninstall.

---

## Terminals

Command interpreters parse `/` before the filesystem ever sees it — `cmd.exe` reads it as a switch, and to a filesystem API a bare `/` means the current drive root. So terminal support is an opt-in adapter rather than a global hook.

```powershell
fwdslash integration cmd enable
fwdslash integration windows-powershell enable
fwdslash integration powershell enable
```

Each adapter records exactly what it replaced and restores it byte-for-byte when you turn it off, and refuses to overwrite anything another program changed in the meantime. Open a **new** shell afterwards — running ones can't reload their profile.

With an adapter on, `dir`/`ls` **and** changing directory both take slash paths:

```text
cmd          cd /Ubuntu/etc     chdir /Ubuntu     pushd /Ubuntu   (cd /d /Ubuntu too)
PowerShell   cd /Ubuntu/etc     chdir  sl  pushd
```

`cmd.exe` cannot make a UNC path current, so the cmd adapter enters the target with `pushd`, which maps a temporary drive letter; `popd` or closing the window releases it. PowerShell moves there directly.

DIR's own switches stay native: `dir /b`, `dir /s`, `dir /w`, `dir /a:d` and friends are handed straight to the shell, even in default-distribution mode where `/b` would otherwise look like a path.

`cd /Windows` no longer means `C:\Windows` in PowerShell — a slash path that the resolver rejects is reported with the resolver's own message instead of silently landing on the current drive. Use `cd \Windows` for that.

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
fwdslash cmd-list /path           Shell adapter DIR (exit 3 = run native DIR)
fwdslash cmd-cd /path             Directory for the cmd CD/PUSHD macros
fwdslash shell-resolve /path      One JSON line for the PowerShell module
fwdslash doctor /path | --all     Diagnose a path
fwdslash bare-slash [list|default [Distro]]
fwdslash bare-slash root <WindowsPath>
fwdslash integrations [--json]
fwdslash integration <name> enable|disable
fwdslash pause | resume           Pause resolution, keep integrations
fwdslash settings [section]       Open the settings app
fwdslash start | stop | install | uninstall
fwdslash version                  Print the running version
```

---

## Settings

The product shows **one** notification-area icon, and the resident broker owns it. Left click opens the settings window; right click gives you an **Enabled** toggle, **Open WSL root**, **Open distribution** (one entry per registered distribution), **Integrations**, and **Exit**.

The settings window itself is a plain window: no icon of its own, and closing it closes the app — the broker keeps running and keeps the tray icon. Every integration is independent, and turning one off runs its reversible uninstall. The General **Disable** switch pauses resolution without forgetting what you installed.

Outdated shell integrations are upgraded automatically — the broker does it at every start, and the settings window repairs any adapter left behind by an older release as soon as it opens. The About page lists the live broker state, each adapter's deployed payload version, and the package version and flavor.

Needs the [Windows App Runtime 2.x](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads) for your architecture. The Store and GitHub packages declare it as a dependency and pull it in; an unpackaged build from source needs it installed separately.

---

## Tests

```powershell
cargo test -p fsw-path -p fsw-core                              # resolver + registry contract
cargo test -p fwdslash --bins --target x86_64-pc-windows-msvc   # shell adapters
.\tools\Test-Sandbox.ps1                                        # lifecycle smoke test
```

`cargo test -p fsw-path` runs on Linux/WSL with no target flag, so it is the fastest check for a resolver change.

---

## Filesystem driver

`driver/` holds an optional minifilter that would extend explicit `/Ubuntu/...` paths to PowerShell, .NET, Python and anything else reaching Windows filesystem APIs. It is **production-gated and not part of any release.**

> **Do not load the unsigned driver on a physical machine.** Build and test it only in a checkpointed Hyper-V guest. See [`SECURITY.md`](SECURITY.md) and [`docs/compatibility.md`](docs/compatibility.md).

[`docs/driver-lab.md`](docs/driver-lab.md) is the operator runbook for that guest: how to create it, why Windows Sandbox cannot be used, and how to run the release gate in it.

---

## Docs

- [`docs/architecture.md`](docs/architecture.md) — trust boundaries and design
- [`docs/compatibility.md`](docs/compatibility.md) — what's verified, and what isn't
- [`PRIVACY.md`](PRIVACY.md) — no data collected; the Store build makes no network calls, the GitHub build checks for updates

---

## License

MIT — see [LICENSE](LICENSE). By Mike Fara, Fara Technologies LLC, New York.

<p align="center">
  <b>Star this repo if you find it useful!</b><br>
  <a href="https://github.com/faratech/fwdslash">https://github.com/faratech/fwdslash</a>
</p>

<p align="center">
  <a href="https://apps.microsoft.com/detail/9P51CM0MTMK2?mode=direct">
    <picture>
      <source media="(prefers-color-scheme: light)" srcset="https://get.microsoft.com/images/en-us%20light.svg">
      <img src="https://get.microsoft.com/images/en-us%20dark.svg" width="220" alt="Get fwdslash from the Microsoft Store">
    </picture>
  </a><br>
  <sub>Available on the Microsoft Store as <b>fwdslash</b> — Store ID <code>9P51CM0MTMK2</code></sub>
</p>
