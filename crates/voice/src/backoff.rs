//! Exponential reconnect backoff — PURA-118 WS-1.
//!
//! Pure logic, no I/O. Decoupled from the bot actor so the unit tests can
//! assert the sequence without spinning a tokio runtime.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BackoffConfig {
    /// First sleep on the first failure. Default `1 s`.
    pub initial: Duration,
    /// Cap. Default `60 s`.
    pub max: Duration,
    /// Per-attempt growth factor. Default `2.0`.
    pub multiplier: f64,
    /// Maximum number of retry attempts before giving up and entering
    /// `Disconnected` permanently. `None` = retry forever. Default
    /// `None`.
    pub max_attempts: Option<u32>,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            multiplier: 2.0,
            max_attempts: None,
        }
    }
}

/// Stateful backoff iterator. Each call to `next_delay()` returns the
/// sleep duration for the next attempt and bumps the internal counter.
#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    config: BackoffConfig,
    attempt: u32,
}

impl ExponentialBackoff {
    pub fn new(config: BackoffConfig) -> Self {
        Self { config, attempt: 0 }
    }

    /// Reset the counter — call after a successful connect.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// How many failed attempts have we recorded so far.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// Returns `Some(duration)` for the next sleep, or `None` if the
    /// configured `max_attempts` has been reached. Caller is expected to
    /// emit a `BotEvent::Error` and stop retrying when `None` comes back.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if let Some(max) = self.config.max_attempts {
            if self.attempt >= max {
                return None;
            }
        }
        // attempt 0 → initial; attempt 1 → initial * mult; clamp to max.
        let scale = self.config.multiplier.powi(self.attempt as i32);
        let secs = self.config.initial.as_secs_f64() * scale;
        let secs = secs.min(self.config.max.as_secs_f64());
        self.attempt = self.attempt.saturating_add(1);
        Some(Duration::from_secs_f64(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(initial_ms: u64, max_secs: u64, mult: f64, max_attempts: Option<u32>) -> BackoffConfig {
        BackoffConfig {
            initial: Duration::from_millis(initial_ms),
            max: Duration::from_secs(max_secs),
            multiplier: mult,
            max_attempts,
        }
    }

    #[test]
    fn doubles_until_cap() {
        let mut b = ExponentialBackoff::new(cfg(100, 1, 2.0, None));
        let seq: Vec<u128> = (0..6).map(|_| b.next_delay().unwrap().as_millis()).collect();
        // 100, 200, 400, 800, 1000 (cap), 1000 (cap).
        assert_eq!(seq, vec![100, 200, 400, 800, 1000, 1000]);
    }

    #[test]
    fn reset_returns_to_initial() {
        let mut b = ExponentialBackoff::new(cfg(100, 60, 2.0, None));
        let _ = b.next_delay();
        let _ = b.next_delay();
        assert_eq!(b.attempts(), 2);
        b.reset();
        assert_eq!(b.attempts(), 0);
        assert_eq!(b.next_delay().unwrap(), Duration::from_millis(100));
    }

    #[test]
    fn max_attempts_returns_none() {
        let mut b = ExponentialBackoff::new(cfg(50, 60, 2.0, Some(3)));
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_none());
        assert!(b.next_delay().is_none());
    }

    #[test]
    fn multiplier_one_is_constant_backoff() {
        let mut b = ExponentialBackoff::new(cfg(250, 60, 1.0, None));
        for _ in 0..4 {
            assert_eq!(b.next_delay().unwrap(), Duration::from_millis(250));
        }
    }
}
