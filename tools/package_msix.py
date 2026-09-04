#!/usr/bin/env python3
import os
import shutil
import subprocess
import sys

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
PACKAGING_ROOT = os.path.join(REPO, "packaging")
MANIFEST_TEMPLATE = os.path.join(PACKAGING_ROOT, "AppxManifest.xml")
PRICONFIG = os.path.join(PACKAGING_ROOT, "priconfig.xml")
ASSETS_SRC = os.path.join(PACKAGING_ROOT, "Assets")
OUTPUT_ROOT = os.path.join(REPO, "out", "msix")

IDENTITY_NAME = "32827MikeFara.fwdslash"
PUBLISHER = "CN=ABDB6B3F-DF9E-447D-BC0E-4DA7BAFD14C4"
PUBLISHER_DISPLAY_NAME = "WindowsForum.com"
VERSION = "0.0.2.0"  # instance-guard + custom-root + driver-protocol fixes

MAKEAPPX = os.path.join(REPO, "packages/Microsoft.Windows.SDK.CPP.10.0.28000.2526/c/bin/10.0.28000.0/arm64/makeappx.exe")
MAKEPRI = os.path.join(REPO, "packages/Microsoft.Windows.SDK.CPP.10.0.28000.2526/c/bin/10.0.28000.0/arm64/makepri.exe")

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

        # Shell directories
        for sh_dir in ["cmd", "powershell"]:
            src_sh = os.path.join(REPO, "out", "user", "arm64", "Release", "shell", sh_dir)
            dst_sh = os.path.join(stage, "shell", sh_dir)
            if os.path.exists(src_sh):
                shutil.copytree(src_sh, dst_sh, dirs_exist_ok=True)

        # Assets
        stage_assets = os.path.join(stage, "Assets")
        shutil.copytree(ASSETS_SRC, stage_assets, dirs_exist_ok=True)
        titlebar = os.path.join(REPO, "assets", "fwdslash-titlebar.png")
        if os.path.exists(titlebar):
            shutil.copy2(titlebar, os.path.join(stage_assets, "fwdslash-titlebar.png"))

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
