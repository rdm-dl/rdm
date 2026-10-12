//! HLS from a server that only answers requests carrying the embedded player's Origin and
//! Referer (as stream CDNs commonly do), with a playlist URL that has no .m3u8 in it.

use rdm_core::{Download, DownloadRequest, EngineConfig, Limiter};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PLAYER: &str = "https://player.example";

async fn serve(files: HashMap<String, (Vec<u8>, &'static str)>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let files = Arc::new(files);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let files = files.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let allowed = head.contains(&format!("\r\norigin: {PLAYER}\r\n")) && head.contains(&format!("\r\nreferer: {PLAYER}/\r\n"));
                let (status, body, ctype) = match (allowed, files.get(&path)) {
                    (false, _) => ("403 Forbidden", vec![], "text/plain"),
                    (true, Some((b, t))) => ("200 OK", b.clone(), *t),
                    (true, None) => ("404 Not Found", vec![], "text/plain"),
                };
                let hdr = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_needs_the_players_origin_and_referer() {
    let mut files = HashMap::new();
    let mut expected = vec![];
    let mut media = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
    for i in 0..6u8 {
        let seg = vec![i.wrapping_mul(37); 40_000];
        expected.extend_from_slice(&seg);
        files.insert(format!("/seg/{i}"), (seg, "video/mp2t"));
        media.push_str(&format!("#EXTINF:4,\n/seg/{i}\n"));
    }
    media.push_str("#EXT-X-ENDLIST\n");
    files.insert("/pl/media".into(), (media.into_bytes(), "text/plain"));
    files.insert("/pl/abc".into(), (b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=1280x720\n/pl/media\n".to_vec(), "text/plain"));
    let port = serve(files).await;
    let base = std::env::temp_dir().join(format!("rdm_hls_hdr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let url = format!("http://127.0.0.1:{port}/pl/abc#rdm-kind=hls");

    // As the extension sends it: the player's Referer and Origin.
    let req = DownloadRequest {
        url: url.clone(),
        dir: base.join("out"),
        file_name: Some("movie".into()),
        referer: Some(format!("{PLAYER}/")),
        headers: vec![("Origin".into(), PLAYER.into())],
        ..Default::default()
    };
    let path = Download::start(req, EngineConfig::default(), base.join("tmp1"), Arc::new(Limiter::default())).wait().await.unwrap().unwrap();
    if path.extension().unwrap() == "ts" {
        assert_eq!(std::fs::read(&path).unwrap(), expected);
    } else {
        assert!(path.exists());
    }

    // With only the page's address (the old behaviour) the server refuses.
    let req = DownloadRequest { url, dir: base.join("out2"), referer: Some("https://page.example/movie".into()), ..Default::default() };
    let r = Download::start(req, EngineConfig::default(), base.join("tmp2"), Arc::new(Limiter::default())).wait().await;
    assert!(matches!(r, Err(rdm_core::DownloadError::Status(s)) if s.as_u16() == 403), "{r:?}");
    let _ = std::fs::remove_dir_all(&base);
}
