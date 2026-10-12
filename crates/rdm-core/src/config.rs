//! Engine tuning parameters.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Maximum simultaneous connections per download (default 8).
    pub max_connections: usize,
    /// Connections opened right after the probe (observed live: 4, i.e. the file in quarters).
    pub initial_connections: usize,
    /// Never split a chunk whose predicted remaining work is below this (0x5B43).
    pub min_split_remaining: u64,
    /// Minimum segment size used by `need_new_connection` (split only if remaining >= 2*this + 3).
    pub min_segment_size: u64,
    /// Bytes each chunk reads past its end to verify the next chunk (observed live: 64).
    pub overlap_bytes: u64,
    /// How far ahead (seconds) the "largest remaining work" prediction looks.
    pub lookahead_secs: f64,
    /// Minimum time between connection ramp-up steps.
    pub ramp_interval: Duration,
    /// Progress snapshot interval for the UI (100 ms).
    pub ui_interval: Duration,
    /// Chunk-map / progress persistence interval (2.5 s).
    pub save_interval: Duration,
    /// Block size used when appending chunk files during assembly (6 MB).
    pub join_block: usize,
    /// No new connections once the speed reaches this fraction of the speed limit.
    pub limiter_headroom: f64,
    /// Longest single pause the speed limiter may impose.
    pub limiter_max_pause: Duration,
    /// Connect/read timeout per request.
    pub timeout: Duration,
    /// Retries per chunk before the download fails (0 = unlimited).
    pub max_retries: u32,
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_connections: 8,
            initial_connections: 4,
            min_split_remaining: 23_363,
            min_segment_size: 11_680,
            overlap_bytes: 64,
            lookahead_secs: 1.0,
            ramp_interval: Duration::from_secs(1),
            ui_interval: Duration::from_millis(100),
            save_interval: Duration::from_millis(2500),
            join_block: 6 * 1024 * 1024,
            limiter_headroom: 0.85,
            limiter_max_pause: Duration::from_secs(12),
            timeout: Duration::from_secs(30),
            max_retries: 20,
            user_agent: format!("Mozilla/5.0 (compatible; RDM/{})", env!("CARGO_PKG_VERSION")),
        }
    }
}
