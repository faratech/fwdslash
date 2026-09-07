# Binary size baseline

Measured, not estimated. This is the recorded size of the three shipping
executables, and the codegen policy that produces them. A change that moves a
number here should move it deliberately.

There is **no size gate in CI**. The `rust-windows` job in
`.github/workflows/build.yml` builds the workspace in release for
`x86_64-pc-windows-msvc` and prints the three executable sizes ("Report binary
sizes"), so a regression is visible in the log and in a PR's checks, but nothing
fails on a number. Comparing against the table below is a human step, and
re-measuring it is required whenever `rust-toolchain.toml` moves.

## Current baseline

Measured 2026-09-06 at version 0.0.8, on the Windows dev host at the pinned
toolchain, with:

```powershell
cargo build --release --target aarch64-pc-windows-msvc --workspace
cargo build --release --target x86_64-pc-windows-msvc  --workspace
```

| Artifact | ARM64 | x64 |
|---|---:|---:|
| `fswbroker.exe` | 379,904 | 393,216 |
| `fwdslash.exe` | 495,104 | 525,824 |
| `fswsettings.exe` | 1,537,024 | 1,546,752 |

Where the size budget matters, in order:

1. **`fswbroker.exe`** — resident, and on the Enter keystroke path. Working set
   and startup matter more than on-disk size, but they track each other.
2. **`fwdslash.exe`** — the shell adapters spawn it once per `dir` and once per
   `cd`, so its cold start is a user-visible cost. Most of its growth over the
   0.0.3 era is the self-update pipeline: WinRT `Services_Store` +
   `Networking_Connectivity`, the vendored `AppInstallManager` bindings, and the
   `reg.exe` settings writer that arrived with them. Whether that costs anything
   that matters is a cold-start question, not a size question.
3. **`fswsettings.exe`** — opened occasionally, and can afford to be large. It
   is a WinUI 3 app on `windows-reactor`; ~1.5 MB is well under the ~4 MB floor
   that was set as the point where the settings app would drop back to plain
   Win32. It also carries no separate `Microsoft.WindowsAppRuntime.Bootstrap.dll`,
   `App.xbf` or PRI files in the payload.

## Codegen policy

The size-tuned release profile lives in the root `Cargo.toml`
(`opt-level = "s"`, fat LTO, `codegen-units = 1`, `panic = "abort"`,
`strip = "symbols"`). The per-target flags live in `.cargo/config.toml` as
per-target `rustflags` — **never** `[build] rustflags`, which would silently
stop applying the moment a target is specified — and cover `crt-static`, the
`/NODEFAULTLIB` /MT link recipe, `control-flow-guard` and `windows_slim_errors`.
The static CRT costs roughly 28 KB per binary; that is the /MT parity price and
it is paid on purpose, so an unpackaged install needs no VC++ redistributable.

**`panic = "abort"` is not negotiable.** Unwinding out of a `WH_KEYBOARD_LL`
callback or out of a COM vtable entry is undefined behavior, and both binaries
do exactly that kind of work. The workspace lints that deny `unwrap_used`,
`expect_used` and `panic` exist for the same reason: under `abort`, any of them
is an instant process death that skips `WM_DESTROY` — the broker would leave its
notification-area icon behind.

Rust's std links panic formatting, backtrace scaffolding and UTF-8/UTF-16
machinery that a `WIN32_LEAN_AND_MEAN` native program never does. Without
`crt-static` + fat LTO + `panic = "abort"` + `strip`, and without the
`windows-bindgen` discipline described in `docs/dependencies.md`, these binaries
are comfortably over 1 MB each.

## Icon policy: one size per binary

Each binary links a different icon on purpose:

| Binary | Icon resource | Why |
|---|---|---|
| `fwdslash.exe` | **none** | a console tool never draws one — worth ~100 KB |
| `fswbroker.exe` | `assets/fwdslash-tray.ico`, 7,878 bytes (16/20/24/32/48) | the tray and window class never request above 48 px |
| `fswsettings.exe` | `assets/fwdslash.ico`, 100,419 bytes | the taskbar, Alt-Tab and jump list need the 256 px frame |

`tools/Build-AppIcon.ps1 -Sizes 16,20,24,32,48 -Destination assets\fwdslash-tray.ico -IconOnly`
regenerates the tray variant from the same master PNG, so the two cannot drift.
Adding it cost the broker 4,096 bytes of on-disk size against the 100 KB the full
icon would have cost.

`fsw-path` is a library, so it has no meaningful standalone size. It builds
clean in the release profile for all three shipping targets
(`aarch64-pc-windows-msvc`, `x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`)
cross-compiled from WSL — `cargo check`/`build` of an rlib needs no MSVC linker,
so the WSL loop works for everything up to the first `[[bin]]`.

## Reproducing

```powershell
cargo build --release --target aarch64-pc-windows-msvc --workspace
cargo build --release --target x86_64-pc-windows-msvc  --workspace
```

Stop the broker and settings window first — `link.exe` cannot overwrite a loaded
image. `fwdslash.exe` alone is never locked by a running product, so
`-p fwdslash` measures the CLI without stopping anything.

## Runtime baseline, measured

Measured 2026-09-04 on the ARM64 dev host, from
`target\aarch64-pc-windows-msvc\release`. Idle CPU is the broker's
total-processor-time delta across a 10 s window with nothing happening, as a
percentage of one core.

| Metric | Value |
|---|---:|
| Broker startup to window (median, 10 runs) | 32.09 ms |
| Broker idle working set (5 s settle) | 16.97 MB |
| Broker idle private bytes | 2.85 MB |
| Broker idle CPU (10 s window, % of one core) | 0.00 % |
| CLI cold start `fwdslash status` (median, 20 runs) | 21.01 ms |
| Settings launch to window (median, 5 runs) | 141.38 ms |

Broker startup is measurable at all because the broker's window is a top-level
tool window rather than message-only (`docs/divergences.md`, "Broker 1"); a
`HWND_MESSAGE` window is not observable from any enumeration route on this host,
and `WaitForInputIdle` never fires for a hidden-window process.

Beware measuring CLI cold start through `Start-Process -Wait`: it carries
~1,045 ms of PowerShell overhead regardless of the child (1,044 ms against 17 ms
for the same executable launched through `System.Diagnostics.Process`). The
numbers above use the latter. Resolver hot-path cost is pinned separately by
`crates/fsw-path/tests/allocations.rs` (zero steady-state allocations) and
`tests/perf.rs` (~54 ns/resolve in release, opt-in).
