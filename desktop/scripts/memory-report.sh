#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! $1 =~ ^[0-9]+$ ]]; then
  printf 'usage: %s <pid>\n' "$0" >&2
  exit 2
fi

pid=$1
process_dir=/proc/$pid
if [[ ! -r $process_dir/status || ! -r $process_dir/smaps_rollup ]]; then
  printf 'process %s is not readable\n' "$pid" >&2
  exit 1
fi

name=$(awk '/^Name:/ { print $2 }' "$process_dir/status")
threads=$(awk '/^Threads:/ { print $2 }' "$process_dir/status")
fds=$(find "$process_dir/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)

printf 'process=%s pid=%s threads=%s fds=%s\n' "$name" "$pid" "$threads" "$fds"
awk '
  /^(Rss|Pss|Private_Clean|Private_Dirty|Shared_Clean|Shared_Dirty|Swap):/ {
    printf "%s %s %s\n", $1, $2, $3
  }
' "$process_dir/smaps_rollup"
