# Performance And Memory

## Objective

Boomux Desktop should remain practical when AI-assisted workflows create far
more terminals and concurrent output than a traditional one-human multiplexer
session. Empty-state efficiency matters, but incremental pane cost and busy
terminal behavior matter more as the system scales.

This document defines measurement rules, not unearned benchmark claims. Numeric
budgets should be added only after repeatable baselines exist on representative
hardware.

## Ownership

Boomux is responsible for the server-side cost of durable Shells, PTYs,
attachments, persistence, output fan-out, and multiple clients. Boomux Desktop
is responsible for the client-side cost of open panes, Ghostty emulator states,
screen snapshots, decoded images, GPU resources, layout, and drawing.

Optimizing one side must not hide unbounded growth on the other. Record both
processes when diagnosing an end-to-end workload.

## Scale Dimensions

Measure these independently before combining them:

1. Idle application with zero or one attached pane.
2. Increasing open panes with quiescent shells.
3. Increasing durable Boomux Shells that are not open in the desktop client.
4. One busy text terminal at controlled byte rates.
5. Multiple busy text terminals.
6. One and multiple Kitty-graphics terminals at controlled dimensions and FPS.
7. Repeated pane open/close cycles to verify memory reclamation.
8. Resize and re-tile loops to expose allocation and texture churn.

## Metrics

- RSS and PSS for Boomux Desktop and the Boomux daemon
- private clean/dirty memory and swap
- incremental memory per attached idle pane
- retained memory after panes and images close
- CPU utilization at fixed output rates
- input-to-paint latency and visible frame stalls
- terminal bytes decoded per second
- image upload count and bytes per second
- thread and file-descriptor count
- optimized binary size as a secondary signal, not a memory proxy

GPU allocations are not fully represented by process RSS. Kitty graphics tests
must also track the number and byte size of live image generations.

## Current Guardrails

- The emulator command queue is bounded at 64 chunks per pane.
- Keyboard events share that bounded per-pane queue and are encoded by one
  reusable Ghostty encoder on the emulator worker, so mode changes and input
  remain ordered without a per-pane input buffer or encoder allocation.
- Ghostty's Kitty image storage is capped at 64 MiB per pane. This is a safety
  ceiling, not a desired steady-state footprint; a global or workload-sensitive
  budget should replace it if measurements show poor multi-pane scaling.
- Each pane uses a 4 MiB Ghostty primary-screen page-memory budget, allocated
  lazily. The pinned API calls this a line count, but its implementation uses
  bytes. Retained row count depends on width and cell contents. This is not a
  total pane RSS cap: Ghostty can exceed it for the visible screen and has
  additional bookkeeping overhead. Closing the pane releases its terminal.
- GPU image generations are retained only while referenced by the current screen
  and are dropped explicitly afterward.
- Overview refresh and Boomux requests stay off the render path.
- Terminal output is coalesced before publishing a new screen snapshot.
- Each pane has a capacity-one terminal-update mailbox. Idle panes do not poll,
  and output bursts wake GPUI once to consume the newest immutable snapshot.
- Screen delivery uses shared immutable snapshots instead of cloning every cell
  into the GPUI model. Common cell text remains inline, avoiding a heap
  allocation for typical one-codepoint cells.
- Each open pane retains at most one shaped-text paint cache for its current
  screen and selection. Layout-only animation frames reuse it; replacement and
  pane closure release the previous cache.
- Kitty pixel buffers and GPUI image objects are reused while their terminal
  generation remains unchanged, and image reconciliation runs only for a new
  screen snapshot.
- Absolute scrollbar seeks use one per-pane atomic latest-value mailbox and at
  most one queued wake-up marker, so pointer movement cannot build an
  unbounded or stale scroll backlog.
- A minimized tab owns only Boomux overview identity and presentation metadata;
  animated minimization retains pane state only for the bounded motion duration,
  then releases its emulator snapshots and GPU images before adding the tab.
- A Workspace switch retains outgoing pane state only for the selected bounded
  motion duration, then detaches it and releases its emulator and GPU resources.
- Omarchy theme changes are event-driven rather than polled. Watcher events
  coalesce in a capacity-one channel, theme files are capped at 64 KiB and read
  off the GPUI thread, UI color lookup is allocation-free, and each emulator
  worker owns at most one pending terminal palette update.

Any new queue, cache, history, retry, image store, or task must document its
bound and cleanup owner.

## Comparison Protocol

For before/after work, use the same machine, display configuration, release
profile, terminal dimensions, workload, warm-up period, and sample duration.
Report raw values and deltas. Run enough repetitions to identify noise, and do
not compare a warmed process with a cold one.

For comparisons with other multiplexers, describe configuration and plugins.
Separate server processes, client processes, shells, and child workloads so the
comparison does not attribute application memory to the wrong component.

## Initial Baseline Procedure

1. Build with `cargo build -p boomux-desktop --release --locked`.
2. Start a known Boomux Workspace and record the daemon report.
3. Launch Boomux Desktop and wait for output and memory to settle.
4. Run `desktop/scripts/memory-report.sh <desktop-pid>` and the same command for the
   Boomux daemon PID.
5. Repeat at 1, 10, 50, and 100 idle panes when automated fixtures support those
   counts.
6. Repeat with controlled text and graphics producers.
7. Close every added pane, wait for settling, and record reclaimed memory.

Automated workload fixtures and stable numeric regression thresholds are the
next performance-scaffolding milestone.
