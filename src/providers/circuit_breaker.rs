//! Circuit breaker for provider failover.
//!
//! Tracks consecutive failures per provider and quarantines broken providers
//! to avoid wasting latency on endpoints that are known to be down. State
//! transitions follow the classic circuit-breaker pattern:
//!
//! - **Closed** (healthy): all calls pass through normally.
//! - **Open** (quarantined): calls are rejected immediately; the provider is
//!   skipped in the fallback loop.
//! - **HalfOpen** (probing): a limited number of calls are allowed through to
//!   test recovery. Success resets to Closed; failure reopens the circuit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ── State types ─────────────────────────────────────────────────────────────

/// High-level circuit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Healthy — all calls pass through.
    Closed,
    /// Quarantined — provider is skipped entirely.
    Open,
    /// Probing — limited calls allowed to test recovery.
    HalfOpen,
}

/// Per-provider internal bookkeeping.
#[derive(Debug, Clone)]
struct ProviderCircuitState {
    state: CircuitState,
    /// Consecutive failure count (reset on success).
    consecutive_failures: u32,
    /// When the circuit was last opened (used to decide half-open transition).
    last_failure_time: Option<Instant>,
    /// Number of probe calls allowed while half-open.
    half_open_calls_remaining: u32,
}

impl ProviderCircuitState {
    fn new(half_open_max_calls: u32) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            last_failure_time: None,
            half_open_calls_remaining: half_open_max_calls,
        }
    }
}

// ── CircuitBreaker ──────────────────────────────────────────────────────────

/// Shared, thread-safe circuit breaker that tracks per-provider health.
///
/// All public methods acquire a `Mutex` internally and are safe to call from
/// any thread. The lock is held only for the duration of a single state
/// read/write, so contention is negligible in practice.
pub struct CircuitBreaker {
    failure_threshold: u32,
    recovery_timeout: Duration,
    half_open_max_calls: u32,
    states: Mutex<HashMap<String, ProviderCircuitState>>,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given parameters.
    pub fn new(
        failure_threshold: u32,
        recovery_timeout: Duration,
        half_open_max_calls: u32,
    ) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            recovery_timeout,
            half_open_max_calls: half_open_max_calls.max(1),
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` when the provider may be called.
    ///
    /// - `Closed` → always available.
    /// - `Open` → available only once `recovery_timeout` has elapsed (transitions
    ///   to `HalfOpen` on the spot).
    /// - `HalfOpen` → available while probe budget remains.
    pub fn is_available(&self, provider: &str) -> bool {
        let mut states = self.states.lock().expect("circuit breaker lock poisoned");
        let entry = states
            .entry(provider.to_owned())
            .or_insert_with(|| ProviderCircuitState::new(self.half_open_max_calls));

        match entry.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if recovery timeout has elapsed → transition to HalfOpen.
                if let Some(last) = entry.last_failure_time {
                    if last.elapsed() >= self.recovery_timeout {
                        entry.state = CircuitState::HalfOpen;
                        entry.half_open_calls_remaining = self.half_open_max_calls;
                        tracing::info!(
                            provider,
                            "Circuit breaker transitioning from Open to HalfOpen (recovery timeout elapsed)"
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    // No recorded failure time — defensive fallback, allow call.
                    true
                }
            }
            CircuitState::HalfOpen => entry.half_open_calls_remaining > 0,
        }
    }

    /// Record a successful call. Resets circuit to `Closed`.
    pub fn record_success(&self, provider: &str) {
        let mut states = self.states.lock().expect("circuit breaker lock poisoned");
        let entry = states
            .entry(provider.to_owned())
            .or_insert_with(|| ProviderCircuitState::new(self.half_open_max_calls));

        let prev = entry.state;
        entry.state = CircuitState::Closed;
        entry.consecutive_failures = 0;
        entry.last_failure_time = None;
        entry.half_open_calls_remaining = self.half_open_max_calls;

        if prev != CircuitState::Closed {
            tracing::info!(
                provider,
                previous_state = ?prev,
                "Circuit breaker closed (provider recovered)"
            );
        }
    }

    /// Record a failed call. May transition to `Open` if the failure threshold
    /// is reached, or reopen the circuit if already in `HalfOpen`.
    pub fn record_failure(&self, provider: &str) {
        let mut states = self.states.lock().expect("circuit breaker lock poisoned");
        let entry = states
            .entry(provider.to_owned())
            .or_insert_with(|| ProviderCircuitState::new(self.half_open_max_calls));

        entry.consecutive_failures += 1;
        entry.last_failure_time = Some(Instant::now());

        match entry.state {
            CircuitState::Closed => {
                if entry.consecutive_failures >= self.failure_threshold {
                    entry.state = CircuitState::Open;
                    tracing::warn!(
                        provider,
                        consecutive_failures = entry.consecutive_failures,
                        threshold = self.failure_threshold,
                        recovery_secs = self.recovery_timeout.as_secs(),
                        "Circuit breaker opened (provider quarantined)"
                    );
                }
            }
            CircuitState::HalfOpen => {
                entry.half_open_calls_remaining = entry.half_open_calls_remaining.saturating_sub(1);
                // Probe failed — reopen.
                entry.state = CircuitState::Open;
                tracing::warn!(
                    provider,
                    "Circuit breaker reopened (half-open probe failed)"
                );
            }
            CircuitState::Open => {
                // Already open — just update failure bookkeeping (handled above).
            }
        }
    }

    /// Return the current state for a provider (or `Closed` if unknown).
    #[cfg(test)]
    pub fn state(&self, provider: &str) -> CircuitState {
        let states = self.states.lock().expect("circuit breaker lock poisoned");
        states
            .get(provider)
            .map(|s| s.state)
            .unwrap_or(CircuitState::Closed)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_provider_is_available() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60), 1);
        assert!(cb.is_available("test_provider"));
        assert_eq!(cb.state("test_provider"), CircuitState::Closed);
    }

    #[test]
    fn stays_closed_below_threshold() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60), 1);
        cb.record_failure("p1");
        cb.record_failure("p1");
        assert!(cb.is_available("p1"));
        assert_eq!(cb.state("p1"), CircuitState::Closed);
    }

    #[test]
    fn opens_at_threshold() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60), 1);
        for _ in 0..3 {
            cb.record_failure("p1");
        }
        assert_eq!(cb.state("p1"), CircuitState::Open);
        assert!(!cb.is_available("p1"));
    }

    #[test]
    fn success_resets_failure_count() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60), 1);
        cb.record_failure("p1");
        cb.record_failure("p1");
        cb.record_success("p1");
        assert_eq!(cb.state("p1"), CircuitState::Closed);
        // Now two more failures should not open (count was reset).
        cb.record_failure("p1");
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Closed);
    }

    #[test]
    fn transitions_to_half_open_after_timeout() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(0), 1);
        cb.record_failure("p1");
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Open);
        // With zero timeout, should immediately transition to HalfOpen.
        assert!(cb.is_available("p1"));
        assert_eq!(cb.state("p1"), CircuitState::HalfOpen);
    }

    #[test]
    fn half_open_success_closes_circuit() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(0), 1);
        cb.record_failure("p1");
        cb.record_failure("p1");
        // Trigger HalfOpen transition.
        assert!(cb.is_available("p1"));
        assert_eq!(cb.state("p1"), CircuitState::HalfOpen);
        // Probe succeeds.
        cb.record_success("p1");
        assert_eq!(cb.state("p1"), CircuitState::Closed);
        assert!(cb.is_available("p1"));
    }

    #[test]
    fn half_open_failure_reopens_circuit() {
        // Use a long timeout so that after reopening, `is_available` stays false.
        let cb = CircuitBreaker::new(2, Duration::from_secs(3600), 1);
        cb.record_failure("p1");
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Open);
        // Manually force HalfOpen by setting last_failure_time far in the past.
        {
            let mut states = cb.states.lock().unwrap();
            let entry = states.get_mut("p1").unwrap();
            entry.state = CircuitState::HalfOpen;
            entry.half_open_calls_remaining = 1;
        }
        assert_eq!(cb.state("p1"), CircuitState::HalfOpen);
        // Probe fails.
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Open);
        assert!(!cb.is_available("p1"));
    }

    #[test]
    fn providers_are_independent() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(60), 1);
        cb.record_failure("p1");
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Open);
        // p2 should be unaffected.
        assert!(cb.is_available("p2"));
        assert_eq!(cb.state("p2"), CircuitState::Closed);
    }

    #[test]
    fn half_open_respects_max_calls() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(0), 2);
        cb.record_failure("p1");
        cb.record_failure("p1");
        // Trigger HalfOpen with budget of 2.
        assert!(cb.is_available("p1"));
        assert_eq!(cb.state("p1"), CircuitState::HalfOpen);
        // First probe call fails but consumes budget.
        cb.record_failure("p1");
        // After a probe failure in HalfOpen, circuit reopens immediately.
        assert_eq!(cb.state("p1"), CircuitState::Open);
    }

    #[test]
    fn open_circuit_not_available_before_timeout() {
        let cb = CircuitBreaker::new(1, Duration::from_secs(3600), 1);
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Open);
        // Timeout is 1 hour — should not be available.
        assert!(!cb.is_available("p1"));
    }

    #[test]
    fn threshold_clamped_to_minimum_one() {
        // Even if someone passes 0, it should require at least 1 failure.
        let cb = CircuitBreaker::new(0, Duration::from_secs(60), 1);
        // One failure should open (threshold clamped to 1).
        cb.record_failure("p1");
        assert_eq!(cb.state("p1"), CircuitState::Open);
    }
}
