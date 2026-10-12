//! Bandwidth limiter: a proportional, self-damping pause.
//!
//! After data arrives, if the rate measured over the current window exceeds the
//! limit, the caller sleeps for `(rate - limit) * elapsed / limit`. The pause is
//! softened while the average speed is well below the limit and capped.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const MAX_PAUSE: Duration = Duration::from_secs(12);
/// No new connections are opened at or above this share of the limit.
pub const RAMP_HEADROOM: f64 = 0.85;
/// Measurement window length.
const WINDOW: Duration = Duration::from_millis(1000);

#[derive(Debug)]
struct State {
    window_start: Instant,
    window_bytes: u64,
    /// Exponentially smoothed speed, bytes/s.
    avg_speed: f64,
}

#[derive(Debug)]
pub struct Limiter {
    limit: Mutex<Option<u64>>,
    state: Mutex<State>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Limiter {
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            limit: Mutex::new(limit.filter(|&l| l > 0)),
            state: Mutex::new(State { window_start: Instant::now(), window_bytes: 0, avg_speed: 0.0 }),
        }
    }

    /// Bytes per second, or `None` for unlimited.
    pub fn limit(&self) -> Option<u64> {
        *self.limit.lock().unwrap()
    }

    pub fn set_limit(&self, limit: Option<u64>) {
        *self.limit.lock().unwrap() = limit.filter(|&l| l > 0);
    }

    /// True when the current speed leaves no room for another connection.
    pub fn near_limit(&self, current_speed: f64) -> bool {
        self.limit().is_some_and(|l| current_speed >= l as f64 * RAMP_HEADROOM)
    }

    /// Record `bytes` just received and return how long the caller should pause.
    pub fn account(&self, bytes: u64) -> Duration {
        let Some(limit) = self.limit() else {
            return Duration::ZERO;
        };
        let mut s = self.state.lock().unwrap();
        s.window_bytes += bytes;
        let secs = s.window_start.elapsed().as_secs_f64().max(0.001);
        let rate = s.window_bytes as f64 / secs;
        let avg = if s.avg_speed == 0.0 { rate } else { s.avg_speed };
        let pause = pause_for(rate, limit as f64, secs, avg);
        if secs >= WINDOW.as_secs_f64() {
            s.avg_speed = if s.avg_speed == 0.0 { rate } else { s.avg_speed * 0.7 + rate * 0.3 };
            // Start the next window, counting the pause we are about to take.
            s.window_start = Instant::now() + pause;
            s.window_bytes = 0;
        }
        pause
    }

    /// Account for `bytes` and sleep if the limit was exceeded.
    pub async fn throttle(&self, bytes: u64) {
        let pause = self.account(bytes);
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
    }
}

/// Pause length for a measured `rate` (bytes/s) over `elapsed_secs` against `limit`.
pub fn pause_for(rate: f64, limit: f64, elapsed_secs: f64, avg_speed: f64) -> Duration {
    if limit <= 0.0 || rate <= limit {
        return Duration::ZERO;
    }
    let mut pause_ms = (rate - limit) * elapsed_secs * 1000.0 / limit;
    if avg_speed < limit / 2.0 {
        pause_ms /= 20.0;
    } else if avg_speed < limit * 0.9 {
        pause_ms /= 2.0;
    }
    Duration::from_millis(pause_ms as u64).min(MAX_PAUSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pause_under_limit() {
        assert_eq!(pause_for(500.0, 1000.0, 1.0, 900.0), Duration::ZERO);
    }

    #[test]
    fn proportional_pause_over_limit() {
        // 2x the limit for 1 s at a steady average -> pause about 1 s.
        assert_eq!(pause_for(2000.0, 1000.0, 1.0, 1000.0), Duration::from_millis(1000));
    }

    #[test]
    fn damped_when_average_is_low() {
        assert_eq!(pause_for(2000.0, 1000.0, 1.0, 400.0), Duration::from_millis(50));
        assert_eq!(pause_for(2000.0, 1000.0, 1.0, 800.0), Duration::from_millis(500));
    }

    #[test]
    fn capped() {
        assert_eq!(pause_for(1e9, 1.0, 10.0, 1e9), MAX_PAUSE);
    }

    #[test]
    fn headroom() {
        let l = Limiter::new(Some(1000));
        assert!(l.near_limit(850.0));
        assert!(!l.near_limit(800.0));
        assert!(!Limiter::new(None).near_limit(1e12));
    }
}
