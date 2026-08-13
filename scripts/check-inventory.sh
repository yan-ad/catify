#!/usr/bin/env bash
set -euo pipefail
commit=$(python3 -c 'import json; print(json.load(open("inventory/shopify-cli.json"))["upstream"]["commit"])')
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git clone --quiet --filter=blob:none --no-checkout https://github.com/Shopify/cli.git "$tmp/shopify-cli"
git -C "$tmp/shopify-cli" checkout --quiet "$commit"
python3 scripts/generate-inventory.py "$tmp/shopify-cli" --check
