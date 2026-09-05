# Architecture

## Shell routing

```text
recognized navigation edit receives Enter
                 |
  low-level hook classifies the window
    and posts HWND + surface kind
                 |
   broker WORKER thread (own STA + UIA)
                 |
   focused control must positively report a
   non-password, writable Edit/ComboBox;
   foreground and focused control rechecked
          /                  \
   not a slash path       slash path
          |                  |
     replay Enter      shared resolver
                         /        \
                    invalid       valid
                       |             |
                block + notify   rewrite control
                                 or navigate target
```

The keyboard callback never performs UI Automation or WSL registry access. It
only recognizes a bounded surface, queues work, and suppresses the matching
key-up. Broker-generated replay input carries a private marker so it cannot
loop through the resolver.

Everything after the classification runs on a **worker thread** with its own STA
and its own `IUIAutomation`, reached through a second message-only window
(`ForwardSlashWindows.BrokerWorker`). UI Automation, the resolver, the shell
open, the replay and the Explorer COM navigation all happen there. Windows
removes a low-level hook whose thread exceeds `LowLevelHooksTimeout` and does
not say so, and binding `\\wsl.localhost\<distro>` boots a stopped distribution
— seconds of blocking on the thread that owns every keystroke on the machine.
If the foreground window or focused control changed while the request was
queued or while a blocking UIA/COM call was running, it is dropped rather than
replayed into whatever the user switched to. UIA failures to establish that a
control is non-password and writable fail closed: its text is never read.

The worker is an admission boundary as well as a queue: if it is unavailable,
the hook passes Enter through natively and the broker reports processing as
unavailable. Menu path opens are likewise dropped and reported when they cannot
be posted to the worker; the hook-owning thread never falls back to an inline
shell open. Pause state changes immediately remove or install the hook, then
enter one FIFO persistence queue. Registry writes and their failures are
reported asynchronously, so rapid pause/resume/pause requests cannot persist an
older toggle last.

## Update handoff and cleanup

Each install attempt owns a tokenized `update-attempt.lock` in the updater
directory. Downloaded GitHub bundles remain `*.part` files until atomically
promoted. The helper records a compact outcome in `last-result.txt`; it never
records a user path or keystroke.

The updater creates uniquely named `fwdslash-update-watchdog-<pid>-<sequence>`
tasks. Their `.cmd` and `.xml` sidecars are immutable temporary files, so one
attempt cannot overwrite another's launch plan. The watchdog relaunches only
after it observes a newer package from the exact package family. A 45-minute
wait timeout is a failed handoff, not permission to relaunch an old package.
Uninstall cancels tasks owned by this product before it sweeps updater storage.

Explorer and native dialogs receive a UNC rewrite followed by Enter, preserving
native behavior and the active Explorer tab. Bare `/` is special-cased by the
shared resolver: by default it targets the WSL provider root (with an Explorer
COM navigation fallback for provider-root builds that open UNC roots in a new
window), or, when the per-user bare-slash mode opts in, the default
distribution root. A user-configured custom root (`BareSlashRoot`, a
Rust-layer feature — docs/divergences.md resolver 6) takes any non-distribution
input instead, so `/` opens an arbitrary folder and `/name` resolves inside it. Search opens the validated path directly and dismisses its
flyout.

Diagnostics record event and error categories only. User-entered and resolved
paths are not logged.

## Settings and optional shell adapters

The settings process is a Rust desktop app built on the vendored
`windows-reactor` crate over the Windows App SDK (`crates/fsw-settings`; the
WinUI 3 C++ app in `src/settings/` is the reference implementation it was ported
from). It delegates every state change to `fwdslash`; it never edits profiles or
registry integration state itself, and it runs those invocations on the thread
pool so the window stays responsive. The app uses a Mica system backdrop and the
Windows App SDK `TitleBar` control, with the caption icon in the `LeftHeader`
slot, while retaining the system caption buttons.

The settings window has **no** notification-area icon and no watchdog: the
product's single tray icon belongs to the resident broker, and closing the
window exits the process. Menu commands from that icon deep-link to sections
through the `fwdslash://` protocol — registered per user by `fwdslash install`
for an unpackaged build, and declared as `windows.protocol` in the manifest for
a packaged one.

Command Prompt and the two PowerShell editions have separate transactional
install records. PowerShell installation snapshots the original profile bytes,
writes a uniquely marked import atomically, and verifies that a fresh process
of the selected edition loads the aliases. Failure, including a Controlled
Folder Access denial, restores the original bytes and removes staged state.
Uninstall removes only the recorded byte sequence and refuses to overwrite an
externally changed transaction block.

## Filesystem routing

```text
interactive process opens drive-root path
                 |
        IRP_MJ_CREATE minifilter
                 |
 eligible token + matching SID/session mapping?
          /                         \
         no                         yes
         |                           |
      fail open          first component is distro?
                               /             \
                              no             yes
                              |               |
                           fail open     STATUS_REPARSE to
                                     \??\UNC\wsl.localhost\...
```

The broker connects to the Filter Manager port and publishes a complete,
versioned distro mapping. The driver derives SID and session identity from the
connection token; the client cannot claim another identity. Up to 16
interactive sessions have isolated mappings. Disconnecting the owning broker
atomically clears its slot.

The driver attaches only to disk filesystems. Reparse targets go to the UNC
provider, preventing recursion back through the disk-volume filter. Every
allocation and name-query failure is fail-open.

## Lifecycle and trust boundaries

- User-mode integration runs as one broker per interactive desktop.
- Shell interception accepts only known Windows navigation surfaces or classic
  dialog windows and revalidates focus before acting.
- The optional driver covers standard/elevated interactive users but excludes
  services, SYSTEM/session zero, AppContainers, and low integrity.
- User-mode pause/uninstall always removes the hook without Explorer injection
  or restart.
- Global disable removes the active hook and makes terminal adapters delegate
  to native behavior without uninstalling the user's selected integrations.
- Driver installation remains VM-only until Microsoft signing and release
  validation are complete.
