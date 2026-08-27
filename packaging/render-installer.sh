#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Mike Gardner
# SPDX-License-Identifier: 0BSD
set -euo pipefail

tag=${1:?usage: render-installer.sh TAG OUTPUT}
output=${2:?usage: render-installer.sh TAG OUTPUT}
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
template="${script_dir}/install-release.sh.in"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'tag must have the form vX.Y.Z: %s\n' "$tag" >&2
  exit 1
fi

rendered=$(<"$template")
rendered=${rendered//@TAG@/$tag}
temporary="${output}.tmp.$$"
trap 'rm -f "$temporary"' EXIT
printf '%s\n' "$rendered" > "$temporary"
chmod 755 "$temporary"
mv "$temporary" "$output"
