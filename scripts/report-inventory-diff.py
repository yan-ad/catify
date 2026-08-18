#!/usr/bin/env python3
"""Report added, removed, and changed commands between inventory snapshots."""
import argparse
import json
import pathlib


def index(snapshot: dict) -> dict[str, dict]:
    return {command["name"]: command for command in snapshot["commands"]}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("before", type=pathlib.Path)
    parser.add_argument("after", type=pathlib.Path)
    args = parser.parse_args()
    before, after = index(json.loads(args.before.read_text())), index(json.loads(args.after.read_text()))
    added = sorted(set(after) - set(before))
    removed = sorted(set(before) - set(after))
    changed = sorted(name for name in set(before) & set(after) if before[name] != after[name])
    print("# Shopify CLI inventory diff\n")
    print(f"- Added: {len(added)}")
    print(f"- Removed: {len(removed)}")
    print(f"- Changed: {len(changed)}\n")
    for title, values in (("Added", added), ("Removed", removed), ("Changed", changed)):
        print(f"## {title}\n")
        for value in values:
            print(f"- `{value}`")
        if not values:
            print("- None")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
