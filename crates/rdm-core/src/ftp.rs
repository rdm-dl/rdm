//! FTP downloads: one FTP session per connection, `REST` to start mid-file, segmented
//! like HTTP when the server reports the file size, with resume across restarts.

use crate::chunks::ChunkMap;
use crate::config::EngineConfig;
use crate::download::{ChunkView, Control, DownloadError, DownloadRequest, Progress, Status};
use crate::limiter::Limiter;
use crate::naming;
use crate::storage::{SavedState, TempDir, assemble};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio::task::JoinSet;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

fn err(e: impl std::fmt::Display) -> DownloadError {
    DownloadError::Other(format!("ftp: {e}"))
}

fn remote_path(url: &Url) -> String {
    percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy().into_owned()
}

async fn session(url: &Url, req: &DownloadRequest, timeout: Duration) -> Result<AsyncFtpStream> {
    let host = url.host_str().ok_or_else(|| err("no host"))?.to_string();
    let port = url.port().unwrap_or(21);
    let mut s = tokio::time::timeout(timeout, AsyncFtpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| err("connect timeout"))?
        .map_err(err)?;
    let user = req
        .username
        .clone()
        .or_else(|| Some(url.username().to_string()).filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "anonymous".into());
    let pass = req.password.clone().or_else(|| url.password().map(str::to_string)).unwrap_or_else(|| "anonymous@".into());
    s.login(user.as_str(), pass.as_str()).await.map_err(err)?;
    s.transfer_type(FileType::Binary).await.map_err(err)?;
    Ok(s)
}

/// Fetch `[pos, end)` of the file into chunk `id`'s temp file (end = None: to EOF).
async fn fetch_range(
    url: &Url,
    req: &DownloadRequest,
    cfg: &EngineConfig,
    tmp: &TempDir,
    map: &Mutex<ChunkMap>,
    id: u32,
    limiters: &[Arc<Limiter>],
    control: &watch::Receiver<Control>,
) -> Result<()> {
    let (pos, end, done) = {
        let m = map.lock().unwrap();
        let c = m.get(id).ok_or_else(|| err("chunk vanished"))?;
        (c.pos(), c.end, c.done)
    };
    let known = map.lock().unwrap().total > 0;
    if known && pos >= end {
        return Ok(());
    }
    let mut s = session(url, req, cfg.timeout).await?;
    if pos > 0 {
        s.resume_transfer(pos as usize).await.map_err(err)?;
    }
    let mut stream = s.retr_as_stream(remote_path(url)).await.map_err(err)?;
    let mut file = tmp.open_chunk(id, done)?;
    let mut buf = vec![0u8; 65536];
    loop {
        if *control.borrow() != Control::Run {
            return Err(DownloadError::Cancelled);
        }
        let read = tokio::time::timeout(cfg.timeout, stream.read(&mut buf)).await;
        let n = match read {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(err("read timeout")),
        };
        if n == 0 {
            break;
        }
        let (take, complete) = {
            let mut m = map.lock().unwrap();
            let total = m.total;
            let Some(c) = m.get_mut(id) else { break };
            let take = if total > 0 { (n as u64).min(c.remaining()) as usize } else { n };
            c.done += take as u64;
            (take, total > 0 && c.is_complete())
        };
        file.write_all(&buf[..take])?;
        for l in limiters {
            l.throttle(take as u64).await;
        }
        if complete {
            break;
        }
    }
    file.flush()?;
    // Dropping the stream closes the data connection (servers may log "aborted").
    drop(stream);
    let _ = s.quit().await;
    Ok(())
}

/// Download an FTP URL. Returns the final path, or `None` when paused.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    req: &DownloadRequest,
    cfg: &EngineConfig,
    tmp: TempDir,
    limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
    progress: &watch::Sender<Progress>,
    control: &mut watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    let url = Url::parse(&req.url).map_err(|e| err(e))?;
    // Size (and a connectivity check) from a first session.
    let size = {
        let mut attempt = 0u64;
        let mut s = loop {
            attempt += 1;
            match session(&url, req, cfg.timeout).await {
                Ok(s) => break s,
                Err(e) if attempt >= 5 => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_secs(2 * attempt)).await,
            }
        };
        let size = s.size(remote_path(&url)).await.ok().map(|v| v as u64);
        let _ = s.quit().await;
        size
    };
    let file_name = req
        .file_name
        .clone()
        .map(|n| naming::sanitize(&n))
        .or_else(|| naming::from_url(&url).map(|n| naming::sanitize(&n)))
        .unwrap_or_else(|| "download".into());
    let saved = tmp.load().ok().flatten().filter(|s| s.url == req.url && s.total == size);
    let mut map = match saved {
        Some(s) => s.map,
        None => {
            tmp.reset()?;
            ChunkMap::new(size.unwrap_or(0))
        }
    };
    let resumable = size.is_some();
    if resumable && map.chunks.len() == 1 && map.chunks[0].done == 0 {
        // Split into the initial connection count (FTP servers often cap this low).
        for _ in 1..cfg.initial_connections.min(cfg.max_connections) {
            if map.split_for_new_connection(&HashMap::new()).is_none() {
                break;
            }
        }
    }
    let map = Arc::new(Mutex::new(map));
    let save = |map: &Mutex<ChunkMap>| {
        let st = SavedState {
            url: req.url.clone(),
            file_name: file_name.clone(),
            total: size,
            etag: None,
            last_modified: None,
            content_type: None,
            resumable,
            single_connection: !resumable,
            map: map.lock().unwrap().clone(),
        };
        let _ = tmp.save(&st);
    };
    save(&map);
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.file_name = file_name.clone();
        p.total = size;
        p.resumable = resumable;
    });

    let limiters = vec![limiter, global_limiter];
    let mut set: JoinSet<(u32, Result<()>)> = JoinSet::new();
    let ids: Vec<u32> = map.lock().unwrap().incomplete_ids();
    let ids = if ids.is_empty() && size.is_none() { vec![0] } else { ids };
    for id in ids {
        let (url, req, cfg, tmp, map, limiters, ctl) =
            (url.clone(), req.clone(), cfg.clone(), tmp.clone(), map.clone(), limiters.clone(), control.clone());
        set.spawn(async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match fetch_range(&url, &req, &cfg, &tmp, &map, id, &limiters, &ctl).await {
                    Ok(()) => {
                        let complete = map.lock().unwrap().get(id).is_none_or(|c| c.is_complete());
                        if complete || size.is_none() {
                            return (id, Ok(()));
                        }
                    }
                    Err(DownloadError::Cancelled) => return (id, Err(DownloadError::Cancelled)),
                    Err(e) if cfg.max_retries > 0 && attempt > cfg.max_retries => return (id, Err(e)),
                    Err(_) => {}
                }
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt.min(10)))).await;
            }
        });
    }

    let mut ui = tokio::time::interval(cfg.ui_interval);
    let mut last_save = Instant::now();
    let (mut st, mut sb, mut speed) = (Instant::now(), map.lock().unwrap().downloaded(), 0.0f64);
    while !set.is_empty() {
        tokio::select! {
            j = set.join_next() => {
                if let Some(j) = j {
                    let (_, r) = j.map_err(|e| err(e))?;
                    match r {
                        Ok(()) | Err(DownloadError::Cancelled) => {}
                        Err(e) => {
                            set.abort_all();
                            save(&map);
                            return Err(e);
                        }
                    }
                }
            }
            _ = control.changed() => {
                if *control.borrow() != Control::Run {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    save(&map);
                    if *control.borrow() == Control::Cancel {
                        let _ = tmp.clone().remove();
                        return Err(DownloadError::Cancelled);
                    }
                    return Ok(None);
                }
            }
            _ = ui.tick() => {
                let (downloaded, chunks) = {
                    let m = map.lock().unwrap();
                    let views = m.chunks.iter().map(|c| ChunkView { id: c.id, start: c.start, end: c.end, pos: c.pos(), active: !c.is_complete() }).collect::<Vec<_>>();
                    (m.downloaded(), views)
                };
                let el = st.elapsed().as_secs_f64();
                if el >= 0.5 {
                    let inst = downloaded.saturating_sub(sb) as f64 / el;
                    speed = if speed == 0.0 { inst } else { speed * 0.6 + inst * 0.4 };
                    st = Instant::now();
                    sb = downloaded;
                }
                let conns = set.len();
                progress.send_modify(|p| {
                    p.downloaded = downloaded;
                    p.chunks = chunks;
                    p.speed = speed;
                    p.connections = conns;
                });
                if last_save.elapsed() >= cfg.save_interval {
                    save(&map);
                    last_save = Instant::now();
                }
            }
        }
    }

    progress.send_modify(|p| p.status = Status::Assembling);
    save(&map);
    let m = map.lock().unwrap().clone();
    let total = size.unwrap_or_else(|| m.chunks.iter().map(|c| c.done).sum());
    let mut m2 = m.clone();
    if size.is_none() {
        m2.chunks[0].end = total;
    }
    std::fs::create_dir_all(&req.dir)?;
    let dest = match &req.file_name {
        Some(_) => req.dir.join(&file_name),
        None => naming::unique_path(&req.dir, &file_name),
    };
    let block = cfg.join_block;
    let tmp2 = tmp.clone();
    let path = tokio::task::spawn_blocking(move || assemble(&tmp2, &m2, total, &dest, block))
        .await
        .map_err(|e| err(e))??;
    let _ = tmp.remove();
    progress.send_modify(|p| {
        p.total = Some(total);
        p.downloaded = total;
    });
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_remote_path() {
        let u = Url::parse("ftp://host/pub/my%20file.zip").unwrap();
        assert_eq!(remote_path(&u), "/pub/my file.zip");
    }
}
