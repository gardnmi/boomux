#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: update-release-notes.sh TAG}
if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  printf 'release tag must be strict vMAJOR.MINOR.PATCH: %s\n' "$tag" >&2
  exit 1
fi
repo=${GH_REPO:-${GITHUB_REPOSITORY:-}}
if [[ -z "$repo" ]]; then
  printf 'GH_REPO or GITHUB_REPOSITORY must be set\n' >&2
  exit 1
fi

marker='<!-- boomux-install-handoff -->'
end_marker='<!-- /boomux-install-handoff -->'
release_id=$(gh api "repos/${repo}/releases/tags/${tag}" --jq .id 2>/dev/null \
  | sed -n '/^[0-9][0-9]*$/p' || true)
if [[ ! "$release_id" =~ ^[0-9]+$ ]]; then
  release_id=$(gh api --paginate "repos/${repo}/releases?per_page=100" \
    --jq ".[] | select((.tag_name == \"$tag\") or (.draft == true and .name == \"$tag\")) | .id" \
    | sed -n '/^[0-9][0-9]*$/p')
fi
if [[ ! "$release_id" =~ ^[0-9]+$ ]]; then
  printf 'could not resolve one release for %s\n' "$tag" >&2
  exit 1
fi
read -r -d '' handoff <<'EOF' || true
<!-- boomux-install-handoff -->
## Boomux Desktop

A native terminal workspace with Hyprland-inspired pane movement, persistent
shells, agent status, and remote workspaces.

### Install

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh -s -- --desktop
```

Installs Desktop and the matching Boomux service together.
The same command selects GNU/Linux x86_64 or Apple Silicon macOS 15+.
macOS support is experimental; the app is ad-hoc signed and not notarized.

### Launch

On macOS, open the installed version of **Boomux** in `~/Applications` using Finder.
If macOS blocks it, use **System Settings > Privacy & Security > Open Anyway**.
To update, quit the app and rerun the installer; older versions are preserved.

On Linux, open **Boomux Desktop** from your application launcher, or run:

```console
boomux-desktop
```

The service starts automatically; no separate CLI setup is needed for Desktop.

[See it in motion](https://gardnmi.github.io/boomux/#in-motion) ·
[Desktop guide](https://github.com/gardnmi/boomux/blob/main/desktop/README.md)
<!-- /boomux-install-handoff -->
EOF

endpoint="repos/${repo}/releases/${release_id}"
count_occurrences() {
  local remaining=$1
  local needle=$2
  local count=0
  while [[ "$remaining" == *"$needle"* ]]; do
    remaining=${remaining#*"$needle"}
    count=$((count + 1))
  done
  printf '%s\n' "$count"
}

for attempt in 1 2 3; do
  body=$(gh api "$endpoint" --jq '.body // ""')
  marker_count=$(count_occurrences "$body" "$marker")
  end_marker_count=$(count_occurrences "$body" "$end_marker")
  if [[ "$marker_count" != 0 || "$end_marker_count" != 0 ]]; then
    if [[ "$marker_count" != 1 || "$end_marker_count" != 1 || "$body" != "$handoff"* ]]; then
      printf 'release notes contain a malformed installation handoff\n' >&2
      exit 1
    fi
    printf 'release notes already contain installation handoff\n'
    exit 0
  fi

  if [[ $(gh api "$endpoint" --jq '.body // ""') != "$body" ]]; then
    continue
  fi

  updated=$handoff
  if [[ -n "$body" ]]; then
    updated+=$'\n\n'"$body"
  fi
  if gh api --method PATCH "$endpoint" -f tag_name="$tag" -f body="$updated" >/dev/null; then
    verified=$(gh api "$endpoint" --jq '.body // ""')
    marker_count=$(count_occurrences "$verified" "$marker")
    end_marker_count=$(count_occurrences "$verified" "$end_marker")
    if [[ "$marker_count" == 1 && "$end_marker_count" == 1 && "$verified" == "$updated" ]]; then
      exit 0
    fi
  fi
done

printf 'release notes changed concurrently or update verification failed; handoff was not applied\n' >&2
exit 1
