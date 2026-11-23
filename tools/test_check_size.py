"""Tests of check-size.py: what ships is counted (tests and README of the extension are not), a surface over its budget
is found, one that holds no file is no pass.

    python -m unittest discover tools
"""

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("check_size", Path(__file__).with_name("check-size.py"))
size = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(size)


class Counted(unittest.TestCase):
    def test_what_does_not_ship_is_left_out(self):
        paths = ["extension/src/a.js", "extension/scripts/test-a.mjs", "extension/README.md", "extension/.mwgrc", "extension/popup/p.js", "other/x.js"]
        self.assertEqual(size.shipped(paths, "extension", size.SURFACES["extension"][1]), ["extension/src/a.js", "extension/popup/p.js"])

    def test_a_folder_of_the_same_prefix_is_another_folder(self):
        self.assertEqual(size.shipped(["extension-old/a.js", "extension/a.js"], "extension", ()), ["extension/a.js"])

    def test_the_files_are_summed(self):
        with tempfile.TemporaryDirectory() as d:
            Path(d, "a").write_bytes(b"x" * 100)
            Path(d, "b").write_bytes(b"y" * 50)
            raw, zipped = size.measure(d, ["a", "b"])
            self.assertEqual(raw, 150)
            self.assertTrue(0 < zipped < 150)


class Budget(unittest.TestCase):
    def test_over_the_budget_is_found(self):
        budgets = {"ui": ("ui", (), 1000)}
        self.assertEqual(size.problems({"ui": 1001}, budgets), [("ui", 1001, 1000)])
        self.assertEqual(size.problems({"ui": 1000}, budgets), [])

    def test_the_repository_is_within_its_budgets(self):
        self.assertEqual(size.main([]), 0)

    def test_a_budget_below_the_measure_fails(self):
        saved = dict(size.SURFACES)
        try:
            folder, excluded, _ = saved["app page"]
            size.SURFACES["app page"] = (folder, excluded, 1000)
            self.assertEqual(size.main([]), 1)
        finally:
            size.SURFACES.clear()
            size.SURFACES.update(saved)

    def test_a_repository_without_the_folders_is_no_pass(self):
        with tempfile.TemporaryDirectory() as d:
            subprocess.run(["git", "init", "-q", d], check=True)
            Path(d, "a.txt").write_text("x")
            subprocess.run(["git", "add", "a.txt"], cwd=d, check=True)
            self.assertEqual(size.main(["--root", d]), 2)


if __name__ == "__main__":
    unittest.main()
