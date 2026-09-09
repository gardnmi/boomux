# Boomux macOS testing preview

This preview requires macOS 15 or newer. Use the aarch64 download for an Apple
Silicon Mac (M1 or newer), or x86_64 for an Intel Mac when available.

1. Extract the ZIP and drag Boomux.app to Applications.
2. Open Boomux.app. This build is ad-hoc signed, not Apple-notarized. If macOS
   blocks it, use System Settings → Privacy & Security → Open Anyway for this
   specific app after confirming you received it from the person testing Boomux.
3. Create a Workspace and Shell. Try ordinary commands, your editor, and agents.
4. Close a pane and reopen its Shell from the sidebar. Its process should survive.
5. Quit the app, reopen it, and reattach to the same Shell.

The CLI is inside the app:

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
Official macOS releases, notarization, and automatic app-bundle updates are not
part of this preview. Replace the app only after coordinating active sessions;
keep the previous download while testing a newer build.
