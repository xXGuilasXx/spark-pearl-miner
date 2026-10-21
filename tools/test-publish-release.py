#!/usr/bin/env python3
"""Offline release contract, integrity, immutability and race regression tests."""

import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("publish_release", Path(__file__).with_name("publish-release.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)
COMMIT = "a" * 40
OTHER = "b" * 40
VERSION = "0.1.0-alpha.3"
TAG = "v" + VERSION
ROOT = f"spark-pearl-miner-{VERSION}-linux-aarch64"
PREFIX = release.API_ROOT


class FakeGitHub:
    """A stateful API double; every write is recorded, never sent to GitHub."""

    def __init__(self):
        self.main = COMMIT
        self.tag = None
        self.release = None
        self.assets = []
        self.writes = []

    def request(self, method, path, data=None, *, missing_ok=False):
        if method != "GET":
            self.writes.append((method, path, data))
        if path == PREFIX + "/git/ref/heads/main":
            return {"object": {"sha": self.main}}
        if path == PREFIX + f"/git/ref/tags/{TAG}":
            return None if self.tag is None else {"object": {"type": "commit", "sha": self.tag}}
        if path == PREFIX + f"/releases/tags/{TAG}":
            # Exercise the authenticated-list fallback for draft releases.
            return self.release if self.release and not self.release["draft"] else None
        if path == PREFIX + "/releases?per_page=100&page=1":
            return [] if self.release is None else [self.release]
        if path == PREFIX + "/releases/generate-notes" and method == "POST":
            return {"body": "## Changes\n\n- Community contribution (#4)"}
        if path == PREFIX + "/releases" and method == "POST":
            if self.release is not None:
                raise AssertionError("release already exists")
            self.release = {"id": 1, "html_url": "https://github.com/example/release", **data}
            return self.release.copy()
        if path == PREFIX + "/releases/1":
            if method == "GET":
                return None if self.release is None else self.release.copy()
            if method == "DELETE":
                self.release = None
                self.assets = []
                return None
            if method == "PATCH":
                self.release.update(data)
                if not self.release["draft"]:
                    self.tag = self.release["target_commitish"]
                return self.release.copy()
        if path == PREFIX + "/releases/1/assets?per_page=100":
            return self.assets.copy()
        if path == PREFIX + f"/git/refs/tags/{TAG}" and method == "DELETE":
            self.tag = None
            return None
        raise AssertionError(f"unexpected API request: {method} {path}")

    def upload(self, tag, assets):
        assert tag == TAG
        self.writes.append(("UPLOAD", tag, None))
        self.assets = [
            {"name": item.name, "state": "uploaded",
             "digest": "sha256:" + hashlib.sha256(item.read_bytes()).hexdigest()}
            for item in assets
        ]


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="spm-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.artifact = self.root / "artifact"
        self.artifact.mkdir()
        self.state = self.root / "state.json"
        (self.root / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{VERSION}"\n')
        self.metadata = {
            "schema_version": 1, "commit": COMMIT, "version": VERSION,
            "target": "linux-aarch64", "gpu_validation": "not-run",
        }
        self.tarball = self.artifact / f"{ROOT}.tar.gz"
        self.archive()
        self.write_metadata()
        self.api = FakeGitHub()
        # Tests never write to an Actions runner's output/summary files.
        self.environment = patch.dict("os.environ", {"GITHUB_OUTPUT": "", "GITHUB_STEP_SUMMARY": ""})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def archive(self, *, extra=None, machine=b"\xb7\x00", archived_version=VERSION):
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        header[18:20] = machine
        with tarfile.open(self.tarball, "w:gz") as archive:
            for name, data in [(f"{ROOT}/VERSION", (archived_version + "\n").encode()),
                               (f"{ROOT}/bin/spark-pearl-miner", bytes(header))]:
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o755
                archive.addfile(member, io.BytesIO(data))
            if extra:
                member = tarfile.TarInfo(extra)
                archive.addfile(member, io.BytesIO(b""))
        self.write_sums()

    def write_sums(self):
        digest = hashlib.sha256(self.tarball.read_bytes()).hexdigest()
        (self.artifact / "SHA256SUMS").write_text(f"{digest}  {self.tarball.name}\n")

    def write_metadata(self):
        (self.artifact / "RELEASE-METADATA.json").write_text(json.dumps(self.metadata))

    def validate(self):
        return release.validate_artifact(self.artifact, COMMIT, self.root)

    def prepare(self):
        with contextlib.redirect_stdout(io.StringIO()):
            release.prepare(self.api, self.artifact, COMMIT, self.state, self.root)

    def publish(self):
        with contextlib.redirect_stdout(io.StringIO()):
            release.publish(self.api, self.artifact, COMMIT, self.state, self.root)

    def test_valid_arm64_artifact(self):
        metadata, assets = self.validate()
        self.assertEqual(metadata["commit"], COMMIT)
        self.assertEqual(len(assets), 3)

    def test_tampered_tarball_rejected(self):
        with self.tarball.open("ab") as stream:
            stream.write(b"tampered")
        with self.assertRaisesRegex(release.ReleaseError, "checksum"):
            self.validate()

    def test_checksum_manifest_cannot_add_another_file(self):
        with (self.artifact / "SHA256SUMS").open("a") as stream:
            stream.write("0" * 64 + "  another-file\n")
        with self.assertRaisesRegex(release.ReleaseError, "SHA256SUMS"):
            self.validate()

    def test_commit_must_be_exact_ci_sha(self):
        self.metadata["commit"] = OTHER
        self.write_metadata()
        with self.assertRaisesRegex(release.ReleaseError, "CI commit"):
            self.validate()

    def test_short_expected_commit_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "full lowercase"):
            release.validate_artifact(self.artifact, COMMIT[:12], self.root)

    def test_metadata_version_must_match_source(self):
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "9.0.0"\n')
        with self.assertRaisesRegex(release.ReleaseError, "Cargo.toml"):
            self.validate()

    def test_metadata_cannot_claim_gpu_execution(self):
        self.metadata["gpu_validation"] = "passed"
        self.write_metadata()
        with self.assertRaisesRegex(release.ReleaseError, "GPU validation"):
            self.validate()

    def test_schema_boolean_is_not_a_version(self):
        self.metadata["schema_version"] = True
        self.write_metadata()
        with self.assertRaisesRegex(release.ReleaseError, "schema"):
            self.validate()

    def test_archive_version_must_match_metadata(self):
        self.archive(archived_version="0.1.0-alpha.2")
        with self.assertRaisesRegex(release.ReleaseError, "archive VERSION differs"):
            self.validate()

    def test_x86_binary_cannot_be_shipped_as_arm64(self):
        self.archive(machine=b"\x3e\x00")
        with self.assertRaisesRegex(release.ReleaseError, "AArch64 ELF"):
            self.validate()

    def test_archive_path_traversal_rejected_without_extracting(self):
        self.archive(extra=f"{ROOT}/../../escape")
        with self.assertRaisesRegex(release.ReleaseError, "unsafe path"):
            self.validate()
        self.assertFalse((self.root / "escape").exists())

    def test_duplicate_archive_member_rejected(self):
        self.archive(extra=f"{ROOT}/VERSION")
        with self.assertRaisesRegex(release.ReleaseError, "duplicate"):
            self.validate()

    def test_artifact_directory_must_have_exact_files(self):
        (self.artifact / "unexpected").write_text("data")
        with self.assertRaisesRegex(release.ReleaseError, "exactly"):
            self.validate()

    def test_superseded_ci_does_not_write_anything(self):
        self.api.main = OTHER
        self.prepare()
        self.assertEqual(self.api.writes, [])
        self.assertFalse(self.state.exists())

    def test_published_version_is_immutable_even_at_different_commit(self):
        self.api.release = {"id": 1, "tag_name": TAG, "draft": False, "target_commitish": OTHER}
        self.api.tag = OTHER
        self.prepare()
        self.assertEqual(self.api.writes, [])
        self.assertEqual(self.api.release["target_commitish"], OTHER)

    def test_conflicting_unpublished_tag_rejected(self):
        self.api.tag = OTHER
        with self.assertRaisesRegex(release.ReleaseError, "different commit"):
            self.prepare()
        self.assertEqual(self.api.writes, [])

    def test_unrelated_draft_is_preserved(self):
        self.api.release = {"id": 1, "tag_name": TAG, "draft": True, "target_commitish": COMMIT, "body": "Human draft"}
        with self.assertRaisesRegex(release.ReleaseError, "unrelated draft"):
            self.prepare()
        self.assertEqual(self.api.writes, [])

    def test_prepare_and_publish_exact_commit_with_all_assets(self):
        self.prepare()
        self.assertTrue(self.api.release["draft"])
        self.assertTrue(self.api.release["prerelease"])
        self.assertIn("did not execute GPU tests", self.api.release["body"])
        self.assertEqual(len(self.api.assets), 3)
        self.publish()
        self.assertFalse(self.api.release["draft"])
        self.assertEqual(self.api.tag, COMMIT)
        writes = len(self.api.writes)
        self.publish()
        self.assertEqual(len(self.api.writes), writes)

    def test_main_advancing_before_publish_removes_own_draft(self):
        self.prepare()
        self.api.main = OTHER
        self.publish()
        self.assertIsNone(self.api.release)
        self.assertIsNone(self.api.tag)

    def test_tag_change_before_publish_rejected(self):
        self.prepare()
        self.api.tag = OTHER
        with self.assertRaisesRegex(release.ReleaseError, "tag changed"):
            self.publish()
        self.assertTrue(self.api.release["draft"])

    def test_remote_asset_digest_must_match_local_artifact(self):
        self.prepare()
        self.api.assets[0]["digest"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(release.ReleaseError, "asset checksum"):
            self.publish()
        self.assertTrue(self.api.release["draft"])

    def test_cleanup_preserves_successfully_published_release(self):
        self.prepare()
        self.publish()
        before = len(self.api.writes)
        release.cleanup(self.api, self.state)
        self.assertEqual(len(self.api.writes), before)
        self.assertFalse(self.api.release["draft"])
        self.assertEqual(self.api.tag, COMMIT)

    def test_cleanup_removes_own_draft_but_preserves_concurrently_created_tag(self):
        self.prepare()
        self.api.tag = COMMIT
        release.cleanup(self.api, self.state)
        self.assertIsNone(self.api.release)
        self.assertEqual(self.api.tag, COMMIT)

    def test_cleanup_preserves_preexisting_tag(self):
        self.api.tag = COMMIT
        self.prepare()
        release.cleanup(self.api, self.state)
        self.assertIsNone(self.api.release)
        self.assertEqual(self.api.tag, COMMIT)

    def test_own_interrupted_draft_can_be_retried(self):
        self.prepare()
        self.prepare()
        self.publish()
        self.assertFalse(self.api.release["draft"])
        self.assertEqual(self.api.tag, COMMIT)


if __name__ == "__main__":
    unittest.main()
