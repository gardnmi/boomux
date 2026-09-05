# Benchmarking Boomux

Boomux uses separate mechanisms for deterministic regression gates and elapsed-time
trend analysis. Benchmarks complement semantic tests; they do not replace protocol,
persistence, lifecycle, or native compatibility coverage.

## Benchmark Tiers

| Tier | Tool | Purpose | Merge policy |
| --- | --- | --- | --- |
| Bounded-work fixtures | Rust tests | Stable cardinality, digest, and limit checks | Required |
| CPU instruction counts | Gungraun and Callgrind | Deterministic algorithmic regressions | Ten-percent soft limit in same-run base/head comparisons |
| Wall-clock trends | Criterion | Latency and throughput investigation | Report-only on shared runners |
| Local kernel | Future local tools | PTY, process, filesystem, and handoff behavior | Report-only |

GitHub-hosted elapsed time is too noisy to reject a change reliably. A controlled
runner is required before Criterion results can become a hard gate.

## Prerequisites

The smoke suite needs the normal stable Rust toolchain. Gungraun execution also
requires Valgrind and the runner version paired with the locked library:

```console
sudo pacman -S valgrind
cargo install gungraun-runner --version 0.19.4 --locked
```

Benchmark dependencies are locked and covered by the repository dependency policy.
The bench profile retains debug information so Callgrind can identify benchmarked
functions.

## Smoke Validation

Run the deterministic fixtures and compile every target:

```console
cargo test --test benchmark_harness --features benchmark-internals --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo bench --bench core_cpu --bench wire --features benchmark-internals --locked -- --test
```

Smoke mode executes each Criterion case once. It verifies fixtures and benchmark
wiring, but does not produce meaningful timing measurements.

## Wall-Clock Measurements

Run the complete Criterion suites with optimized code:

```console
cargo bench --bench core_cpu --features benchmark-internals --locked
cargo bench --bench wire --locked
```

Criterion writes reports under `target/criterion/`. Compare changes on the same
machine, with the same toolchain and minimal competing load. Save a baseline before
changing code, then compare the candidate against that named baseline:

```console
cargo bench --bench core_cpu --features benchmark-internals --locked -- --save-baseline main
cargo bench --bench core_cpu --features benchmark-internals --locked -- --baseline main
```

Treat small differences as investigation signals, not proof. Confirm important
results with repeated runs and inspect the affected benchmark's throughput and
input cardinality.

## Instruction Counts

Run deterministic CPU cases through Callgrind:

```console
cargo bench --bench core_instructions --features benchmark-internals --locked
```

The suite monitors Callgrind's simulated executed-instruction count with a ten-percent
soft regression limit.
Gungraun exits unsuccessfully when a named baseline exists and a limit is exceeded.
Keep `target/gungraun/` when comparing local revisions. The scheduled workflow saves
a baseline from the merge base (or the previous `main` commit) and measures the current
revision against it in the same job; its first run is report-only when the comparison
commit predates the benchmark. Instruction count is not wall-clock latency and does
not model waiting, kernel scheduling, or I/O.

## Current Workloads

`core_cpu` covers:

- retained daemon event pages at the head, middle, and tail of the 8,192-event bound;
- 256/257-event reduced projection cuts;
- blocked Node/focus invalidation coalescing;
- durable and host-catalog Session projection, including shared-directory output
  amplification;
- terminal ingestion, structured preview, and reconstruction with full scrollback.

`wire` covers 16-KiB attachment frames and one-MiB JSON control messages.

`core_instructions` gates a small stable subset: both sides of the 256/257 projection
cutoff, shared catalog projection, and structured terminal preview.

## Adding A Benchmark

1. Start in the module that owns the behavior and read its semantic tests and contract.
2. Expose only an opaque operation through the `benchmark-internals` feature.
3. Use fixed IDs, timestamps, absolute synthetic paths, ordering, and payloads.
4. Construct fixtures outside the measured region unless construction is the operation.
5. Validate an exact cardinality or repeatable summary before registering measurements.
6. Add or identify a semantic test that protects the benchmarked result.
7. Include common and boundary-sized inputs; avoid a single convenient fixture.
8. Document intentional workload or benchmark-name changes in the pull request.

Do not duplicate production modules with path inclusion, expose ordinary public APIs
only for benchmarking, or add benchmark-only branches to production hot paths.

## Safety And Privacy

Benchmarks and uploaded artifacts must use synthetic data. Never include real terminal
contents, user paths, environment variables, configuration, credentials, Node routes,
or external Session IDs.

Hard-gated CPU benchmarks must not use network access, subprocesses, sleep, PTYs,
filesystem scans, or daemon lifecycle operations. Those measurements belong in a
separate report-only local-kernel suite with isolated XDG state.

Benchmark reports are CI artifacts, not product release assets.
