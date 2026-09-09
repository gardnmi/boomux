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

### Local Shell startup diagnostic (2026-09-08)

An opt-in native diagnostic separates create, attach, run lookup, first usable
output, and exact-run reopening without using live user Workspaces:

```console
cargo test --test native_backend shell_startup_phase_timings --locked -- --ignored --nocapture --test-threads=1
BOOMUX_TIMING_STATE_ROOT=/absolute/path/on/test/filesystem cargo test --test native_backend shell_startup_phase_timings --locked -- --ignored --nocapture --test-threads=1
```

The second form creates and removes a uniquely named state directory under the
specified root; runtime sockets and harness settings remain isolated. On the
development host, the same debug test binary and three plain `/bin/sh` Shells
took 75–76 ms from creation through usable output with the default temporary
state location, versus 1.32–1.48 s with state under the project's Btrfs `target`.
On Btrfs, creation took 515–753 ms and first attachment 704–803 ms; exact-run
reopening took 25 ms in both runs. These are backend diagnostic measurements,
not Desktop end-to-end latency, remote latency, or a before/after speedup claim.
They identify the two durable commits as a substantial remaining startup cost.
Do not remove persistence barriers or move durable state to volatile storage as
a latency workaround.

The diagnostic now alternates the old create/attach path with protocol-54
create-and-start, three samples each, six sequential 24×80 plain `/bin/sh`
Shells in one isolated Workspace. An optimized same-binary comparison on this
host's Btrfs state directory used:

```console
BOOMUX_TIMING_STATE_ROOT=/home/gardnmi/Projects/boomux/target cargo test --release --test native_backend shell_startup_phase_timings --locked -- --ignored --nocapture --test-threads=1
```

Old-path usable-output times were 1.631, 1.494, and 1.325 seconds; combined-path
times were 0.780, 0.479, and 0.697 seconds (median reduction about 53%). First
attachment after combined creation took 7–25 ms instead of 600–904 ms. Exact-run
reopening remained about 25 ms. The test took 10.84 seconds including setup and
cleanup. It includes an extra run lookup on both paths; Desktop uses the returned
run directly on the combined path. Disk timings vary with filesystem activity.
These are backend timings, not an end-to-end Desktop or remote benchmark.
Steady-state CPU, memory, and frame latency were not measured in this diagnostic;
the change uses the existing process/reader lifecycle and adds no idle polling,
cache, queue, or per-Shell runtime beyond the existing one.

#### Follow-up: connection wakeups and directory metadata

The daemon now waits for listener readability instead of sleeping 25 ms after
an empty accept. The idle timeout and maintenance checks remain 25 ms; incoming
connections wake the same thread immediately. State-directory validation also
avoids reapplying `0700` when it is already correct, while still checking ownership,
rejecting symlinks, and repairing incorrect modes on every call.

Using the same optimized diagnostic on the same host, exact-run reopen fell from
about 25 ms to 0.088–0.129 ms. Combined creation through usable output with disk
state was 0.603, 0.598, and 0.400 seconds. A fresh pre-change comparison was
0.401, 0.800, and 0.201 seconds, so disk variability prevents claiming an overall
creation speedup from this follow-up. With default temporary memory-backed state,
the new optimized combined path took 1.87–2.04 ms. That measures the backend's
non-disk floor, not a recommended volatile-state configuration or Desktop latency.

An opt-in 8 KiB storage diagnostic separates writing, file synchronization,
rename/directory synchronization, and append/data synchronization:

```console
BOOMUX_TIMING_STATE_ROOT=/absolute/path/on/test/filesystem cargo test --lib persistence_barrier_timings --locked -- --ignored --nocapture --test-threads=1
```

On this host, buffered writes took under 0.2 ms but individual synchronization
barriers took up to about 402 ms. Even the synthetic append-only case varied
from 46 to 402 ms. This is not a production journal implementation or a promise
that switching formats would eliminate disk waits. The follow-up leaves state
formats, fsync barriers, event ordering, and recovery guarantees unchanged.
It adds no worker, queue, or cache; idle CPU and retained memory were not measured.

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
- Settings replaces the covered sidebar body rather than rebuilding its hidden
  Workspace, Shell, Agent, and update-notice elements on every scroll
  render. Closing Settings reads the current overview directly, with no stale
  cached projection. Collapsed Workspaces do not construct hidden Shell rows.
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

## Settings Row Preparation Fixture

`settings_scroll_eliminates_hidden_sidebar_row_preparation` compares the old
unconditional sidebar clone/row preparation with the Settings visibility gate,
using the same synthetic overview and debug test binary. On 2026-09-07, 60
iterations over 100 Workspaces, 2,000 Shells, and 2,000 Agents visited 246,000
rows before and zero afterward (322.961 ms versus 0.023 ms on this machine).
These are row-preparation timings, not GUI frame times or a GPU/RSS measurement.
The deterministic regression asserts eliminated work and fresh data on closing
Settings, not a timing threshold.

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

## Installation Worker Lifecycle

At most one download or restart transaction runs per Desktop window, additionally
serialized across processes by the installation lock. Each subprocess output
stream retains at most 64 KiB; oversized output kills the process group. Download
execution is capped at 30 minutes, daemon handoff at 45 seconds, and replacement
window startup at 30 seconds. Downloads cap the compressed bundle at 512 MiB and
checksum at 4 KiB. The installer owns retained version directories, which remain
available for rollback and are removed with the documented uninstall procedure.
No installation work runs in rendering or once per terminal pane.
