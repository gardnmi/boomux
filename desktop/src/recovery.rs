//! Pure recovery scheduling: no transport, filesystem work, or per-pane timers.
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const MAX_CONCURRENT: usize = 4;

#[derive(Default, Debug)]
pub struct Retry {
    run: Option<String>,
    deadline: Option<Instant>,
    failures: u8,
    stopped: bool,
}

impl Retry {
    pub fn begin(&mut self, run: Option<&str>) {
        if self.run.as_deref() != run {
            self.run = run.map(str::to_owned);
            self.failures = 0;
            self.stopped = false;
        }
        self.deadline = None;
    }

    pub fn succeeded(&mut self, run: Option<&str>) {
        self.begin(run);
        self.failures = 0;
        self.stopped = false;
    }

    pub fn failed(&mut self, stopped: bool, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.stopped = stopped;
        self.deadline = Some(now + Duration::from_secs((1u64 << self.failures.min(5)).min(30)));
    }

    pub fn needs_attention(&self) -> bool {
        self.stopped || self.failures >= 3
    }

    fn waiting(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now < deadline)
    }
}

pub struct Candidate<'a> {
    pub pane: usize,
    pub shell: &'a str,
    pub retry: &'a Retry,
}

#[derive(Clone, Copy)]
pub struct Shell<'a> {
    pub id: &'a str,
    pub run: Option<&'a str>,
    pub eligible: bool,
}

/// Return pane / Shell-index pairs. The caller filters live, attaching, offline,
/// and outgoing panes. Only due retries build the temporary borrowed index.
pub fn plan<'a>(
    candidates: impl Iterator<Item = Candidate<'a>>,
    shells: impl Iterator<Item = Shell<'a>> + Clone,
    available: usize,
    now: Instant,
) -> Vec<(usize, usize)> {
    let limit = available.min(MAX_CONCURRENT);
    let mut result = Vec::new();
    if limit == 0 {
        return result;
    }
    let mut index = None;
    let mut direct_lookups = 0;
    for candidate in candidates {
        if candidate.retry.waiting(now) {
            continue;
        }
        // Common case: at most four successful lookups, with no index allocation.
        // Rejected/missing candidates switch to an index to bound worst-case work.
        let found = if direct_lookups < MAX_CONCURRENT {
            direct_lookups += 1;
            shells
                .clone()
                .enumerate()
                .find(|(_, shell)| shell.id == candidate.shell)
        } else {
            index
                .get_or_insert_with(|| {
                    shells
                        .clone()
                        .enumerate()
                        .map(|(i, shell)| (shell.id, (i, shell)))
                        .collect::<HashMap<_, _>>()
                })
                .get(candidate.shell)
                .copied()
        };
        let Some((i, shell)) = found else {
            continue;
        };
        if !shell.eligible || candidate.retry.stopped && candidate.retry.run.as_deref() == shell.run
        {
            continue;
        }
        result.push((candidate.pane, i));
        if result.len() == limit {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_plan_skips_missing_and_ineligible_candidates_without_using_slots() {
        let retry = Retry::default();
        let keys = [
            "missing-1",
            "missing-2",
            "missing-3",
            "missing-4",
            "finished",
            "ready",
        ];
        let candidates = keys.iter().enumerate().map(|(pane, shell)| Candidate {
            pane,
            shell,
            retry: &retry,
        });
        let shells = [
            Shell {
                id: "finished",
                run: None,
                eligible: false,
            },
            Shell {
                id: "ready",
                run: Some("run"),
                eligible: true,
            },
        ];
        assert_eq!(
            plan(candidates, shells.into_iter(), 1, Instant::now()),
            vec![(5, 1)]
        );
    }

    #[test]
    fn recovery_backoff_is_bounded_and_resets_after_success() {
        let now = Instant::now();
        let mut retry = Retry::default();
        retry.begin(Some("run"));
        for seconds in [2, 4, 8, 16, 30, 30] {
            retry.failed(false, now);
            assert!(retry.waiting(now + Duration::from_secs(seconds - 1)));
            assert!(!retry.waiting(now + Duration::from_secs(seconds)));
        }
        assert!(retry.needs_attention());
        retry.succeeded(Some("run"));
        assert!(!retry.waiting(now));
        assert!(!retry.needs_attention());
    }

    #[test]
    fn recovery_plan_skips_waiting_without_building_a_shell_index() {
        let now = Instant::now();
        let mut retry = Retry::default();
        retry.failed(false, now);
        let candidates = std::iter::once(Candidate {
            pane: 1,
            shell: "s",
            retry: &retry,
        });
        let shells =
            std::iter::from_fn(|| -> Option<Shell<'_>> { panic!("backoff must avoid lookup") });
        assert!(plan(candidates, shells, 4, now).is_empty());
    }

    #[test]
    fn recovery_plan_enforces_capacity_and_preserves_stopped_runs() {
        let now = Instant::now();
        let mut retry = Retry::default();
        retry.begin(Some("old"));
        retry.failed(true, now);
        let later = now + Duration::from_secs(31);
        let candidates = || {
            (0..8).map(|pane| Candidate {
                pane,
                shell: "s",
                retry: &retry,
            })
        };
        let shells = |run| {
            std::iter::once(Shell {
                id: "s",
                run: Some(run),
                eligible: true,
            })
        };
        assert!(plan(candidates(), shells("old"), 4, later).is_empty());
        assert_eq!(plan(candidates(), shells("new"), 2, later).len(), 2);
        assert_eq!(
            plan(candidates(), shells("new"), 9, later).len(),
            MAX_CONCURRENT
        );
        assert!(plan(candidates(), shells("new"), 0, later).is_empty());
    }
}
