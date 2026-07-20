//! Exponential reconnect backoff with jitter.
//!
//! Pattern: supports the **Actor** reconnect loop — delays grow exponentially
//! up to a cap, with multiplicative jitter to avoid reconnect stampedes.

use std::time::Duration;

/// Tunables for reconnect delay calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffConfig {
    /// Delay after the first failure.
    pub initial: Duration,
    /// Upper bound on delay.
    pub max: Duration,
    /// Multiplier applied each attempt (typically 2).
    pub multiplier: u32,
    /// Jitter as hundredths of the base delay (e.g. 25 → ±25%).
    pub jitter_pct: u32,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            multiplier: 2,
            jitter_pct: 25,
        }
    }
}

impl BackoffConfig {
    /// Accelerated schedule for automated soak / reconnect unit tests.
    pub fn accelerated_for_tests() -> Self {
        Self {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(20),
            multiplier: 2,
            jitter_pct: 10,
        }
    }
}

/// Stateful attempt counter for reconnect delays.
#[derive(Debug, Clone)]
pub struct Backoff {
    config: BackoffConfig,
    attempt: u32,
    /// When true, jitter is disabled (deterministic tests).
    deterministic: bool,
}

impl Backoff {
    /// Creates a backoff from config (jitter enabled).
    pub fn new(config: BackoffConfig) -> Self {
        Self {
            config,
            attempt: 0,
            deterministic: false,
        }
    }

    /// Deterministic delays (no jitter) for unit tests.
    pub fn deterministic(config: BackoffConfig) -> Self {
        Self {
            config,
            attempt: 0,
            deterministic: true,
        }
    }

    /// Resets the attempt counter after a successful connect/read session.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Current attempt index (0 before first failure delay).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Computes the next sleep duration and advances the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let base = capped_exp(
            self.config.initial,
            self.config.max,
            self.config.multiplier,
            self.attempt,
        );
        self.attempt = self.attempt.saturating_add(1);
        if self.deterministic || self.config.jitter_pct == 0 {
            return base;
        }
        apply_jitter(base, self.config.jitter_pct)
    }
}

fn capped_exp(initial: Duration, max: Duration, multiplier: u32, attempt: u32) -> Duration {
    let mult = multiplier.max(1);
    let mut ms = initial.as_millis() as u128;
    for _ in 0..attempt {
        ms = ms.saturating_mul(mult as u128);
        if ms >= max.as_millis() as u128 {
            return max;
        }
    }
    Duration::from_millis(ms.min(u64::MAX as u128) as u64).min(max)
}

fn apply_jitter(base: Duration, jitter_pct: u32) -> Duration {
    let base_ms = base.as_millis() as u64;
    if base_ms == 0 {
        return base;
    }
    let span = (base_ms.saturating_mul(jitter_pct as u64) / 100).max(1);
    // Uniform in [base - span, base + span], floored at 0.
    let low = base_ms.saturating_sub(span);
    let high = base_ms.saturating_add(span);
    let picked = fastrand::u64(low..=high);
    Duration::from_millis(picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_grows_then_caps() {
        let mut b = Backoff::deterministic(BackoffConfig {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(80),
            multiplier: 2,
            jitter_pct: 0,
        });
        assert_eq!(b.next_delay(), Duration::from_millis(10));
        assert_eq!(b.next_delay(), Duration::from_millis(20));
        assert_eq!(b.next_delay(), Duration::from_millis(40));
        assert_eq!(b.next_delay(), Duration::from_millis(80));
        assert_eq!(b.next_delay(), Duration::from_millis(80));
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::from_millis(10));
    }

    #[test]
    fn default_and_accelerated_configs() {
        let d = BackoffConfig::default();
        assert!(d.initial < d.max);
        let a = BackoffConfig::accelerated_for_tests();
        assert!(a.max <= Duration::from_millis(50));
    }

    #[test]
    fn jitter_stays_near_base() {
        let mut b = Backoff::new(BackoffConfig {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(1),
            multiplier: 2,
            jitter_pct: 25,
        });
        for _ in 0..20 {
            let d = b.next_delay();
            // First attempt base=100 ±25 → [75, 125]
            assert!(d >= Duration::from_millis(50));
            assert!(d <= Duration::from_millis(200));
            b.reset();
        }
    }

    #[test]
    fn zero_initial_jitter_ok() {
        let mut b = Backoff::new(BackoffConfig {
            initial: Duration::ZERO,
            max: Duration::from_millis(10),
            multiplier: 2,
            jitter_pct: 50,
        });
        assert_eq!(b.next_delay(), Duration::ZERO);
    }
}
