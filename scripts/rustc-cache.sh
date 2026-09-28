#!/bin/sh
# Cargo supplies the compiler followed by its exact argument vector.
# CI retains its existing rust-cache setup; local builds can share kache outputs.
if [ -z "${CI:-}" ] && [ "${KACHE_DISABLED:-0}" != 1 ] && [ "${KACHE_DISABLED:-0}" != true ] && command -v kache >/dev/null 2>&1; then
    exec kache "$@"
fi
exec "$@"
