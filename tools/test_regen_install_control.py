"""Offline regression for the temporary bindings generator's Windows launcher."""

import re
import subprocess
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

from tools import regen_install_control as generator


class GeneratorTests(unittest.TestCase):
    def test_generation_launches_a_neutral_helper(self):
        body = "// generated fixture\n"
        launched = []

        def fake_run(command, *, cwd, env):
            manifest = tomllib.loads((cwd / "Cargo.toml").read_text(encoding="utf-8"))
            names = [manifest["package"]["name"]]
            names.extend(target["name"] for target in manifest.get("bin", []))
            for name in names:
                # Windows installer detection can reject an otherwise ordinary
                # executable with ERROR_ELEVATION_REQUIRED based on its name.
                self.assertIsNone(
                    re.search(r"install|setup|update", name, re.IGNORECASE),
                    f"temporary executable name triggers installer detection: {name}",
                )
            self.assertEqual(env["RUSTUP_TOOLCHAIN"], generator.pinned_toolchain())
            if command[1] == "run":
                launched.append(command)
                output = Path(command[command.index("--out") + 1])
                output.write_text(body, encoding="utf-8")

        probe = subprocess.CompletedProcess(["rustfmt", "--version"], 0, "fixture")
        with (
            patch.object(generator, "locate_winmd", return_value=Path("fixture.winmd")),
            patch.object(generator.subprocess, "run", return_value=probe),
            patch.object(generator, "run", side_effect=fake_run),
        ):
            fresh = generator.generate(verbose=False)

        self.assertEqual(len(launched), 1)
        self.assertTrue(fresh.endswith(body))


if __name__ == "__main__":
    unittest.main()
