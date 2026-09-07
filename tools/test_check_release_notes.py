"""Tests for the release-notes linter.

The fixture that matters is `SHIPPED_0_1_0`: the notes that actually reached a
Store submission before anyone read them. If this suite ever passes that text,
the gate is not doing its job.
"""

import unittest

from tools.check_release_notes import problems

SHIPPED_0_1_0 = """# Forward Slash Windows 0.1.0

## Major fixes

- **Self-update no longer hangs the settings window.** Route 1a now watches queued Store items for a bounded 3-minute admission window instead of 45 minutes in-process. A garbage-collection sweep on every packaged `check`/`status` removes tasks, sidecars and lock tokens. See issue #140.

## Downloads

See the [GitHub release](https://example.invalid) for signed bundles.
"""

GOOD = """- Updating from inside the app no longer gets stuck saying "Installing" forever.
- Typing `cd ..` at a distribution root now resolves to `/` instead of failing.
"""


class LinterTests(unittest.TestCase):
    def test_the_notes_that_shipped_are_rejected(self):
        found = problems(SHIPPED_0_1_0)
        self.assertTrue(found, "the 0.1.0 notes must not pass")
        joined = " ".join(found)
        self.assertIn("heading", joined)
        self.assertIn("#140", joined)
        self.assertIn("Downloads", joined)

    def test_plain_customer_copy_passes(self):
        self.assertEqual(problems(GOOD), [])

    def test_a_command_a_user_types_is_not_an_identifier(self):
        # The product is about typing paths, so these have to survive.
        for line in ("- Type `/etc/apt` in the address bar.", "- `cd ..` works.", "- Use `/` alone."):
            self.assertEqual(problems(line + "\n"), [], line)

    def test_code_identifiers_are_rejected(self):
        for span in (
            "`resolve_user_slash_path`",
            "`UpdateAttemptStore`",
            "`crates/fsw-cli/src/update`",
            "`main.rs`",
            "`fsw_core::update`",
            "`stage_helper()`",
        ):
            found = problems(f"- Something about {span} here.\n")
            self.assertTrue(found, f"{span} should be rejected")

    def test_a_fenced_block_is_rejected(self):
        found = problems("- A change.\n\n```\ncargo test\n```\n")
        self.assertTrue(any("fenced" in problem for problem in found), found)

    def test_empty_notes_are_rejected(self):
        self.assertTrue(problems("\n"))

    def test_the_downloads_heading_is_rejected(self):
        # release.yml appends its own; one here produced the duplicate that
        # shipped in the 0.1.0 release body.
        found = problems("- A change.\n\n## Downloads\n")
        self.assertTrue(any("Downloads" in problem for problem in found), found)


if __name__ == "__main__":
    unittest.main()
