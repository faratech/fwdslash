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
| `windows` | `windows-core ^0.62.2` | 0.62.2, 2025-10-06 (still latest as of 2026-09-04) |
| `windows-sys` | `windows-link` only | 0.61.2, 2025-10-06 (still latest as of 2026-09-04) |
| `windows-reactor` | `windows-core ^0.100.0` | 0.100.0, 2026-09-03 (latest) |
| `windows-registry` | 0.6.x: `windows-link` only; **0.100.0: `windows-core ^0.100.0`** | 0.100.0, 2026-09-03 (latest) |

**`windows-registry` is island-pinned at 0.6.** Its 0.100.0 release exists but
drags `windows-core` 0.100 into the closure of `fsw-core`, which is shared with
the 0.62 island — exactly the collision the rule forbids. `tools/update-deps.py`
enforces this via its `ISLAND_PINNED` list; do not bump it by hand. Unification
of the whole workspace onto the 0.100 line becomes possible only when
`windows`/`windows-sys` publish there **and** `fswbroker` is ported off the UIA
constants/VARIANT surface (see below) — until then the split stands.

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
| `fwdslash.exe` | `fsw-core` + `windows-sys` 0.61 + **`windows` 0.62.2** |
| `fswbroker.exe` | `windows` 0.62.2 |
| `fswsettings.exe` | `windows-reactor` 0.100 + `fsw-core` |

**`fwdslash.exe` joined the 0.62 island** for the self-update path. The Store
query (`Windows.Services.Store.StoreContext`) and the Store install path
(`AppInstallManager`, below) are WinRT, so the CLI now carries `windows` 0.62.2
with the features `Services_Store`, `Foundation`, `Foundation_Collections`,
`ApplicationModel` and `Win32_System_Com` — plus three companions the vendored
bindings name directly:

| Crate | Version | Why it is named directly |
|---|---|---|
| `windows-core` | 0.62.2 | `HSTRING`, `Result`, `Interface`, the vtable machinery every generated method uses |
| `windows-future` | 0.3.2 | `IAsyncOperation<T>`, the return type of every `*Async` method |
| `windows-collections` | 0.3.2 | `IVectorView<AppInstallItem>` |

All three are exactly what `windows` 0.62.2 itself resolves to, and all three
are in `tools/update-deps.py`'s `ISLAND_PINNED` with the reason
"0.62 island — must match windows 0.62.2": bumping one alone would put a second
`windows-core` in the binary, which is the collision this whole file exists to
prevent. The `rust` job asserts it directly, the same way it does for
`fswsettings` (step "Assert one windows-core version in fwdslash").

`fsw-core` and `fsw-path` are untouched by this and stay island-free.

### Vendored bindings: `Windows.ApplicationModel.Store.Preview.InstallControl`

`crates/fsw-cli/src/update/install_control.rs` is committed `windows-bindgen`
output — option 3 of "Adding a dependency" below, and the same arrangement as
`crates/windows-reactor/`.

It exists because **the published `windows` crate 0.62.2 contains no
`Windows.ApplicationModel.Store` tree at all**, and that is where
`AppInstallManager` lives: the API winget drives to install a Store product, and
the only route that can update the Store flavor of this product without a user
gesture. The types are in the Windows SDK's own metadata, so they are generated
once and committed.

| | |
|---|---|
| Namespace | `Windows.ApplicationModel.Store.Preview.InstallControl` |
| Metadata | `C:\Program Files (x86)\Windows Kits\10\UnionMetadata\10.0.26100.0\Windows.winmd` |
| Generator | `windows-bindgen` 0.62.1 |
| Regenerate | `python3 tools/regen_install_control.py` (`--check` for CI mode) |
| Size | ~95 KB, ~2.5k lines, no runtime cost when unused (the ARM64 release `fwdslash.exe` was byte-for-byte the same size before and after adding it: fat LTO drops what nothing calls) |

Two things about the generator are not obvious:

* **`windows-bindgen` is a library, not a binary.** `cargo install
  windows-bindgen` fails with "can't be used with libraries", so the script
  writes a five-line driver crate pinned at `=0.62.1` into a temporary directory
  and calls `windows_bindgen::bindgen(args)` from there.
* **It shells out to `rustfmt`**, and falls back to unformatted output *silently*
  if it cannot find one. The script therefore forces `RUSTUP_TOOLCHAIN` to the
  channel in `rust-toolchain.toml` and refuses to run when that rustfmt is
  missing, so the committed bytes are reproducible.

The arguments are recorded in the generated file's own header. The one that
takes explaining is `--reference windows,skip-root,Windows.Foundation`: it points
the four `TypedEventHandler` parameters at the `windows` crate rather than
regenerating them, and it is deliberately the **only** reference.
`Windows.System` and `Windows.Management.Deployment` are left unreferenced, so
the generator skips the 15 `*ForUserAsync` / `PackageVolume` methods instead of
forcing two more `windows` features into the binary for methods this product
never calls. Their vtable slots survive as `usize` placeholders, so interface
layout is unaffected — the same convention `crates/windows-reactor`'s bindings
use for unimplemented slots.

The `rust-windows` job regenerates the file and runs `git diff --exit-code`
against it, but only when the runner carries the SDK version recorded above
(a different `Windows.winmd` legitimately produces different bindings); it warns
and skips otherwise. `.gitattributes` pins `*.rs` to `eol=lf` so that check is
not defeated by a CRLF checkout.

`fswbroker` stays on 0.62.2 for v1 because 0.100 dropped the three things its UI
Automation path depends on: all 351 `UIA_*PropertyId`/`UIA_*PatternId` constants,
`VARIANT`'s `Drop`/`Clone`/`TryFrom<&VARIANT> for BSTR`, and `Result`-shaped
returns on COM methods without `[retval]`. Migrating it to `windows-bindgen` 0.100
is the phase-2 size lever, after parity — a real port, not a version bump.

### `fsw-core` is shared too, and that is sound

`fswsettings.exe` depends on `fsw-core` so the settings window can read HKCU and
the broker window **in-process**, exactly as `src/settings/main.cpp:754-841`
does. The alternative — parsing `fwdslash status --json` / `integrations --json`
— was tried and removed: one field-name drift (`windowsPowerShell` against
serde's `windowsPowershell`) silently failed the whole parse and left every
toggle reading `false`, which is a failure mode a text contract between two
binaries will keep reproducing.

This does not breach the rule, because **`fsw-core`'s dependency closure contains
no `windows-core` at all**:

```
$ cargo tree -p fsw-core --locked --offline
fsw-core v0.0.3
├── fsw-path v0.0.3
├── windows-registry v0.6.1
│   ├── windows-link v0.2.1
│   ├── windows-result v0.4.1 └── windows-link v0.2.1
│   └── windows-strings v0.5.1 └── windows-link v0.2.1
└── windows-sys v0.61.2 └── windows-link v0.2.1
```

So `fswsettings.exe` still contains exactly one `windows-core`, at 0.100.0, via
`windows-reactor`. What is duplicated is leaf crates — `windows-link`,
`windows-result`, `windows-strings` at both 0.2/0.4/0.5 and 0.100 — and each is
safe here:

- `windows-link` is macro-only: it expands `#[link]` blocks in the consuming
  crate and emits no symbols. The doc already blesses this duplication for
  `windows-sys`.
- `windows-strings` owns its `HSTRING` allocations, so two copies are safe as
  long as an `HSTRING` from one is never freed by the other. `fsw-core`'s public
  API returns only `String`, `Vec<String>`, `bool`, `Option<String>`, `Snapshot`,
  `BrokerState` and `Result<(), u32>` — no WinRT type crosses the boundary.
- `windows-result`'s `Error` can hold an `IErrorInfo`. `fsw-core` never returns
  one: every fallible path maps to `u32` via `e.code().0 as u32`.

The gate to keep this honest, alongside the `fsw-path` one. It runs in the
`rust` job of `.github/workflows/build.yml` (step "Assert one windows-core
version in fswsettings"), which fails the build when more than one line comes
back:

```sh
cargo tree -p fswsettings -e normal --target aarch64-pc-windows-msvc \
  | grep -o 'windows-core v[0-9.]*' | sort -u   # must print only windows-core v0.100.0
```

### `fsw-path` is the only crate shared across *both* islands

That is only sound while it depends on nothing. A crate resolves to exactly one
version of `windows-registry`, and 0.6.1 pulls `windows-result ^0.4.1` /
`windows-strings ^0.5.1` while the 0.100 line pulls the 0.100 set — so a shared
crate that touched either would drag both generations into `fswsettings.exe`.

**`fsw-path` therefore has an intentionally empty `[dependencies]` table** — and
an empty `[dev-dependencies]` table too, because `cargo tree` counts those. The
`rust` job of `.github/workflows/build.yml` (step "Assert fsw-path has no
dependencies") runs exactly this, on every push and pull request:

```sh
test "$(cargo tree -p fsw-path | wc -l)" -eq 1
```

That job also runs `cargo test -p fsw-path -p fsw-core --locked` and
`cargo check --workspace --all-targets` against both MSVC targets; the
companion `rust-windows` job runs the CLI's bin tests and a release build.

The registry adapter **is** shared, through `fsw-core` — see the section above
for why that is sound. `fsw-path` stays dependency-free regardless, because it is
the only crate that would otherwise be free to pull in either generation.

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
