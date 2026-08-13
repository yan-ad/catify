# Performance benchmarks

The harness records environment, binary versions, cold/warm startup, peak RSS, and idle RSS in JSON. Reports distinguish the `cfy` process from external toolchains; future workflow benchmarks must also capture the whole process tree.

## Prerequisites

Build a release binary and install Shopify CLI for the matching comparison:

```sh
cargo build --release -p cfy-cli
shopify version
```

macOS uses `/usr/bin/time -l` and `ps -o rss=`. Linux requires GNU `/usr/bin/time -v` and uses the same `ps` interface. The Shopify idle probe samples its hidden `kitchen-sink async` command; if upstream removes it, the harness fails loudly rather than emitting an incomparable value.

## Run

```sh
./benchmarks/run.sh
```

Override commands or iterations when required:

```sh
CFY_BIN=target/release/cfy \
SHOPIFY_BIN=shopify \
BENCH_ITERATIONS=10 \
./benchmarks/run.sh
```

Cold startup is measured after rebuilding/copying the executable into a fresh temporary path; this is a reproducible proxy, not a guarantee that the OS disk cache is empty. Warm startup is the median of repeated executions. Peak RSS comes from the platform `time` tool. Idle RSS samples a deliberately quiescent command after two seconds.

Commit intentional baselines manually. CI runs smoke measurements and compares medians to a checked baseline only when one exists. A regression warning requires both a relative change (default 20%) and an absolute change (default 5 ms or 4 MiB), avoiding flaky hard limits on shared runners.
