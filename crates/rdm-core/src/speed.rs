//! Speed measurement: a short sliding window plus a smoothed average.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
    window: Duration,
    smoothed: f64,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new(Duration::from_millis(2500))
    }
}

impl SpeedMeter {
    pub fn new(window: Duration) -> Self {
        Self { samples: