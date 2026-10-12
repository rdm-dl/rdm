//! RDM core: a segmented, resumable, multi-connection download engine.

pub mod chunks;
pub mod config;
pub mod download;
pub mod limiter;
pub mod naming;
pub mod storage;

pub use config::EngineConfig;
pub use download::{Control, Download, DownloadError, DownloadRequest, Progress, Status};
pub use limiter::Limiter;
