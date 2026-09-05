#!/usr/bin/env python3
"""Unit tests for tools/bump_version.py.

Every test builds a temporary tree from byte-exact copies of the real
registered files, so the fixtures can never drift from the tree the script
actually maintains -- and `test_check_passes_on_the_repo` asserts the live tree
agrees with itself, which is the same assertion CI makes.

    python3 tools/test_bump_version.py
    python3 -m unittest tools.test_bump_version -v
"""

from __future__ import annotations

import codecs
import io
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import bump_version as bv  # noqa: E402

REPO = Path(__file__).resolve().parent.parent

# Derived, never hardcoded: the fixtures are copies of the live files, so a
# real bump must not turn this suite red.
OLD_V = bv.current_version(REPO)
NEW_V = (OLD_V[0], OLD_V[1], OLD_V[2] + 1)
OLD = bv.format_version(OLD_V)
NEW = bv.format_version(NEW_V)


def _previous(version: tuple[int, int, int]) -> tuple[int, int, int] | None:
    """The largest version below `version`, or None at 0.0.0."""
    major, minor, patch = version
    if patch:
        return (major, minor, patch - 1)
    if minor:
        return (major, minor - 1, 0)
    if major:
        return (major - 1, 0, 0)
    return None


PREV_V = _previous(OLD_V)

# Total literals the SITES table claims, so the counts below track the table.
MEMBERS = bv.workspace_members(REPO)
TOTAL = sum(bv.expected_count(site, MEMBERS) for site in bv.SITES)


def swap(line: str, old: str = OLD, new: str = NEW) -> str:
    """The same line with every shape of `old` rewritten to `new`."""
    o = tuple(int(p) for p in old.split("."))
    n = tuple(int(p) for p in new.split("."))
    for shape in ("commas4", "dotted4", "dotted3"):  # longest/most specific first
        line = line.replace(bv.render(shape, o), bv.render(shape, n))
    return line


def run(*argv: str) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = bv.main(list(argv))
    return code, out.getvalue(), err.getvalue()


class TreeCase(unittest.TestCase):
    """A temp repo holding copies of every file bump_version.py touches."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="fsw-bump-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)

        wanted = {site.path for site in bv.SITES}
        # workspace_members() opens each member manifest for its package name.
        for member in re.findall(
            r'"([^"]+)"',
            re.search(
                r"^\[workspace\]\r?\n(.*?)^members = \[(.*?)\]",
                (REPO / "Cargo.toml").read_text(encoding="utf-8"),
                re.M | re.S,
            )[2],
        ):
            wanted.add(f"{member}/Cargo.toml")

        self.files = sorted(wanted)
        for rel in self.files:
            dest = self.tmp / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO / rel, dest)
        self.before = {rel: (self.tmp / rel).read_bytes() for rel in self.files}

    def bump(self, version: str = NEW, *extra: str) -> tuple[int, str, str]:
        return run(version, "--root", str(self.tmp), "--no-cargo", *extra)

    def check(self, *extra: str) -> tuple[int, str, str]:
        return run("--check", "--root", str(self.tmp), *extra)

    def assert_tree_unchanged(self, *ignore: str) -> None:
        for rel in self.files:
            if rel in ignore:
                continue
            self.assertEqual(
                self.before[rel],
                (self.tmp / rel).read_bytes(),
                f"{rel} was written despite the run failing",
            )


class RepoTests(unittest.TestCase):
    def test_check_passes_on_the_repo(self):
        """The live tree agrees with workspace.package.version -- the CI gate."""
        code, out, err = run("--check", "--root", str(REPO))
        self.assertEqual(code, 0, err)
        self.assertIn("agree", out)

    def test_every_site_matches_the_repo(self):
        """No registered pattern is dead: each matches its declared count."""
        members = bv.workspace_members(REPO)
        _sources, hits = bv.scan(REPO, members)
        by_site: dict[tuple[str, str], int] = {}
        for hit in hits:
            by_site[(hit.site.path, hit.site.what)] = (
                by_site.get((hit.site.path, hit.site.what), 0) + 1
            )
        for site in bv.SITES:
            self.assertEqual(
                by_site.get((site.path, site.what), 0),
                bv.expected_count(site, members),
                f"{site.path}: {site.what}",
            )

    def test_source_of_truth_is_registered_once(self):
        self.assertEqual(bv.current_version(REPO), OLD_V)
        self.assertEqual(
            [s for s in bv.SITES if s.path == "Cargo.toml"], [bv.SOURCE_OF_TRUTH]
        )


class BumpTests(TreeCase):
    def test_bump_moves_every_literal(self):
        code, out, err = self.bump()
        self.assertEqual(code, 0, err)
        self.assertIn(f"{OLD} -> {NEW}", out)

        members = bv.workspace_members(self.tmp)
        _sources, hits = bv.scan(self.tmp, members)
        self.assertEqual(len(hits), TOTAL)
        for hit in hits:
            self.assertEqual(
                hit.found,
                bv.render(hit.site.shape, NEW_V),
                f"{hit.site.path}:{hit.line} ({hit.site.what})",
            )

    def test_bump_changes_only_the_registered_lines(self):
        code, _out, err = self.bump()
        self.assertEqual(code, 0, err)

        touched = 0
        for rel in self.files:
            old = self.before[rel].decode("utf-8")
            new = (self.tmp / rel).read_bytes().decode("utf-8")
            old_lines = old.splitlines(keepends=True)
            new_lines = new.splitlines(keepends=True)
            self.assertEqual(len(old_lines), len(new_lines), f"{rel} changed line count")
            for i, (a, b) in enumerate(zip(old_lines, new_lines), start=1):
                if a == b:
                    continue
                touched += 1
                # A differing line is the old line with the version swapped --
                # nothing else, including its line ending, may move.
                self.assertEqual(swap(a), b, f"{rel}:{i} changed beyond the version")
        # Every registered literal sits on a line of its own.
        self.assertEqual(touched, TOTAL, "expected one changed line per literal")

    def test_member_manifests_are_untouched(self):
        """Member crates inherit `version.workspace = true`; nothing to rewrite."""
        code, _out, err = self.bump()
        self.assertEqual(code, 0, err)
        for rel in self.files:
            if rel.endswith("/Cargo.toml"):
                self.assertEqual(self.before[rel], (self.tmp / rel).read_bytes(), rel)

    def test_cargo_lock_touches_only_workspace_members(self):
        code, _out, err = self.bump()
        self.assertEqual(code, 0, err)
        lock = (self.tmp / "Cargo.lock").read_text(encoding="utf-8")
        members = set(bv.workspace_members(self.tmp))
        for block in lock.split("[[package]]"):
            name = re.search(r'^name = "([^"]+)"', block.strip(), re.M)
            version = re.search(r'^version = "([^"]+)"', block.strip(), re.M)
            if not name or not version:
                continue
            if name[1] in members:
                self.assertEqual(version[1], NEW, name[1])
            else:
                self.assertNotEqual(version[1], NEW, f"{name[1]} was rewritten")

    def test_check_passes_after_a_bump(self):
        self.assertEqual(self.bump()[0], 0)
        code, out, err = self.check()
        self.assertEqual(code, 0, err)
        self.assertIn(NEW, out)

    def test_check_fails_when_one_location_is_stale(self):
        self.assertEqual(self.bump()[0], 0)
        # Put one file back the way it was; Cargo.toml still carries the new one.
        (self.tmp / "CMakeLists.txt").write_bytes(self.before["CMakeLists.txt"])
        code, _out, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("FAILED", err)
        self.assertIn("CMakeLists.txt", err)
        self.assertIn(OLD, err)
        self.assertIn(NEW, err)

    def test_check_lists_every_stale_location(self):
        self.assertEqual(self.bump()[0], 0)
        for rel in ("CMakeLists.txt", "SECURITY.md", "assets/fwdslash.rc"):
            (self.tmp / rel).write_bytes(self.before[rel])
        code, _out, err = self.check()
        self.assertEqual(code, 1)
        # fwdslash.rc carries four of them (two numeric, two string).
        self.assertIn("6 literal(s) disagree", err)

    def test_check_against_an_explicit_version(self):
        code, _out, err = self.check(NEW)
        self.assertEqual(code, 1, f"the tree is at {OLD}, not {NEW}")
        self.assertIn("the command line", err)
        self.assertEqual(self.check(OLD)[0], 0)


class RefusalTests(TreeCase):
    @unittest.skipIf(PREV_V is None, "the tree is at 0.0.0; nothing is lower")
    def test_refuses_to_go_backwards(self):
        code, _out, err = self.bump(bv.format_version(PREV_V))
        self.assertEqual(code, 1)
        self.assertIn("backwards", err)
        self.assert_tree_unchanged()

    def test_refuses_the_same_version(self):
        code, _out, err = self.bump(OLD)
        self.assertEqual(code, 1)
        self.assertIn("Already at", err)
        self.assert_tree_unchanged()

    def test_force_allows_the_same_version(self):
        code, _out, err = self.bump(OLD, "--force")
        self.assertEqual(code, 0, err)
        self.assert_tree_unchanged()  # rewriting a version as itself is a no-op

    def test_force_allows_going_backwards(self):
        self.assertEqual(self.bump()[0], 0)
        code, _out, err = self.bump(OLD, "--force")
        self.assertEqual(code, 0, err)
        for rel in self.files:
            self.assertEqual(self.before[rel], (self.tmp / rel).read_bytes(), rel)

    def test_refuses_a_malformed_version(self):
        for bad in ("0.0", "1.2.3.4", "v1.2.3", "1.2.x", "0.0.3-rc1", "1.2.3 ", "0.0.-1"):
            with self.subTest(version=bad):
                code, _out, err = self.bump(bad)
                self.assertEqual(code, 1, bad)
                self.assertIn("MAJOR.MINOR.PATCH", err)
                self.assert_tree_unchanged()

    def test_refuses_an_empty_version(self):
        code, _out, err = self.bump("")
        self.assertEqual(code, 1)
        self.assertIn("A version is required", err)
        self.assert_tree_unchanged()

    def test_check_rejects_incompatible_flags(self):
        for flag in ("--dry-run", "--commit", "--tag"):
            with self.subTest(flag=flag):
                code, _out, err = run("--check", "--root", str(self.tmp), flag)
                self.assertEqual(code, 1)
                self.assertIn("--check cannot be combined", err)

    def test_a_version_is_required_without_check(self):
        code, _out, err = run("--root", str(self.tmp))
        self.assertEqual(code, 1)
        self.assertIn("A version is required", err)

    def test_dry_run_writes_nothing(self):
        code, out, err = self.bump(NEW, "--dry-run")
        self.assertEqual(code, 0, err)
        self.assertIn("nothing written", out)
        self.assertIn(f'- #define FSW_VER_STR "{OLD}"', out)
        self.assertIn(f'+ #define FSW_VER_STR "{NEW}"', out)
        self.assert_tree_unchanged()


class MatchCountTests(TreeCase):
    def test_a_missing_literal_fails_the_count(self):
        rc = self.tmp / "assets/fwdslash.rc"
        text = rc.read_text(encoding="utf-8")
        line = f' PRODUCTVERSION {bv.render("commas4", OLD_V)}\n'
        rc.write_text(text.replace(line, ""), encoding="utf-8")

        code, _out, err = self.bump()
        self.assertEqual(code, 1)
        self.assertIn("expected 2 match(es)", err)
        self.assertIn("found 1", err)
        self.assertIn("tools/bump_version.py", err)

    def test_nothing_is_written_when_one_site_fails(self):
        rc = self.tmp / "crates/fsw-cli/app.rc"
        rc.write_text(
            rc.read_text(encoding="utf-8").replace(
                f'FSW_VER_STR "{OLD}"', "FSW_VER_STR VER"
            ),
            encoding="utf-8",
        )
        self.before["crates/fsw-cli/app.rc"] = rc.read_bytes()

        code, _out, err = self.bump()
        self.assertEqual(code, 1)
        self.assertIn("crates/fsw-cli/app.rc", err)
        self.assert_tree_unchanged()

    def test_an_extra_literal_fails_the_count(self):
        cmake = self.tmp / "CMakeLists.txt"
        text = cmake.read_text(encoding="utf-8")
        cmake.write_text(
            text.replace(
                f"project(ForwardSlashWindows VERSION {OLD} LANGUAGES CXX)",
                f"project(ForwardSlashWindows VERSION {OLD} LANGUAGES CXX)\n"
                f"project(ForwardSlashWindows VERSION {OLD} LANGUAGES CXX)",
            ),
            encoding="utf-8",
        )
        code, _out, err = self.bump()
        self.assertEqual(code, 1)
        self.assertIn("expected 1 match(es)", err)
        self.assertIn("found 2", err)

    def test_a_missing_file_fails(self):
        (self.tmp / "SECURITY.md").unlink()
        code, _out, err = self.bump()
        self.assertEqual(code, 1)
        self.assertIn("SECURITY.md: registered file is missing", err)
        self.assert_tree_unchanged("SECURITY.md")

    def test_a_new_workspace_member_moves_the_expected_count(self):
        """The Cargo.lock count is derived, so it tracks the members list."""
        members = bv.workspace_members(self.tmp)
        site = next(s for s in bv.SITES if s.path == "Cargo.lock")
        self.assertIsNone(site.count)
        self.assertEqual(bv.expected_count(site, members), len(members))
        self.assertEqual(
            members, ["fsw-core", "fsw-path", "fswbroker", "fswsettings", "fwdslash"]
        )


class EncodingTests(TreeCase):
    def test_crlf_is_preserved(self):
        raw_before = self.before["tools/Package.ps1"]
        self.assertIn(b"\r\n", raw_before)  # guards the fixture itself
        self.assertEqual(self.bump()[0], 0)
        raw_after = (self.tmp / "tools/Package.ps1").read_bytes()
        self.assertEqual(raw_before.count(b"\r\n"), raw_after.count(b"\r\n"))
        self.assertEqual(
            raw_before.count(b"\n") - raw_before.count(b"\r\n"),
            raw_after.count(b"\n") - raw_after.count(b"\r\n"),
            "a lone LF appeared or vanished",
        )
        self.assertIn(f'"forward-slash-windows-{NEW}-{{0}}"'.encode(), raw_after)

    def test_line_endings_survive_in_every_file(self):
        self.assertEqual(self.bump()[0], 0)
        for rel in self.files:
            after = (self.tmp / rel).read_bytes()
            before = self.before[rel]
            with self.subTest(file=rel):
                self.assertEqual(before.count(b"\r\n"), after.count(b"\r\n"))
                self.assertEqual(before.count(b"\n"), after.count(b"\n"))
                self.assertEqual(before.count(b"\r"), after.count(b"\r"))

    def test_trailing_bytes_survive(self):
        """A file with no final newline keeps not having one."""
        self.assertEqual(self.bump()[0], 0)
        for rel in self.files:
            with self.subTest(file=rel):
                self.assertEqual(
                    self.before[rel].endswith(b"\n"),
                    (self.tmp / rel).read_bytes().endswith(b"\n"),
                )

    def _round_trip(self, name: str, raw: bytes, expect_codec: str) -> None:
        path = self.tmp / name
        path.write_bytes(raw)
        source = bv.read_source(path)
        self.assertEqual(source.codec, expect_codec, name)
        self.assertEqual(source.encode(source.text), raw, f"{name} did not round-trip")
        bv.write_source(source, source.text.replace("0.0.3", "0.0.4"))
        self.assertEqual(
            path.read_bytes(), raw.replace("0.0.3".encode(expect_codec), "0.0.4".encode(expect_codec))
        )

    def test_utf16_le_with_bom_round_trips(self):
        body = '#define FSW_VER_STR "0.0.3"\r\n'
        self._round_trip("u16le.rc", codecs.BOM_UTF16_LE + body.encode("utf-16-le"), "utf-16-le")

    def test_utf16_be_with_bom_round_trips(self):
        body = '#define FSW_VER_STR "0.0.3"\r\n'
        self._round_trip("u16be.rc", codecs.BOM_UTF16_BE + body.encode("utf-16-be"), "utf-16-be")

    def test_utf16_le_without_a_bom_round_trips(self):
        body = '#define FSW_VER_STR "0.0.3"\r\n'
        self._round_trip("bare16.rc", body.encode("utf-16-le"), "utf-16-le")

    def test_utf8_with_bom_round_trips(self):
        body = '#define FSW_VER_STR "0.0.3"\r\n'
        self._round_trip("u8bom.rc", codecs.BOM_UTF8 + body.encode("utf-8"), "utf-8")

    def test_ansi_round_trips(self):
        # 0x93/0x94 are cp1252 smart quotes and invalid UTF-8, so this file can
        # only be read as ANSI -- and must be written back as ANSI.
        raw = b'// \x93Forward Slash\x94\r\n#define FSW_VER_STR "0.0.3"\r\n'
        self._round_trip("ansi.rc", raw, "cp1252")

    def test_an_undecodable_file_is_refused(self):
        path = self.tmp / "broken.rc"
        path.write_bytes(b"\xff\xfe\x00")  # a UTF-16-LE BOM and half a code unit
        with self.assertRaises(bv.BumpError):
            bv.read_source(path)


@unittest.skipUnless(shutil.which("git"), "git is not on PATH")
class VcsTests(TreeCase):
    def _init_repo(self) -> None:
        for args in (
            ["init", "--quiet", "-b", "work"],
            ["config", "user.email", "test@example.invalid"],
            ["config", "user.name", "Bump Version Test"],
            ["add", "--all"],
            ["commit", "--quiet", "-m", "fixture"],
        ):
            subprocess.run(
                ["git", *args], cwd=self.tmp, check=True, capture_output=True, text=True
            )

    def _run(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args], cwd=self.tmp, check=True, capture_output=True, text=True
        ).stdout

    def test_commit_and_tag(self):
        self._init_repo()
        code, out, err = self.bump(NEW, "--commit", "--tag")
        self.assertEqual(code, 0, err)
        self.assertIn("Committed", out)
        self.assertIn("Tagged", out)

        message = self._run("log", "-1", "--format=%B").rstrip("\n")
        self.assertEqual(message.splitlines()[0], f"Bump version to {NEW}")
        self.assertTrue(message.endswith(bv.CO_AUTHOR), message)

        self.assertEqual(self._run("tag", "--list").split(), [f"v{NEW}"])
        self.assertEqual(self._run("status", "--porcelain").strip(), "")

    def test_no_commit_by_default(self):
        self._init_repo()
        self.assertEqual(self.bump()[0], 0)
        self.assertEqual(self._run("log", "--oneline").strip().count("\n"), 0)
        self.assertNotEqual(self._run("status", "--porcelain").strip(), "")

    def test_commit_stages_only_the_registered_files(self):
        self._init_repo()
        (self.tmp / "unrelated.txt").write_text("not part of the bump\n", encoding="utf-8")
        self.assertEqual(self.bump(NEW, "--commit")[0], 0)
        self.assertIn("unrelated.txt", self._run("status", "--porcelain"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
