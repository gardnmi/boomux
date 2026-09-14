#!/usr/bin/env bash
set -euo pipefail
poc_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$poc_root"
poc_target=${CARGO_TARGET_DIR:-"$poc_root/target"}
[[ "$poc_target" = /* ]] || poc_target="$poc_root/$poc_target"
cargo build --locked --example webgpu_gateway --bin boomux
case "${1:---isolated}" in
  --isolated)
    export BOOMUX_RUNTIME_DIR="$poc_root/target/webgpu-poc/runtime"
    export BOOMUX_CONFIG_HOME="$poc_root/target/webgpu-poc/config"
    export BOOMUX_STATE_HOME="$poc_root/target/webgpu-poc/state"
    unset BOOMUX_CONFIG
    install -d -m 700 "$BOOMUX_RUNTIME_DIR" "$BOOMUX_CONFIG_HOME" "$BOOMUX_STATE_HOME"
    "$poc_target/debug/boomux" daemon start
    ;;
  --current) ;; # Connect only; never start, stop, or restart the ordinary daemon.
  *) printf 'Usage: %s [--isolated|--current]\n' "$0" >&2; exit 2 ;;
esac
exec "$poc_target/debug/examples/webgpu_gateway"
