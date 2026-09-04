#!/usr/bin/env python3
"""
Workspace-aware Cargo.toml dependency updater.

Adapted from the wfdiag repo's scripts/update-deps.py for this cargo
workspace: walks the root [workspace.dependencies] table and every member
manifest under crates/, looks up the latest published version with
`cargo search`, and rewrites version strings in place after confirmation.

Hard-wired to respect this workspace's rules (docs/dependencies.md):
  * [workspace.dependencies] is where the shared windows-* pins live
  * entries whose value is `workspace = true` are skipped (no version here)
  * path dependencies are skipped
  * [patch.crates-io] is never touched (the vendored windows-reactor path
    entry must survive verbatim)
  * crates/windows-reactor/ is never modified -- upgrading it means
    re-vendoring, never a version edit
  * crates/fsw-path's [dependencies] table must stay EMPTY; the script
    asserts that after any write, so the documented one-line
    `cargo tree -p fsw-path` gate cannot regress silently

Usage: python3 tools/update-deps.py [--dry-run] [--json] [--crate NAME]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VENDORED = "windows-reactor"  # vendored; upgrades by re-vendor only
ZERO_DEP_CRATE = "fsw-path"  # [dependencies] must stay empty

# Crates whose newer generations are forbidden by the version-island rule
# (docs/dependencies.md). Reported as pinned, never offered as updates.
ISLAND_PINNED: dict[str, str] = {
    # 0.100.x depends on windows-core 0.100; fsw-core's closure must stay
    # windows-core-free or both type systems land in fswbroker.exe.
    "windows-registry": "0.100.x pulls windows-core into fsw-core's closure",
    # windows/windows-sys 0.100.x do not exist yet; when they do, migration
    # is a deliberate cross-major port (see docs/dependencies.md), not a bump.
}


def get_latest_version(crate_name: str) -> tuple[str | None, bool]:
    """Latest version on crates.io via `cargo search`. (version, is_prerelease)."""
    try:
        result = subprocess.run(
            ["cargo", "search", crate_name, "--limit", "1"],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if result.returncode != 0:
            return None, False
        match = re.search(
            rf'^{re.escape(crate_name)}\s*=\s*"([^"]+)"', result.stdout, re.MULTILINE
        )
        if match:
            version = match.group(1)
            is_prerelease = any(tag in version.lower() for tag in ("alpha", "beta", "rc"))
            return version, is_prerelease or "-" in version
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass
    return None, False


def version_matches(spec: str, version: str) -> bool:
    """Does `spec` prefix-match `version`? ("0.62" matches "0.62.2".)"""
    spec_parts = spec.split(".")
    version_parts = version.split(".")
    return all(
        i < len(version_parts) and spec_part == version_parts[i]
        for i, spec_part in enumerate(spec_parts)
    )


def parse_dependency_lines(content: str) -> dict[str, list[int]]:
    """
    Find dependency entries with an inline version string.

    Returns {crate: [line indexes]} across [dependencies], [build-dependencies],
    [target.*.dependencies] and [workspace.dependencies]. Skips
    [patch.crates-io] and [package]. Multi-line table entries are handled.
    """
    entries: dict[str, list[int]] = {}
    lines = content.split("\n")
    section_deps = False
    for i, line in enumerate(lines):
        if match := re.match(r"^\[(.+?)\]", line):
            name = match.group(1)
            section_deps = name in (
                "dependencies",
                "build-dependencies",
                "workspace.dependencies",
                "dev-dependencies",
            ) or re.fullmatch(r"target\..+\.dependencies", name) is not None
            continue
        if not section_deps:
            continue
        stripped = line.strip()
        if stripped.startswith("#") or "workspace = true" in stripped:
            continue
        simple = re.match(r'^([\w-]+)\s*=\s*"([^"]+)"', stripped)
        table = re.match(r'^([\w-]+)\s*=\s*\{.*?version\s*=\s*"([^"]+)"', stripped)
        if table:
            entries.setdefault(table.group(1), []).append(i)
        elif simple:
            entries.setdefault(simple.group(1), []).append(i)
    return entries


def update_line(lines: list[str], index: int, new_version: str) -> None:
    lines[index] = re.sub(
        r'(version\s*=\s*")[^"]+(")',
        rf"\g<1>{new_version}\2",
        lines[index],
    ) if "= {" in lines[index] else re.sub(
        r'^(?P<name>[\w-]+\s*=\s*")[^"]+(")',
        rf"\g<name>{new_version}\2",
        lines[index],
    )


def collect_manifests() -> list[Path]:
    manifests = [ROOT / "Cargo.toml"]
    manifests.extend(sorted((ROOT / "crates").glob("*/Cargo.toml")))
    return [m for m in manifests if VENDORED not in m.parts]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--json", action="store_true", help="machine-readable report")
    parser.add_argument("--crate", help="only consider this crate")
    args = parser.parse_args()

    report: list[dict[str, object]] = []
    for manifest in collect_manifests():
        rel = manifest.relative_to(ROOT)
        content = manifest.read_text(encoding="utf-8")
        entries = parse_dependency_lines(content)
        updates: dict[str, tuple[str, int]] = {}
        for crate, indexes in sorted(entries.items()):
            if args.crate and crate != args.crate:
                continue
            if crate == VENDORED:
                report.append({
                    "crate": crate, "manifest": str(rel),
                    "status": "vendored, skipped (re-vendor instead)",
                })
                continue
            current = re.search(
                r'version\s*=\s*"([^"]+)"|"([^"]+)"',
                content.split("\n")[indexes[0]].strip(),
            )
            current_version = (current.group(1) or current.group(2)) if current else None
            if current_version is None:
                continue
            latest, prerelease = get_latest_version(crate)
            if crate in ISLAND_PINNED and latest is not None and not version_matches(
                current_version, latest
            ):
                status = f"pinned by the island rule: {ISLAND_PINNED[crate]}"
                latest = f"{latest} (forbidden)"
            elif latest is None:
                status = "not found"
            elif latest == current_version or version_matches(current_version, latest):
                status = "up to date" + (" (latest is a prerelease)" if prerelease else "")
            elif prerelease:
                status = "prerelease available (not auto-updated)"
            else:
                major_jump = current_version.split(".")[0] != latest.split(".")[0]
                status = "MAJOR update available" if major_jump else "update available"
                updates[crate] = (latest, indexes[0])
            report.append({
                "crate": crate, "manifest": str(rel), "current": current_version,
                "latest": latest, "status": status,
            })

        if updates and not args.dry_run:
            answer = input(
                f"\nApply {len(updates)} update(s) in {rel}? [y/N] "
            ).strip().lower()
            if answer != "y":
                continue
            lines = content.split("\n")
            for latest, index in updates.values():
                update_line(lines, index, latest)
            manifest.write_text("\n".join(lines), encoding="utf-8")
            print(f"[ok] {rel}: {len(updates)} updated")

    if args.json:
        print(json.dumps(report, indent=2))

    # The zero-dependency gate, enforced as a side effect of every run.
    fsw_path = ROOT / "crates" / ZERO_DEP_CRATE / "Cargo.toml"
    section = re.search(
        r"^\[dependencies\]\s*$((?s:.)*)", fsw_path.read_text(encoding="utf-8"), re.MULTILINE
    )
    body = section.group(1) if section else ""
    body = body[: body.index("[")] if "[" in body else body
    if re.search(r"^[\w-]+\s*=", body, re.MULTILINE):
        print(f"[!!] {ZERO_DEP_CRATE} [dependencies] is no longer empty -- "
              "docs/dependencies.md gate violated", file=sys.stderr)
        return 1

    summary_target = sys.stderr if args.json else sys.stdout
    if not any(r.get("status") in ("update available", "MAJOR update available")
               for r in report):
        print("All dependencies are up to date.", file=summary_target)
    if not args.dry_run and any(r.get("status") == "update available" for r in report):
        print("\nAfter updates run:")
        print("  cargo update")
        print('  test "$(cargo tree -p fsw-path | wc -l)" -eq 1')
        print("  cargo tree -p fswsettings -e normal --target aarch64-pc-windows-msvc "
              "| grep -o 'windows-core v[0-9.]*' | sort -u")
    return 0


if __name__ == "__main__":
    sys.exit(main())
