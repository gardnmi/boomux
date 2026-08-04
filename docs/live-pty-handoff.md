# Live PTY Handoff

## Goal

Replace the Boomux daemon without terminating detached shell processes. A
metadata-only restart remains the crash fallback; handoff is an explicit,
acknowledged upgrade path.

## Invariants

- Exactly one daemon reads each PTY at a time.
- The old daemon remains authoritative until the replacement acknowledges every
  imported runtime.
- The listener socket and both daemon lock file descriptions keep their existing
  ownership across the transition.
- A failed replacement resumes the old daemon without killing shells.
- The first implementation rejects active controllers rather than disconnecting
  an attached terminal without consent.
- Received descriptors are close-on-exec, strictly typed by marker, and closed
  on every malformed transfer.

## Transfer Manifest

The private handoff channel will carry a versioned manifest followed by Unix
`SCM_RIGHTS` descriptors:

- Existing listener socket
- Runtime and state lock files
- One PTY master per running shell
- Shell ID, terminal profile, PID/session identity, and sanitized VT state

The PTY master is full duplex, so reader and writer duplicates do not need to be
transferred separately. The replacement cannot inherit Unix parenthood; process
monitoring and cleanup therefore need an imported-process representation, with
a Linux pidfd where available.

Bootstrap starts with the `BOOMUXH1` version header and has bounded read/write
deadlines. The receiver validates the listener path/type, forces nonblocking
mode, matches both lock-file inodes, and establishes exclusive flock ownership
before acknowledging readiness. Explicit abort closes every received duplicate
without affecting descriptors retained by the old daemon.

## Delivery Slices

1. [Complete] Implement and test strict single-descriptor `SCM_RIGHTS` transport.
2. [Complete] Replace erased PTY reader/writer ownership with a Boomux-owned Unix
   runtime and a pauseable, joinable reader task.
3. [Complete for pending shells] Add replacement-daemon bootstrap over a private
   socketpair and transfer the listener and lock descriptors with readiness
   acknowledgement.
   `boomux daemon restart` now uses prepare/finalize commit semantics, preserves
   the socket pathname, and rolls back to the old daemon before finalization.
4. Transfer detached shell runtimes and prove PID, input, output, resize, and
   later cleanup survive replacement.
5. Add cooperative attachment reconnection before allowing handoff with active
   controllers.

## First Acceptance Test

Start a detached shell, record its PID, replace the daemon, reconnect, and prove
the PID is unchanged. Input, output, resize, lock exclusivity, metadata mutation,
and a later destructive `daemon stop` must still work.
