#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: package-release.sh TAG TARGET}
target=${2:?usage: package-release.sh TAG TARGET}
version=${tag#v}
binary="target/${target}/release/boomux"
package="boomux-${tag}-${target}"
archive="${package}.tar.gz"
smoke_dir=

cleanup() {
  if [[ -n "$smoke_dir" ]]; then
    rm -rf "$smoke_dir"
  fi
}
trap cleanup EXIT

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
cp LICENSE README.md THIRD_PARTY_NOTICES.md "dist/${package}/"
tar -C dist -czf "dist/${archive}" "$package"
rm -rf "dist/${package}"

smoke_dir=$(mktemp -d)
tar -C "$smoke_dir" -xzf "dist/${archive}"
smoke_binary="${smoke_dir}/${package}/boomux"
smoke_version=$("$smoke_binary" --version)
if [[ "$smoke_version" != "boomux ${version}" ]]; then
  printf 'packaged binary: expected boomux %s, got %s\n' "$version" "$smoke_version" >&2
  exit 1
fi

for file in LICENSE README.md THIRD_PARTY_NOTICES.md; do
  if ! cmp -s "$file" "${smoke_dir}/${package}/${file}"; then
    printf 'packaged file missing or changed: %s\n' "$file" >&2
    exit 1
  fi
done
packaged_readme="${smoke_dir}/${package}/README.md"
if ! grep -Fq 'releases/latest/download/boomux-installer.sh' "$packaged_readme" \
  || ! grep -Fq 'offers to run `boomux setup` immediately' "$packaged_readme" \
  || ! grep -Fq '~/.local/bin/boomux setup' "$packaged_readme"; then
  printf 'packaged README is missing the installer or setup handoff\n' >&2
  exit 1
fi

smoke_home="${smoke_dir}/home"
smoke_bin="${smoke_dir}/bin"
mkdir -p "${smoke_home}/.local/bin" "$smoke_bin"
cp "$smoke_binary" "${smoke_home}/.local/bin/boomux"
cat > "${smoke_bin}/curl" <<'EOF'
#!/bin/sh
output=
previous=
for argument do
  if [ "$previous" = --output ]; then output=$argument; fi
  previous=$argument
done
[ -n "$output" ] || exit 64
printf '%s' '{"tag_name":"v9999.0.0","html_url":"https://github.com/gardnmi/boomux/releases/tag/v9999.0.0"}' > "$output"
EOF
chmod +x "${smoke_bin}/curl"
update_status=$(
  HOME="$smoke_home" \
    PATH="${smoke_bin}:/usr/bin:/bin" \
    XDG_RUNTIME_DIR="${smoke_dir}/runtime" \
    "${smoke_home}/.local/bin/boomux" --json update status
)
if ! grep -Eq '"install_kind"[[:space:]]*:[[:space:]]*"github_release"' <<< "$update_status"; then
  printf 'packaged binary is not marked as a GitHub release build\n' >&2
  exit 1
fi

(
  cd dist
  sha256sum "$archive" > "${archive}.sha256"
)
