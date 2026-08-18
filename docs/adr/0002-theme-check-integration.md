# ADR 0002: Theme Check Integration

- Status: accepted
- Date: 2026-08-18

## Context

`cfy theme check` needs Shopify-compatible Liquid and theme analysis without making Ruby, Node, or Theme Check libraries part of every `cfy` command's runtime. Reimplementing Theme Check in Rust would duplicate a large, changing rule set and risk diagnostics that disagree with Shopify's tooling.

Pinned upstream facts used for this decision:

- Shopify's command reference documents `shopify theme check` and the common `--path`, `--config`, `--auto-correct`, `--init`, `--list`, and `--output` options: <https://shopify.dev/docs/api/shopify-cli/theme/theme-check> (retrieved 2026-08-18).
- The official Theme Check engine is maintained in Shopify/theme-check and consumed by Shopify's theme tooling. The decision was checked against commit `1ee41f97a1251465d29133df22517bfe51b0ba88`: <https://github.com/Shopify/theme-check/tree/1ee41f97a1251465d29133df22517bfe51b0ba88>.
- The current Node integration lives in Shopify/theme-tools. The decision was checked against package `@shopify/theme-check-node` at commit `21b084718433597882053c6013e70b33ca13119e`: <https://github.com/Shopify/theme-tools/tree/21b084718433597882053c6013e70b33ca13119e/packages/theme-check-node>.
- Explicit adapter overrides support Shopify CLI major version 3 or newer and direct Theme Check major version 1 or newer. Overrides are version-probed before execution so custom sidecars fail with actionable diagnostics. The normal `shopify` fallback is not pre-probed because that would start the Node runtime twice; incompatibility is reported by the delegated command instead. Other `cfy` commands never launch or load these optional tools.

## Options considered

### Native Rust parser and checks

This offers the lowest eventual startup overhead and a single binary, but requires maintaining Liquid parsing, configuration semantics, check behavior, formatting, fixes, and version parity. Rejected because correctness and ongoing compatibility cost dominate the performance benefit.

### In-process sidecar library

Embedding Ruby or Node can reduce repeated process setup, but adds platform packaging, ABI, lifecycle, and dependency-isolation problems. It would increase normal installation size and complicate upgrades. Rejected for now.

### Supervised subprocess to the official engine

This preserves authoritative behavior and lets users upgrade the engine independently. Startup and memory include the external runtime, but these costs affect only Theme Check invocations. Accepted.

## Decision

Use `cfy-process` to invoke an official adapter without a shell. `CFY_THEME_CHECK_BIN` explicitly selects an executable. Without it, `cfy` safely falls back to `shopify theme check`. If the override executable is named `theme-check`, `cfy` invokes the direct engine and uses its positional path convention. An override resolving to `cfy` is rejected to prevent recursion.

The adapter captures stdout and stderr separately, writes them unchanged to their corresponding parent streams, and returns the child's numeric exit status. Configuration, category/exclusion, output, initialization, listing, path, and auto-correction arguments are passed through rather than interpreted by `cfy`. Category flags are retained for useful compatibility with older/direct Theme Check engines even though current Shopify CLI documentation may not expose them.

Explicit overrides are version-probed before checking. Missing dependencies and unsupported override versions produce installation/override guidance. The default `shopify` fallback skips the separate probe to avoid starting Node twice.

## Validation and benchmark

The checked-in fixtures cover a clean minimal theme, a failing theme, and malformed configuration. Against Shopify CLI 4.6.1, the clean fixture exits 0 while the failing and malformed fixtures exit 1 with upstream diagnostics.

`benchmarks/theme-check.sh` measures both paths against the same clean fixture without requiring third-party benchmark tools. On 2026-08-18, macOS arm64 with three release runs produced:

- `cfy theme check`: 1,201 ms median wall time and 179,824 KiB peak RSS.
- `shopify theme check`: 1,110 ms median wall time and 205,200 KiB peak RSS.

The wrapper added roughly 91 ms median wall time in this sample. Peak process-tree sampling varies by platform and external CLI release; the script records both tool versions and should be rerun when the adapter contract changes.

## Consequences

- Normal commands remain independent of Ruby/Node and do not discover or start Shopify tooling.
- Theme Check diagnostics, configuration parsing (including malformed config), fixes, and check results remain upstream responsibilities.
- Users must install Shopify CLI/Theme Check or configure `CFY_THEME_CHECK_BIN`.
- Output fidelity and exit status are maintained, including machine-readable output.
- An optional benchmark scenario compares end-to-end `cfy theme check` with `shopify theme check`; results include adapter overhead and should pin/report the external CLI version.
