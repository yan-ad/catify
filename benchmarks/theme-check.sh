#!/usr/bin/env bash
set -euo pipefail

CFY_BIN=${CFY_BIN:-target/release/cfy}
SHOPIFY_BIN=${SHOPIFY_BIN:-shopify}
THEME_FIXTURE=${THEME_FIXTURE:-crates/cfy-cli/tests/fixtures/theme-check/clean}
RUNS=${BENCH_RUNS:-5}
OUTPUT=${BENCH_OUTPUT:-benchmarks/results/theme-check-latest.json}

[[ -x "$CFY_BIN" ]] || { echo "missing $CFY_BIN; run cargo build --release -p cfy-cli" >&2; exit 2; }
command -v "$SHOPIFY_BIN" >/dev/null || { echo "missing Shopify CLI: $SHOPIFY_BIN" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
[[ -d "$THEME_FIXTURE" ]] || { echo "missing theme fixture: $THEME_FIXTURE" >&2; exit 2; }

case "$(uname -s)" in
  Darwin) PLATFORM=macos ;;
  Linux) PLATFORM=linux ;;
  *) echo "unsupported benchmark platform" >&2; exit 2 ;;
esac

mkdir -p "$(dirname "$OUTPUT")"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

measure_ms() {
  python3 - "$@" <<'PY'
import subprocess, sys, time
start = time.perf_counter_ns()
result = subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
if result.returncode != 0:
    raise SystemExit(f"benchmark command failed with exit {result.returncode}: {sys.argv[1:]}")
print((time.perf_counter_ns() - start) / 1_000_000)
PY
}

measure_peak_kib() {
  local log="$TMP/time.log"
  if [[ "$PLATFORM" == macos ]]; then
    /usr/bin/time -l "$@" >/dev/null 2>"$log"
    awk '/maximum resident set size/ { print int($1 / 1024) }' "$log"
  else
    /usr/bin/time -v "$@" >/dev/null 2>"$log"
    awk -F: '/Maximum resident set size/ { gsub(/ /, "", $2); print $2 }' "$log"
  fi
}

CFY_TIMES="$TMP/cfy-times"
SHOPIFY_TIMES="$TMP/shopify-times"
: >"$CFY_TIMES"
: >"$SHOPIFY_TIMES"
for _ in $(seq 1 "$RUNS"); do
  measure_ms "$CFY_BIN" theme check --path "$THEME_FIXTURE" >>"$CFY_TIMES"
  measure_ms "$SHOPIFY_BIN" theme check --path "$THEME_FIXTURE" >>"$SHOPIFY_TIMES"
done

CFY_PEAK=$(measure_peak_kib "$CFY_BIN" theme check --path "$THEME_FIXTURE")
SHOPIFY_PEAK=$(measure_peak_kib "$SHOPIFY_BIN" theme check --path "$THEME_FIXTURE")

python3 - "$OUTPUT" "$PLATFORM" "$RUNS" "$CFY_TIMES" "$SHOPIFY_TIMES" "$CFY_PEAK" "$SHOPIFY_PEAK" "$CFY_BIN" "$SHOPIFY_BIN" <<'PY'
import datetime, json, os, platform, statistics, subprocess, sys
(output, os_name, runs, cfy_times, shopify_times, cfy_peak, shopify_peak, cfy_bin, shopify_bin) = sys.argv[1:]
def values(path):
    with open(path) as handle:
        return [float(line) for line in handle if line.strip()]
def version(command):
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    return (result.stdout or result.stderr).strip().splitlines()[0]
data = {
    "schema_version": 1,
    "recorded_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "environment": {
        "os": os_name,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cfy": version([cfy_bin, "version"]),
        "shopify": version([shopify_bin, "version"]),
    },
    "runs": int(runs),
    "wall_time_ms": {
        "cfy_adapter_median": statistics.median(values(cfy_times)),
        "shopify_direct_median": statistics.median(values(shopify_times)),
    },
    "peak_rss_kib": {
        "cfy_adapter": int(cfy_peak),
        "shopify_direct": int(shopify_peak),
    },
}
os.makedirs(os.path.dirname(output), exist_ok=True)
with open(output, "w") as handle:
    json.dump(data, handle, indent=2)
    handle.write("\n")
print(output)
PY
