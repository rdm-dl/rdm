//! DASH with separate fragmented-MP4 video and audio tracks ends as ONE file with both
//! tracks (RDM's own joiner), not two side-by-side files.

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


/// Split a fragmented MP4 into its init (ftyp+moov) and media segments (moof+mdat).
fn split(f: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut boxes = vec![];
    let mut p = 0;
    while p + 8 <= f.len() {
        let sz = u32::from_be_bytes(f[p..p + 4].try_into().unwrap()) as usize;
        boxes.push((&f[p + 4..p + 8], &f[p..p + sz]));
        p += sz;
    }
    let init = boxes.iter().take_while(|(k, _)| *k != b"moof").flat_map(|(_, b)| b.to_vec()).collect();
    let segs = boxes.iter().skip_while(|(k, _)| *k != b"moof").collect::<Vec<_>>().chunks(2).map(|c| c.iter().flat_map(|(_, b)| b.to_vec()).collect()).collect();
    (init, segs)
}

async fn serve(files: HashMap<String, (Vec<u8>, &'static str)>) -> u16 {
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
                let path = String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body, t) = match files.get(&path) { Some((b, t)) => ("200 OK", b.clone(), *t), None => ("404 Not Found", vec![], "text/plain") };
                let hdr = format!("HTTP/1.1 {status}
Content-Type: {t}
Content-Length: {}
Connection: close

", body.len());
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dash_video_and_audio_become_one_file() {
    let (vinit, vsegs) = split(&fmp4(90000, 8, 10, 50_000));
    let (ainit, asegs) = split(&fmp4(48000, 8, 100, 8_000));
    let mut files = HashMap::new();
    files.insert("/v/init.mp4".to_string(), (vinit, "video/mp4"));
    files.insert("/a/init.mp4".to_string(), (ainit, "audio/mp4"));
    for (i, s) in vsegs.into_iter().enumerate() { files.insert(format!("/v/{}.m4s", i + 1), (s, "video/mp4")); }
    for (i, s) in asegs.into_iter().enumerate() { files.insert(format!("/a/{}.m4s", i + 1), (s, "audio/mp4")); }
    let mpd = r#"<?xml version="1.0"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT16S"><Period>
<AdaptationSet contentType="video" mimeType="video/mp4"><Representation id="v" bandwidth="4000000" height="720"><SegmentTemplate timescale="1" initialization="v/init.mp4" media="v/$Number$.m4s" startNumber="1"><SegmentTimeline><S t="0" d="2" r="7"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet>
<AdaptationSet contentType="audio" mimeType="audio/mp4"><Representation id="a" bandwidth="128000"><SegmentTemplate timescale="1" initialization="a/init.mp4" media="a/$Number$.m4s" startNumber="1"><SegmentTimeline><S t="0" d="2" r="7"/></SegmentTimeline></SegmentTemplate></Representation></AdaptationSet>
</Period></MPD>"#;
    files.insert("/movie.mpd".to_string(), (mpd.as_bytes().to_vec(), "application/dash+xml"));
    let port = serve(files).await;
    let base = std::env::temp_dir().join(format!("rdm_dashmux_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let req = DownloadRequest { url: format!("http://127.0.0.1:{port}/movie.mpd"), dir: base.join("out"), file_name: Some("Film.mp4".into()), ..Default::default() };
    let path = Download::start(req, EngineConfig::default(), base.join("tmp"), Arc::new(Limiter::default())).wait().await.unwrap().unwrap();
    assert_eq!(path.file_name().unwrap(), "Film.mp4");
    let names: Vec<_> = std::fs::read_dir(base.join("out")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    let out = std::fs::read(&path).unwrap();
    let count = |needle: &[u8]| out.windows(needle.len()).filter(|w| *w == needle).count();
    assert_eq!(count(b"trak"), 2);
    assert_eq!(count(b"moof"), 16);
    let _ = std::fs::remove_dir_all(&base);
}
