"""Validate non-public command classifications and emit compatibility status."""
import argparse
import json
import pathlib
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("snapshot", type=pathlib.Path)
    parser.add_argument("--classifications", type=pathlib.Path, default=pathlib.Path("inventory/command-classifications.json"))
    parser.add_argument("--report", type=pathlib.Path)
    args = parser.parse_args()
    snapshot = json.loads(args.snapshot.read_text())
    data = json.loads(args.classifications.read_text())
    table = data["classifications"]
    rows = []
    invalid = []
    for command in snapshot["commands"]:
        name = command["name"]
        is_hidden = command.get("hidden", False)
        entry = table.get(name, table.get(name.split()[0]))
        if is_hidden and (not entry or not entry.get("kind") or not entry.get("rationale")):
            invalid.append(name)
            continue
        if entry is None:
            entry = {"kind": "public", "status": "missing-implementation", "owner": None,
                     "rationale": "Public runtime command has no complete Catify implementation yet."}
        rows.append({"name": name, "public": not is_hidden, **entry,
                     "intentional_exclusion": entry["status"] == "intentional-exclusion"})
    report = {"schema_version": 1, "commands": rows, "invalid": invalid,
              "intentional_exclusions": sum(r["intentional_exclusion"] for r in rows),
              "missing_implementation": sum(r["status"] == "missing-implementation" for r in rows)}
    report["public_missing_implementation"] = sum(
        r["public"] and r["status"] == "missing-implementation" for r in rows
    )
    report["hidden_intentional_exclusions"] = sum(
        not r["public"] and r["intentional_exclusion"] for r in rows
    )
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    if invalid:
        print("commands without classification rationale:", file=sys.stderr)
        print("\n".join(f"- {name}" for name in invalid), file=sys.stderr)
        return 1
    print(f"classified {len(rows)} non-public/deferred runtime commands; {report['intentional_exclusions']} intentional exclusions, {report['missing_implementation']} missing implementations")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
