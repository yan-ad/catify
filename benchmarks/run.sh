#!/usr/bin/env bash
set -euo pipefail

CFY_BIN=${CFY_BIN:-target/release/cfy}
SHOPIFY_BIN=${SHOPIFY_BIN:-shopify}
ITERATIONS=${BENCH_ITERATIONS:-7}
OUTPUT=${BENCH_OUTPUT:-benchmarks/results/latest.json}

[[ -x "$CFY_BIN" ]] || { echo "missing $CFY_BIN; run cargo build --release -p cfy-cli" >&2; exit 2; }
command -v "$SHOPIFY_BIN" >/dev/null || { echo "missing Shopify CLI: $SHOPIFY_BIN" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

mkdir -p "$(dirname "$OUTPUT")"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

case "$(uname -s)" in
  Darwin) PLATFORM=macos ;;
  Linux) PLATFORM=linux ;;
  *) echo "unsupported benchmark platform" >&2; exit 2 ;;
esac

measure_ms() {
  python3 - "$@" <<'PY'
import subprocess, sys, time
start = time.perf_counter_ns()
subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
print((time.perf_counter_ns() - start) / 1_000_000)
PY
}

measure_peak_kib() {
  local log="$TMP/time.log"
  if [[ "$PLATFORM" == macos ]]; then
    /usr/bin/time -l "$@" >/dev/null 2>"$log" || true
    awk '/maximum resident set size/ { print int($1 / 1024) }' "$log"
  else
    /usr/bin/time -v "$@" >/dev/null 2>"$log" || true
    awk -F: '/Maximum resident set size/ { gsub(/ /, "", $2); print $2 }' "$log"
  fi
}

measure_idle_kib() {
  local kind=$1
  shift
  "$@" >/dev/null 2>&1 &
  local pid=$!
  sleep 2
  local rss
  rss=$(ps -o rss= -p "$pid" | tr -d ' ' || true)
  kill "$pid" >/dev/null 2>&1 || true
  wait "$pid" 2>/dev/null || true
  [[ -n "$rss" ]] || { echo "$kind idle probe exited before sampling" >&2; exit 1; }
  echo "$rss"
}

cp "$CFY_BIN" "$TMP/cfy-cold"
CFY_COLD=$(measure_ms "$TMP/cfy-cold" --help)
SHOPIFY_COLD=$(measure_ms "$SHOPIFY_BIN" help)
CFY_PEAK=$(measure_peak_kib "$CFY_BIN" --help)
SHOPIFY_PEAK=$(measure_peak_kib "$SHOPIFY_BIN" help)
CFY_IDLE=$(measure_idle_kib cfy "$CFY_BIN" internal idle --seconds 10)
CFY_THEME_WATCH_IDLE=$(measure_idle_kib cfy-theme-watch "$CFY_BIN" internal idle --seconds 10 --watch "$TMP")
if [[ "$(basename "$SHOPIFY_BIN")" == "cfy" ]]; then
  SHOPIFY_IDLE=$(measure_idle_kib shopify "$SHOPIFY_BIN" internal idle --seconds 10)
else
  SHOPIFY_IDLE=$(measure_idle_kib shopify "$SHOPIFY_BIN" kitchen-sink async)
fi

CFY_WARM_FILE="$TMP/cfy-warm"
SHOPIFY_WARM_FILE="$TMP/shopify-warm"
: >"$CFY_WARM_FILE"; : >"$SHOPIFY_WARM_FILE"
for _ in $(seq 1 "$ITERATIONS"); do
  measure_ms "$CFY_BIN" --help >>"$CFY_WARM_FILE"
  measure_ms "$SHOPIFY_BIN" help >>"$SHOPIFY_WARM_FILE"
done

python3 - "$OUTPUT" "$PLATFORM" "$ITERATIONS" "$CFY_COLD" "$SHOPIFY_COLD" "$CFY_PEAK" "$SHOPIFY_PEAK" "$CFY_IDLE" "$SHOPIFY_IDLE" "$CFY_THEME_WATCH_IDLE" "$CFY_WARM_FILE" "$SHOPIFY_WARM_FILE" "$CFY_BIN" "$SHOPIFY_BIN" <<'PY'
import datetime, json, os, platform, statistics, subprocess, sys
(output, os_name, iterations, cfy_cold, shopify_cold, cfy_peak, shopify_peak,
  cfy_idle, shopify_idle, cfy_theme_watch_idle, cfy_warm_file, shopify_warm_file, cfy_bin, shopify_bin) = sys.argv[1:]
def values(path):
    with open(path) as f: return [float(line) for line in f if line.strip()]
def version(command):
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    return (result.stdout or result.stderr).strip().splitlines()[0]
data = {
  "schema_version": 1,
  "recorded_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
  "environment": {
    "os": os_name, "platform": platform.platform(), "machine": platform.machine(),
    "rustc": version(["rustc", "--version"]),
    "cfy": version([cfy_bin, "version"]),
    "shopify": version([shopify_bin, "version"]),
  },
  "iterations": int(iterations),
  "startup_ms": {
    "cfy": {"cold": float(cfy_cold), "warm_median": statistics.median(values(cfy_warm_file))},
    "shopify": {"cold": float(shopify_cold), "warm_median": statistics.median(values(shopify_warm_file))},
  },
  "peak_rss_kib": {"cfy": int(cfy_peak), "shopify": int(shopify_peak)},
  "idle_rss_kib": {"cfy": int(cfy_idle), "shopify": int(shopify_idle)},
  "workflow_idle_rss_kib": {"cfy_theme_native_watcher": int(cfy_theme_watch_idle)},
}
os.makedirs(os.path.dirname(output), exist_ok=True)
with open(output, "w") as f: json.dump(data, f, indent=2); f.write("\n")
print(output)
PY
