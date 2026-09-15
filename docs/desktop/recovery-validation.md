# Desktop Recovery Review And Validation

Evidence from the attachment-recovery change on 2026-09-15. This records focused
local validation, not a replacement for CI or a live GUI reboot test.

## Review Scope And Corrections

Reviewed automatic restoration, attachment requests, observer and callback
lifetimes, final screen delivery, retry scheduling, diagnostic storage, and the
existing daemon recovery contracts.

- Generic detach frames do not prove takeover. Automatic retries never request
  takeover; an explicit controller conflict permits the separate user action.
- Running attachments preserve exact owner and run identity. Remote pending and
  manually restarted exited Shells omit obsolete expected-run IDs.
- Pending recovery checks the owner's previous run outcome. Normally finished
  or explicitly terminated commands do not automatically restart.
- Pane attempt generations reject late results. Connection-specific observer
  channels keep the previous terminal live until a replacement succeeds.
- Final output drains into a nonblocking screen snapshot. Recovery controls
  overlay retained output rather than discarding it.
- Closing/frozen windows and outgoing animation panes do not initiate recovery.
- Retry policy is centralized, capped at four concurrent attempts, and backs off
  to 30 seconds. Panes waiting for a deadline avoid Shell lookups.
- Diagnostics use a bounded queue, one idle-blocking worker, private rotating
  files, and nonblocking interprocess locking. Logs contain no terminal content.

## Critical Coverage

| Behavior | Focused coverage |
| --- | --- |
| Interrupted versus normally ended Shells | `automatic_attachment_recovery_starts_only_interrupted_or_new_shells` |
| Exact supported conversation after reboot | Native `cold_recovery_resumes_exact_codex_thread_with_run_scoped_hooks` (passed during implementation) |
| Stale run cannot replace a newer controller | Native `exact_run_attach_rejects_a_run_changed_after_validation_without_takeover` |
| Graceful daemon restart reconnects an attachment | Native `attachment_client_reconnects_across_daemon_restart` |
| Remote running, pending, and exited wire requests | `remote_attachment_and_reconnect_keep_exact_owner_and_run` |
| Stale results and old observer channels | `attachment_generation_rejects_late_reconnect_results`, `attachment_observers_follow_the_connection_not_just_the_shell` |
| Retry timing, capacity, and missing/stopped candidates | `recovery::tests` |
| Final output and nonblocking close under queue pressure | `detached_pane_retains_final_output_and_wakes_the_view`, `detachment_is_published_before_a_full_emulator_queue_drains`, `terminal_full_output_queue_does_not_block_pane_cancellation_or_transport_close` |
| Shared state reclaimed across 100 replacements | `attachment_replacement_releases_shared_state_and_closes_observers` |
| Diagnostic bounds, contention, and symlinks | `attachment_diagnostics::tests` |

Existing native coverage also includes exact Kiro/OpenCode recovery and handoff
rollback. Those unchanged scenarios remain CI coverage; they were not all rerun
locally. Window-closing and outgoing-animation guards were reviewed in source;
full GPUI interaction, end-to-end reboot behavior, and GUI memory/CPU remain live
validation gaps. The shared-state replacement test does not measure GPU memory,
process RSS, or native descriptor lifetimes.

## Scheduling Measurement

Run the standalone optimized harness, which imports the production planner:

```console
rustc -O --edition=2024 desktop/scripts/benchmark-recovery.rs -o /tmp/boomux-recovery-bench
/tmp/boomux-recovery-bench
```

Measured on AMD Ryzen 5 7600X, Linux 7.2.3, Rust 1.98.1. Both implementations run
in the same binary against identical inputs, with five timed batches and median
wall time per scheduling pass. The reference reproduces the previous linear
lookup-before-backoff order. Fixtures use 1, 100, 1,000, and 4,096 panes, a frozen
observation time, and reverse Shell order. The allocator reports transient and
retained requested bytes; this is not process RSS or a Desktop frame benchmark.

| 4,096-pane scenario | Previous | Current | Current allocations / peak transient / retained |
| --- | ---: | ---: | --- |
| Live panes, no recovery candidates | 2.52 µs | 1.13 µs | 0 / 0 B / 0 B |
| All disconnected panes waiting in backoff | 15.70 ms | 5.01 µs | 0 / 0 B / 0 B |
| Four due recoveries | 38.04 µs | 38.06 µs | 1 / 64 B / 0 B |

The improvement is concentrated in retry backoff. Ready-batch cost is effectively
unchanged in this measurement. Retry state occupies 48 bytes per pane and adds no
per-pane timer or task. More than four rejected lookups build an O(Shell-count)
temporary borrowed index, released before the next refresh. There is no persistent
lookup cache. Ordinary terminal output does not enqueue diagnostic records.

These timings do not establish whole-app idle CPU, peak/retained RSS, GPU behavior,
or cold-start latency. No installed Desktop or live daemon was replaced to gather
this evidence. Comprehensive selected checks remain PR CI's responsibility.
