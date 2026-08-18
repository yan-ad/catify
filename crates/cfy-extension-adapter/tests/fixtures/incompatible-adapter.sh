#!/bin/sh
set -eu

case "${1:-}" in
  --cfy-adapter-info)
    printf '%s\n' '{"protocol_version":2,"name":"future-adapter","adapter_version":"2.0.0","extension_types":[]}'
    ;;
  *) exit 11 ;;
esac
