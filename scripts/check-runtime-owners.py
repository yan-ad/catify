#!/usr/bin/env python3
"""Fail when a public runtime command has no roadmap owner."""
import argparse
import json
import pathlib
import sys


def owner_for(name: str, owners: dict) -> int | None:
    if name in owners:
        return owners[name]
    root = name.split()[0]
    return owners.get(root, owners.get("default"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("snapshot", type=pathlib.Path)
    parser.add_argument("--owners", type=pathlib.Path, default=pathlib.Path("inventory/command-owners.json"))
    args = parser.parse_args()
    snapshot = json.loads(args.snapshot.read_text())
    owners = json.loads(args.owners.read_text())
    missing = [
        command["name"]
        for command in snapshot["commands"]
        if not command.get("hidden", False) and owner_for(command["name"], owners) is None
    ]
    if missing:
        print("public runtime commands without roadmap owners:", file=sys.stderr)
        print("\n".join(f"- {name}" for name in missing), file=sys.stderr)
        return 1
    print(f"all {len(snapshot['commands']) - sum(c.get('hidden', False) for c in snapshot['commands'])} public runtime commands have roadmap owners")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
