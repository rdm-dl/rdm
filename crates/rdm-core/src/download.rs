//! A single download: probe, multi-connection segmented transfer, resume, assembly.
//!
//! Flow:
//! 1. **Probe** with a plain GET. Its response becomes chunk 0's stream, so no bytes
//!    are wasted. Headers give size, range support, name and validators.
//! 2. If ranges are supported, split right away into `initial_connections` parts.
//! 3. **Ramp-up** every `ramp_interval`: when every active connection is receiving
//!    data, double the connection count (up to `max_connections`), unless the speed
//!    limiter is near its cap.
//! 4. A connection that finishes its chunk takes over the chunk with the most
//!    predicted remaining work by splitting it in half (connection reuse).
//! 5. Neighbouring chunks overlap by a few bytes; a mismatch means the server returns
//!    inconsistent data for ranges, so the download restarts with one connection.
//! 6. Progress is published every `ui_interval` and saved every `save_interval`.
//! 7. When all chunks are complete, chunk files are assembled into the target file.

use crate::chunks::{ChunkMap, OVERLAP};
use crate::config::EngineConfig;
use crate::limiter::Limiter;
use crate::naming;
use crate::storage::{SavedState, TempDir, assemble};
use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned status {0}")]
    Status(StatusCode),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("too many failures on chunk {0}: {1}")]
    Retries(u32, String),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

type Result<T> = std::result::Result<T, DownloadError>;

/// Everything the caller can specify for a download.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// Folder to save into.
    pub dir: PathBuf,
    /// Forced file name (otherwise chosen from the server / URL).
    pub file_name: Option<String>,
    pub referer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    /// Extra request headers.
    pub headers: Vec<(String, String)>,
    /// POST body (form downloads); `None` = GET.
    pub post_data: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Per-download speed limit in bytes/s.
    pub speed_limit: Option<u64>,
    /// Override the connection count for this download.
    pub max_connections: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Connecting,
    Downloading,
    Paused,
    Assembling,
    Complete,
    Failed(String),
}

/// One chunk as shown in the progress window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkView {
    pub id: u32,
    pub start: u64,
    pub end: u64,
    pub pos: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub status: Status,
    pub file_name: String,
    pub total: Option<u64>,
    pub downloaded: u64,
    /// Bytes per second.
    pub speed: f64,
    pub connections: usize,
    pub resumable: bool,
    pub chunks: Vec<ChunkView>,
    pub final_path: Option<PathBuf>,
    /// While joining pieces into the final file: how far it got (0..1).
    #[serde(default)]
    pub joining: Option<f32>,
}

impl Progress {
    fn new(name: String) -> Self {
        Self {
            status: Status::Connecting,
            file_name: name,
            total: None,
            downloaded: 0,
            speed: 0.0,
            connections: 0,
            resumable: false,
            chunks: vec![],
            final_path: None,
            joining: None,
        }
    }
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        if self.speed < 1.0 {
            return None;
        }
        Some(Duration::from_secs_f64(total.saturating_sub(self.downloaded) as f64 / self.speed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Run,
    Pause,
    Cancel,
}

/// Information learned from the probe response.
#[derive(Debug, Clone, Default)]
struct ServerInfo {
    total: Option<u64>,
    accept_ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    disposition: Option<String>,
    final_url: Option<Url>,
}

impl ServerInfo {
    fn from_response(r: &Response) -> Self {
        let h = r.headers();
        let get = |k: header::HeaderName| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let total = if r.status() == StatusCode::PARTIAL_CONTENT {
            get(header::CONTENT_RANGE).and_then(|v| v.rsplit('/').next().and_then(|t| t.trim().parse().ok()))
        } else {
            r.content_length()
        };
        let accept_ranges = r.status() == StatusCode::PARTIAL_CONTENT
            || get(header::ACCEPT_RANGES).is_some_and(|v| v.to_ascii_lowercase().contains("bytes"));
        Self {
            total,
            accept_ranges,
            etag: get(header::ETAG),
            last_modified: get(header::LAST_MODIFIED),
            content_type: get(header::CONTENT_TYPE),
            disposition: get(header::CONTENT_DISPOSITION),
            final_url: Some(r.url().clone()),
        }
    }
}

/// State shared between the controller and connection tasks.
struct Shared {
    map: ChunkMap,
    /// Chunk id -> bytes/s of its connection.
    speeds: HashMap<u32, u64>,
    /// Chunk id -> last time data arrived.
    last_data: HashMap<u32, Instant>,
    /// Chunks with a connection attached.
    active: HashSet<u32>,
    /// Boundaries (left chunk id) whose overlap has been verified.
    verified: HashSet<u32>,
    corrupted: bool,
    single_connection: bool,
    /// Chunks that have a connection task (started or about to start).
    assigned: HashSet<u32>,
    /// Learned limit on parallel connections (server refused more).
    conn_cap: Option<usize>,
}

impl Shared {
    fn snapshot(&self) -> Vec<ChunkView> {
        self.map
            .chunks
            .iter()
            .map(|c| ChunkView { id: c.id, start: c.start, end: c.end, pos: c.pos(), active: self.active.contains(&c.id) })
            .collect()
    }
}

/// Context shared by all connection tasks of one download.
struct Ctx {
    client: Client,
    url: Url,
    req: DownloadRequest,
    cfg: EngineConfig,
    tmp: TempDir,
    shared: Mutex<Shared>,
    limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
    validator: Option<String>,
}

/// Why a connection task stopped.
#[derive(Debug)]
enum WorkerExit {
    /// Chunk finished and no more work to take over.
    Done,
    /// Server ignored the range request: fall back to one connection.
    NoRanges,
    /// Overlap check failed.
    Corrupt,
}

/// Cloneable remote control for a running download.
#[derive(Clone)]
pub struct DownloadHandle {
    control: Arc<watch::Sender<Control>>,
    limiter: Arc<Limiter>,
}

impl DownloadHandle {
    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }
    /// Change this download's own speed limit while it runs.
    pub fn set_speed_limit(&self, limit: Option<u64>) {
        self.limiter.set_limit(limit);
    }
}

pub struct Download {
    pub progress: watch::Receiver<Progress>,
    control: Arc<watch::Sender<Control>>,
    limiter: Arc<Limiter>,
    task: tokio::task::JoinHandle<Result<Option<PathBuf>>>,
}

impl Download {
    /// Start (or resume, if `temp_dir` holds saved state) a download.
    pub fn start(req: DownloadRequest, cfg: EngineConfig, temp_dir: PathBuf, global_limiter: Arc<Limiter>) -> Self {
        let name = req.file_name.clone().unwrap_or_default();
        let (ptx, prx) = watch::channel(Progress::new(name));
        let (ctx_tx, crx) = watch::channel(Control::Run);
        let limiter = Arc::new(Limiter::new(req.speed_limit));
        let task = tokio::spawn(run(req, cfg, temp_dir, global_limiter, limiter.clone(), ptx, crx));
        Self { progress: prx, control: Arc::new(ctx_tx), limiter, task }
    }

    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }

    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    pub fn handle(&self) -> DownloadHandle {
        DownloadHandle { control: self.control.clone(), limiter: self.limiter.clone() }
    }

    /// Wait for the download to finish. `Ok(Some(path))` when complete, `Ok(None)` when paused.
    pub async fn wait(self) -> Result<Option<PathBuf>> {
        self.task.await.map_err(|e| DownloadError::Other(e.to_string()))?
    }
}

fn build_client(cfg: &EngineConfig, req: &DownloadRequest) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if let Some(r) = &req.referer {
        if let Ok(v) = HeaderValue::from_str(r) {
            headers.insert(header::REFERER, v);
        }
    }
    if let Some(c) = &req.cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            headers.insert(header::COOKIE, v);
        }
    }
    for (k, v) in &req.headers {
        if let (Ok(k), Ok(v)) = (header::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            headers.insert(k, v);
        }
    }
    let ua = req.user_agent.clone().unwrap_or_else(|| cfg.user_agent.clone());
    let mut builder = Client::builder();
    // YouTube stream links are issued to one client address (`ip=`); connect from the same
    // address family, or a dual-stack machine may reach the server over the other one and
    // be refused.
    if let Some(ip) = Url::parse(&req.url).ok().filter(|u| u.host_str().is_some_and(|h| h.ends_with(".googlevideo.com"))).and_then(|u| u.query_pairs().find(|(k, _)| k == "ip").and_then(|(_, v)| v.parse::<std::net::IpAddr>().ok())) {
        builder = builder.local_address(if ip.is_ipv4() { std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED) } else { std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED) });
    }
    match cfg.proxy.as_deref() {
        None | Some("") | Some("system") => {}
        Some("none") => builder = builder.no_proxy(),
        Some(url) => {
            let mut p = reqwest::Proxy::all(url).map_err(DownloadError::Http)?;
            if let Some(u) = &cfg.proxy_user {
                p = p.basic_auth(u, cfg.proxy_pass.as_deref().unwrap_or(""));
            }
            builder = builder.proxy(p);
        }
    }
    Ok(builder
        .default_headers(headers)
        .user_agent(ua)
        .connect_timeout(cfg.timeout)
        .read_timeout(cfg.timeout)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .pool_max_idle_per_host(cfg.max_connections.max(1) * 2)
        .build()?)
}

impl Ctx {
    /// Build a request for `[from, to_inclusive]` (or the whole file when `None`).
    fn request(&self, range: Option<(u64, Option<u64>)>) -> reqwest::RequestBuilder {
        let mut rb = match &self.req.post_data {
            Some(body) => self.client.post(self.url.clone()).body(body.clone()),
            None => self.client.get(self.url.clone()),
        };
        if let Some((from, to)) = range {
            let v = match to {
                Some(to) => format!("bytes={from}-{to}"),
                None => format!("bytes={from}-"),
            };
            rb = rb.header(header::RANGE, v);
            if let Some(val) = &self.validator {
                rb = rb.header(header::IF_RANGE, val.clone());
            }
        }
        if let Some(u) = &self.req.username {
            rb = rb.basic_auth(u, self.req.password.as_deref());
        }
        rb
    }

    fn save(&self, info: &SavedInfo) {
        let sh = self.shared.lock().unwrap();
        let st = SavedState {
            url: self.req.url.clone(),
            file_name: info.file_name.clone(),
            total: info.total,
            etag: info.etag.clone(),
            last_modified: info.last_modified.clone(),
            content_type: info.content_type.clone(),
            resumable: info.resumable,
            single_connection: sh.single_connection,
            map: sh.map.clone(),
        };
        drop(sh);
        if let Err(e) = self.tmp.save(&st) {
            tracing::warn!("saving state failed: {e}");
        }
    }

    /// Compare the overlap between chunk `left` and its right neighbour, if both have
    /// the bytes. Returns `Some(false)` on mismatch.
    fn verify_boundary(&self, left: u32) -> Option<bool> {
        let (right_id, read_at, ready) = {
            let sh = self.shared.lock().unwrap();
            if sh.verified.contains(&left) {
                return Some(true);
            }
            let i = sh.map.chunks.iter().position(|c| c.id == left)?;
            let l = &sh.map.chunks[i];
            let r = sh.map.chunks.get(i + 1)?;
            let ov = l.end.saturating_sub(r.start).min(OVERLAP);
            if ov == 0 {
                return Some(true);
            }
            let ready = l.is_complete() && r.done >= ov;
            (r.id, r.start - l.start, ready.then_some(ov))
        };
        let ov = ready?;
        let a = self.tmp.read_chunk(left, read_at, ov as usize).ok()?;
        let b = self.tmp.read_chunk(right_id, 0, ov as usize).ok()?;
        let ok = a == b;
        if ok {
            self.shared.lock().unwrap().verified.insert(left);
        }
        Some(ok)
    }
}

/// Fields persisted alongside the chunk map.
#[derive(Debug, Clone)]
struct SavedInfo {
    file_name: String,
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    resumable: bool,
}

/// Stream `resp` into chunk `id` until the chunk is complete or the stream ends.
async fn pump(ctx: &Ctx, id: u32, resp: Response, control: &watch::Receiver<Control>) -> Result<bool> {
    let offset = ctx.shared.lock().unwrap().map.get(id).map_or(0, |c| c.done);
    let mut file = ctx.tmp.open_chunk(id, offset)?;
    let mut stream = resp.bytes_stream();
    let mut window_start = Instant::now();
    let mut window_bytes: u64 = 0;
    while let Some(item) = stream.next().await {
        if *control.borrow() != Control::Run {
            return Err(DownloadError::Cancelled);
        }
        let bytes = item?;
        let (take, complete) = {
            let mut sh = ctx.shared.lock().unwrap();
            let Some(c) = sh.map.get_mut(id) else { return Ok(true) };
            let left = c.remaining();
            let take = (bytes.len() as u64).min(left) as usize;
            c.done += take as u64;
            let complete = c.is_complete();
            sh.last_data.insert(id, Instant::now());
            (take, complete)
        };
        file.write_all(&bytes[..take])?;
        window_bytes += take as u64;
        let el = window_start.elapsed();
        if el >= Duration::from_millis(500) {
            let sp = (window_bytes as f64 / el.as_secs_f64()) as u64;
            ctx.shared.lock().unwrap().speeds.insert(id, sp);
            window_start = Instant::now();
            window_bytes = 0;
        }
        ctx.limiter.throttle(take as u64).await;
        ctx.global_limiter.throttle(take as u64).await;
        if complete {
            file.flush()?;
            return Ok(true);
        }
    }
    file.flush()?;
    let complete = ctx.shared.lock().unwrap().map.get(id).is_none_or(|c| c.is_complete());
    Ok(complete)
}

/// One connection: download its chunk (retrying), then take over more work.
async fn worker(ctx: Arc<Ctx>, id: u32, first: Option<Response>, control: watch::Receiver<Control>) -> Result<WorkerExit> {
    let mut current = id;
    let r = worker_inner(&ctx, &mut current, first, control).await;
    let mut sh = ctx.shared.lock().unwrap();
    sh.assigned.remove(&current);
    sh.active.remove(&current);
    r
}

async fn worker_inner(ctx: &Arc<Ctx>, id_ref: &mut u32, mut first: Option<Response>, control: watch::Receiver<Control>) -> Result<WorkerExit> {
    let ctx = ctx.clone();
    let mut id = *id_ref;
    loop {
        *id_ref = id;
        ctx.shared.lock().unwrap().active.insert(id);
        let mut failures = 0u32;
        loop {
            let (pos, end, total) = {
                let sh = ctx.shared.lock().unwrap();
                let Some(c) = sh.map.get(id) else { break };
                (c.pos(), c.end, sh.map.total)
            };
            if pos >= end && total > 0 {
                break;
            }
            let resp = match first.take() {
                Some(r) => Ok(r),
                None => {
                    let to = if end >= total || total == 0 { None } else { Some(end - 1) };
                    ctx.request(Some((pos, to))).send().await
                }
            };
            let result = match resp {
                Ok(r) if r.status() == StatusCode::OK && pos > 0 => {
                    // Server ignored Range (or the file changed under If-Range).
                    ctx.shared.lock().unwrap().active.remove(&id);
                    return Ok(WorkerExit::NoRanges);
                }
                Ok(r) if r.status().is_success() => pump(&ctx, id, r, &control).await,
                Ok(r) => Err(DownloadError::Status(r.status())),
                Err(e) => Err(e.into()),
            };
            match result {
                Ok(true) => break,
                Ok(false) => {
                    // Stream ended early (unknown size: that's the end of the file).
                    if total == 0 {
                        break;
                    }
                }
                Err(DownloadError::Cancelled) => {
                    ctx.shared.lock().unwrap().active.remove(&id);
                    return Err(DownloadError::Cancelled);
                }
                Err(e) => {
                    failures += 1;
                    tracing::debug!("chunk {id}: {e} (attempt {failures})");
                    // The server refuses an extra connection while others work: give the
                    // part back and stop opening more connections for this download.
                    if failures >= 3 {
                        let mut sh = ctx.shared.lock().unwrap();
                        let fresh = sh.map.get(id).is_some_and(|c| c.done == 0);
                        let others = sh.active.iter().filter(|a| **a != id).count();
                        if fresh && others > 0 && sh.map.merge_into_prev(id).is_some() {
                            sh.conn_cap = Some(others.max(1));
                            sh.active.remove(&id);
                            sh.assigned.remove(&id);
                            tracing::info!("server refused another connection; limiting to {others}");
                            return Ok(WorkerExit::Done);
                        }
                    }
                    if ctx.cfg.max_retries > 0 && failures > ctx.cfg.max_retries {
                        ctx.shared.lock().unwrap().active.remove(&id);
                        return Err(DownloadError::Retries(id, e.to_string()));
                    }
                    let backoff = Duration::from_millis(500 * u64::from(failures.min(10)));
                    tokio::time::sleep(backoff).await;
                }
            }
            if *control.borrow() != Control::Run {
                ctx.shared.lock().unwrap().active.remove(&id);
                return Err(DownloadError::Cancelled);
            }
        }

        // Verify this chunk's boundaries with both neighbours.
        let left_neighbour = {
            let sh = ctx.shared.lock().unwrap();
            let i = sh.map.chunks.iter().position(|c| c.id == id);
            i.and_then(|i| i.checked_sub(1)).map(|p| sh.map.chunks[p].id)
        };
        for b in [Some(id), left_neighbour].into_iter().flatten() {
            if ctx.verify_boundary(b) == Some(false) {
                ctx.shared.lock().unwrap().active.remove(&id);
                return Ok(WorkerExit::Corrupt);
            }
        }

        // If the next part's connection hasn't started, take that part over directly.
        {
            let mut sh = ctx.shared.lock().unwrap();
            let next = sh.map.chunks.iter().position(|c| c.id == id).and_then(|i| sh.map.chunks.get(i + 1)).map(|c| (c.id, c.done));
            if let Some((nid, 0)) = next {
                if !sh.assigned.contains(&nid) && sh.map.merge_next(id).is_some() {
                    sh.verified.remove(&id);
                    continue;
                }
            }
        }

        // Connection reuse: take over half of the chunk with the most work left.
        let next = {
            let mut sh = ctx.shared.lock().unwrap();
            sh.active.remove(&id);
            sh.speeds.remove(&id);
            if sh.single_connection || sh.corrupted {
                None
            } else {
                let speeds = sh.speeds.clone();
                sh.map.split_for_new_connection(&speeds)
            }
        };
        match next {
            Some(n) => {
                let mut sh = ctx.shared.lock().unwrap();
                sh.assigned.remove(&id);
                sh.assigned.insert(n);
                id = n;
            }
            None => return Ok(WorkerExit::Done),
        }
    }
}

/// YouTube refuses a freshly issued stream link (HTTP 403) for roughly the first 10–15
/// seconds, then serves it normally; how long to keep asking before giving up.
const FRESH_LINK_WAIT: Duration = Duration::from_secs(60);

fn is_youtube_stream(url: &Url) -> bool {
    url.host_str().is_some_and(|h| h.ends_with(".googlevideo.com"))
}

async fn probe(ctx_client: &Client, url: &Url, req: &DownloadRequest, range_from: Option<u64>, validator: Option<&str>) -> Result<Response> {
    let started = Instant::now();
    let r = loop {
        let mut rb = match &req.post_data {
            Some(body) => ctx_client.post(url.clone()).body(body.clone()),
            None => ctx_client.get(url.clone()),
        };
        if let Some(from) = range_from {
            rb = rb.header(header::RANGE, format!("bytes={from}-"));
            if let Some(v) = validator {
                rb = rb.header(header::IF_RANGE, v);
            }
        }
        if let Some(u) = &req.username {
            rb = rb.basic_auth(u, req.password.as_deref());
        }
        let r = rb.send().await?;
        if r.status() == StatusCode::FORBIDDEN && is_youtube_stream(url) && started.elapsed() < FRESH_LINK_WAIT {
            drop(r);
            tokio::time::sleep(Duration::from_millis(2500)).await;
            continue;
        }
        break r;
    };
    if !r.status().is_success() {
        if std::env::var_os("RDM_YT_DEBUG").is_some() {
            eprintln!("[http] {} {} -> {} {:?}\n[http] response headers: {:?}", if req.post_data.is_some() { "POST" } else { "GET" }, url, r.status(), r.version(), r.headers());
        }
        return Err(DownloadError::Status(r.status()));
    }
    Ok(r)
}

async fn run(
    req: DownloadRequest,
    cfg: EngineConfig,
    temp_dir: PathBuf,
    global_limiter: Arc<Limiter>,
    limiter: Arc<Limiter>,
    progress: watch::Sender<Progress>,
    mut control: watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    let result = run_inner(&req, cfg, temp_dir, global_limiter, limiter, &progress, &mut control).await;
    progress.send_modify(|p| match &result {
        Ok(Some(path)) => {
            p.status = Status::Complete;
            p.final_path = Some(path.clone());
            p.speed = 0.0;
            p.connections = 0;
        }
        Ok(None) => {
            p.status = Status::Paused;
            p.speed = 0.0;
            p.connections = 0;
        }
        Err(e) => {
            p.status = Status::Failed(e.to_string());
            p.speed = 0.0;
            p.connections = 0;
        }
    });
    result
}

async fn run_inner(
    req: &DownloadRequest,
    mut cfg: EngineConfig,
    temp_dir: PathBuf,
    global_limiter: Arc<Limiter>,
    limiter: Arc<Limiter>,
    progress: &watch::Sender<Progress>,
    control: &mut watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    if let Some(n) = req.max_connections {
        cfg.max_connections = n.max(1);
    }
    // YouTube jobs become a plain stream (or video + audio pair) request first.
    if crate::youtube::is_job(&req.url) {
        let resolved = crate::youtube::resolve(req, &cfg).await?;
        return Box::pin(run_inner(&resolved, cfg, temp_dir, global_limiter, limiter, progress, control)).await;
    }
    let url = Url::parse(&req.url).map_err(|e| DownloadError::Other(format!("bad url: {e}")))?;
    let client = build_client(&cfg, req)?;
    let tmp = TempDir::new(&temp_dir)?;
    if crate::pair::audio_url(&req.url).is_some() {
        return crate::pair::run(req, &cfg, tmp, global_limiter, limiter, progress, control).await;
    }
    if url.scheme() == "ftp" {
        return crate::ftp::run(req, &cfg, tmp, global_limiter, limiter, progress, control).await;
    }
    if crate::hls::looks_like_hls(&req.url, None) {
        return crate::hls::run(req, client, cfg.max_connections, tmp, limiter, global_limiter, progress, control).await;
    }
    if crate::dash::looks_like_dash(&req.url, None) {
        return crate::dash::run(req, client, cfg.max_connections, tmp, limiter, global_limiter, progress, control).await;
    }
    let saved = tmp.load().ok().flatten().filter(|s| crate::youtube::same_stream(&s.url, &req.url) && s.resumable);

    // Probe (fresh) or validate (resume).
    let (map, info, first_resp, first_id, single) = match saved {
        Some(st) if !st.map.is_complete() => {
            let validator = st.etag.clone().or(st.last_modified.clone());
            let first_inc = st.map.chunks.iter().find(|c| !c.is_complete()).map(|c| (c.id, c.pos()));
            match first_inc {
                Some((cid, pos)) => {
                    let r = probe(&client, &url, req, Some(pos), validator.as_deref()).await?;
                    let si = ServerInfo::from_response(&r);
                    if r.status() == StatusCode::PARTIAL_CONTENT && si.total == st.total {
                        let info = SavedInfo {
                            file_name: st.file_name.clone(),
                            total: st.total,
                            etag: st.etag.clone(),
                            last_modified: st.last_modified.clone(),
                            content_type: st.content_type.clone(),
                            resumable: true,
                        };
                        (st.map, info, Some(r), cid, st.single_connection)
                    } else {
                        // File changed or ranges no longer honoured: start over.
                        tmp.reset()?;
                        drop(r);
                        fresh(&client, &url, req, &tmp).await?
                    }
                }
                None => {
                    let info = SavedInfo {
                        file_name: st.file_name.clone(),
                        total: st.total,
                        etag: st.etag,
                        last_modified: st.last_modified,
                        content_type: st.content_type,
                        resumable: true,
                    };
                    (st.map, info, None, 0, st.single_connection)
                }
            }
        }
        Some(st) => {
            let info = SavedInfo {
                file_name: st.file_name.clone(),
                total: st.total,
                etag: st.etag,
                last_modified: st.last_modified,
                content_type: st.content_type,
                resumable: true,
            };
            (st.map, info, None, 0, st.single_connection)
        }
        None => {
            tmp.reset()?;
            fresh(&client, &url, req, &tmp).await?
        }
    };

    // Playlist served without a .m3u8 extension.
    if crate::hls::looks_like_hls(&req.url, info.content_type.as_deref()) {
        drop(first_resp);
        tmp.reset()?;
        return crate::hls::run(req, client, cfg.max_connections, tmp, limiter, global_limiter, progress, control).await;
    }
    if crate::dash::looks_like_dash(&req.url, info.content_type.as_deref()) {
        drop(first_resp);
        tmp.reset()?;
        return crate::dash::run(req, client, cfg.max_connections, tmp, limiter, global_limiter, progress, control).await;
    }
    let validator = info.etag.clone().or(info.last_modified.clone());
    let ctx = Arc::new(Ctx {
        client,
        url,
        req: req.clone(),
        cfg: cfg.clone(),
        tmp,
        shared: Mutex::new(Shared {
            map,
            speeds: HashMap::new(),
            last_data: HashMap::new(),
            active: HashSet::new(),
            verified: HashSet::new(),
            corrupted: false,
            single_connection: single || !info.resumable,
            assigned: HashSet::new(),
            conn_cap: None,
        }),
        limiter,
        global_limiter,
        validator,
    });
    ctx.save(&info);
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.file_name = info.file_name.clone();
        p.total = info.total;
        p.resumable = info.resumable;
    });

    let mut workers: JoinSet<(u32, Result<WorkerExit>)> = JoinSet::new();
    let worker_control = control.clone();
    let spawn = |workers: &mut JoinSet<(u32, Result<WorkerExit>)>, id: u32, first: Option<Response>| {
        ctx.shared.lock().unwrap().assigned.insert(id);
        let c = ctx.clone();
        let ctl = worker_control.clone();
        workers.spawn(async move { (id, worker(c, id, first, ctl).await) });
    };

    // Chunk 0 (or the first incomplete chunk) uses the probe's response.
    let mut started: HashSet<u32> = HashSet::new();
    let incomplete: Vec<u32> = ctx.shared.lock().unwrap().map.incomplete_ids();
    let mut first_resp = first_resp;
    if incomplete.contains(&first_id) {
        spawn(&mut workers, first_id, first_resp.take());
        started.insert(first_id);
    }
    drop(first_resp);
    let single_now = ctx.shared.lock().unwrap().single_connection;
    // Resume: reconnect every incomplete chunk at once.
    for id in incomplete {
        if started.len() >= cfg.max_connections || single_now {
            break;
        }
        if started.insert(id) {
            spawn(&mut workers, id, None);
        }
    }
    // Fresh: split straight into the initial connection count.
    if !single_now {
        while started.len() < cfg.initial_connections.min(cfg.max_connections) {
            let id = {
                let mut sh = ctx.shared.lock().unwrap();
                let speeds = sh.speeds.clone();
                sh.map.split_for_new_connection(&speeds)
            };
            match id {
                Some(id) => {
                    started.insert(id);
                    spawn(&mut workers, id, None);
                }
                None => break,
            }
        }
    }

    let mut ui_tick = tokio::time::interval(cfg.ui_interval);
    let mut last_save = Instant::now();
    let mut last_ramp = Instant::now();
    let mut last_orphan = Instant::now();
    let mut speed_t = Instant::now();
    let mut speed_bytes = ctx.shared.lock().unwrap().map.downloaded();
    let mut speed = 0.0f64;
    let mut restart_single = false;

    loop {
        tokio::select! {
            joined = workers.join_next(), if !workers.is_empty() => {
                if let Some(joined) = joined {
                    let (id, res) = joined.map_err(|e| DownloadError::Other(e.to_string()))?;
                    match res {
                        Ok(WorkerExit::Done) => {}
                        Ok(WorkerExit::NoRanges) | Ok(WorkerExit::Corrupt) => {
                            restart_single = true;
                            tracing::warn!("chunk {id}: inconsistent range support, falling back to one connection");
                        }
                        Err(DownloadError::Cancelled) => {}
                        Err(e) => {
                            workers.abort_all();
                            ctx.save(&info);
                            return Err(e);
                        }
                    }
                }
            }
            _ = control.changed() => {
                let c = *control.borrow();
                if c != Control::Run {
                    workers.abort_all();
                    while workers.join_next().await.is_some() {}
                    ctx.save(&info);
                    if c == Control::Cancel {
                        let _ = ctx.tmp.clone().remove();
                        return Err(DownloadError::Cancelled);
                    }
                    return Ok(None);
                }
            }
            _ = ui_tick.tick() => {
                let (downloaded, chunks, conns, all_receiving, speeds) = {
                    let sh = ctx.shared.lock().unwrap();
                    let now = Instant::now();
                    let all_receiving = !sh.active.is_empty() && sh.active.iter().all(|id| {
                        sh.last_data.get(id).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(1))
                    });
                    (sh.map.downloaded(), sh.snapshot(), sh.active.len(), all_receiving, sh.speeds.clone())
                };
                let el = speed_t.elapsed().as_secs_f64();
                if el >= 0.5 {
                    let inst = downloaded.saturating_sub(speed_bytes) as f64 / el;
                    speed = if speed == 0.0 { inst } else { speed * 0.6 + inst * 0.4 };
                    speed_t = Instant::now();
                    speed_bytes = downloaded;
                }
                progress.send_modify(|p| {
                    p.downloaded = downloaded;
                    p.chunks = chunks;
                    p.connections = conns;
                    p.speed = speed;
                });
                if last_save.elapsed() >= cfg.save_interval {
                    ctx.save(&info);
                    last_save = Instant::now();
                }
                // Ramp-up: double the connection count while every connection is productive.
                let (single_now, cap, orphans, assigned) = {
                    let sh = ctx.shared.lock().unwrap();
                    let cap = sh.conn_cap.unwrap_or(cfg.max_connections).min(cfg.max_connections);
                    let orphans: Vec<u32> = sh.map.chunks.iter().filter(|c| !c.is_complete() && !sh.assigned.contains(&c.id)).map(|c| c.id).collect();
                    (sh.single_connection, cap, orphans, sh.assigned.len())
                };
                // Parts without a connection (e.g. handed back) get one when there is room.
                if !restart_single && !orphans.is_empty() && last_orphan.elapsed() >= Duration::from_secs(2) {
                    for oid in orphans.into_iter().take(cap.saturating_sub(assigned)) {
                        spawn(&mut workers, oid, None);
                    }
                    last_orphan = Instant::now();
                }
                let near_limit = ctx.limiter.near_limit(speed) || ctx.global_limiter.near_limit(speed);
                if !single_now && !restart_single && all_receiving && !near_limit
                    && conns < cap && last_ramp.elapsed() >= cfg.ramp_interval
                {
                    let target = (conns * 2).min(cap);
                    for _ in conns..target {
                        let id = {
                            let mut sh = ctx.shared.lock().unwrap();
                            let speeds = speeds.clone();
                            sh.map.split_for_new_connection(&speeds)
                        };
                        match id {
                            Some(id) => spawn(&mut workers, id, None),
                            None => break,
                        }
                    }
                    last_ramp = Instant::now();
                }
            }
        }

        if restart_single && workers.is_empty() {
            // Inconsistent ranges: discard everything and use a single connection.
            restart_single = false;
            ctx.tmp.reset()?;
            {
                let mut sh = ctx.shared.lock().unwrap();
                let total = sh.map.total;
                sh.map = ChunkMap::new(total);
                sh.single_connection = true;
                sh.verified.clear();
                sh.corrupted = false;
            }
            ctx.save(&info);
            spawn(&mut workers, 0, None);
        } else if restart_single {
            // Stop the other connections first.
            ctx.shared.lock().unwrap().single_connection = true;
            workers.abort_all();
        }

        if workers.is_empty() && !restart_single {
            let complete = {
                let sh = ctx.shared.lock().unwrap();
                sh.map.total == 0 || sh.map.is_complete()
            };
            if complete {
                break;
            }
            // Work left but no connections (e.g. all chunks reverted): restart missing ones.
            let ids = ctx.shared.lock().unwrap().map.incomplete_ids();
            for id in ids.into_iter().take(cfg.max_connections) {
                spawn(&mut workers, id, None);
            }
        }
    }

    // Final boundary check over every neighbour pair.
    let lefts: Vec<u32> = {
        let sh = ctx.shared.lock().unwrap();
        sh.map.chunks.iter().take(sh.map.chunks.len().saturating_sub(1)).map(|c| c.id).collect()
    };
    for l in lefts {
        if ctx.verify_boundary(l) == Some(false) {
            return Err(DownloadError::Other("chunk overlap mismatch after completion".into()));
        }
    }

    progress.send_modify(|p| p.status = Status::Assembling);
    ctx.save(&info);
    let (map, total) = {
        let sh = ctx.shared.lock().unwrap();
        let total = if sh.map.total == 0 { sh.map.chunks.iter().map(|c| c.done).sum() } else { sh.map.total };
        (sh.map.clone(), total)
    };
    let dest = match &req.file_name {
        Some(_) => req.dir.join(&info.file_name),
        None => naming::unique_path(&req.dir, &info.file_name),
    };
    let tmp = ctx.tmp.clone();
    let block = cfg.join_block;
    let watch_file = dest.clone();
    let path = crate::join::with_progress(progress, total, crate::join::file_size(watch_file), move || assemble(&tmp, &map, total, &dest, block)).await?;
    let _ = ctx.tmp.clone().remove();
    progress.send_modify(|p| {
        p.downloaded = total;
        p.total = Some(total);
    });
    Ok(Some(path))
}

/// Start from scratch: probe and decide the name, size and range support.
async fn fresh(
    client: &Client,
    url: &Url,
    req: &DownloadRequest,
    tmp: &TempDir,
) -> Result<(ChunkMap, SavedInfo, Option<Response>, u32, bool)> {
    let r = probe(client, url, req, None, None).await?;
    let si = ServerInfo::from_response(&r);
    let final_url = si.final_url.clone().unwrap_or_else(|| url.clone());
    let file_name = match &req.file_name {
        Some(n) => naming::sanitize(n),
        None => naming::choose(&final_url, si.disposition.as_deref(), si.content_type.as_deref()),
    };
    let resumable = si.accept_ranges && si.total.is_some();
    let map = ChunkMap::new(si.total.unwrap_or(0));
    let info = SavedInfo {
        file_name,
        total: si.total,
        etag: si.etag,
        last_modified: si.last_modified,
        content_type: si.content_type,
        resumable,
    };
    let _ = tmp;
    Ok((map, info, Some(r), 0, !resumable))
}

/// What a URL points to, learned without downloading the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub file_name: String,
    pub total: Option<u64>,
    pub content_type: Option<String>,
    pub resumable: bool,
    pub final_url: String,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
}

/// Request the URL, read only the headers, and report name/size/range support.
pub async fn probe_info(req: &DownloadRequest, cfg: &EngineConfig) -> Result<ProbeInfo> {
    // A YouTube job isn't fetched until it starts; report what the extension knows.
    if crate::youtube::is_job(&req.url) {
        return Ok(crate::youtube::probe_info(req));
    }
    let url = Url::parse(&req.url).map_err(|e| DownloadError::Other(format!("bad url: {e}")))?;
    let client = build_client(cfg, req)?;
    let r = probe(&client, &url, req, Some(0), None).await?;
    let si = ServerInfo::from_response(&r);
    drop(r);
    let final_url = si.final_url.clone().unwrap_or(url);
    let file_name = match &req.file_name {
        Some(n) => naming::sanitize(n),
        None => naming::choose(&final_url, si.disposition.as_deref(), si.content_type.as_deref()),
    };
    Ok(ProbeInfo {
        file_name,
        total: si.total,
        content_type: si.content_type,
        resumable: si.accept_ranges && si.total.is_some(),
        final_url: final_url.to_string(),
        last_modified: si.last_modified,
        etag: si.etag,
    })
}
