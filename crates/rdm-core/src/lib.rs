//! RDM core: a segmented, resumable, multi-connection download engine.

pub mod chunks;
pub mod config;
pub mod dash;
pub mod ipc;
pub mod download;
pub mod ftp;
pub mod grabber;
pub mod hls;
pub mod limiter;
pub mod manager;
pub mod naming;
pub mod settings;
pub mod storage;

pub use config::EngineConfig;
pub use download::{ChunkView, Control, Download, DownloadError, DownloadHandle, DownloadRequest, ProbeInfo, Progress, Status, probe_info};
pub use limiter::Limiter;
pub use manager::{AddOptions, Event, Item, ItemId, ItemState, Manager};
pub use settings::Settings;
