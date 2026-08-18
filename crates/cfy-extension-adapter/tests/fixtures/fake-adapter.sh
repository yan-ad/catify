#!/bin/sh
set -eu

case "${1:-}" in
  --cfy-adapter-info)
    printf '%s\n' '{"protocol_version":1,"name":"fake","adapter_version":"1.2.3","extension_types":["ui_extension"]}'
    ;;
  --cfy-build-adapter)
    request="$(cat)"
    case "$request" in
      *'"protocol_version":1'*'"extension_type":"ui_extension"'*) ;;
      *) echo "missing or invalid machine-readable request" >&2; exit 12 ;;
    esac
    sleep "${CFY_FAKE_SLEEP:-0}"
    printf '%s\n' '{"protocol_version":1,"artifacts":["dist/main.js"],"diagnostics":[]}'
    ;;
  *)
    echo "unknown adapter operation" >&2
    exit 11
    ;;
esac
