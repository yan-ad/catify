#!/usr/bin/env bash
set -euo pipefail

ARCHIVE=${1:?usage: smoke-release-artifact.sh <archive>}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

tar -xzf "$ARCHIVE" -C "$TMP"
ROOT=$(find "$TMP" -mindepth 1 -maxdepth 1 -type d -name 'cfy-v*' -print -quit)
test -n "$ROOT"
test -x "$ROOT/cfy"
test -x "$ROOT/catify"

"$ROOT/cfy" version
"$ROOT/catify" --help >/dev/null

PROJECT="$TMP/shopify-app"
mkdir -p "$PROJECT"
cat > "$PROJECT/shopify.app.toml" <<'TOML'
client_id = "release-smoke"
name = "Release Smoke"
application_url = "https://example.com"
embedded = true

[access_scopes]
scopes = "read_products"
TOML

"$ROOT/cfy" app info --path "$PROJECT" >/dev/null
"$ROOT/cfy" app build --path "$PROJECT" --no-color >/dev/null

echo "Release artifact smoke passed: $ARCHIVE"
