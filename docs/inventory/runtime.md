# Runtime command inventory

The source inventory in `inventory/shopify-cli.json` is pinned to a Shopify CLI checkout and is reproducible. It is not sufficient for public parity because installed CLI plugins and runtime command registration can add commands that are absent from the source scan.

Capture the installed runtime manifest with:

```bash
python3 scripts/generate-inventory.py --runtime "$(command -v shopify)"
```

This writes `inventory/runtime-shopify-cli.json` from `shopify commands --json` and records:

- command IDs and normalized names
- aliases and flags
- environment variables declared by flags
- plugin name/type
- hidden command status
- runtime version and executable provenance

Validate a captured snapshot without rewriting it:

```bash
python3 scripts/generate-inventory.py --runtime "$(command -v shopify)" --check
```

Runtime snapshots are machine-specific evidence, not a replacement for the pinned source snapshot. Public parity work must use the runtime snapshot to discover ownership gaps and the source snapshot to reason about implementation details.
