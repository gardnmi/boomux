#!/bin/sh
# Exercise the public release installer against the actual native CI bundle.
set -eu
root=$(pwd)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/boomux-install-test.XXXXXX")
trap 'rm -rf "$fixture"' EXIT
mkdir "$fixture/bin" "$fixture/home with spaces"
cat > "$fixture/bin/curl" <<'CURL'
#!/bin/sh
set -eu
while [ "$1" != -o ]; do shift; done
shift
output=$1
shift
cp "$BOOMUX_INSTALL_FIXTURES/${1##*/}" "$output"
CURL
chmod +x "$fixture/bin/curl"
tag=$(python3 -c 'import tomllib; print("v" + tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
bash packaging/render-installer.sh "$tag" "$fixture/installer"
HOME="$fixture/home with spaces" PATH="$fixture/bin:$PATH" \
    BOOMUX_INSTALL_FIXTURES="$root/dist/macos/release" sh "$fixture/installer" --desktop
app=$fixture/home\ with\ spaces/Applications/Boomux-${tag#v}.app
codesign --verify --deep --strict "$app"
test "$("$app/Contents/MacOS/boomux" --version)" = "boomux ${tag#v}"
