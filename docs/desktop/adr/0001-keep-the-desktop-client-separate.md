# ADR 0001: Keep Boomux Desktop Separate From Boomux

- Status: Superseded by [Boomux ADR 0015](../../adr/0015-consolidate-native-desktop-workspace.md)
- Date: 2026-09-02

## Context

Boomux is a headless terminal and Workspace backend. Boomux Desktop adds GPUI,
Wayland/X11, GPU rendering, Ghostty terminal decoding, images, animation, and
Omarchy-specific presentation. Combining them would make the core daemon's
build, dependency graph, platform support, and release lifecycle depend on a
graphical client.

## Decision

Boomux Desktop is maintained as a separate repository and consumes Boomux as a
versioned client dependency. Generic daemon, protocol, persistence, attachment,
and PTY performance work remains in Boomux. Desktop layout, rendering, input,
and per-pane resource management remains here.

The existing `omarchy-boomux` Quickshell plugin may coexist with the native
client until migration is justified by feature and performance evidence.

## Consequences

- Boomux remains usable headlessly and keeps its smaller dependency surface.
- Boomux Desktop can iterate on GPUI and Ghostty versions independently.
- Cross-repository protocol changes require explicit compatibility updates.
- An exact Boomux Git revision is pinned until a focused published client crate
  exists.
- End-to-end performance work must measure both processes rather than treating
  either one as the entire system.
