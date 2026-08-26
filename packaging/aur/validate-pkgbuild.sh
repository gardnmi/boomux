#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Mike Gardner
# SPDX-License-Identifier: 0BSD
set -euo pipefail

directory=${1:?usage: validate-pkgbuild.sh DIRECTORY}
pkgbuild="${directory}/PKGBUILD"
srcinfo="${directory}/.SRCINFO"

for file in "$pkgbuild" "$srcinfo"; do
  if [[ ! -f "$file" ]]; then
    printf 'AUR metadata not found: %s\n' "$file" >&2
    exit 1
  fi
done
if ! command -v makepkg >/dev/null 2>&1; then
  printf 'makepkg is required to validate AUR metadata\n' >&2
  exit 1
fi
if grep -Eq '@(VERSION|X86_64_SHA256|AARCH64_SHA256)@' "$pkgbuild" "$srcinfo"; then
  printf 'AUR metadata contains unresolved template placeholders\n' >&2
  exit 1
fi

bash -n "$pkgbuild"
bash -c '
  set -euo pipefail
  source "$1"
  [[ "$pkgname" == boomux-bin ]]
  [[ "${arch[*]}" == "x86_64 aarch64" ]]
  [[ "${depends[*]}" == "curl gcc-libs git glibc xdg-terminal-exec" ]]
  [[ "${provides[*]}" == "boomux=${pkgver}" ]]
  [[ "${conflicts[*]}" == boomux ]]
  [[ ${#source_x86_64[@]} -eq 1 && ${#source_aarch64[@]} -eq 1 ]]
  [[ ${sha256sums_x86_64[0]} =~ ^[0-9a-f]{64}$ ]]
  [[ ${sha256sums_aarch64[0]} =~ ^[0-9a-f]{64}$ ]]
  [[ ${sha256sums_x86_64[0]} != "${sha256sums_aarch64[0]}" ]]
' _ "$pkgbuild"

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
cp "$pkgbuild" "${tmp_dir}/PKGBUILD"
(
  cd "$tmp_dir"
  makepkg --printsrcinfo > .SRCINFO
)
cmp "$srcinfo" "${tmp_dir}/.SRCINFO"
