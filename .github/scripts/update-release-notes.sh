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
release_id=$(gh api "repos/${repo}/releases/tags/${tag}" --jq .id 2>/dev/null || true)
if [[ -z "$release_id" ]]; then
  release_id=$(gh api --paginate "repos/${repo}/releases?per_page=100" \
    --jq ".[] | select(.tag_name == \"$tag\") | .id")
fi
if [[ ! "$release_id" =~ ^[0-9]+$ ]]; then
  printf 'could not resolve one release for %s\n' "$tag" >&2
  exit 1
fi
read -r -d '' handoff <<'EOF' || true
<!-- boomux-install-handoff -->
## Install Boomux

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh
```

The verified installer offers to run `boomux setup` immediately. If setup is
deferred or interrupted, run `~/.local/bin/boomux setup` to continue.
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
    if [[ "$marker_count" != 1 || "$end_marker_count" != 1 || "$body" != *"$handoff" ]]; then
      printf 'release notes contain a malformed installation handoff\n' >&2
      exit 1
    fi
    printf 'release notes already contain installation handoff\n'
    exit 0
  fi

  response=$(gh api --include "$endpoint")
  etag=$(printf '%s\n' "$response" | sed -n 's/^[Ee][Tt][Aa][Gg]:[[:space:]]*//p' | tr -d '\r' | sed -n '1p')
  if [[ -z "$etag" ]]; then
    printf 'release response did not include an ETag\n' >&2
    exit 1
  fi
  if [[ $(gh api "$endpoint" --jq '.body // ""') != "$body" ]]; then
    continue
  fi

  updated=$body
  if [[ -n "$updated" ]]; then
    updated+=$'\n\n'
  fi
  updated+=$handoff
  if gh api --method PATCH "$endpoint" -H "If-Match: $etag" -f body="$updated" >/dev/null; then
    verified=$(gh api "$endpoint" --jq '.body // ""')
    marker_count=$(count_occurrences "$verified" "$marker")
    end_marker_count=$(count_occurrences "$verified" "$end_marker")
    if [[ "$marker_count" == 1 && "$end_marker_count" == 1 && "$verified" == *"$handoff" ]]; then
      exit 0
    fi
  fi
done

printf 'release notes changed concurrently; handoff was not applied\n' >&2
exit 1
