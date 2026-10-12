use rdm_core::grabber::{GrabberProject, GrabberStatus, preset, run};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

async fn serve(pages: HashMap<&'static str, (&'static str, &'static str)>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let pages = Arc::new(pages);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let pages = pages.clone();
            tokio::spawn(async move {
                let mut b = vec![0u8; 4096];
                let n = s.read(&mut b).await.unwrap_or(0);
                let path = String::from_utf8_lossy(&b[..n]).split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, ct, body) = match pages.get(path.as_str()) {
                    Some((ct, body)) => ("200 OK", *ct, *body),
                    None => ("404 Not Found", "text/plain", ""),
                };
                let r = format!("HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(r.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
async fn crawls_two_levels_and_respects_robots() {
    let mut pages = HashMap::new();
    pages.insert("/", ("text/html", r#"<a href="/gallery/">g</a> <img src="/logo.png"> <a href="/private/p.html">p</a>"#));
    pages.insert("/gallery/", ("text/html", r#"<a href="a.jpg">a</a> <a href="b.JPG">b</a> <a href="deeper.html">d</a> <a href="doc.pdf">x</a>"#));
    pages.insert("/gallery/deeper.html", ("text/html", r#"<img src="c.jpg">"#));
    pages.insert("/private/p.html", ("text/html", r#"<img src="secret.jpg">"#));
    pages.insert("/robots.txt", ("text/plain", "User-agent: *\nDisallow: /private\n"));
    let port = serve(pages).await;

    let project = GrabberProject { start_url: format!("http://127.0.0.1:{port}/"), depth: 1, include: preset("images"), ..Default::default() };
    let (tx, rx) = watch::channel(GrabberStatus::default());
    let (_c, crx) = watch::channel(false);
    run(project, tx, crx).await;
    let s = rx.borrow().clone();
    let mut files: Vec<String> = s.files.iter().map(|f| f.url.rsplit('/').next().unwrap().to_string()).collect();
    files.sort();
    // depth 1: start page + /gallery/ explored; deeper.html (level 2) and /private (robots) are not.
    assert_eq!(files, vec!["a.jpg", "b.JPG", "logo.png"]);
    assert!(s.done);
    assert_eq!(s.pages_explored, 2);
}
