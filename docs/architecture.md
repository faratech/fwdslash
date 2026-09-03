# Architecture

## Shell routing

```text
recognized navigation edit receives Enter
                 |
      low-level hook snapshots HWND
                 |
       broker message-loop worker
                 |
     UIA value + surface revalidated
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

Explorer and native dialogs receive a UNC rewrite followed by Enter, preserving
native behavior and the active Explorer tab. Bare `/` is special-cased by the
shared resolver: by default it targets the WSL provider root (with an Explorer
COM navigation fallback for provider-root builds that open UNC roots in a new
window), or, when the per-user bare-slash mode opts in, the default
distribution root. Search opens the validated path directly and dismisses its
flyout.

Diagnostics record event and error categories only. User-entered and resolved
paths are not logged.

## Settings and optional shell adapters

The settings process is an unpackaged WinUI 3 desktop app. It delegates every
state change to `fswctl`; it never edits profiles or registry integration state
itself. The app uses a Mica system backdrop and the Windows App SDK `TitleBar`
control with an `ImageIconSource`, while retaining the system caption buttons.
Tray commands deep-link to sections through the per-user `fwdslash` URI
registration.

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
