# Binary size baseline

Measured, not estimated. These are the numbers the Rust port has to beat, and
the source for the CI size-budget gate.

Measured 2026-09-03 from `tools/Build-UserMode.ps1 -Configuration Release` on
VS 18 (MSVC 14.51.36231), Windows SDK 10.0.28000. Both architectures built with
0 warnings under `/W4 /WX`.

## C++ Release, as shipped today

| Artifact | ARM64 | x64 |
|---|---:|---:|
| `fwdslash.exe` | 436,224 | 444,928 |
| `fswbroker.exe` | 261,632 | 273,920 |
| `fswsettings.exe` | 395,264 | 348,672 |
| `Microsoft.WindowsAppRuntime.Bootstrap.dll` | 392,504 | 399,712 |
| `App.xbf` | 692 | 692 |
| `fswsettings.pri` | 1,568 | 1,568 |

`assets/fwdslash.ico` is **100,419 bytes** and is linked into all three
binaries, including `fwdslash.exe`, which never draws it. Splitting the resource
script per binary is therefore worth ~100 KB on the CLI before any codegen
tuning — see the build design in the port plan.

Netting the icon out, the actual code is roughly:

| Binary | ARM64 code | x64 code |
|---|---:|---:|
| `fwdslash.exe` | ~336 KB | ~344 KB |
| `fswbroker.exe` | ~161 KB | ~173 KB |
| `fswsettings.exe` | ~295 KB | ~248 KB |

## What the Rust port has to clear

Nothing here is a target yet — targets get set once a Rust binary exists to
measure. Two cautions worth writing down before anyone picks a number:

1. **A Rust rewrite can easily come out bigger.** Rust's std links panic
   formatting, backtrace scaffolding and UTF-8/UTF-16 machinery that a
   `WIN32_LEAN_AND_MEAN` C++ program never does. Without `crt-static` + fat LTO +
   `panic = "abort"` + `strip`, and without the `windows-bindgen` discipline, the
   naive result is comfortably over 1 MB against a 161 KB broker.
2. **The settings app is a deliberate regression.** Microsoft's only published
   figure for a `windows-reactor` app is ~3 MB, against 395,264 bytes today —
   roughly 8x on the binary. It buys Mica, dark mode, `NavigationView` and
   `InfoBar` that a comctl32 dialog cannot give at any price, and it deletes the
   392 KB bootstrap DLL, `App.xbf` and the PRI files from the payload. That trade
   is argued in the plan but **has never been measured for this app**. Spike S2
   builds `crates/samples/reactor/framework_dependent` with the size-tuned
   profile and diffs it against the reactor gallery; if the floor lands above
   ~4 MB the settings app drops to plain Win32.

The size budget belongs on the binaries that matter for responsiveness — the
resident broker on the Enter keystroke path, and the CLI the shell adapters
spawn per `dir`. The settings window is opened occasionally and can afford to be
large.

## Rust side, measured

Built 2026-09-04 with `cargo build --release --target aarch64-pc-windows-msvc`
from an ARM64 VS 18 developer shell, at the size-tuned release profile in the
root `Cargo.toml` (`opt-level = "s"`, fat LTO, `codegen-units = 1`,
`panic = "abort"`, `strip = "symbols"`) **plus the committed
`.cargo/config.toml` target flags** (`crt-static`, the `/NODEFAULTLIB` /MT
link recipe, `control-flow-guard`, `windows_slim_errors`). The previous table
(2026-09-03) was measured without those flags; the static CRT costs ~28 KB
per native binary, which is the /MT parity price the C++ product also pays.

x64, same flags: `fwdslash.exe` 200,192 / `fswbroker.exe` 200,704 /
`fswsettings.exe` 1,680,896.

| Artifact | ARM64 Rust | C++ ARM64 code | Verdict |
|---|---:|---:|---|
| `fwdslash.exe` | 196,608 | ~336 KB | about half the C++ |
| `fswbroker.exe` | 198,656 | ~161 KB | larger than the C++ code, well under the whole C++ binary (261,632) |
| `fswsettings.exe` | 1,736,704 | ~295 KB | ~5.9x, but see below |

`fswsettings.exe` also **deletes** the 392 KB `Microsoft.WindowsAppRuntime.Bootstrap.dll`,
`App.xbf` and the PRI files from the payload, so the shipped delta is smaller
than the binary delta. It comfortably clears the ~4 MB floor that spike S2 set as
the point where the settings app would drop back to plain Win32.

### Icon policy: one size per binary

The C++ links the same 100,419-byte `assets/fwdslash.ico` into all three
binaries, including `fwdslash.exe`, which never draws it. The Rust build does not
repeat that:

| Binary | Icon resource | Why |
|---|---|---|
| `fwdslash.exe` | **none** | a console tool never draws one — this is the ~100 KB saving |
| `fswbroker.exe` | `assets/fwdslash-tray.ico`, 7,878 bytes (16/20/24/32/48) | the tray and window class never request above 48 px |
| `fswsettings.exe` | `assets/fwdslash.ico`, 100,419 bytes | the taskbar, Alt-Tab and jump list need the 256 px frame |

`tools/Build-AppIcon.ps1 -Sizes 16,20,24,32,48 -Destination assets\fwdslash-tray.ico -IconOnly`
regenerates the tray variant from the same master PNG, so the two cannot drift.
Adding it cost the broker 4,096 bytes of on-disk size against the 100 KB the full
icon would have cost.

## Rust side, earlier notes

`fsw-path` is a library, so it has no meaningful standalone size. It builds
clean in the release profile for all three shipping targets
(`aarch64-pc-windows-msvc`, `x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`)
cross-compiled from WSL — `cargo check`/`build` of an rlib needs no MSVC linker,
so the WSL loop works for everything up to the first `[[bin]]`.

## Reproducing

```powershell
.\tools\Build-UserMode.ps1 -Architecture ARM64 -Configuration Release
.\tools\Build-UserMode.ps1 -Architecture x64   -Configuration Release
```

Stop the broker and settings window first — `link.exe` cannot overwrite a loaded
image.

## Runtime baseline, measured

Produced 2026-09-04 by `tools\Measure-Runtime.ps1 -Architecture ARM64`
(C++ side from `out\user\arm64\ReleaseCpp`, Rust side from
`target\aarch64-pc-windows-msvc\release`). Idle CPU is the broker's total-processor-time
delta across a 10 s window with nothing happening, as a percentage of one core.

| Metric | C++ | Rust |
|---|---:|---:|
| Broker startup to window (median, 10 runs) | n/a (a) | 32.09 ms |
| Broker idle working set (5 s settle) | 16.43 MB | 16.97 MB |
| Broker idle private bytes | 2.83 MB | 2.85 MB |
| Broker idle CPU (10 s window, % of one core) | 0.00 % | 0.00 % |
| CLI cold start `fwdslash status` (median, 20 runs) | 22.26 ms | 21.01 ms |
| Settings launch to window (median, 5 runs) | n/a (b) | 141.38 ms |

(a) The C++ broker's window is message-only (`HWND_MESSAGE`), which no
enumeration route from this host can observe — `FindWindow`/`FindWindowEx`
from an interop PowerShell and `EnumChildWindows` from `HWND_MESSAGE` were all
tried; the C++ broker's own CLI finds it, but only at 1 s call granularity.
`WaitForInputIdle` never fires for a hidden-window process on either side.
The Rust broker's window is a top-level tool window (docs/divergences.md,
"Broker 1"), which is why its startup is measurable at all.

(b) `ReleaseCpp` deliberately skips the WinUI 3 settings app (no vcxproj
configuration); build it via `-Configuration Release` after the MSIX staging
is no longer needed to measure that side.

An earlier draft of this table reported `fwdslash status` at ~1,014 ms on both
sides. That was the harness measuring itself: `Start-Process -Wait` carries
~1,045 ms of PowerShell overhead regardless of the child (measured 1,044 ms vs
17 ms for the same executable). `Measure-CliColdStart` now launches through
`System.Diagnostics.Process` and reports the real numbers above — the two
implementations are at parity to within noise, and there is no slow "WSL
query" to optimize. Resolver hot-path cost is pinned separately by
`crates/fsw-path/tests/allocations.rs` (zero steady-state allocations) and
`tests/perf.rs` (~54 ns/resolve in release, opt-in).
