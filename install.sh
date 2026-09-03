#!/bin/sh
set -eu

REPOSITORY=${CFY_REPOSITORY:-yan-ad/catify}
RELEASE_BASE_URL=${CFY_RELEASE_BASE_URL:-https://github.com/${REPOSITORY}/releases/download}
INSTALL_DIR=${CFY_INSTALL_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: $1 is required" >&2
    exit 1
  }
}

need curl
need tar

download() {
  url=$1
  destination=$2
  case "$url" in
    file://*) cp "${url#file://}" "$destination" ;;
    *) curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$destination" ;;
  esac
}

if [ -n "${CFY_VERSION:-}" ]; then
  VERSION=${CFY_VERSION#v}
else
  LATEST_URL=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' \
    "https://github.com/${REPOSITORY}/releases/latest")
  VERSION=${LATEST_URL##*/}
  VERSION=${VERSION#v}
fi

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  Linux-aarch64|Linux-arm64) TARGET=aarch64-unknown-linux-gnu ;;
  Linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  *)
    echo "error: unsupported platform $(uname -s)/$(uname -m)" >&2
    exit 1
    ;;
esac

ARCHIVE="cfy-v${VERSION}-${TARGET}.tar.gz"
ASSET_BASE="${RELEASE_BASE_URL%/}/v${VERSION}"
TMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t catify-install)
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

download "${ASSET_BASE}/${ARCHIVE}" "${TMP_DIR}/${ARCHIVE}"
download "${ASSET_BASE}/SHA256SUMS" "${TMP_DIR}/SHA256SUMS"

EXPECTED=$(awk -v archive="$ARCHIVE" '$2 == archive || $2 == "*" archive { print $1; exit }' "${TMP_DIR}/SHA256SUMS")
[ -n "$EXPECTED" ] || {
  echo "error: ${ARCHIVE} is missing from SHA256SUMS" >&2
  exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL=$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')
else
  echo "error: sha256sum or shasum is required" >&2
  exit 1
fi

[ "$ACTUAL" = "$EXPECTED" ] || {
  echo "error: checksum mismatch for ${ARCHIVE}" >&2
  exit 1
}

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"
SOURCE="${TMP_DIR}/cfy-v${VERSION}-${TARGET}/cfy"
[ -f "$SOURCE" ] || {
  echo "error: release archive does not contain cfy" >&2
  exit 1
}

mkdir -p "$INSTALL_DIR"
cp "$SOURCE" "${INSTALL_DIR}/cfy"
chmod 755 "${INSTALL_DIR}/cfy"

echo "Installed Catify ${VERSION} to ${INSTALL_DIR}/cfy"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Add ${INSTALL_DIR} to PATH, then run: cfy version" ;;
esac
