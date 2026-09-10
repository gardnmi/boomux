# macOS preview implementation

Status: Apple Silicon testing preview built and validated; no official macOS
release is advertised yet.

The port targets Apple Silicon first and retains Linux behavior. The initial
artifact is a testing preview, not an official signed/notarized release.

## Completed preview scope

1. Establish a native macOS CI baseline and document platform boundaries.
2. Extract OS-specific process, wakeup, filesystem, peer-identity, and runtime
   path operations without changing resource authority or Linux behavior.
3. Validate PTY creation, process monitoring, session cleanup, attached and
   detached handoff, failed replacement rollback, and crash recovery.
4. Port CLI setup, integration execution, remote bootstrap, and launch services.
5. Build Desktop with macOS input, clipboard, native launch, and default theme;
   package a matching CLI and a test guide for a MacBook tester.

## Invariants

A Node remains authoritative for its own resources. Detaching or quitting Desktop
never closes a Shell. Handoff must retain one PTY reader, preserve the Shell PID,
and roll back before authority transfers if preparation fails. Process exit does
not establish permanent Agent completion. Runtime paths must be private and
consistent between GUI, terminal, and SSH invocations. Configuration and state
continue to honor Boomux and XDG overrides. Platform work does not by itself
justify a public protocol or persisted-state version bump.

## Validation evidence

The native runner is Apple Silicon on macOS 15.7.9. Backend compilation,
SCM_RIGHTS descriptor transfer, native process identity and signaling, executable
pinning, detached and attached handoff, failed replacement rollback, bounded
connection admission, and OpenCode/Kiro lifecycle fixtures have passed there.
Cold recovery also passed after fixing Darwin's terminal-output drain during
session-leader exit. All ten selected native lifecycle scenarios passed at
`caeae0d` in [native CI run 34421149607](https://github.com/gardnmi/boomux/actions/runs/34421149607).
The final preview was built from `88ab1fa6500f23b4115194bf1ea6f7176d5ceda9`.
[CI run 34422165909](https://github.com/gardnmi/boomux/actions/runs/34422165909)
passed both native and packaging jobs. Packaging checked executable versions,
ARM64 architecture, system-only dynamic libraries, ad-hoc code signatures, and
nonempty native glyph pixels for system UI text and Menlo. The packaged app
opened a native window, received PTY output, preserved its ShellRun across app
close/reopen, and reconnected after live daemon restart. Visual inspection of
the screenshot confirmed readable UI labels and terminal output.

The 1024×768 CI display clips part of the initial 1180-point-wide window; small
display window fitting remains a preview issue. Human keyboard, clipboard,
Retina, notification, and sleep/wake checks remain in the testing guide.
Native harness fixtures do not establish compatibility with actual macOS
harness versions or Intel Macs.

Artifact: `boomux-macos-preview-aarch64-88ab1fa6.zip` (11,470,562 bytes), containing
`Boomux.app`, its matching CLI, licenses, `build.json`, and `READ ME FIRST.md`.
The downloaded ZIP passed an integrity check and its SHA-256 matched CI:

```text
8aa13ea0d8b5735d2f42833809bc6654abe03794a1b770bb240666521ff7a458
```

Focused Linux checks passed for backend and Desktop compilation, descriptor
transfer, private handoff headers, detached handoff, connection admission, and
safe filesystem purge. Complete root/Desktop suites were not run locally.

## Preview boundaries

- The app bundles its matching CLI; neither Homebrew nor a separate Boomux
  installation is required to launch it. The GUI uses GPUI/Metal and Ghostty.
- Command shortcuts, Menlo, clipboard, Terminal.app launch, and macOS
  notifications have native implementations. Input methods, Option keys,
  Retina/fullscreen, notification permissions, sleep/wake, and human interaction
  remain in the [MacBook testing guide](macos-testing.md).
- SSH runtime discovery recognizes Darwin and uses the same private runtime
  root as local startup. Official macOS auto-download assets do not exist yet;
  manually install the matching CLI to test a remote macOS Node. Cross-host SSH
  sessions and live third-party harness versions require separate validation.
- Ad-hoc signing supports this testing handoff. Notarization, official release
  publishing, login services, Intel builds, and app-bundle automatic updates
  remain production-distribution work.
- Executable pinning currently requires the installed CLI and runtime directory
  to share a filesystem. Unsupported pinning or identity APIs fail before live
  ownership transfer, without a PID-only fallback.
- The Kiro parent-death guard adds one event-driven helper process and kqueue
  per launch. Its native fixture measured about 2.8 MiB RSS; this is a process
  snapshot, not a steady-state or scale performance benchmark.
