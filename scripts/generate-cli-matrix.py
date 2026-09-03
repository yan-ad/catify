#!/usr/bin/env python3
"""Validate and render Catify's Shopify CLI command parity matrix."""

from __future__ import annotations

import argparse
import collections
import json
import pathlib
import sys
from typing import Any

VALID_STATUSES = {
    "native",
    "adapter",
    "partial",
    "library-only",
    "blocked",
    "name-mismatch",
    "missing",
}
IMPLEMENTED_STATUSES = {"native", "adapter"}


def load(path: pathlib.Path) -> Any:
    return json.loads(path.read_text())


def command_domain(name: str) -> str:
    return name.split(" ", 1)[0]


def validate(runtime: dict[str, Any], status_doc: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    runtime_names = {entry["name"] for entry in runtime["commands"]}
    entries = status_doc.get("commands", {})
    status_names = set(entries)

    for name in sorted(runtime_names - status_names):
        errors.append(f"missing status entry: {name}")
    for name in sorted(status_names - runtime_names):
        errors.append(f"status entry not present in runtime inventory: {name}")

    for name, entry in sorted(entries.items()):
        status = entry.get("status")
        if status not in VALID_STATUSES:
            errors.append(f"{name}: invalid status {status!r}")
        if not isinstance(entry.get("owner"), int):
            errors.append(f"{name}: owner must be an issue number")
        if not entry.get("implementation"):
            errors.append(f"{name}: implementation detail is required")
        evidence = entry.get("evidence", [])
        if not isinstance(evidence, list):
            errors.append(f"{name}: evidence must be a list")
        if status in IMPLEMENTED_STATUSES and not evidence:
            errors.append(f"{name}: implemented commands require test evidence")
        if not isinstance(entry.get("live_verified"), bool):
            errors.append(f"{name}: live_verified must be boolean")

    return errors


def compatibility_summary(result: dict[str, Any]) -> dict[str, Any]:
    exact_matches = []
    expected_deviations = []
    unexpected_mismatches = []
    for scenario in result.get("scenarios", []):
        if all(scenario.get("same", {}).values()):
            exact_matches.append(scenario["name"])
        elif scenario.get("expected_deviation"):
            expected_deviations.append(scenario["name"])
        else:
            unexpected_mismatches.append(scenario["name"])

    summary = {
        "generated_at": result.get("generated_at"),
        "catify_version": result.get("catify", {}).get("version", "unknown"),
        "shopify_version": result.get("upstream", {}).get("version", "unknown"),
        "scenarios": len(result.get("scenarios", [])),
        "exact_matches": exact_matches,
        "expected_deviations": expected_deviations,
        "unexpected_mismatches": unexpected_mismatches,
    }
    if result.get("command_catalog"):
        summary["command_catalog"] = result["command_catalog"]
    return summary


def report(
    runtime: dict[str, Any],
    status_doc: dict[str, Any],
    compatibility: dict[str, Any] | None = None,
) -> dict[str, Any]:
    entries = status_doc["commands"]
    rows = []
    for command in runtime["commands"]:
        name = command["name"]
        entry = entries[name]
        rows.append(
            {
                "command": name,
                "domain": command_domain(name),
                "status": entry["status"],
                "owner": entry["owner"],
                "implementation": entry["implementation"],
                "tested": bool(entry.get("evidence")),
                "evidence": entry.get("evidence", []),
                "live_verified": entry["live_verified"],
                "notes": entry.get("notes"),
            }
        )

    status_counts = collections.Counter(row["status"] for row in rows)
    domain_counts: dict[str, dict[str, int]] = {}
    for row in rows:
        counts = domain_counts.setdefault(row["domain"], collections.Counter())
        counts[row["status"]] += 1

    data = {
        "schema_version": 1,
        "upstream": {"version": runtime["runtime"]["version"]},
        "summary": {
            "total": len(rows),
            "implemented": sum(row["status"] in IMPLEMENTED_STATUSES for row in rows),
            "tested": sum(row["tested"] for row in rows),
            "live_verified": sum(row["live_verified"] for row in rows),
            "by_status": dict(sorted(status_counts.items())),
            "by_domain": {
                domain: dict(sorted(counts.items()))
                for domain, counts in sorted(domain_counts.items())
            },
        },
        "commands": rows,
    }
    if compatibility is not None:
        data["runtime_compatibility"] = compatibility_summary(compatibility)
    return data


def markdown(data: dict[str, Any]) -> str:
    summary = data["summary"]
    lines = [
        "# Catify CLI parity matrix",
        "",
        f"> Upstream: `{data['upstream']['version']}`. Generated from `inventory/runtime-shopify-cli.json` and `inventory/cli-command-status.json`.",
        "",
        "## Summary",
        "",
        f"- Total upstream commands: **{summary['total']}**",
        f"- Implemented (`native` + `adapter`): **{summary['implemented']}**",
        f"- Commands with automated evidence: **{summary['tested']}**",
        f"- Live-verified commands: **{summary['live_verified']}**",
        "",
        "| Status | Count | Meaning |",
        "|---|---:|---|",
    ]
    meanings = {
        "native": "Implemented in Rust and exposed at the upstream command path.",
        "adapter": "Implemented through an explicit external runtime adapter.",
        "partial": "Exact command path exists, but behavior is not yet fully compatible.",
        "library-only": "Core/backend exists, but the public command is not fully wired.",
        "blocked": "Command path exists but required backend behavior is incomplete.",
        "name-mismatch": "Behavior exists under a non-compatible command path.",
        "missing": "No compatible command implementation yet.",
    }
    for status, count in summary["by_status"].items():
        lines.append(f"| `{status}` | {count} | {meanings[status]} |")

    compatibility = data.get("runtime_compatibility")
    if compatibility:
        exact = len(compatibility["exact_matches"])
        expected = len(compatibility["expected_deviations"])
        unexpected = len(compatibility["unexpected_mismatches"])
        lines.extend([
            "",
            "## Runtime black-box parity",
            "",
            f"> Last run: `{compatibility['generated_at']}` using Catify `{compatibility['catify_version']}` against Shopify CLI `{compatibility['shopify_version']}`.",
            "",
            f"- Scenarios: **{compatibility['scenarios']}**",
            f"- Exact stdout/stderr/exit matches: **{exact}**",
            f"- Documented expected deviations: **{expected}**",
            f"- Unexpected mismatches: **{unexpected}**",
            "",
            "This fixture suite compares observable command contracts. It is separate from authenticated store/app verification and does not change the live-verified command count above.",
        ])
        catalog = compatibility.get("command_catalog")
        if catalog:
            lines.extend([
                "",
                f"Live command catalog: Catify **{catalog['catify_count']}**, Shopify **{catalog['shopify_count']}**, missing in Catify **{len(catalog['missing_in_catify'])}**, extra in Catify **{len(catalog['extra_in_catify'])}**.",
            ])
        if compatibility["expected_deviations"]:
            deviations = ", ".join(f"`{name}`" for name in compatibility["expected_deviations"])
            lines.extend(["", f"Expected deviations: {deviations}."])

    lines.extend([
        "",
        "## Commands",
        "",
        "| Command | Status | Tested | Live | Owner | Implementation / gap |",
        "|---|---|:---:|:---:|---:|---|",
    ])
    for row in data["commands"]:
        evidence = "; ".join(row["evidence"])
        detail = row["implementation"]
        if evidence:
            detail += f" Evidence: {evidence}."
        if row.get("notes"):
            detail += f" {row['notes']}"
        detail = detail.replace("|", "\\|").replace("\n", " ")
        lines.append(
            f"| `{row['command']}` | `{row['status']}` | "
            f"{'yes' if row['tested'] else 'no'} | "
            f"{'yes' if row['live_verified'] else 'no'} | "
            f"[#{row['owner']}](https://github.com/yan-ad/catify/issues/{row['owner']}) | {detail} |"
        )
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", type=pathlib.Path, default=pathlib.Path("inventory/runtime-shopify-cli.json"))
    parser.add_argument("--status", type=pathlib.Path, default=pathlib.Path("inventory/cli-command-status.json"))
    parser.add_argument("--compatibility", type=pathlib.Path, default=pathlib.Path("compatibility/results/latest.json"))
    parser.add_argument("--json-output", type=pathlib.Path, default=pathlib.Path("inventory/CLI-PARITY.json"))
    parser.add_argument("--markdown-output", type=pathlib.Path, default=pathlib.Path("inventory/CLI-PARITY.md"))
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    runtime = load(args.runtime)
    status_doc = load(args.status)
    compatibility = load(args.compatibility) if args.compatibility.exists() else None
    errors = validate(runtime, status_doc)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    data = report(runtime, status_doc, compatibility)
    json_text = json.dumps(data, indent=2, sort_keys=False) + "\n"
    markdown_text = markdown(data)

    if args.check:
        stale = []
        if not args.json_output.exists() or args.json_output.read_text() != json_text:
            stale.append(str(args.json_output))
        if not args.markdown_output.exists() or args.markdown_output.read_text() != markdown_text:
            stale.append(str(args.markdown_output))
        if stale:
            print(f"stale generated parity reports: {', '.join(stale)}", file=sys.stderr)
            return 1
        return 0

    args.json_output.write_text(json_text)
    args.markdown_output.write_text(markdown_text)
    print(json.dumps(data["summary"], indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
