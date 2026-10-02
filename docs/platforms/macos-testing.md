# Boomux macOS testing preview

> [!WARNING]
> **macOS is a largely untested experimental preview.** It has limited automated
> CI and smoke-test coverage, with very little real-world testing. Everyday use,
> hardware compatibility, and session reliability are not established. Expect
> bugs; use it for evaluation only, not important work. Inclusion in a regular
> release does not make the macOS build stable or production-ready.

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
are preserved. The daemon may still be running from an older app. Select the new
app's bundled CLI explicitly when restarting, replacing `<version>` below with
the installed version:

```sh
new_cli="$HOME/Applications/Boomux-<version>.app/Contents/MacOS/boomux"
"$new_cli" daemon restart --executable "$new_cli"
"$new_cli" daemon status --json
```

A plain `daemon restart` reuses the running daemon's executable, even when the
command comes from a newer CLI. Keep the older app until the explicit restart
succeeds and the status result's `data.executable` identifies the new CLI. Do not use `daemon stop` unless
you intend to terminate every managed Shell.

## Finder, Dock, and shell startup

The bundled launcher prepares the environment before opening Desktop:

- An explicitly supplied `SHELL` is retained. If it is empty or missing, the
  account's configured shell is used; a failed account lookup falls back to
  `/bin/zsh` with a visible warning. An explicit `HOME` and Boomux/XDG overrides
  are retained
- The selected shell runs once as an interactive login shell (`-ilc`), without
  a terminal, to discover its exported `PATH`. Only `PATH` is imported. Shell
  aliases, functions, secrets, and other variables are not imported or saved
- The matching bundled CLI directory is always first. A custom launch `PATH`
  retains precedence over discovered entries. Finder's standard system-only
  `PATH` is treated as a fallback so login-profile tools and version managers
  can be found. Homebrew and system directories are final fallbacks. Empty and
  relative path entries are ignored
- Account lookup has a two-second deadline; login-shell discovery has a
  three-second deadline and a 64 KiB output limit. Noninteractive prompts,
  failures, or excessive startup output fall back to the launch PATH rather
  than stopping the app. Shell output is discarded and never logged or saved
- Daemon startup has a ten-second deadline. A failed or missing bundled CLI
  still opens Desktop with a dismissible recovery message. Desktop retries in
  the background, using the exact bundled CLI rather than another installation

If a warning mentions shell startup, fix the shell's startup files and quit and
reopen Desktop. You can launch from Terminal with explicit `SHELL`, `HOME`, and
`PATH` values for diagnosis. A compatible daemon that is already running is
reused; it keeps its existing environment. Applying a new environment to that
owner requires an explicit daemon restart from the desired environment, as
shown above. Existing managed ShellRuns are never restarted just to discover
login tools. Newly attached Shells receive Desktop's resolved startup environment;
their own interactive startup files can subsequently change it.

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
/Applications/Boomux.app/Contents/MacOS/boomux daemon restart \
  --executable /Applications/Boomux.app/Contents/MacOS/boomux
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
