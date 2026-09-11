#!/bin/sh
# SPDX-FileCopyrightText: 2026 Mike Gardner
# SPDX-License-Identifier: 0BSD
# Embedded in the version-pinned universal release installer.
set -eu

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) ;;
    *) printf 'Boomux Desktop on macOS requires Apple Silicon.\n' >&2; exit 1 ;;
esac
macos_version=$(sw_vers -productVersion)
macos_major=${macos_version%%.*}
case $macos_major in
    ''|*[!0-9]*) printf 'Cannot determine macOS version.\n' >&2; exit 1 ;;
esac
[ "$macos_major" -ge 15 ] || { printf 'Boomux Desktop requires macOS 15 or newer.\n' >&2; exit 1; }
for command in curl shasum ditto codesign lipo mktemp mkdir mv rm rmdir stat id; do
    command -v "$command" >/dev/null 2>&1 || { printf '%s is required.\n' "$command" >&2; exit 1; }
done
case ${HOME-} in
    /*) ;;
    *) printf 'HOME must be an absolute path.\n' >&2; exit 1 ;;
esac
macos_validate_directory() {
    [ -d "$1" ] && [ ! -L "$1" ] || { printf 'Expected a real directory: %s\n' "$1" >&2; exit 1; }
    owner=$(stat -f '%u' "$1")
    mode=$(stat -f '%Lp' "$1")
    if [ "$owner" != "$(id -u)" ] || [ $(((mode / 10) % 10 & 2)) -ne 0 ] || [ $((mode % 10 & 2)) -ne 0 ]; then
        printf 'Directory must be owned by you and not group/world-writable: %s\n' "$1" >&2
        exit 1
    fi
}
macos_validate_directory "$HOME"
applications=$HOME/Applications
if [ ! -e "$applications" ] && [ ! -L "$applications" ]; then
    (umask 077 && mkdir "$applications")
fi
macos_validate_directory "$applications"
destination=$applications/Boomux-${tag#v}.app
if [ -e "$destination" ] || [ -L "$destination" ]; then
    printf 'Boomux already exists at %s; leaving it unchanged.\n' "$destination" >&2
    exit 1
fi
lock=$applications/.boomux-install.lock
(umask 077 && mkdir "$lock") || { printf 'Another installation holds %s.\n' "$lock" >&2; exit 1; }
temporary=
macos_cleanup() {
    if [ -n "$temporary" ]; then rm -rf "$temporary"; fi
    rmdir "$lock"
}
trap macos_cleanup EXIT
trap 'exit 1' HUP INT TERM
temporary=$(mktemp -d "$applications/.boomux-install.XXXXXX")
archive=boomux-desktop-aarch64-apple-darwin.zip
base_url=$repository/releases/download/$tag
printf 'WARNING: macOS is a largely untested experimental preview with very little real-world testing.\n'
printf 'Limited CI and smoke tests do not establish everyday reliability. Use for evaluation only, not important work.\n'
printf 'Downloading Boomux %s for Apple Silicon macOS (experimental)...\n' "${tag#v}"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fLsS --max-filesize 134217728 -o "$temporary/$archive" "$base_url/$archive"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fLsS --max-filesize 1024 -o "$temporary/$archive.sha256" "$base_url/$archive.sha256"
(
    cd "$temporary"
    {
        IFS=' ' read -r expected expected_archive trailing || exit 1
        if IFS= read -r unexpected; then printf 'Unexpected checksum entries.\n' >&2; exit 1; fi
        case $expected in ''|*[!0-9a-f]*) exit 1 ;; esac
        [ "${#expected}" -eq 64 ] && [ "$expected_archive" = "$archive" ] && [ -z "$trailing" ] || exit 1
        printf '%s  %s\n' "$expected" "$archive" | shasum -a 256 -c -
    } < "$archive.sha256"
) || { printf 'Release checksum mismatch or malformed checksum.\n' >&2; exit 1; }
ditto -x -k "$temporary/$archive" "$temporary/unpacked"
candidate=$temporary/unpacked/Boomux\ macOS\ Preview/Boomux.app
[ -d "$candidate" ] && [ ! -L "$candidate" ] || { printf 'Missing application bundle.\n' >&2; exit 1; }
codesign --verify --deep --strict "$candidate"
for name in boomux boomux-desktop; do
    binary=$candidate/Contents/MacOS/$name
    [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] || exit 1
    [ "$(lipo -archs "$binary")" = arm64 ] || { printf 'Unexpected binary architecture.\n' >&2; exit 1; }
    [ "$("$binary" --version)" = "$name ${tag#v}" ] || { printf 'Unexpected binary version.\n' >&2; exit 1; }
done
mv -n "$candidate" "$destination"
[ ! -e "$candidate" ] || { printf 'Destination appeared during installation; leaving it unchanged.\n' >&2; exit 1; }
printf '\nInstalled %s\nOpen this app in Finder to launch Boomux.\n' "$destination"
printf 'macOS support is experimental; the app is ad-hoc signed and not notarized.\n'
printf 'If macOS blocks opening it, follow System Settings > Privacy & Security > Open Anyway.\n'
printf 'For a later version, quit Boomux and run this command again; older apps are preserved.\n'
