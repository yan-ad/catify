#!/usr/bin/env python3
"""Compare benchmark medians using relative and absolute noise guards."""
import json, sys

if len(sys.argv) != 3:
    raise SystemExit("usage: compare.py BASELINE CURRENT")
with open(sys.argv[1]) as f: baseline = json.load(f)
with open(sys.argv[2]) as f: current = json.load(f)
checks = [
    ("warm startup", baseline["startup_ms"]["cfy"]["warm_median"], current["startup_ms"]["cfy"]["warm_median"], 5.0),
    ("peak RSS", baseline["peak_rss_kib"]["cfy"], current["peak_rss_kib"]["cfy"], 4096.0),
]
failed = []
for name, old, new, absolute_guard in checks:
    delta = new - old
    if old > 0 and delta / old > 0.20 and delta > absolute_guard:
        failed.append(f"{name}: {old:.2f} -> {new:.2f}")
if failed:
    print("material benchmark regression:\n" + "\n".join(failed), file=sys.stderr)
    raise SystemExit(1)
print("no material benchmark regression")
