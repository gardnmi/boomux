# macOS preview implementation

Status: in development; no supported macOS release is advertised yet.

The port targets Apple Silicon first and retains Linux behavior. The initial
artifact is a testing preview, not an official signed/notarized release.

## Delivery sequence

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

Native build and runtime results will be recorded here as they become available.
Linux checks remain focused on the affected behavior. macOS release builds are
required to produce the explicitly requested preview artifact.
