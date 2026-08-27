#!/usr/bin/env bash
set -euo pipefail

root=$(mktemp -d)
cleanup() {
  rm -rf "$root"
}
trap cleanup EXIT

mkdir "$root/bin"
printf 'Generated release notes' > "$root/body"
printf '0' > "$root/updates"
cat > "$root/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ " $* " == *' --method PATCH '* ]]; then
  for argument in "$@"; do
    case $argument in
      body=*) printf '%s' "${argument#body=}" > "$RELEASE_BODY" ;;
    esac
  done
  updates=$(<"$RELEASE_UPDATES")
  printf '%s' "$((updates + 1))" > "$RELEASE_UPDATES"
elif [[ " $* " == *' --jq .id '* ]]; then
  printf '42\n'
elif [[ " $* " == *' --include '* ]]; then
  printf 'HTTP/2 200\netag: "fixture-%s"\n\n' "$(<"$RELEASE_UPDATES")"
  cat "$RELEASE_BODY"
else
  cat "$RELEASE_BODY"
fi
EOF
chmod 755 "$root/bin/gh"

for _ in first second; do
  GH_REPO=gardnmi/boomux \
    RELEASE_BODY="$root/body" \
    RELEASE_UPDATES="$root/updates" \
    PATH="$root/bin:/usr/bin:/bin" \
    bash .github/scripts/update-release-notes.sh v1.2.3 >/dev/null
done

[[ $(<"$root/updates") == 1 ]]
grep -F 'Generated release notes' "$root/body" >/dev/null
grep -F 'releases/latest/download/boomux-installer.sh' "$root/body" >/dev/null
grep -F '`~/.local/bin/boomux setup`' "$root/body" >/dev/null
grep -F '<!-- /boomux-install-handoff -->' "$root/body" >/dev/null

for malformed in \
  '<!-- /boomux-install-handoff -->' \
  '<!-- boomux-install-handoff --><!-- boomux-install-handoff -->'; do
  printf '%s\n' 'Generated release notes' "$malformed" > "$root/body"
  if GH_REPO=gardnmi/boomux RELEASE_BODY="$root/body" \
    RELEASE_UPDATES="$root/updates" PATH="$root/bin:/usr/bin:/bin" \
    bash .github/scripts/update-release-notes.sh v1.2.3 >/dev/null 2>&1; then
    printf 'release-note updater accepted malformed marker structure\n' >&2
    exit 1
  fi
  [[ $(<"$root/updates") == 1 ]]
done

printf '%s\n' 'Generated release notes' '<!-- boomux-install-handoff -->' 'broken' > "$root/body"
if GH_REPO=gardnmi/boomux RELEASE_BODY="$root/body" \
  RELEASE_UPDATES="$root/updates" PATH="$root/bin:/usr/bin:/bin" \
  bash .github/scripts/update-release-notes.sh v1.2.3 >/dev/null 2>&1; then
  printf 'release-note updater accepted a malformed managed block\n' >&2
  exit 1
fi
[[ $(<"$root/updates") == 1 ]]
