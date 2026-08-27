#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: upload-release-assets.sh TAG ASSET...}
shift
if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  printf 'release tag must be strict vMAJOR.MINOR.PATCH: %s\n' "$tag" >&2
  exit 1
fi

if (($# == 0)); then
  printf 'no release assets supplied\n' >&2
  exit 1
fi

repo=${GH_REPO:-${GITHUB_REPOSITORY:-}}
if [[ -z "$repo" ]]; then
  printf 'GH_REPO or GITHUB_REPOSITORY must be set\n' >&2
  exit 1
fi

declare -A expected_assets=()
expected_assets[boomux-installer.sh]=1
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  archive="boomux-${tag}-${target}.tar.gz"
  expected_assets["$archive"]=1
  expected_assets["${archive}.sha256"]=1
done

if (($# != ${#expected_assets[@]})); then
  printf 'expected exactly %d release assets, got %d\n' "${#expected_assets[@]}" "$#" >&2
  exit 1
fi

declare -A local_assets=()
for asset in "$@"; do
  if [[ ! -f "$asset" ]]; then
    printf 'release asset not found: %s\n' "$asset" >&2
    exit 1
  fi
  name=$(basename "$asset")
  if [[ ! -v "expected_assets[$name]" ]]; then
    printf 'unexpected release asset: %s\n' "$name" >&2
    exit 1
  fi
  if [[ -v "local_assets[$name]" ]]; then
    printf 'duplicate local release asset: %s\n' "$name" >&2
    exit 1
  fi
  local_assets["$name"]=$asset
done

for name in "${!expected_assets[@]}"; do
  if [[ ! -v "local_assets[$name]" ]]; then
    printf 'missing release asset: %s\n' "$name" >&2
    exit 1
  fi
done

for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  archive="boomux-${tag}-${target}.tar.gz"
  (
    cd "$(dirname "${local_assets[$archive]}")"
    sha256sum --check "$(basename "${local_assets[${archive}.sha256]}")"
  )
done
installer=${local_assets[boomux-installer.sh]}
sh -n "$installer"
if ! grep -Fq "tag='$tag'" "$installer"; then
  printf 'release installer does not target %s\n' "$tag" >&2
  exit 1
fi
if ! grep -Fq "repository='https://github.com/gardnmi/boomux'" "$installer" \
  || grep -Fq 'BOOMUX_INSTALL_BASE_URL' "$installer" \
  || ! grep -Fq 'Run the guided setup now? [Y/n]' "$installer"; then
  printf 'release installer does not preserve the reviewed origin and setup handoff\n' >&2
  exit 1
fi

release_id=$(gh api "repos/${repo}/releases/tags/${tag}" --jq .id 2>/dev/null || true)
if [[ -z "$release_id" ]]; then
  release_id=$(gh api --paginate "repos/${repo}/releases?per_page=100" \
    --jq ".[] | select(.tag_name == \"$tag\") | .id")
fi
if [[ ! "$release_id" =~ ^[0-9]+$ ]]; then
  printf 'could not resolve one release for %s\n' "$tag" >&2
  exit 1
fi
declare -A remote_digests=()
declare -A remote_ids=()

while IFS=$'\x1f' read -r name digest id; do
  if [[ -v "remote_ids[$name]" ]]; then
    printf 'release has duplicate asset name: %s\n' "$name" >&2
    exit 1
  fi
  remote_digests["$name"]=$digest
  remote_ids["$name"]=$id
done < <(
  gh api --paginate "repos/${repo}/releases/${release_id}/assets?per_page=100" \
    --jq '.[] | [.name, (.digest // ""), (.id | tostring)] | join("\u001f")'
)

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

for asset in "$@"; do
  name=$(basename "$asset")
  local_digest=$(sha256sum "$asset" | cut -d ' ' -f 1)
  if [[ -v "remote_ids[$name]" ]]; then
    remote_digest=${remote_digests[$name]}
    if [[ -n "$remote_digest" ]]; then
      if [[ "$remote_digest" != sha256:* ]]; then
        printf 'release asset %s has unsupported digest: %s\n' "$name" "$remote_digest" >&2
        exit 1
      fi
      remote_digest=${remote_digest#sha256:}
    else
      gh api "repos/${repo}/releases/assets/${remote_ids[$name]}" \
        -H 'Accept: application/octet-stream' > "${tmp_dir}/${name}"
      remote_digest=$(sha256sum "${tmp_dir}/${name}" | cut -d ' ' -f 1)
    fi

    if [[ "$local_digest" == "$remote_digest" ]]; then
      printf 'release asset already matches, skipping: %s\n' "$name"
      continue
    fi

    printf 'release asset digest conflict: %s (local %s, remote %s)\n' \
      "$name" "$local_digest" "$remote_digest" >&2
    exit 1
  fi

  gh release upload "$tag" "$asset" --repo "$repo"
done
