//! HLS (`.m3u8`) stream downloading.
//!
//! The master playlist's highest-bandwidth variant (or the best one at or below a
//! requested height) is chosen together with its separate audio rendition, if any; segments are fetched in
//! parallel (one file each, so a pause/resume only refetches missing segments),
//! AES-128 segments are decrypted, and everything is joined into one file. If `ffmpeg`
//! is available, MPEG-TS output is remuxed into MP4.

use crate::download::{ChunkView, Control, DownloadError, DownloadRequest, Progress, Status};
use crate::limiter::Limiter;
use crate::naming;
use crate::storage::TempDir;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use reqwest::Client;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use url::Url;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Result<T> = std::result::Result<T, DownloadError>;

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub uri: Url,
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub url: Url,
    /// (length, offset)
    pub byterange: Option<(u64, u64)>,
    pub key: Option<Key>,
    pub seq: u64,
    pub duration: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaPlaylist {
    pub init: Option<(Url, Option<(u64, u64)>)>,
    pub segments: Vec<Segment>,
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub url: Url,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    /// `AUDIO` group id of a separate audio rendition.
    pub audio: Option<String>,
}

pub fn looks_like_hls(url: &str, content_type: Option<&str>) -> bool {
    let path_hls = Url::parse(url).map(|u| u.path().to_ascii_lowercase().ends_with(".m3u8")).unwrap_or(false);
    let type_hls = content_type.is_some_and(|t| {
        let t = t.to_ascii_lowercase();
        t.contains("mpegurl") || t.contains("vnd.apple.mpegurl")
    });
    path_hls || type_hls
}

fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let (mut key, mut val, mut in_q, mut in_val) = (String::new(), String::new(), false, false);
    for c in s.chars().chain(std::iter::once(',')) {
        match c {
            '"' => in_q = !in_q,
            '=' if !in_q && !in_val => in_val = true,
            ',' if !in_q => {
                if !key.is_empty() {
                    out.insert(key.trim().to_ascii_uppercase(), val.trim().to_string());
                }
                key.clear();
                val.clear();
                in_val = false;
            }
            _ if in_val => val.push(c),
            _ => key.push(c),
        }
    }
    out
}

fn parse_byterange(v: &str, next_offset: u64) -> (u64, u64) {
    match v.split_once('@') {
        Some((len, off)) => (len.trim().parse().unwrap_or(0), off.trim().parse().unwrap_or(0)),
        None => (v.trim().parse().unwrap_or(0), next_offset),
    }
}

fn parse_iv(v: &str) -> Option<[u8; 16]> {
    let hex = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    let hex = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(iv)
}

/// Variants of a master playlist (empty if this is a media playlist).
pub fn parse_master(text: &str, base: &Url) -> Vec<Variant> {
    let mut out = vec![];
    let mut pending: Option<HashMap<String, String>> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attrs(rest));
        } else if !line.is_empty() && !line.starts_with('#') {
            if let (Some(a), Ok(url)) = (pending.take(), base.join(line)) {
                out.push(Variant {
                    url,
                    bandwidth: a.get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                    resolution: a.get("RESOLUTION").cloned(),
                    audio: a.get("AUDIO").cloned(),
                });
            }
        }
    }
    out
}

pub fn parse_media(text: &str, base: &Url) -> Result<MediaPlaylist> {
    let mut pl = MediaPlaylist { live: true, ..Default::default() };
    let mut seq: u64 = 0;
    let mut key: Option<Key> = None;
    let mut duration = 0.0;
    let mut byterange: Option<(u64, u64)> = None;
    let mut next_offset = 0u64;
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            duration = v.split(',').next().and_then(|d| d.trim().parse().ok()).unwrap_or(0.0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let br = parse_byterange(v, next_offset);
            next_offset = br.1 + br.0;
            byterange = Some(br);
        } else if let Some(v) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attrs(v);
            key = match a.get("METHOD").map(String::as_str) {
                Some("NONE") | None => None,
                Some("AES-128") => {
                    let uri = a.get("URI").ok_or_else(|| DownloadError::Other("HLS key without URI".into()))?;
                    Some(Key { uri: base.join(uri).map_err(|e| DownloadError::Other(e.to_string()))?, iv: a.get("IV").and_then(|v| parse_iv(v)) })
                }
                Some(m) => return Err(DownloadError::Other(format!("unsupported HLS encryption: {m}"))),
            };
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(v);
            if let Some(uri) = a.get("URI") {
                let br = a.get("BYTERANGE").map(|b| parse_byterange(b, 0));
                pl.init = Some((base.join(uri).map_err(|e| DownloadError::Other(e.to_string()))?, br));
            }
        } else if line == "#EXT-X-ENDLIST" {
            pl.live = false;
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = base.join(line).map_err(|e| DownloadError::Other(e.to_string()))?;
            pl.segments.push(Segment { url, byterange: byterange.take(), key: key.clone(), seq, duration });
            seq += 1;
        }
    }
    if pl.segments.is_empty() {
        return Err(DownloadError::Other("playlist has no segments".into()));
    }
    Ok(pl)
}

async fn fetch(client: &Client, url: &Url, range: Option<(u64, u64)>, req: &DownloadRequest) -> Result<Vec<u8>> {
    let mut rb = client.get(url.clone());
    if let Some((len, off)) = range {
        rb = rb.header(reqwest::header::RANGE, format!("bytes={off}-{}", off + len.saturating_sub(1)));
    }
    if let Some(u) = &req.username {
        rb = rb.basic_auth(u, req.password.as_deref());
    }
    let r = rb.send().await?;
    if !r.status().is_success() {
        return Err(DownloadError::Status(r.status()));
    }
    Ok(r.bytes().await?.to_vec())
}

fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| DownloadError::Other("segment decryption failed".into()))
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Download an HLS stream. Returns the final file path, or `None` if paused.
/// An `EXT-X-MEDIA` audio rendition with its own playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioRendition {
    pub group: String,
    pub url: Url,
    pub default: bool,
    pub language: Option<String>,
}

/// Audio renditions (separate audio playlists) declared by a master playlist.
pub fn parse_audio(text: &str, base: &Url) -> Vec<AudioRendition> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("#EXT-X-MEDIA:"))
        .map(attrs)
        .filter(|a| a.get("TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO")))
        .filter_map(|a| {
            Some(AudioRendition {
                group: a.get("GROUP-ID")?.clone(),
                url: base.join(a.get("URI")?).ok()?,
                default: a.get("DEFAULT").is_some_and(|d| d.eq_ignore_ascii_case("YES")),
                language: a.get("LANGUAGE").cloned(),
            })
        })
        .collect()
}

/// Requested maximum video height, passed as a `#rdm-quality=720` URL fragment
/// (fragments never reach the server).
pub fn quality_hint(url: &Url) -> Option<u32> {
    url.fragment()?.split('&').find_map(|kv| kv.strip_prefix("rdm-quality=")?.trim_end_matches('p').parse().ok())
}

fn variant_height(v: &Variant) -> u32 {
    v.resolution.as_deref().and_then(|r| r.split_once(['x', 'X'])).and_then(|(_, h)| h.parse().ok()).unwrap_or(0)
}

/// The variant to download: the best one, or the best at or below the requested height.
pub fn pick_variant(variants: &[Variant], max_height: Option<u32>) -> Option<&Variant> {
    match max_height {
        Some(h) => variants
            .iter()
            .filter(|v| variant_height(v) <= h)
            .max_by_key(|v| (variant_height(v), v.bandwidth))
            .or_else(|| variants.iter().min_by_key(|v| (variant_height(v), v.bandwidth))),
        None => variants.iter().max_by_key(|v| (v.bandwidth, variant_height(v))),
    }
}

fn segment_ext(pl: &MediaPlaylist) -> &'static str {
    if pl.init.is_some() {
        return "mp4";
    }
    let ext = pl.segments.first().and_then(|s| s.url.path().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()));
    match ext.as_deref() {
        Some("aac") => "aac",
        Some("mp3") => "mp3",
        Some("m4a") | Some("m4s") | Some("mp4") => "mp4",
        _ => "ts",
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    req: &DownloadRequest,
    client: Client,
    connections: usize,
    tmp: TempDir,
    limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
    progress: &watch::Sender<Progress>,
    control: &mut watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    let master_url = Url::parse(&req.url).map_err(|e| DownloadError::Other(e.to_string()))?;
    let master = String::from_utf8_lossy(&fetch(&client, &master_url, None, req).await?).into_owned();
    let variants = parse_master(&master, &master_url);
    // (kind, playlist): video first, then a separate audio rendition when the variant has one.
    let mut tracks: Vec<(&'static str, MediaPlaylist)> = vec![];
    match pick_variant(&variants, quality_hint(&master_url)) {
        Some(v) => {
            let text = String::from_utf8_lossy(&fetch(&client, &v.url, None, req).await?).into_owned();
            tracks.push(("video", parse_media(&text, &v.url)?));
            if let Some(group) = &v.audio {
                let renditions: Vec<_> = parse_audio(&master, &master_url).into_iter().filter(|r| &r.group == group).collect();
                if let Some(r) = renditions.iter().find(|r| r.default).or(renditions.first()) {
                    let text = String::from_utf8_lossy(&fetch(&client, &r.url, None, req).await?).into_owned();
                    tracks.push(("audio", parse_media(&text, &r.url)?));
                }
            }
        }
        None => tracks.push(("video", parse_media(&master, &master_url)?)),
    }
    if tracks.iter().any(|(_, pl)| pl.live) {
        return Err(DownloadError::Other("live HLS streams are not supported yet".into()));
    }
    let base_name = req
        .file_name
        .clone()
        .map(|n| naming::sanitize(&n))
        .or_else(|| naming::from_url(&master_url).map(|n| naming::sanitize(&n)))
        .unwrap_or_else(|| "stream".into());
    let stem = base_name.rsplit_once('.').map_or(base_name.clone(), |(s, e)| if ["m3u8", "mp4", "ts"].contains(&e.to_ascii_lowercase().as_str()) { s.to_string() } else { base_name.clone() });
    let can_remux = ffmpeg_available();
    let video_ext = segment_ext(&tracks[0].1);
    let ext = if video_ext == "mp4" || can_remux { "mp4" } else { video_ext };
    let file_name = format!("{stem}.{ext}");
    // Flat job list over all tracks.
    let jobs: Vec<(usize, usize)> = tracks.iter().enumerate().flat_map(|(t, (_, pl))| (0..pl.segments.len()).map(move |i| (t, i))).collect();
    let n = jobs.len();
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.file_name = file_name.clone();
        p.resumable = true;
        p.total = None;
    });

    let seg_path = |(t, i): (usize, usize)| tmp.path.join(format!("t{t}_seg{i:06}.bin"));
    let init_path = |t: usize| tmp.path.join(format!("t{t}_init.bin"));
    let mut done: Vec<bool> = jobs.iter().map(|&j| seg_path(j).exists()).collect();
    let bytes_done = Arc::new(std::sync::atomic::AtomicU64::new(
        jobs.iter().filter(|&&j| seg_path(j).exists()).map(|&j| std::fs::metadata(seg_path(j)).map_or(0, |m| m.len())).sum(),
    ));
    let keys: Arc<Mutex<HashMap<Url, [u8; 16]>>> = Arc::new(Mutex::new(HashMap::new()));
    let req = Arc::new(req.clone());

    for (t, (_, pl)) in tracks.iter().enumerate() {
        if let Some((init_url, br)) = &pl.init {
            let p = init_path(t);
            if !p.exists() {
                let data = fetch(&client, init_url, *br, &req).await?;
                std::fs::write(&p, data)?;
            }
        }
    }
    let tracks = Arc::new(tracks);

    let mut set: JoinSet<(usize, Result<u64>)> = JoinSet::new();
    let mut next = 0usize;
    let mut ui = tokio::time::interval(Duration::from_millis(200));
    let (mut speed_t, mut speed_b, mut speed) = (Instant::now(), bytes_done.load(std::sync::atomic::Ordering::Relaxed), 0.0f64);
    loop {
        while set.len() < connections.max(1) && next < n {
            if done[next] {
                next += 1;
                continue;
            }
            let (job, (t, si)) = (next, jobs[next]);
            let (client, tracks, req, keys, path, limiter, global, counter) =
                (client.clone(), tracks.clone(), req.clone(), keys.clone(), seg_path((t, si)), limiter.clone(), global_limiter.clone(), bytes_done.clone());
            set.spawn(async move {
                let seg = &tracks[t].1.segments[si];
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    let r: Result<u64> = async {
                        let mut data = fetch(&client, &seg.url, seg.byterange, &req).await?;
                        if let Some(k) = &seg.key {
                            let key = {
                                let mut cache = keys.lock().await;
                                match cache.get(&k.uri) {
                                    Some(v) => *v,
                                    None => {
                                        let raw = fetch(&client, &k.uri, None, &req).await?;
                                        let arr: [u8; 16] = raw.get(..16).and_then(|s| s.try_into().ok()).ok_or_else(|| DownloadError::Other("bad HLS key".into()))?;
                                        cache.insert(k.uri.clone(), arr);
                                        arr
                                    }
                                }
                            };
                            let iv = k.iv.unwrap_or_else(|| {
                                let mut iv = [0u8; 16];
                                iv[8..].copy_from_slice(&seg.seq.to_be_bytes());
                                iv
                            });
                            data = decrypt(&data, &key, &iv)?;
                        }
                        limiter.throttle(data.len() as u64).await;
                        global.throttle(data.len() as u64).await;
                        let tmp_path = path.with_extension("part");
                        std::fs::write(&tmp_path, &data)?;
                        std::fs::rename(&tmp_path, &path)?;
                        counter.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        Ok(data.len() as u64)
                    }
                    .await;
                    match r {
                        Ok(b) => return (job, Ok(b)),
                        Err(e) if attempt >= 8 => return (job, Err(e)),
                        Err(_) => tokio::time::sleep(Duration::from_millis(500 * attempt)).await,
                    }
                }
            });
            next += 1;
        }
        if set.is_empty() && next >= n {
            break;
        }
        tokio::select! {
            j = set.join_next() => {
                if let Some(j) = j {
                    let (i, r) = j.map_err(|e| DownloadError::Other(e.to_string()))?;
                    r?;
                    done[i] = true;
                }
            }
            _ = control.changed() => {
                if *control.borrow() != Control::Run {
                    set.abort_all();
                    if *control.borrow() == Control::Cancel {
                        let _ = tmp.clone().remove();
                        return Err(DownloadError::Cancelled);
                    }
                    return Ok(None);
                }
            }
            _ = ui.tick() => {
                let b = bytes_done.load(std::sync::atomic::Ordering::Relaxed);
                let el = speed_t.elapsed().as_secs_f64();
                if el >= 0.5 {
                    let inst = b.saturating_sub(speed_b) as f64 / el;
                    speed = if speed == 0.0 { inst } else { speed * 0.6 + inst * 0.4 };
                    speed_t = Instant::now();
                    speed_b = b;
                }
                let finished = done.iter().filter(|d| **d).count();
                // Estimate the total from the average segment size so far.
                let est = if finished > 0 { Some(b / finished as u64 * n as u64) } else { None };
                progress.send_modify(|p| {
                    p.downloaded = b;
                    p.total = est;
                    p.speed = speed;
                    p.connections = set.len();
                    p.chunks = (0..n.min(400)).map(|i| ChunkView { id: i as u32, start: i as u64, end: i as u64 + 1, pos: if done[i] { i as u64 + 1 } else { i as u64 }, active: false }).collect();
                });
            }
        }
    }

    // Join each track's segments.
    progress.send_modify(|p| p.status = Status::Assembling);
    let mut joined = vec![];
    for (t, (kind, pl)) in tracks.iter().enumerate() {
        let path = tmp.path.join(format!("{kind}.{}", segment_ext(pl)));
        let mut out = std::fs::File::create(&path)?;
        if init_path(t).exists() {
            out.write_all(&std::fs::read(init_path(t))?)?;
        }
        for i in 0..pl.segments.len() {
            out.write_all(&std::fs::read(seg_path((t, i)))?)?;
        }
        out.sync_all()?;
        joined.push((*kind, path));
    }
    std::fs::create_dir_all(&req.dir)?;
    let dest = naming::unique_path(&req.dir, &file_name);
    let result = if joined.len() == 1 && (video_ext == "mp4" || !can_remux) {
        move_file(&joined[0].1, &dest)?;
        dest
    } else if can_remux {
        let mut cmd = tokio::process::Command::new("ffmpeg");
        cmd.args(["-y", "-loglevel", "error"]);
        for (_, f) in &joined {
            cmd.arg("-i").arg(f);
        }
        if joined.len() > 1 {
            cmd.args(["-map", "0:v:0?", "-map", "1:a:0"]);
        }
        cmd.args(["-c", "copy"]).arg(&dest);
        if cmd.status().await.is_ok_and(|s| s.success()) { dest } else { keep_separate(&joined, &req.dir, &stem)? }
    } else {
        keep_separate(&joined, &req.dir, &stem)?
    };
    let total = std::fs::metadata(&result).map(|m| m.len()).unwrap_or(0);
    progress.send_modify(|p| {
        p.total = Some(total);
        p.downloaded = total;
    });
    let _ = tmp.remove();
    Ok(Some(result))
}

/// Without ffmpeg (or if muxing fails): keep tracks as separate files; returns the video path.
fn keep_separate(tracks: &[(&str, PathBuf)], dir: &Path, stem: &str) -> Result<PathBuf> {
    let mut video = None;
    let single = tracks.len() == 1;
    for (kind, f) in tracks {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("bin");
        let ext = if *kind == "audio" && ext == "mp4" { "m4a" } else { ext };
        let name = if single { format!("{stem}.{ext}") } else { format!("{stem}.{kind}.{ext}") };
        let dest = naming::unique_path(dir, &name);
        move_file(f, &dest)?;
        if *kind == "video" || video.is_none() {
            video = Some(dest);
        }
    }
    video.ok_or_else(|| DownloadError::Other("nothing downloaded".into()))
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_hint_and_variant_choice() {
        let base = Url::parse("https://cdn.example.com/m.m3u8#rdm-quality=720p").unwrap();
        assert_eq!(quality_hint(&base), Some(720));
        assert_eq!(quality_hint(&Url::parse("https://x/m.m3u8").unwrap()), None);
        let m = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"a/en.m3u8\"\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"s\",URI=\"s.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"aac\"\n360.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,AUDIO=\"aac\"\n720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,AUDIO=\"aac\"\n1080.m3u8\n";
        let v = parse_master(m, &base);
        assert_eq!(pick_variant(&v, None).unwrap().url.path(), "/1080.m3u8");
        assert_eq!(pick_variant(&v, Some(720)).unwrap().url.path(), "/720.m3u8");
        assert_eq!(pick_variant(&v, Some(144)).unwrap().url.path(), "/360.m3u8");
        assert_eq!(v[0].audio.as_deref(), Some("aac"));
        let a = parse_audio(m, &base);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].url.as_str(), "https://cdn.example.com/a/en.m3u8");
        assert!(a[0].default);
    }

    #[test]
    fn master_picks_variants() {
        let base = Url::parse("https://cdn.example.com/v/master.m3u8").unwrap();
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhigh/index.m3u8\n";
        let v = parse_master(m, &base);
        assert_eq!(v.len(), 2);
        let best = v.iter().max_by_key(|x| x.bandwidth).unwrap();
        assert_eq!(best.url.as_str(), "https://cdn.example.com/v/high/index.m3u8");
        assert_eq!(best.resolution.as_deref(), Some("1280x720"));
    }

    #[test]
    fn media_playlist_with_key_and_ranges() {
        let base = Url::parse("https://cdn.example.com/v/high/index.m3u8").unwrap();
        let t = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00000000000000000000000000000001\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:1000@0\nall.ts\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:500\nall.ts\n#EXT-X-ENDLIST\n";
        let pl = parse_media(t, &base).unwrap();
        assert!(!pl.live);
        assert_eq!(pl.segments.len(), 2);
        assert_eq!(pl.segments[0].seq, 7);
        assert_eq!(pl.segments[1].byterange, Some((500, 1000)));
        let k = pl.segments[0].key.as_ref().unwrap();
        assert_eq!(k.uri.as_str(), "https://cdn.example.com/v/high/key.bin");
        assert_eq!(k.iv.unwrap()[15], 1);
    }

    #[test]
    fn decrypt_round_trip() {
        use aes::cipher::BlockEncryptMut;
        type Enc = cbc::Encryptor<aes::Aes128>;
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let plain = b"hello hls segment data".to_vec();
        let enc = Enc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain);
        assert_eq!(decrypt(&enc, &key, &iv).unwrap(), plain);
    }

    #[test]
    fn detects_hls() {
        assert!(looks_like_hls("https://x/y/master.m3u8?token=1", None));
        assert!(looks_like_hls("https://x/y/play", Some("application/vnd.apple.mpegurl")));
        assert!(!looks_like_hls("https://x/y/file.mp4", Some("video/mp4")));
    }
}
