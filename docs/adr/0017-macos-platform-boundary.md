# 0017: Isolate host operations for the macOS preview

Status: accepted for the feature/macos preview; native validation evidence is
tracked in ../platforms/macos.md before support is advertised.

## Decision

Keep Node, Workspace, ShellRun, Agent, persistence, and event authority in the
existing backend. Add `src/platform/` for host-local process, descriptor,
filesystem, and runtime-path operations. Keep Linux eventfd/pidfd behavior.

Darwin uses a local kqueue process monitor and a private, unlinked, read-only
identity descriptor containing the process PID and kernel unique identifier.
The receiver validates that identity and creates its own monitor. Signaling
revalidates the unique identifier and invokes Apple's audit-token signal API,
which checks the exec generation in the kernel. There is no PID-only fallback
when that operation is unavailable. This API is resolved at runtime and tested
on the preview's native CI baseline; its availability is a release gate.

Darwin cannot execute a Mach-O through `/dev/fd/N`. Replacement preparation
therefore creates a private hard link and verifies its inode against the open
executable before quiescing. It executes the alias while retaining the original
absolute argv[0] for subsequent restart and integration paths. A cross-filesystem
pin fails before transfer. The replacement removes its alias on exit; cold
startup under the daemon lock reclaims stale aliases.

Darwin session-leader exit can wait for queued terminal output to drain even
following SIGKILL. During destructive shutdown, after signaling the session,
Boomux flushes the unread PTY output tail before waiting for the child. The
reader remains paused to preserve the lifecycle transaction's lock ordering.
Live handoff and failed-preparation rollback never flush terminal output.

macOS uses a distinct private `BOOMUXM1` handoff header. Linux retains H8. The
public protocol and persisted registry schema do not change simply because a
new host is supported. Process identity descriptors contain no environment.

The default macOS runtime root is `/tmp/boomux-<uid>`, validated as an owner-only
non-symlink directory before daemon startup. The socket remains under its
`boomux/` subdirectory. This keeps Unix socket names short and agrees across
Finder, terminal, and SSH launches. Boomux/XDG overrides retain precedence;
configuration and durable state retain the existing paths.

Terminal.app receives only a fixed bridge invocation in a temporary `.command`
file. The exact command argv, cwd, and ephemeral startup environment travel over
an owner-verified, bounded Unix socket request. Command arguments are never
converted into shell source and environments are never written to disk.

Darwin lacks `PR_SET_PDEATHSIG`. A Kiro launch therefore uses one small helper
process that inherits the terminal descriptors and exact command environment.
It watches the originating launcher and the actual child with one kqueue,
terminates and reaps its unreaped child if the launcher dies, and otherwise
returns the child's status. This works independently of the daemon. The helper
has no polling loop, PTY, terminal model, or output forwarding queue; its cost is
one process and one kqueue per Kiro launch, not per Shell or attachment.

Desktop keeps GPUI and Ghostty, uses a macOS filesystem watcher and system Menlo
font, and bundles a matching CLI. The preview requires macOS 15+, starts on
Apple Silicon, and uses ad-hoc signing. Notarization, official release assets,
and app-bundle self-update remain separate production-distribution work.

## Consequences

Native lifecycle tests are required: a cross-compile cannot prove kqueue,
process identity, PTY ownership, or descriptor-transfer semantics. Linux tests
continue to exercise the Linux implementation. Do not claim compatibility with
untested Intel hardware or older macOS releases.
