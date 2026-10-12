//! Video + audio streams downloaded side by side and joined natively.

use rdm_core::{Download, DownloadRequest, EngineConfig, Limiter};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    bx(kind, &[&[0u8, 0, 0, 0][..], body].concat())
}

fn fmp4(timescale: u32, n: u32, fill: u8, frag: usize) -> Vec<u8> {
    let mut tkhd = vec![0u8; 8];
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 68]);
    let mut mdhd = vec![0u8; 8];
    mdhd.extend_from_slice(&timescale.to_be_bytes());
    mdhd.extend_from_slice(&[0u8; 8]);
    let trak = bx(b"trak", &[full(b"tkhd", &tkhd), bx(b"mdia", &full(b"mdhd", &mdhd))].concat());
    let trex = [1u32.to_be_bytes().to_vec(), vec![0u8; 16]].concat();
    let moov = bx(b"moov", &[full(b"mvhd", &[0u8; 96]), trak, bx(b"mvex", &full(b"trex", &trex))].concat());
    let mut out = [bx(b"ftyp", b"isom\0\0\0\0"), moov].concat();
    for i in 0..n {
        let tfhd = [vec![0u8, 0x02, 0, 0], 1u32.to_be_bytes().to_vec()].concat();
        let tfdt = [vec![1u8, 0, 0, 0], (u64::from(i) * u64::from(timescale)).to_be_bytes().to_vec()].concat();
        let traf = bx(b"traf", &[bx(b"tfhd", &tfhd), bx(b"tfdt", &tfdt)].concat());
        out.extend(bx(b"moof", &[full(b"mfhd", &(i + 1).to_be_bytes()), traf].concat()));
        out.extend(bx(b"mdat", &vec![fill.wrapping_add(i as u8); frag]));
    }
    out
}

async fn serve(files: HashMap<String, Vec<u8>>) -> u16 {
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
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap().to_string();
                let Some(body) = files.get(&path) else {
                    let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                    return;
                };
                let range = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_string));
                let (start, end) = match range {
                    Some(r) => {
                        let (a, b) = r.trim().split_once('-').unwrap();
                        let a: usize = a.parse().unwrap();
                        let b: usize = if b.is_empty() { body.len() - 1 } else { b.parse::<usize>().unwrap().min(body.len() - 1) };
                        (a, b)
                    }
                    None => (0, body.len() - 1),
                };
                let part = &body[start..=end];
                let status = if start > 0 || end < body.len() - 1 { "206 Partial Content" } else { "200 OK" };
                let hdr = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Type: video/mp4\r\nConnection: close\r\n\r\n",
                    part.len(),
                    body.len()
                );
                let _ = s.write_all(hdr.as_bytes()).await;
                for c in part.chunks(16384) {
                    if s.write_all(c).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_and_audio_are_downloaded_and_joined() {
    let video = fmp4(90000, 12, 10, 200_000);
    let audio = fmp4(48000, 12, 100, 40_000);
    let mut files = HashMap::new();
    files.insert("/videoplayback/v".to_string(), video.clone());
    files.insert("/videoplayback/a".to_string(), audio.clone());
    let port = serve(files).await;
    let base = std::env::temp_dir().join(format!("rdm_pair_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let url = rdm_core::pair::pair_url(&format!("http://127.0.0.1:{port}/videoplayback/v?itag=137"), &format!("http://127.0.0.1:{port}/videoplayback/a?itag=140"));
    let req = DownloadRequest { url, dir: base.join("out"), file_name: Some("My Clip.mp4".into()), ..Default::default() };
    let dl = Download::start(req, EngineConfig::default(), base.join("tmp"), Arc::new(Limiter::default()));
    let path = dl.wait().await.unwrap().unwrap();
    assert_eq!(path.file_name().unwrap(), "My Clip.mp4");
    let out = std::fs::read(&path).unwrap();
    // Both tracks' media data is all there, and the header lists two tracks.
    let count = |needle: &[u8]| out.windows(needle.len()).filter(|w| *w == needle).count();
    assert_eq!(count(b"moof"), 24);
    assert_eq!(count(b"trak"), 2);
    assert_eq!(count(b"trex"), 2);
    let media = 12 * 200_000 + 12 * 40_000;
    assert!(out.len() > media && out.len() < media + 4096, "size {}", out.len());
    // Interleaved by time: first fragment video (fill 10), then audio (fill 100).
    let first_mdat = out.windows(4).position(|w| w == b"mdat").unwrap();
    assert_eq!(out[first_mdat + 4], 10);
    let second_mdat = first_mdat + 4 + out[first_mdat + 4..].windows(4).position(|w| w == b"mdat").unwrap();
    assert_eq!(out[second_mdat + 4], 100);
    let _ = std::fs::remove_dir_all(&base);
}
