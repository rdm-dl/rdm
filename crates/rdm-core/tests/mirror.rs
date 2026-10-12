//! Site grabber offline copy: pages, stylesheet, images (including one only referenced by
//! the stylesheet) saved locally with links rewritten.

use rdm_core::grabber::{self, GrabberProject, GrabberStatus};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

async fn serve(files: HashMap<&'static str, (&'static str, Vec<u8>)>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let files = Arc::new(files);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let files = files.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, ctype, body) = match files.get(path.as_str()) {
                    Some((t, b)) => ("200 OK", *t, b.clone()),
                    None => ("404 Not Found", "text/plain", vec![]),
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
async fn mirrors_a_site_for_offline_browsing() {
    let html = |s: &str| ("text/html", s.as_bytes().to_vec());
    let mut files = HashMap::new();
    files.insert("/", html(r#"<html><head><link rel="stylesheet" href="/css/site.css"></head><body><a href="docs/guide.html#intro">Guide</a> <img src="img/logo.png"> <a href="https://example.org/">ext</a> <a href="/files/report.pdf">pdf</a></body></html>"#));
    files.insert("/docs/guide.html", html(r#"<a href="../">home</a><img src="/img/logo.png"><a href="/docs/missing.html">gone</a>"#));
    files.insert("/css/site.css", ("text/css", b"body{background:url('../img/bg.png')}".to_vec()));
    files.insert("/img/logo.png", ("image/png", vec![1; 300]));
    files.insert("/img/bg.png", ("image/png", vec![2; 200]));
    files.insert("/files/report.pdf", ("application/pdf", vec![3; 500]));
    let port = serve(files).await;
    let root = std::env::temp_dir().join(format!("rdm_mirror_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let project = GrabberProject {
        start_url: format!("http://127.0.0.1:{port}/"),
        depth: 2,
        include: grabber::preset("documents"),
        respect_robots: false,
        mirror_dir: Some(root.clone()),
        ..Default::default()
    };
    let (tx, rx) = watch::channel(GrabberStatus::default());
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    grabber::run(project, tx, cancel_rx).await;
    let st = rx.borrow().clone();
    assert!(st.error.is_none(), "{:?}", st.error);
    let site = root.join(format!("127.0.0.1_{port}"));
    let index = st.mirror_index.clone().expect("start page saved");
    assert_eq!(index, site.join("index.html"));

    let read = |p: &str| std::fs::read_to_string(site.join(p)).unwrap();
    let home = read("index.html");
    assert!(home.contains(r#"href="css/site.css""#), "{home}");
    assert!(home.contains(r#"href="docs/guide.html#intro""#), "{home}");
    assert!(home.contains(r#"src="img/logo.png""#), "{home}");
    assert!(home.contains(r#"href="files/report.pdf""#), "{home}");
    assert!(home.contains(r#"href="https://example.org/""#), "{home}");
    let guide = read("docs/guide.html");
    assert!(guide.contains(r#"href="../index.html""#), "{guide}");
    assert!(guide.contains(r#"src="../img/logo.png""#), "{guide}");
    assert!(guide.contains(&format!(r#"href="http://127.0.0.1:{port}/docs/missing.html""#)), "{guide}");
    assert_eq!(read("css/site.css"), "body{background:url('../img/bg.png')}");
    assert_eq!(std::fs::read(site.join("img/bg.png")).unwrap(), vec![2; 200]);
    assert_eq!(std::fs::read(site.join("files/report.pdf")).unwrap(), vec![3; 500]);
    assert_eq!(st.files.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}
