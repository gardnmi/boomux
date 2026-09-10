#!/bin/bash
# Installer for one immutable, CI-validated preview. No GitHub login required.
set -euo pipefail

fail() { printf '%s\n' "$*" >&2; exit 1; }
[ "$(uname -s)" = Darwin ] || fail 'This preview requires macOS.'
[ "$(uname -m)" = arm64 ] || fail 'This preview requires an Apple Silicon Mac. Run Terminal without Rosetta.'
version=$(sw_vers -productVersion)
[ "${version%%.*}" -ge 15 ] || fail 'This preview requires macOS 15 or newer.'

readonly tag=preview-macos-20260910.34427759652
readonly archive=boomux-macos-preview-aarch64-66bca02a.zip
readonly sha256=65088ef421eb25bd35576d059bc37e0f318aa8cefdf12d29f23049da8bd174a4
readonly destination="$HOME/Applications/Boomux Preview-66bca02a.app"
[ ! -e "$destination" ] && [ ! -L "$destination" ] || fail "Already installed: $destination. Open it from Finder."

staging=$(mktemp -d "${TMPDIR:-/tmp}/boomux-preview.XXXXXX")
trap 'rm -rf "$staging"' EXIT
printf 'Downloading Boomux development preview (ad-hoc signed, not notarized)...\n'
curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
  --connect-timeout 20 --max-time 300 --max-filesize 134217728 \
  "https://github.com/gardnmi/boomux/releases/download/$tag/$archive" -o "$staging/$archive"
actual=$(shasum -a 256 "$staging/$archive")
[ "${actual%% *}" = "$sha256" ] || fail 'Download checksum mismatch; nothing was installed.'
ditto -x -k "$staging/$archive" "$staging/unpacked"
app="$staging/unpacked/Boomux macOS Preview/Boomux.app"
[ -d "$app" ] && [ ! -L "$app" ] || fail 'The download does not contain the expected application.'
codesign --verify --deep --strict "$app"
for executable in boomux boomux-desktop; do
  [ "$(lipo -archs "$app/Contents/MacOS/$executable")" = arm64 ] || fail 'Unexpected executable architecture.'
done
mkdir -p "$HOME/Applications"
# -n prevents replacement if another installer created the destination meanwhile.
mv -n "$app" "$destination"
[ ! -e "$app" ] || fail 'Another installation already exists; it was not replaced.'
printf '\nInstalled: %s\nOpen this app in Finder to start Boomux.\n' "$destination"
printf 'If macOS blocks it, use System Settings > Privacy & Security > Open Anyway.\n'
printf 'This is a testing preview; it does not install a login service or a global CLI.\n'
