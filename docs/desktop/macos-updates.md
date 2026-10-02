# Session-aware macOS app updates

This is an experimental, user-initiated whole-app transaction. It does not make
an ad-hoc preview production-safe. The first notarized Developer ID release must
be installed manually; existing ad-hoc apps remain ineligible. No credentials,
Apple agreement, signing access, or release publication is created by this code.

## Eligibility and preparation

A user-owned, non-symlink app in a user-owned, non-group/world-writable directory
is eligible only after strict complete-bundle signature verification, an explicit
Apple Developer ID Application certificate requirement, stable bundle identity,
and Gatekeeper assessment. A read-only/system-owned installation remains manual.
No `xcrun`, `stapler`, GNU `timeout`, Python or Xcode runtime is needed by the
updater; signing/build infrastructure separately requires the Apple tools.

The user chooses Download, then Restart now. Download reads the version-pinned
GitHub asset/checksum over bounded HTTPS. The checksum is transport integrity,
not trust: the staged complete app must have the same Team ID and bundle identity
as the running signed app, match the requested stable version and have matching
Apple Silicon CLI/Desktop/launcher/gateway executables. Prereleases/downgrades,
wrong-team signatures and tampered bundles are rejected.

ZIP extraction has a 256 MiB download cap, 300 MiB per-entry and 1 GiB aggregate
uncompressed cap, and 20,000-entry limit. A restricted central/local-header check
rejects traversal, links, duplicate paths, split/encrypted and ZIP64 forms. The ZIP
library's decoded metadata is checked again, and extraction explicitly creates
new regular files in a private directory; it never follows archive links or
invokes `ditto` on downloaded bytes. New versions use a same-filesystem atomic
no-replace rename into `Boomux-VERSION.app`. Existing versions are never moved,
replaced or deleted.

## Handoff and recovery

An owner-checked persistent flock inode excludes simultaneous updater instances.
The kernel releases it if an updater dies; do not unlink that lock file. A legacy
installer directory lock is treated as busy and is not silently removed. Existing
manual installers cannot overwrite a versioned destination during the no-replace
publish step.

Restart first flushes/freeze-protects layout, then launches a separate helper that
owns the transaction even if the old GUI quits. The helper revalidates current,
pending and candidate bundles, confirms which CLI owns the live daemon, invokes
`daemon restart --executable` with the exact new bundled CLI, and checks daemon
readiness. It never calls `daemon stop`, deletes Shells, or kills a daemon group.
A daemon owned by a different installation must first be migrated explicitly; the
updater will not invent a rollback executable.

The replacement GUI has its own process group. It must remain alive and write a
private one-time acknowledgement after GPUI creates its window. Only then is the
owner-private current-app selection atomically committed and the old GUI told to
quit, independently of whether its original Workspace still exists. A failed
handoff or window launch requests the same session-preserving restart back to the
retained old CLI. Rollback failure is reported explicitly. A diagnostic/marker
cleanup failure after commit does not pretend the update failed.

Old versioned Finder/Dock launch entries that contain this updater follow the
current-app selection only after verifying its newer matching-team signed target.
No Dock settings are changed. Entries from releases predating this updater cannot
redirect; open/pin the newly installed app once when adopting it. Missing, unsafe
or unverifiable selection state leaves the original app available. Ordinary
launch redirection never restarts the daemon. During source integration,
`bundle_update::dispatch()` must precede `macos_startup::dispatch()`.

Normal temporary staging directories are removed. A forced process kill can leave
an owner-private `.boomux-update-*` staging directory; it does not hold the flock
or prevent retry. Inspect and remove only known abandoned staging directories when
no update helper is running. Old versioned apps are retained for deliberate manual
rollback and are not automatically pruned. Disk use is bounded per attempt, not
across indefinitely repeated forced crashes.

## Required native acceptance

Portable tests cover extraction, metadata attacks, no-replace installation,
locks, bounded subprocess/descendant pipes, transaction ordering, failure/recovery
and private selection state. An Apple-target type-check is not native execution.
Before treating the updater as production-ready, test a real signed old/new pair:

- Preserve exact active ShellRun identities and process PIDs through update and rollback
- Two instances race Download/Restart; legacy installer races destination publication
- Close the old window, Cmd-Q and force-kill the old GUI during each helper stage
- Candidate GUI crash/timeout and daemon handoff refusal; rollback failure
- Missing/tampered/wrong-team/non-Developer-ID/unstapled candidate and bad selection target
- Successful update when pending cleanup fails; relaunch from an older Dock/Finder entry
- Read-only install, low disk space, offline stapled launch, app without Xcode CLT
- Repeated interrupted prepares, retained versions and abandoned staging inspection

Readiness proves the replacement daemon and initial GUI window responded. It does
not by itself prove successful terminal reattachment, VoiceOver, IME or long-term
health. The existing daemon lifecycle suite remains the authority for PID-preserving
handoff; real signed-bundle acceptance must verify that guarantee end to end.

Linux keeps its existing bundle updater and launcher. Mac-only dependency routing
and all selected Linux CI checks are required on the final combined source head.
