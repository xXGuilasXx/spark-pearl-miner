#!/usr/bin/env python3
"""Publish only CI-validated main artifacts; never replace a published release.

The workflow runs prepare -> provenance attestation -> publish. All GitHub writes
are confined to this helper and its write-enabled release job. Unit tests replace
the API client, so verification never creates remote releases or tags.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile
import tomllib
import urllib.error
import urllib.request


REPOSITORY = "xXGuilasXx/spark-pearl-miner"
API_ROOT = f"/repos/{REPOSITORY}"
APP = "spark-pearl-miner"
VERSION_PATTERN = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)


class ReleaseError(RuntimeError):
    """An unsafe, inconsistent or incomplete release was rejected."""


class GitHub:
    def request(self, method: str, path: str, data=None, *, missing_ok=False):
        token = os.environ.get("GH_TOKEN")
        if not token:
            raise ReleaseError("GH_TOKEN is required")
        request = urllib.request.Request(
            "https://api.github.com" + path,
            data=None if data is None else json.dumps(data).encode(),
            method=method,
            headers={
                "Authorization": f"Bearer {token}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                body = response.read()
                return json.loads(body) if body else None
        except urllib.error.HTTPError as error:
            if error.code == 404 and missing_ok:
                return None
            # Do not print the request or token in error output.
            raise ReleaseError(f"GitHub {method} {path}: HTTP {error.code}") from error

    def upload(self, tag: str, assets: list[Path]):
        subprocess.run(
            ["gh", "release", "upload", tag, *map(str, assets), "--repo", REPOSITORY],
            check=True,
        )


def marker(commit: str) -> str:
    return f"<!-- spm-automated-release-v1 commit:{commit} -->"


def output(name: str, value):
    destination = os.environ.get("GITHUB_OUTPUT")
    if destination:
        with open(destination, "a", encoding="utf-8") as stream:
            stream.write(f"{name}={value}\n")


def notice(message: str):
    print(message, flush=True)
    destination = os.environ.get("GITHUB_STEP_SUMMARY")
    if destination:
        with open(destination, "a", encoding="utf-8") as stream:
            stream.write(message + "\n\n")


def validate_artifact(directory: Path, expected_commit: str, source_root: Path):
    if re.fullmatch(r"[0-9a-f]{40}", expected_commit) is None:
        raise ReleaseError("expected commit must be a full lowercase SHA-1")
    metadata_file = directory / "RELEASE-METADATA.json"
    metadata = json.loads(metadata_file.read_text(encoding="utf-8"))
    if not isinstance(metadata, dict):
        raise ReleaseError("release metadata must be an object")
    if type(metadata.get("schema_version")) is not int or metadata["schema_version"] != 1:
        raise ReleaseError("unsupported release metadata schema")
    if metadata.get("commit") != expected_commit:
        raise ReleaseError("artifact commit differs from the CI commit")
    version = metadata.get("version", "")
    if not isinstance(version, str) or VERSION_PATTERN.fullmatch(version) is None:
        raise ReleaseError("invalid release version")
    source_version = tomllib.loads((source_root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if source_version != version:
        raise ReleaseError("artifact version differs from Cargo.toml")
    if metadata.get("target") != "linux-aarch64" or metadata.get("gpu_validation") != "not-run":
        raise ReleaseError("unexpected target or GPU validation claim")
    basename = f"{APP}-{version}-linux-aarch64"
    tarball = directory / f"{basename}.tar.gz"
    sums_file = directory / "SHA256SUMS"
    assets = [tarball, sums_file, metadata_file]
    if set(directory.iterdir()) != set(assets) or any(not item.is_file() or item.is_symlink() for item in assets):
        raise ReleaseError("artifact must contain exactly the tarball, SHA256SUMS and metadata")
    with tarball.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if sums_file.read_text(encoding="ascii") != f"{digest}  {tarball.name}\n":
        raise ReleaseError("tarball checksum or SHA256SUMS contents do not match")
    with tarfile.open(tarball, "r:gz") as archive:
        members = archive.getmembers()
        names = [member.name.rstrip("/") for member in members]
        if len(names) != len(set(names)):
            raise ReleaseError("archive contains duplicate entries")
        for member in members:
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != basename:
                raise ReleaseError("archive has an unexpected root or unsafe path")
            if not (member.isdir() or member.isfile()):
                raise ReleaseError("archive contains a link or special file")
        version_member = archive.getmember(f"{basename}/VERSION")
        binary_member = archive.getmember(f"{basename}/bin/{APP}")
        if not version_member.isfile() or version_member.size > 256 or not binary_member.isfile():
            raise ReleaseError("archive VERSION or binary is invalid")
        if archive.extractfile(version_member).read() != (version + "\n").encode():
            raise ReleaseError("archive VERSION differs from the metadata")
        header = archive.extractfile(binary_member).read(20)
        if len(header) != 20 or header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\xb7\x00":
            raise ReleaseError("release executable must be a 64-bit little-endian AArch64 ELF")
        if not binary_member.mode & 0o111:
            raise ReleaseError("release binary is not executable")
    return metadata, assets


def main_sha(api) -> str:
    return api.request("GET", API_ROOT + "/git/ref/heads/main")["object"]["sha"]


def tag_sha(api, tag: str):
    ref = api.request("GET", API_ROOT + f"/git/ref/tags/{tag}", missing_ok=True)
    if ref is None:
        return None
    obj = ref["object"]
    # Accept existing annotated tags only if they resolve to the exact commit.
    for _ in range(8):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            break
        obj = api.request("GET", API_ROOT + f"/git/tags/{obj['sha']}")["object"]
    raise ReleaseError("release tag does not resolve to a commit")


def get_release(api, tag: str):
    # The tag endpoint does not consistently expose unpublished drafts. Include
    # authenticated drafts from the paginated release listing before creating one.
    release = api.request("GET", API_ROOT + f"/releases/tags/{tag}", missing_ok=True)
    if release is not None:
        return release
    for page in range(1, 101):
        releases = api.request("GET", API_ROOT + f"/releases?per_page=100&page={page}")
        for release in releases:
            if release["tag_name"] == tag:
                return release
        if len(releases) < 100:
            return None
    raise ReleaseError("too many releases to safely locate existing drafts")


def owned_draft(release, commit: str) -> bool:
    return bool(release.get("draft") and release.get("target_commitish") == commit
                and marker(commit) in (release.get("body") or ""))


def save_state(path: Path, state):
    path.write_text(json.dumps(state), encoding="utf-8")


def cleanup(api, state_path: Path):
    state = json.loads(state_path.read_text())
    release = api.request("GET", API_ROOT + f"/releases/{state['release_id']}", missing_ok=True)
    if release is not None:
        if not owned_draft(release, state["commit"]):
            # A successful publish must survive subsequent runner/API failures.
            return
        api.request("DELETE", API_ROOT + f"/releases/{state['release_id']}")
    # No Git refs are created by prepare: GitHub creates the release tag when
    # publishing the complete draft. Never delete a ref that a human may have
    # created concurrently, even if it happens to point at the same commit.


def prepare(api, directory: Path, commit: str, state_path: Path, source_root: Path):
    output("should_publish", "false")
    metadata, assets = validate_artifact(directory, commit, source_root)
    version = metadata["version"]
    tag = "v" + version
    if main_sha(api) != commit:
        notice(f"Skipped {tag}: main has advanced beyond the validated commit.")
        return
    release = get_release(api, tag)
    if release is not None and not release.get("draft"):
        # Even when this version was built from another commit, it is immutable.
        notice(f"Skipped {tag}: this version is already published; its assets were preserved.")
        return
    existing_tag = tag_sha(api, tag)
    if existing_tag is not None and existing_tag != commit:
        raise ReleaseError(f"{tag} already points to a different commit; bump the version")
    if release is not None:
        if not owned_draft(release, commit):
            raise ReleaseError(f"{tag} has an unrelated draft; refusing to modify it")
        # A previous run may have been interrupted during upload/attestation.
        api.request("DELETE", API_ROOT + f"/releases/{release['id']}")
    generated = api.request("POST", API_ROOT + "/releases/generate-notes", {
        "tag_name": tag, "target_commitish": commit,
    })
    body = (
        marker(commit) + "\n\n" + generated["body"] + "\n\n"
        "### Validation and hardware coverage\n\n"
        "I ran the CPU/host test suites, Clippy, fee/checksum consistency checks, "
        "and the Linux ARM64 build with CUDA compilation and SASS checks before publishing.\n\n"
        "The archive targets Linux ARM64 / DGX Spark GB10. I did not execute GPU tests, "
        "mining, or pool end-to-end validation on a physical GB10 for this release. "
        "Compilation and host tests do not establish GB10 performance, power, or stability.\n\n"
        f"Validated source commit: `{commit}`. `RELEASE-METADATA.json` records this limitation.\n"
    )
    # Check again after API reads: an older queued run must not create a release.
    if main_sha(api) != commit:
        notice(f"Skipped {tag}: main advanced while preparing the release.")
        return
    release = api.request("POST", API_ROOT + "/releases", {
        "tag_name": tag, "target_commitish": commit,
        "name": f"{APP} {version}", "body": body,
        "draft": True, "prerelease": "-" in version, "make_latest": "false",
    })
    state = {"release_id": release["id"], "tag": tag, "commit": commit,
             "asset_hashes": {asset.name: hashlib.sha256(asset.read_bytes()).hexdigest() for asset in assets}}
    save_state(state_path, state)
    output("release_id", release["id"])
    api.upload(tag, assets)
    verify_assets(api, state)
    output("should_publish", "true")
    notice(f"Prepared draft {tag} from `{commit}`; awaiting provenance attestation.")


def verify_assets(api, state):
    assets = api.request("GET", API_ROOT + f"/releases/{state['release_id']}/assets?per_page=100")
    if len(assets) != len(state["asset_hashes"]):
        raise ReleaseError("release has missing or unexpected assets")
    for asset in assets:
        expected = state["asset_hashes"].get(asset["name"])
        if expected is None or asset.get("state") != "uploaded" or asset.get("digest") != "sha256:" + expected:
            raise ReleaseError("uploaded release asset checksum does not match the validated artifact")


def publish(api, directory: Path, commit: str, state_path: Path, source_root: Path):
    validate_artifact(directory, commit, source_root)
    state = json.loads(state_path.read_text())
    if state["commit"] != commit:
        raise ReleaseError("release state does not match the validated commit")
    release = api.request("GET", API_ROOT + f"/releases/{state['release_id']}")
    if not release.get("draft"):
        notice(f"Skipped {state['tag']}: the release is already published.")
        return
    if not owned_draft(release, commit) or release["tag_name"] != state["tag"]:
        raise ReleaseError("release draft changed unexpectedly")
    verify_assets(api, state)
    existing_tag = tag_sha(api, state["tag"])
    if existing_tag is not None and existing_tag != commit:
        raise ReleaseError("release tag changed before publication")
    if main_sha(api) != commit:
        cleanup(api, state_path)
        notice(f"Skipped {state['tag']}: main advanced before publication; removed the unpublished draft.")
        return
    # The workflow serializes release jobs. The tag always targets this full SHA,
    # even if a new main push occurs immediately after the final main check.
    release = api.request("PATCH", API_ROOT + f"/releases/{state['release_id']}", {
        "draft": False, "make_latest": "false",
    })
    if release.get("draft") or tag_sha(api, state["tag"]) != commit:
        raise ReleaseError("published release did not resolve to the validated commit")
    notice(f"Published [{state['tag']}]({release['html_url']}) from `{commit}` with all three assets and provenance.")


def cli():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stage", choices=("prepare", "publish", "cleanup"))
    parser.add_argument("--artifact-dir", required=True, type=Path)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--state", required=True, type=Path)
    args = parser.parse_args()
    api = GitHub()
    source_root = Path(__file__).resolve().parent.parent
    if args.stage == "cleanup":
        cleanup(api, args.state)
    else:
        {"prepare": prepare, "publish": publish}[args.stage](
            api, args.artifact_dir, args.expected_commit, args.state, source_root,
        )


if __name__ == "__main__":
    try:
        cli()
    except (ReleaseError, OSError, ValueError, KeyError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"Release rejected: {error}", file=sys.stderr)
        sys.exit(1)
