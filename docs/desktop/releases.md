# Desktop releases

## Distribution and installation

One Boomux release contains the CLI packages for x86_64/ARM64 and the Desktop
bundle for x86_64 GNU/Linux. The Desktop archive contains `bin/boomux`, the
`bin/boomux-desktop` launcher, `libexec/boomux-desktop`, application integration,
licenses/notices, and `release.txt`. The CLI executable is byte-identical to the
standalone x86_64 archive from the same source SHA and version.

After the first unified release is published:

```sh
curl -fsSL https://github.com/gardnmi/boomux/releases/latest/download/boomux-desktop-installer.sh | sh
```

The release installer pins its own release by default. To select another release
containing Desktop assets, set `BOOMUX_DESKTOP_VERSION=vX.Y.Z` on the `sh` process.
Unsupported old versions fail without changing the current installation.

Installed paths remain `~/.local/share/boomux-desktop/releases/<version>-<digest>`
and the atomic `current` link. Commands are linked under `~/.local/bin`; existing
independent CLI installations are preserved. XDG directories and absolute
`BOOMUX_DESKTOP_INSTALL_DIR` / `BOOMUX_DESKTOP_BIN_DIR` overrides remain supported.
No updater independently replaces bundle-owned Boomux, including through its
optional CLI symlink. Rerunning the installer updates the whole bundle without
restarting a running daemon or overwriting running executable files.

The old repository is retained until cutover. Existing development builds should
rerun the canonical installer after the first unified release; their old update
endpoint cannot be changed by moving source. Install paths and preferences need
no data migration. Old-repository installer forwarding and archival happen only
after the new release route has been verified.

## Build and publish

Root Release Please manages both package versions, the shared lockfile,
changelog, and one `vX.Y.Z` GitHub release. There is no separate Desktop release
manifest. Fixtures in `.github/release-tests` exercise the pinned Rust strategy
for both Desktop features and backend fixes. Existing token/settings behavior
is retained; this migration adds no new release credential requirement.

CI builds the official CLI once per architecture. `desktop-build.yml` builds
Desktop and packages the exact x86_64 CLI candidate. Provenance records identify
artifact kind, source SHA, tag, target, and digest. The publisher also compares
the CLI bytes across both archives and rejects incomplete or conflicting assets.

Publication follows successful exact-main CI and the existing omarchy-boomux
capability check. It reuses tested artifacts; missing automatic artifacts fail.
Manual recovery resolves an exact source, rebuilds/packages candidates, and runs
Desktop display tests before publishing the draft. Already published releases
are preserved. A required failure on either component blocks the shared release.

For a local candidate, from the repository root:

```sh
BOOMUX_DISTRIBUTION=github-release cargo build -p boomux --release --locked --target x86_64-unknown-linux-gnu
cargo build -p boomux-desktop --release --locked --target x86_64-unknown-linux-gnu
bash .github/scripts/package-release.sh vX.Y.Z x86_64-unknown-linux-gnu
sh desktop/scripts/package-release.sh dist/boomux-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
```

Substitute the version in root `Cargo.toml`. `release.txt` records the checkout
SHA; hosted CI uses a clean checkout. Local uncommitted builds are development
evidence and are not publishable CI artifacts.

## Validation

Run the root and Desktop `AGENTS.md` checks, Python installer/packaging/publication
fixtures, and release-version fixtures. Install display/runtime dependencies
from `.github/workflows/desktop-smoke.yml`, then run:

```sh
python3 desktop/scripts/smoke-desktop.py --backend x11 --cpu-model Nehalem \
  --archive dist/boomux-desktop-x86_64-unknown-linux-gnu.tar.gz --output smoke-results/x11
python3 desktop/scripts/smoke-desktop.py --backend wayland \
  --archive dist/boomux-desktop-x86_64-unknown-linux-gnu.tar.gz --output smoke-results/wayland
```

The harness creates private XDG directories, an isolated display and D-Bus, and
uses Mesa software rendering. It checks mapped/committed frames, pending Shell
attachment and PTY output, then close/reopen with the same daemon and ShellRun.
The X11 path emulates the foreground daemon and CLI as well as Desktop, asserting
that daemon/Desktop process identities remain QEMU. It does not exercise daemon
self-reexecution under QEMU; native backend handoff tests cover replacement.
The Wayland path exercises the actual launcher, settings transactions, and
bundle-owned CLI update refusal. Both binaries are part of the test contract.

See [shared CI](ci.md) for support limits and artifact reuse.

## Desktop Integration And Preferences

The bundle includes a temporary four-tile SVG icon and an application entry.
The installer registers `org.omarchy.boomux-desktop.desktop` in
`${XDG_DATA_HOME:-$HOME/.local/share}/applications` and links the icon into the
matching `icons/hicolor/scalable/apps` directory. It preserves unrelated menu
entries and icons by refusing to replace them. The installed entry uses the
absolute bundled launcher path, including proper quoting for paths with spaces;
launching from the menu does not depend on the session's PATH.

Desktop preferences are stored in
`${XDG_CONFIG_HOME:-$HOME/.config}/boomux-desktop/settings.toml`. The app loads
this file on startup and saves changes made through Settings automatically.
It persists sidebar visibility, pane headings, corner style, spacing, focus
highlight strength, motion speed, pane scope, tiled/tabbed presentation, and
removal confirmation. Window geometry, pane arrangements, minimized Shells, and
Workspace ordering are not yet restored across restarts. Boomux still owns
Shell persistence and resource identities.

Example preferences (omitted keys use defaults):

```toml
pane_gap = 8
motion_speed = "smooth" # instant, fast, smooth
pane_corner_style = "rounded" # rounded, square, mixed
pane_headings_visible = true
confirm_destructive_actions = true
```

Writes use a capacity-one background queue and atomic file replacement. Normal
application shutdown waits asynchronously for the final queued write, within
GPUI's shutdown deadline. Forced termination can still interrupt a pending save.
With several Desktop instances, the last completed save wins. Manual file edits
are read on the next launch. Invalid or oversized files are left intact: the app
uses defaults, disables saving for that session, and reports the problem in
Settings. Correct the file (or move it aside) and restart to resume saving.

Settings is one list grouped into Layout & workspaces, Appearance, Notifications &
sounds, Recovery & history, and Safety.
Changes save automatically. Text fields use Done or Enter to save, Escape to cancel,
and Ctrl+A to clear. Clipboard paste is supported.

Shared preferences are saved through Boomux's supported `config edit` transaction.
Controls show configured values from the active file, global configuration, and
workspace defaults; only edited fields are written. `BOOMUX_CONFIG` selects the active
override file when set. Boomux validates the candidate, checks ownership and
conflicts, and atomically commits it. Comments and unrelated fields are preserved.

Notifications is a master switch for desktop popups and sounds. Turning it off
disables both channels and dims dependent controls while preserving event choices
and sound names. Turning it back on enables popups; sounds can then be enabled
separately. Sound-name controls are unavailable while sounds are off.

Notification and recovery changes set one restart reminder
in the settings header. Individual settings have no restart labels and saving
does not interrupt editing. Closing settings offers **Restart now** or **Later**;
the header button also opens confirmation when ready. The reminder survives
Desktop restarts. Restart invokes the
local `boomux daemon restart` graceful handoff on a worker thread, preserving
running shells and commands. A failed restart retains the reminder and reports
the error. An external restart may leave a conservative reminder until a confirmed
restart from Settings. Other preferences do not request a daemon restart.
Remote Node configuration remains managed on those Nodes.

## Uninstall

Close Desktop first. Remove only the links owned by this installer:

```sh
release_data_dir=${XDG_DATA_HOME:-$HOME/.local/share}
release_install_dir=${BOOMUX_DESKTOP_INSTALL_DIR:-$release_data_dir/boomux-desktop}
release_bin_dir=${BOOMUX_DESKTOP_BIN_DIR:-$HOME/.local/bin}
release_install_dir=$(cd "$release_install_dir" && pwd -P) || exit 1
release_app_id=org.omarchy.boomux-desktop

remove_owned_link() {
    if [ "$(readlink "$1" 2>/dev/null)" = "$2" ]; then
        rm -- "$1"
    fi
}
remove_owned_link "$release_bin_dir/boomux-desktop" "$release_install_dir/current/bin/boomux-desktop"
remove_owned_link "$release_bin_dir/boomux" "$release_install_dir/current/bin/boomux"
remove_owned_link "$release_data_dir/applications/$release_app_id.desktop" "$release_install_dir/desktop-entry"
remove_owned_link "$release_data_dir/icons/hicolor/scalable/apps/$release_app_id.svg" "$release_install_dir/current/share/icons/hicolor/scalable/apps/$release_app_id.svg"
```

Use the same custom directory overrides used during installation. Existing
standalone Boomux executables are preserved. These steps do not stop a daemon,
close Shells, or delete Boomux configuration/history.

The release directory may also supply a running Boomux daemon. Keep it until no
Desktop or Boomux process uses its executables. If you will keep using Boomux,
install it separately and use Boomux's own lifecycle procedure to move off the
bundled daemon before deleting the directory. Once it is unused, remove the
bundle directory printed by `printf '%s\n' "$release_install_dir"`.

Desktop preferences are retained for reinstalling. To reset them, remove only
`${XDG_CONFIG_HOME:-$HOME/.config}/boomux-desktop/settings.toml`. Do not remove
Boomux's configuration or data directories to uninstall Desktop.
