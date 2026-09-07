# Roadmap

This is non-authoritative future intent. Current behavior is defined by source,
tests, `CONTEXT.md`, architecture documentation, and accepted ADRs.

## Repository Readiness

- [x] Publish the unified Linux release bundle with Wayland/X11 smoke coverage
  (v1.10.0). Continue compatibility validation using `releases.md`.
- Replace the temporary application icon with final artwork and refine Omarchy integration.
- Consider focused client/protocol crates within the workspace when justified
  by dependency or build measurements.

## Performance

- Add deterministic terminal workloads for 1, 10, 50, and 100 idle panes.
- Add controlled text throughput and Kitty graphics frame-rate fixtures.
- Establish same-machine RSS/PSS and CPU baselines before selecting numeric
  regression budgets.
- Measure retained memory after repeated pane and image open/close cycles.
- Introduce a global graphics budget if per-pane Kitty ceilings scale poorly.
- Track damage-driven painting and avoid rebuilding unchanged terminal cells.

## Product

- [x] Add compact native Node status and entry points for guided SSH setup and
  reauthentication, reusing Boomux's terminal flows.
- Present remote Shells and Agents with exact Node-qualified identities and
  coordinated Workspace membership in the native canvas.
- Preserve remote panes across connection loss with exact-run recovery and
  actionable authentication/identity failures.
- Extend persisted Desktop preferences to window geometry and pane arrangement restoration.
- Reach feature parity with the essential `omarchy-boomux` Workspace and Agent
  workflows before considering replacement of that client.
- Add multi-Workspace and multi-monitor presentation without duplicating Boomux
  ownership or Hyprland compositor authority.
