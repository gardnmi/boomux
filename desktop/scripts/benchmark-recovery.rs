//! Scheduling microbenchmark. Run with rustc -O --edition=2024; no GUI or daemon.
#![allow(dead_code)]
#[path = "../src/recovery.rs"]
mod recovery;
use recovery::{Candidate, Retry, Shell};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            ALLOCS.fetch_add(1, Relaxed);
            let bytes = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(bytes, Relaxed);
        }
        result
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

// Reference for the pre-cleanup lookup order: a full Shell scan happened before
// checking the retry deadline, even when every pane was already in backoff.
fn previous(ids: &[String], order: &[usize], waiting: bool, active: bool) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    for &pane in order {
        if black_box(active) {
            continue;
        }
        let Some(index) = ids.iter().position(|id| id == &ids[pane]) else {
            continue;
        };
        if waiting {
            continue;
        }
        result.push((pane, index));
        if result.len() == 4 {
            break;
        }
    }
    result
}

fn measure(
    mut work: impl FnMut() -> Vec<(usize, usize)>,
    iterations: usize,
) -> (u128, usize, usize, usize) {
    drop(black_box(work()));
    let start_bytes = LIVE.load(Relaxed);
    PEAK.store(start_bytes, Relaxed);
    let start_allocs = ALLOCS.load(Relaxed);
    let mut samples = [0; 5];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iterations {
            drop(black_box(work()));
        }
        *sample = start.elapsed().as_nanos() / iterations as u128;
    }
    samples.sort();
    (
        samples[2],
        (ALLOCS.load(Relaxed) - start_allocs) / (iterations * 5),
        PEAK.load(Relaxed).saturating_sub(start_bytes),
        LIVE.load(Relaxed).saturating_sub(start_bytes),
    )
}

fn main() {
    println!(
        "panes,scenario,implementation,median_ns_per_pass,allocations_per_pass,peak_transient_bytes,retained_bytes"
    );
    for count in [1, 100, 1000, 4096] {
        let ids: Vec<_> = (0..count).map(|i| format!("shell-{i:08}")).collect();
        let order: Vec<_> = (0..count).rev().collect();
        for (scenario, waiting, active) in [
            ("live", false, true),
            ("backoff", true, false),
            ("due", false, false),
        ] {
            let now = Instant::now();
            let mut retry = Retry::default();
            retry.begin(Some("run"));
            if waiting {
                retry.failed(false, now);
            }
            // A frozen observation time makes the backoff fixture deterministic.
            let current = || {
                recovery::plan(
                    order
                        .iter()
                        .filter(|_| !black_box(active))
                        .map(|pane| Candidate {
                            pane: *pane,
                            shell: &ids[*pane],
                            retry: &retry,
                        }),
                    ids.iter().map(|id| Shell {
                        id,
                        run: Some("run"),
                        eligible: true,
                    }),
                    4,
                    black_box(now),
                )
            };
            assert_eq!(previous(&ids, &order, waiting, active), current());
            let iterations = (20_000 / count).clamp(3, 20_000);
            for (implementation, values) in [
                (
                    "previous",
                    measure(
                        || {
                            previous(
                                black_box(&ids),
                                black_box(&order),
                                black_box(waiting),
                                black_box(active),
                            )
                        },
                        iterations,
                    ),
                ),
                ("current", measure(current, iterations)),
            ] {
                println!(
                    "{count},{scenario},{implementation},{},{},{},{}",
                    values.0, values.1, values.2, values.3
                );
            }
        }
    }
    eprintln!(
        "Retry state: {} bytes per pane; no per-pane task or timer",
        std::mem::size_of::<Retry>()
    );
}
