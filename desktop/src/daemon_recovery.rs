//! One bounded daemon-start retry stream for Desktop's background overview watcher.
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Recovery {
    next_attempt: Option<Instant>,
    failures: u8,
}

impl Recovery {
    pub fn ensure(
        &mut self,
        mut connected: impl FnMut() -> Result<bool, String>,
        start: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        // A compatible daemon becoming available bypasses startup backoff.
        if connected()? {
            self.next_attempt = None;
            self.failures = 0;
            return Ok(());
        }
        if self.next_attempt.is_some_and(|next| Instant::now() < next) {
            return Err("Waiting to retry Boomux daemon startup".into());
        }
        let result = start().and_then(|()| {
            connected()?
                .then_some(())
                .ok_or_else(|| "Boomux daemon is still unavailable".into())
        });
        if result.is_ok() {
            self.next_attempt = None;
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
            self.next_attempt =
                Some(Instant::now() + Duration::from_secs((1u64 << self.failures.min(5)).min(30)));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn starts_missing_daemon_and_verifies_connection() {
        let live = Cell::new(false);
        let mut recovery = Recovery::default();
        recovery
            .ensure(
                || Ok(live.get()),
                || {
                    live.set(true);
                    Ok(())
                },
            )
            .unwrap();
        recovery
            .ensure(|| Ok(true), || panic!("must reuse daemon"))
            .unwrap();
        assert_eq!(recovery.failures, 0);
    }

    #[test]
    fn failed_start_is_throttled_but_external_recovery_is_immediate() {
        let mut recovery = Recovery::default();
        for failures in 1..=8 {
            recovery.next_attempt = None;
            assert!(
                recovery
                    .ensure(|| Ok(false), || Err("spawn failed".into()))
                    .is_err()
            );
            assert_eq!(recovery.failures, failures);
            assert!(recovery.next_attempt.unwrap() <= Instant::now() + Duration::from_secs(30));
            assert!(
                recovery
                    .ensure(|| Ok(false), || panic!("startup storm"))
                    .is_err()
            );
        }
        recovery
            .ensure(|| Ok(true), || panic!("already running"))
            .unwrap();
        assert_eq!(recovery.failures, 0);
        assert!(recovery.next_attempt.is_none());
    }

    #[test]
    fn protocol_error_does_not_start_replacement_and_start_must_be_verified() {
        let mut recovery = Recovery::default();
        assert!(
            recovery
                .ensure(|| Err("incompatible".into()), || panic!("must not replace"))
                .is_err()
        );
        assert!(recovery.ensure(|| Ok(false), || Ok(())).is_err());
        assert!(recovery.next_attempt.is_some());
    }
}
