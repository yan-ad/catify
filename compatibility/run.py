#!/usr/bin/env python3
import argparse, json, os, re, subprocess, sys, time
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
ABS = re.compile(r"/(?:private|var|tmp|Users|Volumes)/[^\n ]+")
TIMESTAMP = re.compile(r"\b20\d{2}-\d{2}-\d{2}(?:[T ][0-9:.Z+-]+)?\b")
UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f-]{27,}\b", re.I)
VERSION = re.compile(r"(?:@shopify/cli/|shopify/|cfy/)\d+\.\d+\.\d+(?:[-+][^\s]+)?")

def normalize(text):
    text = ANSI.sub("<ansi>", text.replace("\\r\\n", "\\n"))
    text = ABS.sub("<path>", text)
    text = TIMESTAMP.sub("<timestamp>", text)
    text = UUID.sub("<uuid>", text)
    return VERSION.sub("<version>", text)

def run(binary, args, env):
    started = time.monotonic()
    p = subprocess.run([binary, *args], env=env, text=True, capture_output=True, timeout=30)
    return {"exit": p.returncode, "stdout": normalize(p.stdout), "stderr": normalize(p.stderr), "duration_ms": round((time.monotonic()-started)*1000)}

def command_catalog(binary, env):
    try:
        p = subprocess.run([binary, "commands", "--json"], env=env, text=True, capture_output=True, timeout=30)
        commands = json.loads(p.stdout)
        names = []
        for command in commands:
            name = command.get("name") or command.get("id", "").replace(":", " ")
            if name:
                names.append(name)
        return sorted(names)
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError, KeyError, TypeError):
        return []

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cfy", default="./target/debug/cfy")
    ap.add_argument("--shopify", default="shopify")
    ap.add_argument("--output", default="compatibility/results/latest.json")
    ns = ap.parse_args()
    root = Path(__file__).parent
    scenarios = json.loads((root/"scenarios.json").read_text())["scenarios"]
    deviations = json.loads((root/"deviations.json").read_text())["deviations"]
    base = os.environ.copy()
    def version(binary):
        try: return subprocess.run([binary, "version"], env=base, text=True, capture_output=True, timeout=10).stdout.strip()
        except (OSError, subprocess.SubprocessError): return "unavailable"
    rows=[]; failures=[]
    for scenario in scenarios:
        env=base.copy(); env.update(scenario.get("env", {}))
        cfy=run(ns.cfy, scenario["args"], env)
        shop=run(ns.shopify, scenario["args"], env)
        same = {k: cfy[k] == shop[k] for k in ("exit","stdout","stderr")}
        mismatch = not all(same.values())
        expected = scenario["name"] in deviations
        if mismatch and not expected: failures.append(scenario["name"])
        rows.append({"name":scenario["name"],"args":scenario["args"],"cfy":cfy,"shopify":shop,"same":same,"expected_deviation":expected})
    cfy_commands=command_catalog(ns.cfy,base); shopify_commands=command_catalog(ns.shopify,base)
    report={"schema_version":1,"generated_at":time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),"catify":{"version":version(ns.cfy)},"upstream":{"version":version(ns.shopify)},"command_catalog":{"catify_count":len(cfy_commands),"shopify_count":len(shopify_commands),"missing_in_catify":sorted(set(shopify_commands)-set(cfy_commands)),"extra_in_catify":sorted(set(cfy_commands)-set(shopify_commands))},"scenarios":rows,"unexpected_mismatches":failures}
    out=Path(ns.output); out.parent.mkdir(parents=True, exist_ok=True); out.write_text(json.dumps(report,indent=2)+"\n")
    print(f"compatibility: {len(rows)} scenarios, {len(failures)} unexpected mismatches; upstream={report['upstream']['version']}")
    return 1 if failures else 0

if __name__ == "__main__": raise SystemExit(main())
