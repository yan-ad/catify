#!/usr/bin/env python3
"""Validate that a release tag, Cargo workspace, and npm package share one version."""

import argparse
import json
import pathlib
import re
import tomllib


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()

    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", args.tag):
        parser.error("tag must look like v1.2.3 or v1.2.3-rc.1")

    root = pathlib.Path(__file__).resolve().parents[1]
    cargo_version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    npm_version = json.loads((root / "package.json").read_text())["version"]
    tag_version = args.tag.removeprefix("v")

    versions = {"tag": tag_version, "Cargo.toml": cargo_version, "package.json": npm_version}
    if len(set(versions.values())) != 1:
        details = ", ".join(f"{name}={version}" for name, version in versions.items())
        parser.error(f"release versions do not match: {details}")
    print(tag_version)


if __name__ == "__main__":
    main()
