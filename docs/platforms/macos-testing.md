# Boomux macOS testing preview

macOS support is experimental and requires Apple Silicon (M1 or newer) and
macOS 15 or newer. Intel builds are not published. The app is ad-hoc signed,
not Apple-notarized.

## Install from a release

Use the same command as Linux; it automatically selects the macOS bundle:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh -s -- --desktop
```

It verifies the matching release ZIP, signature, architecture, and version, then
installs `~/Applications/Boomux-<version>.app`. Open that app in Finder. If macOS
blocks it, use System Settings → Privacy & Security → Open Anyway after checking
that you downloaded the official Boomux release. The installer does not disable
Gatekeeper or remove quarantine attributes.

For a newer version, quit Desktop and run the command again. Existing versions
are preserved. The daemon may still be running from an older app: use the new
app's bundled CLI with `daemon restart` before removing the older app. Do not
use `daemon stop` unless you intend to terminate every managed Shell.

## Manual ZIP testing

Regular releases include `boomux-desktop-aarch64-apple-darwin.zip`. Development
prereleases also offer commit-named preview ZIPs. Both contain the same app layout.

1. Extract the ZIP and drag Boomux.app to Applications.
2. Open Boomux.app. This build is ad-hoc signed, not Apple-notarized. If macOS
   blocks it, use System Settings → Privacy & Security → Open Anyway for this
   specific app after confirming its release source and checksum.
3. Create a Workspace and Shell. Try ordinary commands, your editor, and agents.
4. Close a pane and reopen its Shell from the sidebar. Its process should survive.
5. Quit the app, reopen it, and reattach to the same Shell.

The CLI is inside the app. For a manual install into `/Applications`:

```sh
/Applications/Boomux.app/Contents/MacOS/boomux --version
/Applications/Boomux.app/Contents/MacOS/boomux daemon status
/Applications/Boomux.app/Contents/MacOS/boomux daemon restart
```

Command+C/V copy and paste; Command+W detaches the pane; Command+Enter creates a
Shell; Command+Q quits Desktop. Control keys remain available to terminal apps
except the existing documented Boomux shortcuts. Control+Space opens Layout mode.

Please test non-US keyboard input, Option combinations, IME if used, selection,
clipboard, Retina scaling, fullscreen, sleep/wake, and reconnecting after a daemon
restart. Share the source commit in build.json, macOS version, chip, steps, and
any error text. Avoid sharing terminal secrets or credentials.

The daemon intentionally keeps running after the app quits. `boomux daemon stop`
terminates all managed Shell processes; use it only when you intend to end them.

This preview does not install a login service or replace a package-managed CLI.
The experimental app is included with regular releases. Notarization and
automatic app-bundle updates are not provided. Replace the app only after coordinating active sessions;
keep the previous download while testing a newer build.
