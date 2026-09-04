#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PACKAGE_VERSION=$(node -p "require('$ROOT/package.json').version")
VERSION=$(python3 "$ROOT/scripts/check-release-version.py" --tag "v$PACKAGE_VERSION")
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  Linux-aarch64|Linux-arm64) TARGET=aarch64-unknown-linux-gnu ;;
  Linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  *) echo "unsupported test platform" >&2; exit 1 ;;
esac
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
RELEASES="$TMP/releases/v$VERSION"
mkdir -p "$RELEASES"

python3 "$ROOT/scripts/package-release.py" \
  --binary "$ROOT/target/release/cfy" \
  --version "$VERSION" \
  --target "$TARGET" \
  --output "$RELEASES"

CFY_VERSION="$VERSION" \
CFY_RELEASE_BASE_URL="file://$TMP/releases" \
CFY_INSTALL_DIR="$TMP/shell-bin" \
sh "$ROOT/install.sh"
"$TMP/shell-bin/cfy" version
"$TMP/shell-bin/catify" version

cd "$ROOT"
PACKAGE=$(CFY_SKIP_DOWNLOAD=1 npm pack --silent)
CFY_RELEASE_BASE_URL="file://$TMP/releases" \
  npm install --global --prefix "$TMP/npm" "./$PACKAGE"
"$TMP/npm/bin/cfy" version
"$TMP/npm/bin/catify" version
rm -f "$ROOT/$PACKAGE"
