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
native behavior and the active Explorer tab. Bare `/` is special-cased to the
WSL provider root, with an Explorer COM navigation fallback for provider-root
builds that open UNC roots in a new window. Search opens the validated path
directly and dismisses its flyout.

Diagnostics record event and error categories only. User-entered and resolved
paths are not logged.

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
- Driver installation remains VM-only until Microsoft signing and release
  validation are complete.
