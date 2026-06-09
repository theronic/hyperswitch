//! A minimal in-memory circuit breaker for the limits connector.
//!
//! Tracks consecutive failures; once the configured threshold is reached the
//! breaker *opens* for a cooldown window, during which [`is_open`](CircuitBreaker::is_open)
//! returns `true` and the decision service short-circuits to the configured
//! fail-mode without calling the provider. A single success closes it again.
//!
//! This is deliberately process-local and dependency-light (interior mutability
//! via a `Mutex`); a distributed breaker is out of scope for Phase 4.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// Process-local consecutive-failures circuit breaker.
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    state: Mutex<BreakerState>,
}

#[derive(Debug, Default)]
struct BreakerState {
    consecutive_failures: u32,
    /// When set, the breaker is open until this instant.
    open_until: Option<Instant>,
}

impl CircuitBreaker {
    /// Create a breaker that opens after `threshold` consecutive failures and
    /// stays open for `cooldown`.
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            state: Mutex::new(BreakerState::default()),
        }
    }

    /// Whether the breaker is currently open (provider calls should be skipped).
    ///
    /// Re-checks the cooldown: if it has elapsed the breaker is treated as
    /// half-open and reports closed so the next call probes the provider.
    pub fn is_open(&self) -> bool {
        self.is_open_at(Instant::now())
    }

    /// Record a successful provider call (closes the breaker).
    pub fn record_success(&self) {
        let mut state = self.lock();
        state.consecutive_failures = 0;
        state.open_until = None;
    }

    /// Record a failed provider call. Opens the breaker once the threshold of
    /// consecutive failures is reached.
    pub fn record_failure(&self) {
        self.record_failure_at(Instant::now());
    }

    // --- time-injectable internals (for deterministic tests) ---

    fn is_open_at(&self, now: Instant) -> bool {
        let mut state = self.lock();
        match state.open_until {
            Some(until) if now < until => true,
            Some(_) => {
                // Cooldown elapsed: half-open. Allow the next probe through.
                state.open_until = None;
                false
            }
            None => false,
        }
    }

    fn record_failure_at(&self, now: Instant) {
        let mut state = self.lock();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= self.threshold {
            state.open_until = Some(now + self.cooldown);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BreakerState> {
        // A poisoned breaker mutex must not take down the payment path; recover
        // the guard and carry on (the worst case is a stale failure count).
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_consecutive_failures() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(30));
        assert!(!breaker.is_open());

        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.is_open(), "must stay closed below threshold");

        breaker.record_failure();
        assert!(breaker.is_open(), "must open at threshold");
    }

    #[test]
    fn success_resets_failure_count() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(30));
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_success();
        breaker.record_failure();
        breaker.record_failure();
        assert!(
            !breaker.is_open(),
            "a success in the middle must reset the consecutive count"
        );
    }

    #[test]
    fn half_opens_after_cooldown() {
        let now = Instant::now();
        let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
        breaker.record_failure_at(now);
        assert!(breaker.is_open_at(now));
        // Within cooldown -> still open.
        assert!(breaker.is_open_at(now + Duration::from_secs(10)));
        // After cooldown -> half-open (reports closed to allow a probe).
        assert!(!breaker.is_open_at(now + Duration::from_secs(31)));
    }
}
