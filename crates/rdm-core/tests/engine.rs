//! End-to-end tests against a tiny in-process HTTP server.

use rdm_core::{Download, DownloadRequest, EngineConfig, Limiter, Status};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn content(len: usize) -> Arc<Vec<u8>> {
    Arc::new((0..len).map(|i| ((i * 31 + i / 7) % 251) as u8).collect())
}

struct Server {
    port: u16,
    ranged_requests: Arc<AtomicUsize>,
}

/// Serve `data`; `ranges` toggles Range support; `rate` = bytes/s per connection.
async fn serve(data: Arc<Vec<u8>>, ranges: bool, rate: usize) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let ranged = Arc::new(AtomicUsize::new(0));
    let ranged2 = ranged.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let data = data.clone();
            let ranged = ranged2.clone();
            tokio::spawn(async move {
                loop {
                    let mut buf = vec![0u8; 8192];
                    let mut n = 0;
                    loop {
                        match sock.read(&mut buf[n..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(k) => n += k,
                        }
                        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let range = head.lines().find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("range: bytes=").map(|r| r.trim().to_string())
                    });
                    let total = data.len();
                    let (start, end, status) = match (ranges, range) {
                        (true, Some(r)) => {
                            ranged.fetch_add(1, Ordering::SeqCst);
                            let (a, b) = r.split_once('-').unwrap();
                            let a: usize = a.parse().unwrap();
                            let b: usize = if b.is_empty() { total - 1 } else { b.parse::<usize>().unwrap().min(total - 1) };
                            (a, b, 206)
                        }
                        _ => (0, total - 1, 200),
                    };
                    let mut hdr = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"data.bin\"\r\nETag: \"v1\"\r\n",
                        if status == 206 { "Partial Content" } else { "OK" },
                        end - start + 1
                    );
                    if ranges {
                        hdr.push_str("Accept-Ranges: bytes\r\n");
                    }
                    if status == 206 {
                        hdr.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
                    }
                    hdr.push_str("\r\n");
                    if sock.write_all(hdr.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut pos = start;
                    let step = (rate / 20).max(1024);
                    while pos <= end {
                        let to = (pos + step).min(end + 1);
                        if sock.write_all(&data[pos..to]).await.is_err() {
                            return;
                        }
                        pos = to;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            });
        }
    });
    Server { port, ranged_requests: ranged }
}

fn dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("rdm_it_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    (base.join("out"), base.join("tmp"))
}

fn fast_cfg() -> EngineConfig {
    EngineConfig { ramp_interval: Duration::from_millis(300), ..EngineConfig::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_download_is_byte_exact() {
    let data = content(3 * 1024 * 1024);
    let srv = serve(data.clone(), true, 400 * 1024).await;
    let (out, tmp) = dirs("seg");
    let req = DownloadRequest { url: format!("http://127.0.0.1:{}/data.bin", srv.port), dir: out.clone(), ..Default::default() };
    let dl = Download::start(req, fast_cfg(), tmp, Arc::new(Limiter::default()));
    let mut prog = dl.progress.clone();
    let mut max_conns = 0;
    let watcher = tokio::spawn(async move {
        while prog.changed().await.is_ok() {
            let p = prog.borrow().clone();
            max_conns = max_conns.max(p.connections);
            if matches!(p.status, Status::Complete | Status::Failed(_)) {
                break;
            }
        }
        max_conns
    });
    let path = dl.wait().await.unwrap().unwrap();
    let max_conns = watcher.await.unwrap();
    assert_eq!(path.file_name().unwrap(), "data.bin");
    assert_eq!(std::fs::read(&path).unwrap(), *data);
    assert!(max_conns >= 4, "expected at least 4 connections, saw {max_conns}");
    assert!(srv.ranged_requests.load(Ordering::SeqCst) >= 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume() {
    let data = content(2 * 1024 * 1024);
    let srv = serve(data.clone(), true, 48 * 1024).await;
    let (out, tmp) = dirs("resume");
    let req = DownloadRequest { url: format!("http://127.0.0.1:{}/data.bin", srv.port), dir: out.clone(), ..Default::default() };
    let dl = Download::start(req.clone(), fast_cfg(), tmp.clone(), Arc::new(Limiter::default()));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    dl.pause();
    assert!(dl.wait().await.unwrap().is_none(), "pause should return None");
    assert!(tmp.join("state.json").exists());
    let dl = Download::start(req, fast_cfg(), tmp.clone(), Arc::new(Limiter::default()));
    let path = dl.wait().await.unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), *data);
    assert!(!tmp.exists(), "temp folder should be removed after assembly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_ranges_uses_one_connection() {
    let data = content(512 * 1024);
    let srv = serve(data.clone(), false, 2 * 1024 * 1024).await;
    let (out, tmp) = dirs("norange");
    let req = DownloadRequest { url: format!("http://127.0.0.1:{}/data.bin", srv.port), dir: out, ..Default::default() };
    let dl = Download::start(req, fast_cfg(), tmp, Arc::new(Limiter::default()));
    let path = dl.wait().await.unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), *data);
    assert_eq!(srv.ranged_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speed_limit_is_respected() {
    let data = content(600 * 1024);
    let srv = serve(data.clone(), true, 4 * 1024 * 1024).await;
    let (out, tmp) = dirs("limit");
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{}/data.bin", srv.port),
        dir: out,
        speed_limit: Some(200 * 1024),
        ..Default::default()
    };
    let t = std::time::Instant::now();
    let dl = Download::start(req, fast_cfg(), tmp, Arc::new(Limiter::default()));
    let path = dl.wait().await.unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), *data);
    // 600 KB at 200 KB/s should take roughly 3 s, certainly more than 1.5 s.
    assert!(t.elapsed() > Duration::from_millis(1500), "finished too fast: {:?}", t.elapsed());
}
