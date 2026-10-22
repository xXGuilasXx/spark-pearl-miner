#!/usr/bin/env python3
"""Bump the workspace release version without changing external dependencies.

Requires Python 3.11+ (tomllib); does not run Git or Cargo. All inputs are validated
before any file is replaced. Use --dry-run to preview the next version.
"""

import argparse
import os
from pathlib import Path
import re
import sys
import tempfile
import tomllib


VERSION_RE = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-(alpha|beta|rc)\.(0|[1-9][0-9]*))?"
)


def version_key(version):
    match = VERSION_RE.fullmatch(version)
    if not match:
        raise ValueError(f"unsupported release version: {version!r}; use X.Y.Z[-alpha.N|-beta.N|-rc.N]")
    major, minor, patch, channel, number = match.groups()
    return (int(major), int(minor), int(patch),
            {"alpha": 0, "beta": 1, "rc": 2, None: 3}[channel], int(number or 0))


def next_version(version):
    major, minor, patch, _, _ = version_key(version)
    match = VERSION_RE.fullmatch(version)
    channel, number = match.group(4, 5)
    if channel:
        return f"{major}.{minor}.{patch}-{channel}.{int(number) + 1}"
    return f"{major}.{minor}.{patch + 1}"


def replace_version_line(text, old, new, description):
    pattern = re.compile(r'(?m)^(version\s*=\s*)"' + re.escape(old) + r'"(?=\s*(?:#.*)?$)')
    result, count = pattern.subn(lambda m: m.group(1) + f'"{new}"', text)
    if count != 1:
        raise ValueError(f"{description}: expected exactly one version = {old!r}")
    return result


def owned_packages(root, workspace):
    members = workspace.get("members", [])
    excluded = {path.resolve() for pattern in workspace.get("exclude", []) for path in root.glob(pattern)}
    manifests = set()
    for pattern in members:
        matches = [path for path in root.glob(pattern) if path.resolve() not in excluded]
        if not matches:
            raise ValueError(f"workspace member not found: {pattern}")
        manifests.update(path / "Cargo.toml" for path in matches)
    names = set()
    for manifest in sorted(manifests):
        if not manifest.resolve().is_relative_to(root):
            raise ValueError(f"workspace member escapes repository: {manifest}")
        package = tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {})
        name = package.get("name")
        if not name or name in names:
            raise ValueError(f"missing or duplicate workspace package name in {manifest}")
        if package.get("version") != {"workspace": True}:
            raise ValueError(f"{name}: expected version.workspace = true")
        names.add(name)
    if not names:
        raise ValueError("workspace has no packages")
    return names


def update_lock(text, names, old, new):
    # Read each TOML package block; only source-less workspace entries are ours.
    blocks = re.split(r"(?m)(?=^\[\[package\]\]\s*$)", text)
    seen = set()
    output = [blocks[0]]
    for block in blocks[1:]:
        package = tomllib.loads(block)["package"][0]
        name = package["name"]
        if name in names and "source" not in package:
            if name in seen or package["version"] != old:
                raise ValueError(f"Cargo.lock: duplicate or mismatched workspace entry for {name}")
            seen.add(name)
            block = replace_version_line(block, old, new, f"Cargo.lock/{name}")
        # Cargo adds a version to dependency references when names are ambiguous.
        # Source-qualified registry/Git references must retain their old version.
        for name in names:
            reference = re.compile(r'(?m)^(\s*"' + re.escape(name + " " + old) + r')("\s*,?\s*)$')
            block = reference.sub(lambda m, name=name: m.group(1).replace(name + " " + old, name + " " + new) + m.group(2), block)
        output.append(block)
    if seen != names:
        raise ValueError("Cargo.lock: missing workspace packages: " + ", ".join(sorted(names - seen)))
    result = "".join(output)
    tomllib.loads(result)
    return result


def plan_bump(root, explicit=None):
    root = root.resolve()
    paths = [root / name for name in ("Cargo.toml", "Cargo.lock", "README.md", "README.pt-BR.md")]
    for path in paths:
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"expected a regular, non-symlink file: {path}")
    originals = {path: path.read_bytes() for path in paths}
    texts = {path: content.decode("utf-8") for path, content in originals.items()}
    manifest_path, lock_path, *readmes = paths
    manifest = tomllib.loads(texts[manifest_path])
    workspace = manifest["workspace"]
    old = workspace["package"]["version"]
    version_key(old)
    new = explicit if explicit is not None else next_version(old)
    if version_key(new) <= version_key(old):
        raise ValueError(f"new version {new!r} must be greater than current version {old!r}")
    names = owned_packages(root, workspace)

    section = re.compile(r"(?ms)^\[workspace\.package\][^\n]*\n.*?(?=^\[|\Z)")
    matches = list(section.finditer(texts[manifest_path]))
    if len(matches) != 1:
        raise ValueError("Cargo.toml: expected exactly one [workspace.package] table")
    match = matches[0]
    replacement = replace_version_line(match.group(), old, new, "Cargo.toml")
    texts[manifest_path] = texts[manifest_path][:match.start()] + replacement + texts[manifest_path][match.end():]
    texts[lock_path] = update_lock(texts[lock_path], names, old, new)
    for path in readmes:
        marker = re.compile(r"(?m)^(>[^\n]*(?:pre-release|pré-lançamento) \*\*)" + re.escape(old) + r"(\*\*)")
        texts[path], count = marker.subn(lambda m: m.group(1) + new + m.group(2), texts[path])
        if count != 1:
            raise ValueError(f"{path.name}: expected one status version marker for {old}")
    if tomllib.loads(texts[manifest_path])["workspace"]["package"]["version"] != new:
        raise ValueError("Cargo.toml: new version validation failed")
    return old, new, originals, {path: text.encode("utf-8") for path, text in texts.items()}


def write_plan(originals, updated):
    staged = {}
    backups = {}
    replaced = []
    retained_backups = set()
    try:
        # Prepare both new contents and rollback copies before changing any input.
        for path in originals:
            if path.read_bytes() != originals[path]:
                raise ValueError(f"file changed while planning: {path}")
            for contents, destination in ((updated[path], staged), (originals[path], backups)):
                with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".spm-version-", delete=False) as temp:
                    destination[path] = Path(temp.name)
                    temp.write(contents)
                    temp.flush()
                    os.fsync(temp.fileno())
                os.chmod(destination[path], path.stat().st_mode & 0o777)
        for path in originals:
            if path.read_bytes() != originals[path]:
                raise ValueError(f"file changed before writing: {path}")
        for path in originals:
            os.replace(staged[path], path)
            replaced.append(path)
    except Exception as error:
        failures = []
        for path in reversed(replaced):
            try:
                os.replace(backups[path], path)
            except OSError as rollback_error:
                retained_backups.add(backups[path])
                failures.append(f"{path}: original preserved at {backups[path]} ({rollback_error})")
        if failures:
            raise OSError("rollback incomplete; " + "; ".join(failures)) from error
        raise
    finally:
        for temp in (*staged.values(), *backups.values()):
            if temp not in retained_backups:
                temp.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1], help="repository root")
    parser.add_argument("--version", help="explicit higher version (otherwise increment prerelease number or patch)")
    parser.add_argument("--dry-run", action="store_true", help="validate all inputs and print the proposed bump without writing")
    args = parser.parse_args()
    try:
        old, new, originals, updated = plan_bump(args.root, args.version)
        if not args.dry_run:
            write_plan(originals, updated)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"error: {error}\n")
    print(f"{'Would bump' if args.dry_run else 'Bumped'} workspace version: {old} -> {new}")


if __name__ == "__main__":
    main()
