#!/usr/bin/env python3
"""Lint `docs/release-notes/<version>.md`.

That file is the only thing Microsoft Store customers read: `release.yml` appends
its own asset table under a `## Downloads` heading, and `publish-to-store.yml`
truncates the finished body there, so everything the notes file contains becomes
the Store's "What's new". CLAUDE.md's rule for it is plain language, one short
sentence per user-visible change, with no file or function names, no issue
numbers and no internals. The file must not carry the heading itself, or the
release body gets two.

0.1.0 shipped notes that broke every part of that rule and reached a Store
submission before anyone noticed, which is why this is a gate rather than a
convention.

What is deliberately *not* banned: backticks around things a user types, like
`cd ..` or `/etc/apt`. Those are the subject of the product. What is banned is
backticks around code identifiers, which is the tell that a note was written
for developers.

    python3 tools/check_release_notes.py            # every notes file
    python3 tools/check_release_notes.py 0.1.0      # just one
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

NOTES_DIR = Path(__file__).resolve().parent.parent / "docs" / "release-notes"

# The marker publish-to-store.yml truncates at. Everything above it is customer
# copy; the line itself is the only heading a notes file may carry.
DOWNLOADS_MARKER = "## Downloads"

# A backticked span that names code rather than something a user types.
IDENTIFIER_TELLS = (
    re.compile(r"::"),                       # Rust paths
    re.compile(r"\.(rs|toml|ps1|py|yml|md|cmd|psm1)\b"),
    re.compile(r"\b(crates|docs|tools|shell)/"),
    re.compile(r"\(\)$"),                    # a function call
    re.compile(r"^[a-z]+(_[a-z0-9]+)+$"),    # snake_case identifier
    re.compile(r"^[A-Z][a-z0-9]+([A-Z][a-z0-9]*)+$"),  # CamelCase type
)


def problems(text: str) -> list[str]:
    """Every rule this file breaks, as sentences naming the offending line."""
    found: list[str] = []
    body = text.partition(DOWNLOADS_MARKER)[0]

    for number, line in enumerate(body.splitlines(), start=1):
        stripped = line.strip()
        if stripped.startswith("```"):
            found.append(f"line {number}: a fenced code block. Describe it in words.")
        if stripped.startswith("#"):
            found.append(
                f"line {number}: a heading ({stripped!r}). The notes are a flat list "
                f"of sentences; the only heading is the {DOWNLOADS_MARKER!r} marker."
            )
        for issue in re.findall(r"(?<![\w/])#\d+", line):
            found.append(f"line {number}: an issue reference ({issue}). Customers cannot open it.")
        for span in re.findall(r"`([^`]+)`", line):
            for tell in IDENTIFIER_TELLS:
                if tell.search(span):
                    found.append(
                        f"line {number}: `{span}` names code, not something a user types."
                    )
                    break

    if DOWNLOADS_MARKER in text:
        found.append(
            f"a {DOWNLOADS_MARKER!r} heading. release.yml appends its own asset table "
            f"under that heading, so one here produces a duplicate — which is exactly "
            f"what shipped in the 0.1.0 release body."
        )
    if not body.strip():
        found.append("no notes at all above the marker.")
    return found


def main(argv: list[str]) -> int:
    if argv:
        files = [NOTES_DIR / f"{version}.md" for version in argv]
    else:
        files = sorted(NOTES_DIR.glob("*.md"))
    missing = [path for path in files if not path.is_file()]
    for path in missing:
        print(f"error: {path} does not exist", file=sys.stderr)
    if missing:
        return 1

    failed = False
    for path in files:
        found = problems(path.read_text(encoding="utf-8"))
        if found:
            failed = True
            print(f"{path.relative_to(NOTES_DIR.parent.parent)}:", file=sys.stderr)
            for problem in found:
                print(f"  {problem}", file=sys.stderr)
    if failed:
        print(
            "\nThese notes are what Store customers read. Write one short sentence "
            "per user-visible change, in plain language.",
            file=sys.stderr,
        )
        return 1
    print(f"release notes: {len(files)} file(s) read as customer copy.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
