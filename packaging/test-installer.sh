#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Mike Gardner
# SPDX-License-Identifier: 0BSD
set -euo pipefail

root=$(mktemp -d)
cleanup() {
  rm -rf "$root"
}
trap cleanup EXIT

tag=v1.2.3
target=x86_64-unknown-linux-gnu
package="boomux-${tag}-${target}"
archive="${package}.tar.gz"
mkdir -p "$root/release/$package" "$root/bin" "$root/home"
cat > "$root/release/$package/boomux" <<'EOF'
#!/bin/sh
if [ "${1-}" = --version ]; then printf 'boomux 1.2.3\n'; exit 0; fi
exit 97
EOF
chmod 755 "$root/release/$package/boomux"
tar -C "$root/release" -czf "$root/release/$archive" "$package"
(
  cd "$root/release"
  sha256sum "$archive" > "${archive}.sha256"
)

cat > "$root/bin/curl" <<'EOF'
#!/bin/sh
output=
url=
previous=
for argument do
  if [ "$previous" = --output ]; then output=$argument; fi
  previous=$argument
  url=$argument
done
[ -n "$output" ] || exit 64
cp "${BOOMUX_INSTALL_FIXTURES}/${url##*/}" "$output"
EOF
chmod 755 "$root/bin/curl"

packaging/render-installer.sh "$tag" "$root/installer"
if grep -Fq 'BOOMUX_INSTALL_BASE_URL' "$root/installer" \
  || ! grep -Fq 'https://github.com/gardnmi/boomux' "$root/installer"; then
  printf 'installer download origin is not fixed to the official repository\n' >&2
  exit 1
fi
output=$(
  BOOMUX_INSTALL_FIXTURES="$root/release" \
  HOME="$root/home" \
  PATH="$root/bin:/usr/bin:/bin" \
  "$root/installer" --no-setup
)
[[ "$output" == *"Boomux 1.2.3 installed"* ]]
[[ "$output" == *"Next: $root/home/.local/bin/boomux setup"* ]]
[[ $("$root/home/.local/bin/boomux" --version) == 'boomux 1.2.3' ]]

if BOOMUX_INSTALL_FIXTURES="$root/release" \
  HOME="$root/home" \
  PATH="$root/bin:/usr/bin:/bin" \
  "$root/installer" --no-setup >"$root/reinstall.out" 2>"$root/reinstall.err"; then
  printf 'installer replaced an existing Boomux installation\n' >&2
  exit 1
fi
grep -F 'Boomux is already installed' "$root/reinstall.err" >/dev/null

if HOME="$root/home" PATH="$root/bin:/usr/bin:/bin" \
  "$root/installer" --no-setup unexpected >/dev/null 2>&1; then
  printf 'installer accepted an extra argument\n' >&2
  exit 1
fi

unsafe_home="$root/unsafe-home"
mkdir "$unsafe_home"
chmod 0777 "$unsafe_home"
if BOOMUX_INSTALL_FIXTURES="$root/release" HOME="$unsafe_home" \
  PATH="$root/bin:/usr/bin:/bin" "$root/installer" --no-setup >/dev/null 2>&1; then
  printf 'installer accepted an unsafe HOME directory\n' >&2
  exit 1
fi

checksum_home="$root/checksum-home"
mkdir "$checksum_home"
printf '%064d  %s\n' 0 "$archive" > "$root/release/${archive}.sha256"
if BOOMUX_INSTALL_FIXTURES="$root/release" HOME="$checksum_home" \
  PATH="$root/bin:/usr/bin:/bin" "$root/installer" --no-setup >/dev/null 2>&1; then
  printf 'installer accepted a checksum mismatch\n' >&2
  exit 1
fi
[[ ! -e "$checksum_home/.local/bin/boomux" ]]
archive_digest=$(sha256sum "$root/release/$archive")
archive_digest=${archive_digest%% *}
printf '%s  other-archive.tar.gz\n' "$archive_digest" > "$root/release/${archive}.sha256"
if BOOMUX_INSTALL_FIXTURES="$root/release" HOME="$checksum_home" \
  PATH="$root/bin:/usr/bin:/bin" "$root/installer" --no-setup >/dev/null 2>&1; then
  printf 'installer accepted a checksum for the wrong archive\n' >&2
  exit 1
fi
[[ ! -e "$checksum_home/.local/bin/boomux" ]]
(
  cd "$root/release"
  sha256sum "$archive" > "${archive}.sha256"
)

race_home="$root/race-home"
mkdir "$race_home"
cat > "$root/bin/ln" <<'EOF'
#!/bin/sh
case ${BOOMUX_RACE_KIND:-file} in
  file) : > "$BOOMUX_RACE_DESTINATION" ;;
  directory) mkdir "$BOOMUX_RACE_DESTINATION" ;;
esac
exec /usr/bin/ln "$@"
EOF
chmod 755 "$root/bin/ln"
if BOOMUX_INSTALL_FIXTURES="$root/release" \
  BOOMUX_RACE_DESTINATION="$race_home/.local/bin/boomux" \
  HOME="$race_home" PATH="$root/bin:/usr/bin:/bin" \
  "$root/installer" --no-setup >/dev/null 2>&1; then
  printf 'installer replaced a destination created during installation\n' >&2
  exit 1
fi
[[ -f "$race_home/.local/bin/boomux" ]]
[[ ! -s "$race_home/.local/bin/boomux" ]]

race_directory_home="$root/race-directory-home"
mkdir "$race_directory_home"
if BOOMUX_INSTALL_FIXTURES="$root/release" \
  BOOMUX_RACE_DESTINATION="$race_directory_home/.local/bin/boomux" \
  BOOMUX_RACE_KIND=directory HOME="$race_directory_home" \
  PATH="$root/bin:/usr/bin:/bin" \
  "$root/installer" --no-setup >/dev/null 2>&1; then
  printf 'installer accepted a directory created at the destination\n' >&2
  exit 1
fi
[[ -d "$race_directory_home/.local/bin/boomux" ]]
[[ ! -e "$race_directory_home/.local/bin/boomux/.boomux-install."* ]]
