# Live PTY Handoff

> **Status: Current invariant reference and completed delivery record.** The
> invariants and transfer behavior are current; all delivery slices below are
> implemented.

## Goal

Replace the Boomux daemon without terminating running shell processes or ending
active terminal sessions. Metadata-only recovery remains the crash fallback;
handoff is an explicit, acknowledged upgrade path.

## Invariants

- Exactly one daemon reads each PTY at a time.
- The old daemon remains authoritative until the replacement acknowledges every
  imported runtime.
- The listener socket and both daemon lock file descriptions keep their existing
  ownership across the transition.
- A failed replacement resumes the old daemon without killing shells.
- Active controllers acknowledge a reconnect boundary before PTY transfer.
- Received descriptors are close-on-exec, strictly typed by marker, and closed
  on every malformed transfer.

## Transfer Manifest

The private handoff channel carries a versioned manifest followed by Unix
`SCM_RIGHTS` descriptors:

- Existing listener socket
- Runtime and state lock files
- One PTY master per running shell
- Shell and run IDs, terminal profile, PID/session identity, output revision,
  and sanitized VT state
- Exited-shell run identity, exit status, output revision, terminal profile, and
  sanitized final VT state without process descriptors
- The latest non-durable focused-terminal revision and workspace/shell/run
  identity, when it still refers to a transferred runtime

The PTY master is full duplex, so reader and writer duplicates do not need to be
transferred separately. The replacement cannot inherit Unix parenthood; process
monitoring and cleanup therefore need an imported-process representation, with
a Linux pidfd where available.

Bootstrap starts with the `BOOMUXH4` version header and has bounded read/write
deadlines. The receiver validates the listener path/type, forces nonblocking
mode, matches both lock-file inodes, and establishes exclusive flock ownership
before acknowledging readiness. Explicit abort closes every received duplicate
without affecting descriptors retained by the old daemon.

For each running shell, any active controller first acknowledges its reconnect
boundary and the old reader pauses before the manifest is captured. The
replacement validates the session leader and pidfd, reconstructs terminal state,
and waits at `PREPARED`; it does not begin reading the PTY until `FINALIZE`, so
rollback cannot consume output belonging to the old daemon.
PTY/session identity is cross-checked with `TIOCGSID`, terminal reconstructions
use separate bounded frames, and imported process/session cleanup uses pidfds to
avoid signaling a reused numeric PID.

Exited shells use a separate static transfer record and bounded reconstruction
frame. The replacement validates that the transferred identity and exit status
match the persisted completed run, then restores the exited lifecycle without
opening a PTY or starting a process.

Handoff version 4 also transfers the event stream UUID, high-water mark, and
bounded retained events. The replacement publishes `handoff_completed` before
resuming readers, so output revisions remain ordered after the ownership
boundary. The optional focused-terminal snapshot crosses the same graceful
handoff so dashboard following does not forget the last focused managed window;
ordinary cold recovery does not restore it.

## Accepted Remote Node Boundary

Protocol 35 implements the accepted remote attachment boundary. A remote PTY
descriptor, process handle, runtime, and reconstruction state remain
owned by the remote daemon and never enter a local handoff manifest. Remote
daemon replacement uses this existing descriptor-transfer protocol on the
remote machine, and its attachment reconnect frames pass unchanged through the
SSH bridge.

Local daemon replacement instead closes federation admission, drains admitted
Node requests and projection commits to known outcome boundaries, stops
synchronization workers, establishes a reconnect boundary with each local
attachment client, and quiesces SSH proxies without claiming remote PTYs. The
replacement opens fresh identity-verified bridges and starts workers only after
finalization. Rollback returns routing and workers to the old local daemon and
does not alter remote ownership. Simultaneous local and remote replacement must
preserve both prepare/finalize boundaries independently; neither side can infer
success from transport EOF. See [`remote-nodes.md`](remote-nodes.md).

## Delivery Slices

1. [Complete] Implement and test strict single-descriptor `SCM_RIGHTS` transport.
2. [Complete] Replace erased PTY reader/writer ownership with a Boomux-owned Unix
   runtime and a pauseable, joinable reader task.
3. [Complete] Add replacement-daemon bootstrap over a private
   socketpair and transfer the listener and lock descriptors with readiness
   acknowledgement.
   `boomux daemon restart` now uses prepare/finalize commit semantics, preserves
   the socket pathname, and rolls back to the old daemon before finalization.
4. [Complete] Transfer detached shell runtimes and prove PID, input, output,
   resize, repeated replacement, rollback, and later cleanup survive.
5. [Complete] Add cooperative attachment reconnection with ordered
   `Reconnect`/`ReconnectAck` framing. Input sent before the ACK is processed by
   the old daemon; later terminal input remains queued while the client retries
   the replacement in raw mode.
6. [Complete] Transfer exited-run metadata and bounded final terminal state as
   static lifecycle records alongside any live runtimes.

## First Acceptance Test

Start a detached shell, record its PID, replace the daemon, reconnect, and prove
the PID is unchanged. Input, output, resize, lock exclusivity, metadata mutation,
and a later destructive `daemon stop` must still work.
