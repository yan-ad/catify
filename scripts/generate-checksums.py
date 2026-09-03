#!/usr/bin/env python3
"""Generate a deterministic SHA256SUMS file for release assets."""

import argparse
import hashlib
import pathlib


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("files", nargs="+", type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()

    files = sorted({path.resolve() for path in args.files if path.is_file()}, key=lambda path: path.name)
    if not files:
        parser.error("no release files found")
    if len({path.name for path in files}) != len(files):
        parser.error("release asset filenames must be unique")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("".join(f"{digest(path)}  {path.name}\n" for path in files))
    print(args.output)


if __name__ == "__main__":
    main()
