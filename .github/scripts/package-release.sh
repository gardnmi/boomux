#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: package-release.sh TAG TARGET}
target=${2:?usage: package-release.sh TAG TARGET}
version=${tag#v}
binary="target/${target}/release/boomux"
package="boomux-${tag}-${target}"
archive="${package}.tar.gz"

if [[ ! -x "$binary" ]]; then
  printf 'release binary not found: %s\n' "$binary" >&2
  exit 1
fi

actual_version=$("$binary" --version)
if [[ "$actual_version" != "boomux ${version}" ]]; then
  printf 'expected boomux %s, got %s\n' "$version" "$actual_version" >&2
  exit 1
fi

rm -rf dist
mkdir -p "dist/${package}"
cp "$binary" "dist/${package}/boomux"
cp LICENSE README.md "dist/${package}/"
tar -C dist -czf "dist/${archive}" "$package"
rm -rf "dist/${package}"

(
  cd dist
  sha256sum "$archive" > "${archive}.sha256"
)
