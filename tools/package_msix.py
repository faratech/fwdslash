#!/usr/bin/env python3
"""Stages and packs the Rust product as an MSIX bundle, from WSL.

The WSL-runnable equivalent of `tools/Package-Msix.ps1 -BinarySource Rust`:
it shells out to the SDK's makeappx.exe/makepri.exe through `wslpath`, so
packaging never requires leaving WSL for native PowerShell. Build first with
`cargo build --release --target <triple> --workspace` on the Windows side.
"""
import os
import platform
import re
import shutil
import subprocess
import sys

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
PACKAGING_ROOT = os.path.join(REPO, "packaging")
MANIFEST_TEMPLATE = os.path.join(PACKAGING_ROOT, "AppxManifest.xml")
PRICONFIG = os.path.join(PACKAGING_ROOT, "priconfig.xml")
ASSETS_SRC = os.path.join(PACKAGING_ROOT, "Assets")
OUTPUT_ROOT = os.path.join(REPO, "out", "msix")

CARGO_TOML = os.path.join(REPO, "Cargo.toml")

IDENTITY_NAME = "32827MikeFara.fwdslash"
PUBLISHER = "CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4"
PUBLISHER_DISPLAY_NAME = "WindowsForum.com"

# The shell payload the CLI's adapters copy out of the package at
# `fwdslash integration <name> enable`. Keep in step with the payload lists in
# crates/fsw-cli/src/adapters/cmd.rs, tools/Package.ps1 and
# tools/Package-Msix.ps1.
REQUIRED_PAYLOAD = [
    ("cmd", "fsw-autorun.cmd"),
    ("cmd", "fsw-cd.cmd"),
    ("cmd", "fsw-dir.cmd"),
    ("cmd", "fsw-pushd.cmd"),
    ("powershell", "ForwardSlashWindows.psm1"),
]

# The SDK ships makeappx/makepri per host architecture. Picking one at random
# (this hardcoded arm64 for a while) fails outright on the other kind of dev
# box; FSW_SDK_TOOL_ARCH overrides the detection when a host runs, say, the x64
# tools under emulation.
SDK_BIN_ROOT = os.path.join(
    REPO, "packages/Microsoft.Windows.SDK.CPP.10.0.28000.2526/c/bin/10.0.28000.0"
)


def sdk_tool_arch():
    override = os.environ.get("FSW_SDK_TOOL_ARCH")
    if override:
        return override
    machine = platform.machine().lower()
    if machine in ("aarch64", "arm64"):
        return "arm64"
    return "x64"


TOOL_ARCH = sdk_tool_arch()
MAKEAPPX = os.path.join(SDK_BIN_ROOT, TOOL_ARCH, "makeappx.exe")
MAKEPRI = os.path.join(SDK_BIN_ROOT, TOOL_ARCH, "makepri.exe")


def read_version():
    """The MSIX four-part version, from `workspace.package.version`.

    The workspace version is the one place a release is declared; a literal
    here is how 0.0.2 shipped binaries stamped 0.0.1.0. MSIX identities are
    always four-part, so the revision field is appended.
    """
    with open(CARGO_TOML, "r", encoding="utf-8") as f:
        text = f.read()
    section = re.search(r"^\[workspace\.package\]$(.*?)(?=^\[|\Z)", text, re.M | re.S)
    if not section:
        sys.exit(f"No [workspace.package] section in {CARGO_TOML}")
    match = re.search(r'^version\s*=\s*"([^"]+)"', section.group(1), re.M)
    if not match:
        sys.exit(f"No workspace.package.version in {CARGO_TOML}")
    version = match.group(1)
    fields = version.split(".")
    if len(fields) == 3:
        fields.append("0")
    if len(fields) != 4:
        sys.exit(f"Unsupported workspace version {version!r}; expected 3 or 4 fields")
    return ".".join(fields)


VERSION = read_version()


def to_win_path(p):
    out = subprocess.check_output(["wslpath", "-w", p]).decode("utf-8").strip()
    return out

def run_tool(cmd):
    print("Running:", " ".join(cmd))
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        print("Error output:", res.stderr)
        print("Stdout:", res.stdout)
        sys.exit(res.returncode)

def main():
    os.makedirs(OUTPUT_ROOT, exist_ok=True)
    architectures = ["arm64", "x64"]
    produced = []

    with open(MANIFEST_TEMPLATE, "r", encoding="utf-8") as f:
        template = f.read()

    for arch in architectures:
        print(f"\n=== Packaging {arch} ===")
        stage = os.path.join(OUTPUT_ROOT, f"stage-{arch}")
        if os.path.exists(stage):
            shutil.rmtree(stage)
        os.makedirs(stage)

        # Source binaries from target/<arch>-pc-windows-msvc/release
        rust_arch = "aarch64-pc-windows-msvc" if arch == "arm64" else "x86_64-pc-windows-msvc"
        bin_dir = os.path.join(REPO, "target", rust_arch, "release")

        # Payload files
        for exe in ["fwdslash.exe", "fswbroker.exe", "fswsettings.exe"]:
            src = os.path.join(bin_dir, exe)
            dst = os.path.join(stage, exe)
            shutil.copy2(src, dst)
            print(f"  Staged {exe} ({os.path.getsize(dst)} bytes)")

        # License
        shutil.copy2(os.path.join(REPO, "LICENSE"), os.path.join(stage, "LICENSE"))

        # Shell adapter payload, from the repo tree.
        #
        # NOT from out/user/<arch>/Release/shell: that is whatever
        # Build-UserMode.ps1 last staged for the C++ build, and 0.0.2 shipped a
        # ForwardSlashWindows.psm1 two days older than the repo's because of it.
        for sh_dir in ["cmd", "powershell"]:
            src_sh = os.path.join(REPO, "shell", sh_dir)
            dst_sh = os.path.join(stage, "shell", sh_dir)
            if not os.path.isdir(src_sh):
                sys.exit(f"Missing shell payload directory: {src_sh}")
            shutil.copytree(src_sh, dst_sh, dirs_exist_ok=True)

        # A silently incomplete payload is the worst outcome: the adapter
        # installs, and the DOSKEY macro it wrote calls a file that is not
        # there.
        missing = [
            os.path.join("shell", sh_dir, name)
            for sh_dir, name in REQUIRED_PAYLOAD
            if not os.path.isfile(os.path.join(stage, "shell", sh_dir, name))
        ]
        if missing:
            sys.exit("Shell payload incomplete; missing: " + ", ".join(missing))

        # Assets. The titlebar PNG is deliberately not staged: the Rust
        # settings app embeds it with include_bytes! and never resolves an
        # ms-appx:/// URI, so a copy in the package is dead weight (#35).
        stage_assets = os.path.join(stage, "Assets")
        shutil.copytree(ASSETS_SRC, stage_assets, dirs_exist_ok=True)

        # AppxManifest.xml
        manifest = (
            template.replace("{{IDENTITY_NAME}}", IDENTITY_NAME)
            .replace("{{PUBLISHER}}", PUBLISHER)
            .replace("{{PUBLISHER_DISPLAY_NAME}}", PUBLISHER_DISPLAY_NAME)
            .replace("{{VERSION}}", VERSION)
            .replace("{{ARCHITECTURE}}", arch)
        )
        with open(os.path.join(stage, "AppxManifest.xml"), "w", encoding="utf-8") as f:
            f.write(manifest)

        # Resources.pri
        stage_win = to_win_path(stage)
        priconfig_win = to_win_path(PRICONFIG)
        pri_out_win = to_win_path(os.path.join(stage, "resources.pri"))

        run_tool([
            MAKEPRI,
            "new",
            "/pr", stage_win,
            "/cf", priconfig_win,
            "/of", pri_out_win,
            "/in", IDENTITY_NAME,
            "/o",
        ])

        # MakeAppx pack
        pkg_path = os.path.join(OUTPUT_ROOT, f"fwdslash-{VERSION}-{arch}.msix")
        pkg_win = to_win_path(pkg_path)
        run_tool([MAKEAPPX, "pack", "/d", stage_win, "/p", pkg_win, "/o"])
        print(f"  Packed {pkg_path} ({os.path.getsize(pkg_path)} bytes)")
        produced.append(pkg_path)

        # Copy to /mnt/c/code/
        shutil.copy2(pkg_path, f"/mnt/c/code/fwdslash-{arch}.msix")
        shutil.copy2(pkg_path, f"/mnt/c/code/fwdslash-{VERSION}-{arch}.msix")

    # MakeAppx bundle
    print("\n=== Bundling MSIX ===")
    bundle_input = os.path.join(OUTPUT_ROOT, "bundle-input")
    if os.path.exists(bundle_input):
        shutil.rmtree(bundle_input)
    os.makedirs(bundle_input)
    for pkg in produced:
        shutil.copy2(pkg, bundle_input)

    bundle_path = os.path.join(OUTPUT_ROOT, f"fwdslash-{VERSION}.msixbundle")
    bundle_input_win = to_win_path(bundle_input)
    bundle_win = to_win_path(bundle_path)
    run_tool([MAKEAPPX, "bundle", "/d", bundle_input_win, "/p", bundle_win, "/o"])
    shutil.rmtree(bundle_input)
    print(f"  Bundled {bundle_path} ({os.path.getsize(bundle_path)} bytes)")

    # Copy to /mnt/c/code/
    shutil.copy2(bundle_path, "/mnt/c/code/fwdslash.msixbundle")
    shutil.copy2(bundle_path, f"/mnt/c/code/fwdslash-{VERSION}.msixbundle")
    print("\nSuccessfully produced unsigned MSIX and MSIXBUNDLE for Microsoft Store!")

if __name__ == "__main__":
    main()
