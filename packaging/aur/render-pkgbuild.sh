#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Mike Gardner
# SPDX-License-Identifier: 0BSD
set -euo pipefail

version=${1:?usage: render-pkgbuild.sh VERSION X86_64_ARCHIVE AARCH64_ARCHIVE OUTPUT_DIR}
x86_archive=${2:?usage: render-pkgbuild.sh VERSION X86_64_ARCHIVE AARCH64_ARCHIVE OUTPUT_DIR}
aarch64_archive=${3:?usage: render-pkgbuild.sh VERSION X86_64_ARCHIVE AARCH64_ARCHIVE OUTPUT_DIR}
output_dir=${4:?usage: render-pkgbuild.sh VERSION X86_64_ARCHIVE AARCH64_ARCHIVE OUTPUT_DIR}
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
template="${script_dir}/PKGBUILD.in"

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'version must have the form X.Y.Z: %s\n' "$version" >&2
  exit 1
fi
if [[ ! -d "$output_dir" ]]; then
  printf 'output directory not found: %s\n' "$output_dir" >&2
  exit 1
fi
output_dir=$(cd -- "$output_dir" && pwd)
if ! command -v makepkg >/dev/null 2>&1; then
  printf 'makepkg is required to render AUR metadata\n' >&2
  exit 1
fi

declare -A archives=(
  [x86_64]="$x86_archive"
  [aarch64]="$aarch64_archive"
)
declare -A targets=(
  [x86_64]=x86_64-unknown-linux-gnu
  [aarch64]=aarch64-unknown-linux-gnu
)

for arch in x86_64 aarch64; do
  archive=${archives[$arch]}
  expected="boomux-v${version}-${targets[$arch]}.tar.gz"
  if [[ ! -f "$archive" ]]; then
    printf '%s archive not found: %s\n' "$arch" "$archive" >&2
    exit 1
  fi
  if [[ $(basename -- "$archive") != "$expected" ]]; then
    printf '%s archive must be named %s\n' "$arch" "$expected" >&2
    exit 1
  fi
done

x86_sha256=$(sha256sum "$x86_archive" | cut -d ' ' -f 1)
aarch64_sha256=$(sha256sum "$aarch64_archive" | cut -d ' ' -f 1)
rendered=$(<"$template")
rendered=${rendered//@VERSION@/$version}
rendered=${rendered//@X86_64_SHA256@/$x86_sha256}
rendered=${rendered//@AARCH64_SHA256@/$aarch64_sha256}

pkgbuild_tmp="${output_dir}/PKGBUILD.tmp.$$"
srcinfo_tmp="${output_dir}/.SRCINFO.tmp.$$"
trap 'rm -f "$pkgbuild_tmp" "$srcinfo_tmp"' EXIT
printf '%s\n' "$rendered" > "$pkgbuild_tmp"
mv "$pkgbuild_tmp" "${output_dir}/PKGBUILD"
(
  cd "$output_dir"
  makepkg --printsrcinfo > "$srcinfo_tmp"
)
mv "$srcinfo_tmp" "${output_dir}/.SRCINFO"
