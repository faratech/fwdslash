# Dependency policy

The brief is "minimal, Microsoft-only crates if at all possible, binary size
extremely low". This file is the rule a reviewer applies to a PR that adds one.

## The version-island rule

On **2026-09-03** windows-rs shipped release 74, moving the whole crate family to
`0.100.0` — and deliberately **not** publishing the two umbrella crates. Microsoft's
words, from the August 2026 newsletter (microsoft/windows-rs#4867):

> We may initially publish the release without `windows` and `windows-sys`. These
> two umbrella crates cover a very large API surface and need more testing before
> publication.

So the ecosystem is split, and the split is load-bearing:

| Crate | Requires | Published |
|---|---|---|
| `windows` | `windows-core ^0.62.2` | 0.62.2, 2025-10-06 |
| `windows-sys` | `windows-link` only | 0.61.2, 2025-10-06 |
| `windows-reactor` | `windows-core ^0.100.0` | 0.100.0, 2026-09-03 |

Two `windows-core` majors are two incompatible `IUnknown`/`IInspectable`/`Interface`
type systems. **COM objects cannot cross that boundary**, so the two generations
must never meet inside one binary.

`windows-sys` is exempt: it has no `windows-core` at all, so it links into either
island at the cost of a duplicated `windows-link`.

### One island per binary

The three executables are separate processes that talk over a window class name,
`WM_APP+10..12`, CLI exit codes and the HKCU schema. No COM object crosses between
them, so they do not have to agree.

| Crate | Island |
|---|---|
| `fsw-path` | **none — zero dependencies** |
| `fsw-core` | `windows-registry` 0.6 + `windows-sys` 0.61 |
| `fwdslash.exe` | `fsw-core` + `windows-sys` 0.61 |
| `fswbroker.exe` | `windows` 0.62.2 |
| `fswsettings.exe` | `windows-reactor` 0.100 |

`fswbroker` stays on 0.62.2 for v1 because 0.100 dropped the three things its UI
Automation path depends on: all 351 `UIA_*PropertyId`/`UIA_*PatternId` constants,
`VARIANT`'s `Drop`/`Clone`/`TryFrom<&VARIANT> for BSTR`, and `Result`-shaped
returns on COM methods without `[retval]`. Migrating it to `windows-bindgen` 0.100
is the phase-2 size lever, after parity — a real port, not a version bump.

### `fsw-path` is the only crate shared across islands

That is only sound while it depends on nothing. A crate resolves to exactly one
version of `windows-registry`, and 0.6.1 pulls `windows-result ^0.4.1` /
`windows-strings ^0.5.1` while the 0.100 line pulls the 0.100 set — so a shared
crate that touched either would drag both generations into `fswsettings.exe`.

**`fsw-path` therefore has an intentionally empty `[dependencies]` table**, and CI
asserts:

```sh
test "$(cargo tree -p fsw-path | wc -l)" -eq 1
```

The registry adapter is **not** shared. Each binary owns a thin `registry.rs` on
its own island, all producing the same plain types defined in `fsw-path`.

## Adding a dependency

In order, prefer:

1. **Nothing.** The `.res` writer, the JSON escaper and the argument parser are all
   in-repo for this reason — each replaced a crate with fewer lines than the
   crate's own `Cargo.toml` section would have cost in binary size.
2. **A focused Microsoft crate** — `windows-core`, `windows-registry`,
   `windows-strings`, `windows-result`, `windows-link`, `windows-threading`,
   `windows-collections`, `windows-future`. This is Microsoft's own current
   guidance: *"Libraries should prefer smaller shared crates… then generate any
   other required APIs locally with `windows-bindgen`."*
3. **`windows-bindgen` output**, committed to the repo with a checked-in filter
   file, regenerated in CI with `git diff --exit-code`. This is how windows-rs
   builds its own crates; `windows-window`'s entire surface is a 4.5 KB
   `bindings.rs`.
4. **A build-dependency**, which costs zero binary size and zero runtime deps.
5. **A runtime dependency.** Needs a note here saying what it buys and what was
   measured.

### Known exceptions

- **`rustc-hash`** (rust-lang, not Microsoft) arrives transitively through
  `windows-reactor` and cannot be removed without forking it. It has no mandatory
  dependencies of its own.

## Build flags that are part of the policy

Set in `.cargo/config.toml` under `[target.<triple>]`, never `[build] rustflags` —
the two sources are mutually exclusive and the per-target entries would be
silently dropped. Always build with an explicit `--target` so they do not leak
into build scripts.

- `-C target-feature=+crt-static` with
  `/DEFAULTLIB:ucrt.lib /NODEFAULTLIB:vcruntime.lib /NODEFAULTLIB:msvcrt.lib /NODEFAULTLIB:libucrt.lib`
  — the `/MT` parity recipe that `microsoft/edit`, `microsoft/sudo` and
  `microsoft/coreutils` all independently converge on. Verify with
  `dumpbin /dependents` in CI: the `/NODEFAULTLIB:vcruntime.lib` trick breaks the
  moment something in the graph pulls a vcruntime-only symbol.
- `-C control-flow-guard` — the `/guard:cf` parity flag. The C++ build applies it
  to the two native binaries but not to `fswsettings.exe`; all three get it here.
- `--cfg=windows_slim_errors` — stores only the 4-byte HRESULT and drops COM/WinRT
  extended error info. A pure win, because the never-log-a-path rule already
  forbids surfacing rich error text.

`panic = "abort"` is not negotiable: unwinding out of a `WH_KEYBOARD_LL` callback
or a COM vtable is undefined behaviour. The consequence is that every fallible
operation on the Enter path must return a `Result` that degrades to "replay the
keystroke untouched".
