#!/usr/bin/env python3
"""Move every version literal in the tree in one deterministic step.

`[workspace.package] version` in the root `Cargo.toml` is the single source of
truth for the product version. A dozen other files carry a hand-written copy of
it -- the `#ifndef` VERSIONINFO fallbacks, two side-by-side manifests, the CMake
project version, the ZIP stage directory, the About header, two docs -- and a
release used to mean editing all of them by hand from a list in CLAUDE.md, where
any one could be missed silently.

Every copy is registered below as an explicit, anchored pattern with an expected
match count, so that:

  * `--check` (the CI mode) fails when any copy disagrees with Cargo.toml;
  * a bump rewrites all of them at once, or none of them -- every pattern is
    matched and its count verified before a single byte is written;
  * a literal that moves, changes shape or disappears fails loudly on the count
    rather than being skipped;
  * line endings and encoding survive: only the captured version span is
    replaced, and the file is re-encoded with the codec and BOM it arrived with
    (`tools/Package.ps1` is CRLF; a `.rc` may be UTF-16 or ANSI).

Usage:
    python3 tools/bump_version.py --check           # CI: do all copies agree?
    python3 tools/bump_version.py 0.0.4             # bump
    python3 tools/bump_version.py 0.0.4 --dry-run
    python3 tools/bump_version.py 0.0.4 --commit --tag

Adding a version literal to the tree means adding a `Site` below; there is
deliberately no fallback global search-and-replace.

Deliberately NOT registered, because they are records of a past event rather
than copies of the current version:
  * `docs/store-submission.md` section 6 -- the verification checklist for one
    specific submission, and the sample `fwdslash version` output inside it.
  * the publish-to-store workflow comment naming the version the Store has
    already published; that tracks Partner Center, not this tree.
  * `docs/compatibility.md`, `docs/divergences.md`, `docs/dependencies.md` --
    "new in X.Y.Z" evidence rows and sample `cargo tree` output.
  * test fixtures and doc comments under `crates/` that exercise version
    comparison (`crates/fsw-core/tests/update.rs` and friends).
  * `packaging/AppxManifest.xml` -- templated `{{VERSION}}`, filled by the
    packagers from `cargo metadata` / the workspace TOML.
"""

from __future__ import annotations

import argparse
import codecs
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CO_AUTHOR = "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"

# A version literal's written shape: the regex that recognises one, and how a
# (major, minor, patch) triple renders into it. The fourth field is always 0 --
# the Store reserves the revision field (docs/store-submission.md section 5).
SHAPES: dict[str, tuple[str, object]] = {
    "dotted3": (r"\d+\.\d+\.\d+", lambda v: "{0}.{1}.{2}".format(*v)),
    "dotted4": (r"\d+\.\d+\.\d+\.\d+", lambda v: "{0}.{1}.{2}.0".format(*v)),
    "commas4": (r"\d+,\d+,\d+,\d+", lambda v: "{0},{1},{2},0".format(*v)),
}


@dataclass(frozen=True)
class Site:
    """One registered version literal (or set of identical literals)."""

    path: str  # repo-relative
    what: str  # what a human should look for in that file
    shape: str  # key into SHAPES
    pattern: str  # regex; `{V}` is the version, `{NAMES}` the member names
    count: int | None  # expected matches; None means "one per workspace member"


# The source of truth. Anchored inside its own table so a member crate's
# `version.workspace = true` or a dependency pin can never be mistaken for it.
SOURCE_OF_TRUTH = Site(
    path="Cargo.toml",
    what="[workspace.package] version",
    shape="dotted3",
    pattern=r'^\[workspace\.package\][^\[]*?^version = "{V}"\r?$',
    count=1,
)

SITES: tuple[Site, ...] = (
    SOURCE_OF_TRUTH,
    Site(
        path="Cargo.lock",
        what="[[package]] version of each workspace member",
        shape="dotted3",
        pattern=r'^\[\[package\]\]\r?\nname = "(?:{NAMES})"\r?\nversion = "{V}"\r?$',
        count=None,
    ),
    Site(
        path="crates/fsw-broker/app.rc",
        what="#ifndef FSW_VER_COMMAS fallback",
        shape="commas4",
        pattern=r"^#define FSW_VER_COMMAS {V}\r?$",
        count=1,
    ),
    Site(
        path="crates/fsw-broker/app.rc",
        what="#ifndef FSW_VER_STR fallback",
        shape="dotted3",
        pattern=r'^#define FSW_VER_STR "{V}"\r?$',
        count=1,
    ),
    Site(
        path="crates/fsw-cli/app.rc",
        what="#ifndef FSW_VER_COMMAS fallback",
        shape="commas4",
        pattern=r"^#define FSW_VER_COMMAS {V}\r?$",
        count=1,
    ),
    Site(
        path="crates/fsw-cli/app.rc",
        what="#ifndef FSW_VER_STR fallback",
        shape="dotted3",
        pattern=r'^#define FSW_VER_STR "{V}"\r?$',
        count=1,
    ),
    Site(
        path="crates/fsw-settings/app.rc",
        what="#ifndef FSW_VER_COMMAS fallback",
        shape="commas4",
        pattern=r"^#define FSW_VER_COMMAS {V}\r?$",
        count=1,
    ),
    Site(
        path="crates/fsw-settings/app.rc",
        what="#ifndef FSW_VER_STR fallback",
        shape="dotted3",
        pattern=r'^#define FSW_VER_STR "{V}"\r?$',
        count=1,
    ),
    Site(
        path="crates/fsw-settings/app.manifest",
        what="assemblyIdentity version",
        shape="dotted4",
        pattern=r'<assemblyIdentity version="{V}" name="ForwardSlashWindows\.Settings\.app"',
        count=1,
    ),
    Site(
        path="src/settings/app.manifest",
        what="assemblyIdentity version",
        shape="dotted4",
        pattern=r'<assemblyIdentity version="{V}" name="ForwardSlashWindows\.Settings\.app"',
        count=1,
    ),
    Site(
        path="src/settings/main.cpp",
        what="About page header literal",
        shape="dotted3",
        pattern=r'PageHeader\(L"About", L"Forward Slash Windows {V}"\)',
        count=1,
    ),
    Site(
        path="assets/fwdslash.rc",
        what="FILEVERSION / PRODUCTVERSION",
        shape="commas4",
        pattern=r"^ (?:FILEVERSION|PRODUCTVERSION) {V}\r?$",
        count=2,
    ),
    Site(
        path="assets/fwdslash.rc",
        what='VALUE "FileVersion" / "ProductVersion"',
        shape="dotted3",
        pattern=r'VALUE "(?:File|Product)Version", "{V}\\0"',
        count=2,
    ),
    Site(
        path="CMakeLists.txt",
        what="project(ForwardSlashWindows VERSION ...)",
        shape="dotted3",
        pattern=r"^project\(ForwardSlashWindows VERSION {V} LANGUAGES",
        count=1,
    ),
    Site(
        path="tools/Package.ps1",
        what="ZIP stage directory name",
        shape="dotted3",
        pattern=r'"forward-slash-windows-{V}-\{0\}"',
        count=1,
    ),
    Site(
        path="SECURITY.md",
        what="supported version",
        shape="dotted3",
        pattern=r"branch and version `{V}`",
        count=1,
    ),
    Site(
        path="docs/store-submission.md",
        what="section 5 identity version",
        shape="dotted4",
        pattern=r"\*\*Version is `{V}`\.\*\*",
        count=1,
    ),
    Site(
        path="CLAUDE.md",
        what="Conventions version bullet",
        shape="dotted3",
        pattern=r"^- \*\*Version `{V}`\.\*\*",
        count=1,
    ),
)


class BumpError(Exception):
    """A registered site could not be read, matched, or rewritten."""


# --------------------------------------------------------------------------
# Version parsing
# --------------------------------------------------------------------------

VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def parse_version(text: str) -> tuple[int, int, int]:
    match = VERSION_RE.match(text)
    if not match:
        raise BumpError(f"Version must be MAJOR.MINOR.PATCH (got {text!r}).")
    return (int(match[1]), int(match[2]), int(match[3]))


def render(shape: str, version: tuple[int, int, int]) -> str:
    return SHAPES[shape][1](version)  # type: ignore[operator]


def format_version(version: tuple[int, int, int]) -> str:
    return "{0}.{1}.{2}".format(*version)


# --------------------------------------------------------------------------
# Byte-preserving file IO
# --------------------------------------------------------------------------

# Longest BOM first: BOM_UTF32_LE starts with BOM_UTF16_LE.
_BOMS: tuple[tuple[bytes, str], ...] = (
    (codecs.BOM_UTF32_LE, "utf-32-le"),
    (codecs.BOM_UTF32_BE, "utf-32-be"),
    (codecs.BOM_UTF8, "utf-8"),
    (codecs.BOM_UTF16_LE, "utf-16-le"),
    (codecs.BOM_UTF16_BE, "utf-16-be"),
)


@dataclass
class Source:
    """A file read so that it can be written back byte-for-byte."""

    path: Path
    text: str
    codec: str
    bom: bytes
    raw: bytes

    def encode(self, text: str) -> bytes:
        return self.bom + text.encode(self.codec)


def read_source(path: Path) -> Source:
    r"""Read a file, detecting its encoding, and prove the round-trip is exact.

    Nothing here normalises line endings: patterns match `\r?$` and only the
    version span is ever replaced, so a CRLF file stays CRLF and a mixed file
    stays mixed.
    """
    try:
        raw = path.read_bytes()
    except OSError as exc:
        raise BumpError(f"{path}: cannot read ({exc}).") from exc

    bom = b""
    codec = ""
    for marker, name in _BOMS:
        if raw.startswith(marker):
            bom, codec = marker, name
            break

    if not codec:
        # A BOM-less UTF-16 .rc is legal and VS has been known to write one.
        if len(raw) >= 4 and raw[1] == 0 and raw[3] == 0 and raw[0] != 0:
            codec = "utf-16-le"
        elif len(raw) >= 4 and raw[0] == 0 and raw[2] == 0 and raw[1] != 0:
            codec = "utf-16-be"

    body = raw[len(bom) :]
    candidates = [codec] if codec else ["utf-8", "cp1252"]
    for name in candidates:
        try:
            text = body.decode(name)
        except (UnicodeDecodeError, LookupError):
            continue
        if bom + text.encode(name) == raw:
            return Source(path=path, text=text, codec=name, bom=bom, raw=raw)

    raise BumpError(
        f"{path}: could not decode with an encoding that round-trips "
        f"(tried {', '.join(candidates)}); refusing to rewrite it."
    )


def write_source(source: Source, text: str) -> None:
    source.path.write_bytes(source.encode(text))


# --------------------------------------------------------------------------
# Workspace members (for the Cargo.lock site)
# --------------------------------------------------------------------------


def workspace_members(root: Path) -> list[str]:
    """Package names of the workspace members, read from their manifests.

    The lock uses package names (`fwdslash`, `fswbroker`, `fswsettings`), not
    the directory names, so each member manifest has to be opened. Deriving the
    list keeps the Cargo.lock site correct when a crate is added -- and makes
    the expected match count move with it instead of silently under-counting.
    """
    manifest = read_source(root / "Cargo.toml").text
    block = re.search(r"^\[workspace\]\r?\n(.*?)^members = \[(.*?)\]", manifest, re.M | re.S)
    if not block:
        raise BumpError("Cargo.toml: no [workspace] members list.")
    names: list[str] = []
    for member in re.findall(r'"([^"]+)"', block[2]):
        member_toml = root / member / "Cargo.toml"
        text = read_source(member_toml).text
        found = re.search(r'^\[package\][^\[]*?^name = "([^"]+)"', text, re.M | re.S)
        if not found:
            raise BumpError(f"{member_toml}: no [package] name.")
        names.append(found[1])
    if not names:
        raise BumpError("Cargo.toml: [workspace] members is empty.")
    return sorted(names)


# --------------------------------------------------------------------------
# Scanning
# --------------------------------------------------------------------------


@dataclass
class Hit:
    site: Site
    start: int
    end: int
    found: str
    line: int


def compile_site(site: Site, members: list[str]) -> re.Pattern[str]:
    shape_re = SHAPES[site.shape][0]
    pattern = site.pattern.replace("{V}", f"(?P<ver>{shape_re})")
    pattern = pattern.replace("{NAMES}", "|".join(re.escape(n) for n in members))
    return re.compile(pattern, re.M)


def expected_count(site: Site, members: list[str]) -> int:
    return len(members) if site.count is None else site.count


def scan(root: Path, members: list[str]) -> tuple[dict[str, Source], list[Hit]]:
    """Match every site. Raises on a missing file or a wrong match count."""
    sources: dict[str, Source] = {}
    hits: list[Hit] = []
    problems: list[str] = []

    for site in SITES:
        path = root / site.path
        if not path.is_file():
            problems.append(f"{site.path}: registered file is missing.")
            continue
        if site.path not in sources:
            try:
                sources[site.path] = read_source(path)
            except BumpError as exc:
                problems.append(str(exc))
                continue
        source = sources[site.path]
        found = list(compile_site(site, members).finditer(source.text))
        want = expected_count(site, members)
        if len(found) != want:
            problems.append(
                f"{site.path}: expected {want} match(es) for {site.what!r}, "
                f"found {len(found)}. The literal moved or changed shape -- "
                f"update the Site in tools/bump_version.py."
            )
            continue
        for match in found:
            hits.append(
                Hit(
                    site=site,
                    start=match.start("ver"),
                    end=match.end("ver"),
                    found=match["ver"],
                    line=source.text.count("\n", 0, match.start("ver")) + 1,
                )
            )

    if problems:
        raise BumpError("\n".join(problems))
    return sources, hits


def current_version(root: Path) -> tuple[int, int, int]:
    source = read_source(root / SOURCE_OF_TRUTH.path)
    match = compile_site(SOURCE_OF_TRUTH, []).search(source.text)
    if not match:
        raise BumpError(f"{SOURCE_OF_TRUTH.path}: no {SOURCE_OF_TRUTH.what}.")
    return parse_version(match["ver"])


# --------------------------------------------------------------------------
# Rewriting
# --------------------------------------------------------------------------


def rewrite(
    sources: dict[str, Source], hits: list[Hit], version: tuple[int, int, int]
) -> dict[str, str]:
    """New text for every source. Nothing is written here."""
    by_file: dict[str, list[Hit]] = {}
    for hit in hits:
        by_file.setdefault(hit.site.path, []).append(hit)

    updated: dict[str, str] = {}
    for path, file_hits in by_file.items():
        text = sources[path].text
        # Right to left, so earlier offsets stay valid.
        for hit in sorted(file_hits, key=lambda h: h.start, reverse=True):
            text = text[: hit.start] + render(hit.site.shape, version) + text[hit.end :]
        updated[path] = text
    return updated


# --------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------


def summarise(hits: list[Hit]) -> list[tuple[str, str, int]]:
    counted: dict[tuple[str, str], int] = {}
    order: list[tuple[str, str]] = []
    for hit in hits:
        key = (hit.site.path, hit.site.what)
        if key not in counted:
            order.append(key)
        counted[key] = counted.get(key, 0) + 1
    return [(path, what, counted[(path, what)]) for path, what in order]


def print_table(rows: list[tuple[str, ...]], headers: tuple[str, ...], stream=None) -> None:
    out = stream or sys.stdout
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))
    header = "  ".join(h.ljust(widths[i]) for i, h in enumerate(headers))
    print("  " + header.rstrip(), file=out)
    print("  " + "  ".join("-" * w for w in widths), file=out)
    for row in rows:
        line = "  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row))
        print("  " + line.rstrip(), file=out)


# --------------------------------------------------------------------------
# Commands
# --------------------------------------------------------------------------


def do_check(root: Path, wanted: str | None) -> int:
    members = workspace_members(root)
    version = parse_version(wanted) if wanted else current_version(root)
    origin = "the command line" if wanted else "workspace.package.version"
    _sources, hits = scan(root, members)

    stale = [h for h in hits if h.found != render(h.site.shape, version)]
    if stale:
        print(
            f"version check FAILED: expected {format_version(version)} ({origin})",
            file=sys.stderr,
        )
        print(file=sys.stderr)
        rows = [
            (
                f"{h.site.path}:{h.line}",
                h.site.what,
                h.found,
                render(h.site.shape, version),
            )
            for h in stale
        ]
        print_table(rows, ("FILE", "WHAT", "FOUND", "EXPECTED"), stream=sys.stderr)
        print(file=sys.stderr)
        print(
            f"  {len(stale)} literal(s) disagree. Run "
            f"`python3 tools/bump_version.py {format_version(version)} --force` "
            "to bring them into line.",
            file=sys.stderr,
        )
        return 1

    files = len({h.site.path for h in hits})
    print(f"version check: {format_version(version)} ({origin})")
    print(f"  {len(hits)} literal(s) in {files} file(s) agree.")
    return 0


def canonicalise_lock(root: Path, version: tuple[int, int, int], members: list[str]) -> None:
    """Let cargo own Cargo.lock's shape; verify whatever comes back.

    The targeted rewrite above already produced a correct lock, so this is a
    no-op on a healthy tree -- it exists so the lock is whatever cargo would
    have written, not whatever this script's regex produced.
    """
    if not shutil.which("cargo"):
        print("  Cargo.lock: cargo not on PATH; kept the targeted rewrite.")
        return
    result = subprocess.run(
        [
            "cargo",
            "update",
            "--workspace",
            "--offline",
            "--quiet",
            "--manifest-path",
            str(root / "Cargo.toml"),
        ],
        capture_output=True,
        text=True,
        cwd=root,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip().splitlines()
        tail = detail[-1].strip() if detail else f"exit {result.returncode}"
        print(f"  Cargo.lock: `cargo update --workspace --offline` failed ({tail});")
        print("              kept the targeted rewrite.")
        return

    site = next(s for s in SITES if s.path == "Cargo.lock")
    text = read_source(root / "Cargo.lock").text
    found = [m["ver"] for m in compile_site(site, members).finditer(text)]
    want = render(site.shape, version)
    if len(found) != len(members) or any(v != want for v in found):
        raise BumpError(
            "Cargo.lock: `cargo update --workspace --offline` left the member "
            f"versions at {found or 'nothing'}, expected {len(members)} x {want}."
        )
    print("  Cargo.lock: canonicalised by `cargo update --workspace --offline`.")


def do_bump(
    root: Path,
    wanted: str,
    *,
    force: bool,
    dry_run: bool,
    use_cargo: bool,
    commit: bool,
    tag: bool,
) -> int:
    version = parse_version(wanted)
    members = workspace_members(root)
    old = current_version(root)

    if version == old and not force:
        raise BumpError(
            f"Already at {format_version(old)}. Pass --force to rewrite anyway "
            "(useful when --check found a stale copy)."
        )
    if version < old and not force:
        raise BumpError(
            f"Refusing to go backwards: {format_version(old)} -> "
            f"{format_version(version)}. Pass --force if that is deliberate."
        )

    sources, hits = scan(root, members)
    updated = rewrite(sources, hits, version)

    verb = "would rewrite" if dry_run else "rewriting"
    print(f"{format_version(old)} -> {format_version(version)} ({verb})")
    print()
    rows = [(path, what, str(n)) for path, what, n in summarise(hits)]
    print_table(rows, ("FILE", "WHAT", "N"))
    print()

    if dry_run:
        for path in sorted(updated):
            diff = changed_lines(sources[path].text, updated[path])
            if not diff:
                continue
            print(f"  {path}")
            for before, after in diff:
                print(f"    - {before}")
                print(f"    + {after}")
        print(f"  {len(hits)} literal(s) in {len(updated)} file(s); nothing written.")
        return 0

    # Every pattern matched and every count checked; only now does anything
    # touch the disk.
    for path in sorted(updated):
        write_source(sources[path], updated[path])
    print(f"  {len(hits)} literal(s) in {len(updated)} file(s) rewritten.")

    if use_cargo:
        canonicalise_lock(root, version, members)

    if commit or tag:
        vcs_finish(root, version, sorted(updated), commit=commit, tag=tag)

    return 0


def changed_lines(before: str, after: str) -> list[tuple[str, str]]:
    old_lines = before.splitlines()
    new_lines = after.splitlines()
    if len(old_lines) != len(new_lines):
        return []
    return [(a.rstrip("\r"), b.rstrip("\r")) for a, b in zip(old_lines, new_lines) if a != b]


def vcs(root: Path, *args: str) -> None:
    result = subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise BumpError(f"git {' '.join(args)} failed: {(result.stderr or '').strip()}")


def vcs_finish(
    root: Path, version: tuple[int, int, int], paths: list[str], *, commit: bool, tag: bool
) -> None:
    """Commit the rewritten files and/or tag. Never pushes."""
    label = format_version(version)
    if commit:
        vcs(root, "add", "--", *paths)
        vcs(root, "commit", "-m", f"Bump version to {label}\n\n{CO_AUTHOR}")
        print(f"  Committed: Bump version to {label}")
    if tag:
        vcs(root, "tag", "-a", f"v{label}", "-m", f"v{label}")
        print(f"  Tagged: v{label} (not pushed)")


# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="bump_version.py",
        description="Move every registered version literal in the tree at once.",
        epilog="Registered locations are the SITES table at the top of this file.",
    )
    parser.add_argument(
        "version",
        nargs="?",
        help="the new MAJOR.MINOR.PATCH version; omit it with --check",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify every literal agrees with Cargo.toml (or with VERSION) and exit",
    )
    parser.add_argument("--dry-run", action="store_true", help="show the edits, write nothing")
    parser.add_argument(
        "--force",
        action="store_true",
        help="allow the same version or a lower one",
    )
    parser.add_argument(
        "--no-cargo",
        action="store_true",
        help="skip the `cargo update --workspace --offline` pass over Cargo.lock",
    )
    parser.add_argument(
        "--commit", action="store_true", help="commit the rewritten files (never pushes)"
    )
    parser.add_argument(
        "--tag", action="store_true", help="create the annotated tag vX.Y.Z (never pushes)"
    )
    parser.add_argument(
        "--root",
        default=str(ROOT),
        help="repository root (defaults to the tree this script lives in)",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    root = Path(args.root).resolve()

    try:
        if args.check:
            for flag in ("dry_run", "commit", "tag"):
                if getattr(args, flag):
                    raise BumpError(
                        f"--check cannot be combined with --{flag.replace('_', '-')}."
                    )
            return do_check(root, args.version)
        if not args.version:
            raise BumpError("A version is required (or use --check).")
        if (args.commit or args.tag) and args.dry_run:
            raise BumpError("--dry-run cannot be combined with --commit or --tag.")
        return do_bump(
            root,
            args.version,
            force=args.force,
            dry_run=args.dry_run,
            use_cargo=not args.no_cargo,
            commit=args.commit,
            tag=args.tag,
        )
    except BumpError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
