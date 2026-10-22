#!/usr/bin/env python3
"""Permanent release-version regression tests; all fixtures live in TemporaryDirectory."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).with_name("bump-version.py")
SPEC = importlib.util.spec_from_file_location("bump_version", SCRIPT)
bump = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bump)


class BumpVersionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="spm-version-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.create_fixture("0.1.0-alpha.3")

    def create_fixture(self, version):
        (self.root / "crates/own").mkdir(parents=True, exist_ok=True)
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/own"]\n\n[workspace.package]\n'
            f'version = "{version}" # release version\nedition = "2021"\n\n'
            '[workspace.dependencies]\nexternal = "1"\n', encoding="utf-8")
        (self.root / "crates/own/Cargo.toml").write_text(
            '[package]\nname = "own"\nversion.workspace = true\n', encoding="utf-8")
        (self.root / "Cargo.lock").write_text(
            '# Cargo lockfile\nversion = 4\n\n[[package]]\nname = "own"\n'
            f'version = "{version}"\ndependencies = [\n "external",\n]\n\n'
            '[[package]]\nname = "external"\n'
            f'version = "{version}"\nsource = "registry+https://example.test/index"\n'
            'checksum = "unchanged"\ndependencies = [\n'
            f' "own {version}",\n]\n\n'
            '[[package]]\nname = "own"\n'
            f'version = "{version}"\nsource = "registry+https://example.test/index"\n'
            'checksum = "also-unchanged"\ndependencies = [\n'
            f' "own {version} (registry+https://example.test/index)",\n]\n', encoding="utf-8")
        for name, phrase in (("README.md", "pre-release"), ("README.pt-BR.md", "pré-lançamento")):
            (self.root / name).write_text(
                f'# Miner\n> Status: {phrase} **{version}**.\nHistorical version {version}.\n', encoding="utf-8")

    def snapshot(self):
        return {path.relative_to(self.root): path.read_bytes() for path in self.root.rglob("*") if path.is_file()}

    def run_script(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.root), *args],
                              capture_output=True, text=True, check=False)

    def assert_rejected_unchanged(self, *args):
        before = self.snapshot()
        result = self.run_script(*args)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("error:", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_default_bump_updates_only_owned_versions_and_markers(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0.1.0-alpha.3 -> 0.1.0-alpha.4", result.stdout)
        manifest = tomllib.loads((self.root / "Cargo.toml").read_text())
        self.assertEqual(manifest["workspace"]["package"]["version"], "0.1.0-alpha.4")
        packages = tomllib.loads((self.root / "Cargo.lock").read_text())["package"]
        self.assertEqual(packages[0]["version"], "0.1.0-alpha.4")
        self.assertEqual(packages[1]["version"], "0.1.0-alpha.3")
        self.assertEqual(packages[1]["dependencies"], ["own 0.1.0-alpha.4"])
        self.assertEqual(packages[1]["checksum"], "unchanged")
        self.assertEqual(packages[2]["version"], "0.1.0-alpha.3")
        self.assertEqual(packages[2]["dependencies"], [
            "own 0.1.0-alpha.3 (registry+https://example.test/index)"])
        for name in ("README.md", "README.pt-BR.md"):
            text = (self.root / name).read_text()
            self.assertIn("**0.1.0-alpha.4**", text)
            self.assertIn("Historical version 0.1.0-alpha.3.", text)
        self.assertFalse(list(self.root.rglob(".spm-version-*")))

    def test_prerelease_channels_and_stable_increment(self):
        for old, expected in (("1.2.3-alpha.9", "1.2.3-alpha.10"),
                              ("1.2.3-beta.2", "1.2.3-beta.3"),
                              ("1.2.3-rc.0", "1.2.3-rc.1"), ("1.2.3", "1.2.4")):
            with self.subTest(old=old):
                self.assertEqual(bump.next_version(old), expected)

    def test_explicit_version_accepts_release_transition(self):
        result = self.run_script("--version", "0.1.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('version = "0.1.0"', (self.root / "Cargo.toml").read_text())

    def test_dry_run_validates_without_writing(self):
        before = self.snapshot()
        result = self.run_script("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Would bump", result.stdout)
        self.assertEqual(self.snapshot(), before)

    def test_invalid_equal_and_older_versions_are_rejected(self):
        for version in ("v0.1.0-alpha.4", "0.01.0", "0.1.0-alpha.04", "0.1.0-alpha",
                        "0.1.0-dev.4", "0.1.0-alpha.4+build", "0.1.0-alpha.3",
                        "0.1.0-alpha.2", "0.0.9", "0.1.0-alpha.4\n"):
            with self.subTest(version=version):
                self.assert_rejected_unchanged("--version", version)

    def test_mismatched_lock_rejects_all_writes(self):
        path = self.root / "Cargo.lock"
        path.write_text(path.read_text().replace('version = "0.1.0-alpha.3"', 'version = "0.0.1"', 1))
        self.assert_rejected_unchanged()

    def test_missing_lock_entry_rejects_all_writes(self):
        path = self.root / "Cargo.lock"
        path.write_text(path.read_text().replace('name = "own"', 'name = "other"', 1))
        self.assert_rejected_unchanged()

    def test_missing_readme_marker_rejects_all_writes(self):
        (self.root / "README.pt-BR.md").write_text("# Sem marcador\n")
        self.assert_rejected_unchanged()

    def test_write_failure_restores_files_and_cleans_staging(self):
        _, _, originals, updated = bump.plan_bump(self.root)
        before = self.snapshot()
        real_replace = bump.os.replace
        calls = 0

        def fail_second_replace(source, destination):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("simulated write failure")
            return real_replace(source, destination)

        with mock.patch.object(bump.os, "replace", side_effect=fail_second_replace):
            with self.assertRaisesRegex(OSError, "simulated write failure"):
                bump.write_plan(originals, updated)
        self.assertEqual(self.snapshot(), before)

    def test_changed_input_is_preserved_and_rejected(self):
        _, _, originals, updated = bump.plan_bump(self.root)
        (self.root / "README.md").write_text("concurrent edit\n")
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "file changed while planning"):
            bump.write_plan(originals, updated)
        self.assertEqual(self.snapshot(), before)

    def test_rollback_failure_preserves_original_recovery_copy(self):
        _, _, originals, updated = bump.plan_bump(self.root)
        real_replace = bump.os.replace
        calls = 0

        def fail_after_first_replace(source, destination):
            nonlocal calls
            calls += 1
            if calls >= 2:
                raise OSError("simulated filesystem failure")
            return real_replace(source, destination)

        with mock.patch.object(bump.os, "replace", side_effect=fail_after_first_replace):
            with self.assertRaisesRegex(OSError, "original preserved at"):
                bump.write_plan(originals, updated)
        copies = list(self.root.rglob(".spm-version-*"))
        self.assertEqual(len(copies), 1)
        self.assertEqual(copies[0].read_bytes(), originals[self.root.resolve() / "Cargo.toml"])


if __name__ == "__main__":
    unittest.main()
