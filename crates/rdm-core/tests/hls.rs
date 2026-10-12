//! End-to-end HLS: master -> media playlist -> AES-128 segments -> joined file.

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use rdm_core::{Download, DownloadRequest, EngineConfig, Limiter};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Enc = cbc::Encryptor<aes::Aes128>;

async fn serve(files: HashMap<String, Vec<u8>>) -> u16 {
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
                let (status, body) = match files.get(&path) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", vec![]),
                };
                let hdr = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_encrypted_stream_is_joined() {
    let key = [3u8; 16];
    let mut files = HashMap::new();
    let mut expected = vec![];
    let mut media = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"/key.bin\"\n");
    for i in 0..12u64 {
        let plain: Vec<u8> = (0..(50_000 + i as usize * 17)).map(|j| ((j as u64 * 13 + i * 7) % 256) as u8).collect();
        expected.extend_from_slice(&plain);
        let mut iv = [0u8; 16];
        iv[8..].copy_from_slice(&i.to_be_bytes());
        let enc = Enc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain);
        files.insert(format!("/hi/seg{i}.ts"), enc);
        media.push_str(&format!("#EXTINF:4.0,\nseg{i}.ts\n"));
    }
    media.push_str("#EXT-X-ENDLIST\n");
    files.insert("/hi/index.m3u8".into(), media.into_bytes());
    files.insert("/lo/index.m3u8".into(), b"#EXTM3U\n#EXTINF:4,\nnope.ts\n#EXT-X-ENDLIST\n".to_vec());
    files.insert("/key.bin".into(), key.to_vec());
    files.insert(
        "/master.m3u8".into(),
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=500000\nlo/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1920x1080\nhi/index.m3u8\n".to_vec(),
    );
    let port = serve(files).await;

    let base = std::env::temp_dir().join(format!("rdm_hls_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{port}/master.m3u8"),
        dir: base.join("out"),
        file_name: Some("movie".into()),
        ..Default::default()
    };
    let dl = Download::start(req, EngineConfig::default(), base.join("tmp"), Arc::new(Limiter::default()));
    let path = dl.wait().await.unwrap().unwrap();
    let ext = path.extension().unwrap().to_string_lossy().to_string();
    if ext == "ts" {
        // No ffmpeg: the joined, decrypted stream must match byte for byte.
        assert_eq!(std::fs::read(&path).unwrap(), expected);
    } else {
        // ffmpeg remux of synthetic data may fail; then a .ts fallback is produced instead.
        assert!(path.exists());
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_quality_and_separate_audio() {
    let mut files = HashMap::new();
    let mut add = |dir: &str, ext: &str, fill: u8| -> Vec<u8> {
        let mut all = vec![];
        let mut pl = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:4\n");
        for i in 0..5 {
            let data = vec![fill.wrapping_add(i as u8); 30_000];
            all.extend_from_slice(&data);
            files.insert(format!("/{dir}/s{i}.{ext}"), data);
            pl.push_str(&format!("#EXTINF:4,\ns{i}.{ext}\n"));
        }
        pl.push_str("#EXT-X-ENDLIST\n");
        files.insert(format!("/{dir}/index.m3u8"), pl.into_bytes());
        all
    };
    let low = add("v360", "ts", 10);
    let _high = add("v1080", "ts", 100);
    let audio = add("aud", "aac", 200);
    files.insert(
        "/master.m3u8".into(),
        b"#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"en\",DEFAULT=YES,URI=\"aud/index.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"a\"\nv360/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO=\"a\"\nv1080/index.m3u8\n".to_vec(),
    );
    let port = serve(files).await;
    let base = std::env::temp_dir().join(format!("rdm_hls_q_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let req = DownloadRequest {
        url: format!("http://127.0.0.1:{port}/master.m3u8#rdm-quality=480"),
        dir: base.join("out"),
        file_name: Some("clip".into()),
        ..Default::default()
    };
    let dl = Download::start(req, EngineConfig::default(), base.join("tmp"), Arc::new(Limiter::default()));
    let path = dl.wait().await.unwrap().unwrap();
    let out = base.join("out");
    if path.extension().unwrap() == "ts" {
        // No ffmpeg: 360p video and the audio rendition are kept as separate files.
        assert_eq!(std::fs::read(out.join("clip.video.ts")).unwrap(), low);
        assert_eq!(std::fs::read(out.join("clip.audio.aac")).unwrap(), audio);
    } else {
        assert!(path.exists());
    }
    let _ = std::fs::remove_dir_all(&base);
}
