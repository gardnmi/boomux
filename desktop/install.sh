#!/bin/sh
# Install the latest stable Linux release, or BOOMUX_DESKTOP_VERSION=vX.Y.Z.
set -eu

fail() { printf 'boomux-desktop: %s\n' "$*" >&2; exit 1; }

main() {
    prepare=false
    case "$*" in
        '') ;;
        --prepare) prepare=true ;;
        *) fail 'usage: boomux-desktop-installer.sh [--prepare]' ;;
    esac
    [ "$(uname -s)" = Linux ] || fail 'only Linux is currently supported'
    [ "$(uname -m)" = x86_64 ] || fail 'only x86_64 is currently supported'
    for tool in curl tar sha256sum readlink mktemp sed awk getconf timeout; do
        command -v "$tool" >/dev/null 2>&1 || fail "required command missing: $tool"
    done
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || fail 'this release requires glibc 2.39 or newer; musl/Alpine is not supported'
    printf '%s\n' "$glibc" | awk '$1 == "glibc" { split($2, v, "."); if (v[1] > 2 || (v[1] == 2 && v[2] >= 39)) ok=1 } END { exit !ok }' ||
        fail "this release requires glibc 2.39 or newer (found $glibc); use Ubuntu 24.04+, a current Arch system, or a compatible distribution"

    repository=https://github.com/gardnmi/boomux
    version=${BOOMUX_DESKTOP_VERSION:-} # release-version
    if [ -z "$version" ]; then
        latest=$(curl --proto '=https' --proto-redir '=https' -fsSL --retry 3 \
            --connect-timeout 15 --max-time 120 -o /dev/null -w '%{url_effective}' \
            "$repository/releases/latest")
        version=${latest##*/}
    fi
    printf '%s\n' "$version" | awk '/^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ { ok=1 } END { exit !ok }' || fail 'expected a stable release version vX.Y.Z'

    install_dir=${BOOMUX_DESKTOP_INSTALL_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/boomux-desktop}
    bin_dir=${BOOMUX_DESKTOP_BIN_DIR:-$HOME/.local/bin}
    data_dir=${XDG_DATA_HOME:-$HOME/.local/share}
    case "$data_dir" in /*) ;; *) fail 'XDG_DATA_HOME must be absolute' ;; esac
    # Desktop entry values cannot contain line breaks; Exec forbids '=' in paths.
    case "$install_dir$bin_dir$data_dir" in
        *'
'*|*'='*) fail 'install paths cannot contain newlines or equals signs' ;;
    esac
    case "$install_dir:$bin_dir" in /*:/*) ;; *) fail 'install directories must be absolute paths' ;; esac
    mkdir -p "$install_dir/releases"
    [ "$prepare" = true ] || mkdir -p "$bin_dir"
    # Resolve parent symlinks so ownership checks also work on subsequent installs.
    install_dir=$(cd "$install_dir" && pwd -P)
    if [ "$prepare" = false ]; then bin_dir=$(cd "$bin_dir" && pwd -P); fi
    desktop_link=$install_dir/current/bin/boomux-desktop
    if [ -e "$install_dir/current" ] && [ ! -L "$install_dir/current" ]; then
        fail 'current is not an installer-owned release link'
    fi
    if [ "$prepare" = true ]; then
        [ -L "$install_dir/current" ] || fail 'install Desktop before preparing an update'
    fi
    app_id=org.omarchy.boomux-desktop
    applications=$data_dir/applications
    icons=$data_dir/icons/hicolor/scalable/apps
    entry_target=$install_dir/desktop-entry
    icon_target=$install_dir/current/share/icons/hicolor/scalable/apps/$app_id.svg
    if [ "$prepare" = false ]; then
        for item in "$applications/$app_id.desktop" "$icons/$app_id.svg"; do
            case "$item" in *.desktop) expected=$entry_target ;; *) expected=$icon_target ;; esac
            if [ -e "$item" ] || [ -L "$item" ]; then
                [ "$(readlink "$item" || true)" = "$expected" ] || fail "desktop integration already exists and is not owned by this installer: $item"
            fi
        done
        if [ -e "$bin_dir/boomux-desktop" ] || [ -L "$bin_dir/boomux-desktop" ]; then
            [ "$(readlink "$bin_dir/boomux-desktop" || true)" = "$desktop_link" ] ||
                fail "$bin_dir/boomux-desktop already exists and is not owned by this installer"
        fi
    fi

    lock=$install_dir/.install-lock
    mkdir "$lock" 2>/dev/null || fail "another install is active; if it was interrupted, remove $lock and retry"
    stage=
    trap '[ -z "$stage" ] || rm -rf -- "$stage"; rmdir "$lock"' EXIT
    trap 'exit 1' HUP INT TERM
    stage=$(mktemp -d "$install_dir/.install.XXXXXXXX")
    asset=boomux-desktop-x86_64-unknown-linux-gnu.tar.gz
    base=$repository/releases/download/$version
    for file in "$asset" "$asset.sha256"; do
        limit=536870912
        [ "$file" = "$asset" ] || limit=4096
        curl --max-filesize "$limit" --proto '=https' --proto-redir '=https' -fsSL --retry 3 \
            --connect-timeout 15 --max-time 600 "$base/$file" -o "$stage/$file"
    done
    digest=$(awk -v name="$asset" 'NR == 1 && NF == 2 && $2 == name { digest=$1 } END { if (NR != 1 || digest == "") exit 1; print digest }' "$stage/$asset.sha256") || fail 'checksum must name exactly the Desktop archive'
    [ "${#digest}" -eq 64 ] || fail 'invalid checksum file'
    case "$digest" in *[!0-9a-f]*) fail 'invalid checksum file' ;; esac
    printf '%s  %s\n' "$digest" "$asset" > "$stage/checksum"
    (cd "$stage" && sha256sum -c checksum) || fail 'download checksum mismatch'

    mkdir "$stage/payload"
    tar -xzf "$stage/$asset" -C "$stage/payload" --no-same-owner --no-same-permissions \
        bin/boomux bin/boomux-desktop libexec/boomux-desktop LICENSE LICENSE.boomux THIRD_PARTY_NOTICES.md release.txt share
    for file in bin/boomux bin/boomux-desktop libexec/boomux-desktop; do
        [ -f "$stage/payload/$file" ] && [ ! -L "$stage/payload/$file" ] &&
            [ -x "$stage/payload/$file" ] || fail "invalid executable: $file"
    done
    [ -f "$stage/payload/share/applications/$app_id.desktop" ] || fail 'desktop entry missing'
    [ -f "$stage/payload/share/icons/hicolor/scalable/apps/$app_id.svg" ] || fail 'application icon missing'
    actual=$(timeout --kill-after=1s 10s "$stage/payload/bin/boomux" --version) || fail 'the bundled Boomux executable cannot run on this system'
    [ "$actual" = "boomux ${version#v}" ] || fail 'bundled Boomux version does not match the release'
    actual=$(timeout --kill-after=1s 10s "$stage/payload/libexec/boomux-desktop" --version) || fail 'Desktop cannot load its runtime libraries; see https://github.com/gardnmi/boomux#requirements'
    [ "$actual" = "boomux-desktop ${version#v}" ] || fail 'Desktop version does not match the release'
    timeout --kill-after=1s 10s "$stage/payload/libexec/boomux-desktop" --check-runtime || fail 'install the missing runtime libraries listed above, then rerun this installer'

    release=$install_dir/releases/$version-$digest
    if [ ! -d "$release" ]; then
        mv "$stage/payload" "$release"
    fi
    if [ "$prepare" = true ]; then
        ln -s "$release" "$stage/pending"
        mv -Tf "$stage/pending" "$install_dir/pending"
        printf 'Update %s is ready. Restart from Desktop to apply it.\n' "$version"
        return
    fi

    # Escape both desktop-entry string syntax and quoted Exec argument syntax.
    escaped_exec=$(printf '%s' "$desktop_link" | sed 's/\\/\\\\\\\\/g; s/"/\\\\"/g; s/\$/\\\\$/g; s/`/\\\\`/g; s/%/%%/g')
    { sed '/^Exec=/d' "$release/share/applications/$app_id.desktop"
      printf 'Exec=/usr/bin/env "%s"\n' "$escaped_exec"
    } > "$stage/desktop-entry"
    mkdir -p "$applications" "$icons"

    ln -s "$release" "$stage/current"
    mv -Tf "$stage/current" "$install_dir/current"
    mv -f "$stage/desktop-entry" "$entry_target"
    [ -L "$applications/$app_id.desktop" ] || ln -s "$entry_target" "$applications/$app_id.desktop"
    [ -L "$icons/$app_id.svg" ] || ln -s "$icon_target" "$icons/$app_id.svg"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$applications" >/dev/null 2>&1 || true
    fi
    if [ ! -L "$bin_dir/boomux-desktop" ]; then
        ln -s "$desktop_link" "$bin_dir/boomux-desktop"
    fi
    # Keep an existing user's CLI installation intact.
    if [ ! -e "$bin_dir/boomux" ] && [ ! -L "$bin_dir/boomux" ]; then
        ln -s "$install_dir/current/bin/boomux" "$bin_dir/boomux"
    fi

    printf '\nInstalled Boomux Desktop %s (Boomux included).\n' "$version"
    printf 'Launch: %s/boomux-desktop\n' "$bin_dir"
    printf 'Or select Boomux Desktop from your application menu.\n'
    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *) printf 'Add this directory to your shell PATH: %s\n' "$bin_dir" ;;
    esac
    printf 'Rerun this installer to update. Existing daemon and Shells are left running.\n'
}

# Keep execution last so a truncated piped download cannot run a partial install.
main "$@"
