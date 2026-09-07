#!/usr/bin/env python3
"""Prepare, validate, tag, and atomically push a Catify release."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
from dataclasses import dataclass

ROOT = pathlib.Path(__file__).resolve().parents[1]
SEMVER_RE = re.compile(
    r"^(?P<major>0|[1-9]\d*)\."
    r"(?P<minor>0|[1-9]\d*)\."
    r"(?P<patch>0|[1-9]\d*)"
    r"(?:-(?P<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
CFY_DEPENDENCY_RE = re.compile(
    r'^(?P<prefix>cfy-[A-Za-z0-9-]+\s*=\s*\{\s*version\s*=\s*")'
    r'(?P<version>[^"]+)'
    r'(?P<suffix>".*)$',
    re.MULTILINE,
)


class ReleaseError(RuntimeError):
    """A release invariant was not satisfied."""


@dataclass(frozen=True, order=True)
class Version:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...] = ()

    @classmethod
    def parse(cls, value: str) -> "Version":
        match = SEMVER_RE.fullmatch(value)
        if match is None:
            raise ReleaseError(f"invalid semantic version: {value}")
        prerelease = match.group("prerelease")
        return cls(
            int(match.group("major")),
            int(match.group("minor")),
            int(match.group("patch")),
            tuple(prerelease.split(".")) if prerelease else (),
        )

    def __str__(self) -> str:
        base = f"{self.major}.{self.minor}.{self.patch}"
        return f"{base}-{'.'.join(self.prerelease)}" if self.prerelease else base


def next_version(
    current: Version,
    bump: str = "prerelease",
    preid: str = "pre",
) -> Version:
    if not re.fullmatch(r"[0-9A-Za-z-]+", preid):
        raise ReleaseError("PREID must contain only ASCII letters, digits, and hyphens")

    if bump == "release":
        if not current.prerelease:
            raise ReleaseError(f"{current} is already a stable release")
        return Version(current.major, current.minor, current.patch)

    if bump == "prerelease":
        if not current.prerelease:
            return Version(current.major, current.minor, current.patch + 1, (preid, "0"))
        parts = list(current.prerelease)
        if parts[-1].isdigit():
            parts[-1] = str(int(parts[-1]) + 1)
        else:
            parts.append("0")
        return Version(current.major, current.minor, current.patch, tuple(parts))

    if bump == "patch":
        return Version(current.major, current.minor, current.patch + 1)
    if bump == "minor":
        return Version(current.major, current.minor + 1, 0)
    if bump == "major":
        return Version(current.major + 1, 0, 0)
    if bump == "prepatch":
        return Version(current.major, current.minor, current.patch + 1, (preid, "0"))
    if bump == "preminor":
        return Version(current.major, current.minor + 1, 0, (preid, "0"))
    if bump == "premajor":
        return Version(current.major + 1, 0, 0, (preid, "0"))
    raise ReleaseError(
        "BUMP must be prerelease, release, patch, minor, major, prepatch, preminor, or premajor"
    )


def workspace_version(root: pathlib.Path = ROOT) -> Version:
    cargo = (root / "Cargo.toml").read_text()
    match = re.search(r"\[workspace\.package\].*?^version\s*=\s*\"([^\"]+)\"", cargo, re.DOTALL | re.MULTILINE)
    if match is None:
        raise ReleaseError("workspace.package.version is missing from Cargo.toml")
    return Version.parse(match.group(1))


def replace_versions(root: pathlib.Path, old: Version, new: Version) -> None:
    cargo_path = root / "Cargo.toml"
    cargo = cargo_path.read_text()
    old_text = str(old)
    new_text = str(new)
    package_pattern = re.compile(
        r"(\[workspace\.package\].*?^version\s*=\s*\")([^\"]+)(\")",
        re.DOTALL | re.MULTILINE,
    )
    cargo, count = package_pattern.subn(rf"\g<1>{new_text}\g<3>", cargo, count=1)
    if count != 1:
        raise ReleaseError("could not update workspace.package.version")

    def replace_dependency(match: re.Match[str]) -> str:
        if match.group("version") != old_text:
            raise ReleaseError(
                f"workspace dependency version {match.group('version')} does not match {old_text}"
            )
        return f"{match.group('prefix')}{new_text}{match.group('suffix')}"

    cargo, dependency_count = CFY_DEPENDENCY_RE.subn(replace_dependency, cargo)
    if dependency_count == 0:
        raise ReleaseError("no cfy workspace dependency versions were updated")
    cargo_path.write_text(cargo)

    package_path = root / "package.json"
    package = json.loads(package_path.read_text())
    if package.get("version") != old_text:
        raise ReleaseError(
            f"package.json version {package.get('version')} does not match workspace version {old_text}"
        )
    package["version"] = new_text
    package_path.write_text(json.dumps(package, indent=2) + "\n")


def sync_lockfile(root: pathlib.Path = ROOT) -> None:
    """Refresh workspace package versions before any --locked release gate."""
    run("cargo", "update", "--workspace", "--offline", root=root)


def run(*args: str, root: pathlib.Path = ROOT, capture: bool = False) -> str:
    result = subprocess.run(
        args,
        cwd=root,
        check=True,
        text=True,
        capture_output=capture,
    )
    return result.stdout.strip() if capture else ""


def ensure_release_safe(remote: str, branch: str, root: pathlib.Path = ROOT) -> str:
    if run("git", "status", "--porcelain", root=root, capture=True):
        raise ReleaseError("working tree must be clean before releasing")
    current_branch = run("git", "branch", "--show-current", root=root, capture=True)
    if current_branch != branch:
        raise ReleaseError(f"release must run on {branch}; current branch is {current_branch or 'detached'}")
    run("git", "fetch", remote, branch, "--tags", root=root)
    upstream = f"{remote}/{branch}"
    counts = run(
        "git", "rev-list", "--left-right", "--count", f"{upstream}...HEAD", root=root, capture=True
    ).split()
    if len(counts) != 2:
        raise ReleaseError("could not compare the local branch with its remote")
    behind = int(counts[0])
    if behind:
        raise ReleaseError(f"local {branch} is behind {upstream} by {behind} commit(s)")
    run("git", "config", "user.name", root=root, capture=True)
    run("git", "config", "user.email", root=root, capture=True)
    return run("git", "rev-parse", "HEAD", root=root, capture=True)


def release(args: argparse.Namespace) -> None:
    current = workspace_version()
    if args.version:
        selected = Version.parse(args.version)
        if selected <= current:
            raise ReleaseError(f"VERSION must be greater than current version {current}")
    else:
        selected = next_version(current, args.bump, args.preid)
    tag = f"v{selected}"

    original_head = ensure_release_safe(args.remote, args.branch)
    if run("git", "tag", "--list", tag, capture=True):
        raise ReleaseError(f"tag {tag} already exists locally")
    remote_tag = run("git", "ls-remote", "--tags", args.remote, f"refs/tags/{tag}", capture=True)
    if remote_tag:
        raise ReleaseError(f"tag {tag} already exists on {args.remote}")

    print(f"Catify release plan: {current} -> {selected} ({tag})")
    if args.dry_run:
        print("Dry run only: no files, commits, tags, or remotes were changed.")
        return

    committed = False
    tagged = False
    try:
        replace_versions(ROOT, current, selected)
        sync_lockfile()
        run("make", "_release-candidate", f"VERSION={selected}")
        run("git", "add", "Cargo.toml", "Cargo.lock", "package.json")
        run("git", "commit", "-m", f"chore(release): {tag}")
        committed = True
        run("git", "tag", "-a", tag, "-m", f"Catify {tag}")
        tagged = True
        run(
            "git",
            "push",
            "--atomic",
            args.remote,
            f"HEAD:refs/heads/{args.branch}",
            f"refs/tags/{tag}",
        )
    except (ReleaseError, subprocess.CalledProcessError):
        if tagged:
            subprocess.run(["git", "tag", "-d", tag], cwd=ROOT, check=False)
        if committed:
            subprocess.run(["git", "reset", "--hard", original_head], cwd=ROOT, check=False)
        else:
            subprocess.run(
                ["git", "restore", "--source", original_head, "--", "Cargo.toml", "Cargo.lock", "package.json"],
                cwd=ROOT,
                check=False,
            )
        raise

    print(f"Released and pushed Catify {tag}.")
    print("GitHub Actions will build and publish the release artifacts from the tag.")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bump", default="prerelease")
    parser.add_argument("--preid", default="pre")
    parser.add_argument("--version")
    parser.add_argument("--remote", default="origin")
    parser.add_argument("--branch", default="main")
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args()


def main() -> None:
    try:
        release(parse_args())
    except ReleaseError as error:
        print(f"release failed: {error}", file=sys.stderr)
        raise SystemExit(2) from error
    except subprocess.CalledProcessError as error:
        command = " ".join(str(part) for part in error.cmd)
        print(f"release failed while running: {command}", file=sys.stderr)
        raise SystemExit(error.returncode or 1) from error


if __name__ == "__main__":
    main()
