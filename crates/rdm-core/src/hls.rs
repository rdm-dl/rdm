//! HLS (`.m3u8`) stream downloading.
//!
//! The master playlist's highest-bandwidth variant (or the best one at or below a
//! requested height) is chosen together with its separate audio rendition, if any; segments
//! are fetched in parallel (one file each, so a pause/resume only refetches missing
//! segments), AES-128 segments are decrypted, and each track is joined into one file. The
//! tracks then become one MP4: fragmented MP4 through [`crate::mux`], MPEG-TS and raw AAC/MP3
//! through [`crate::ts`] first. `ffmpeg` is only needed for what those can't handle.
//!
//! A player may also load separate video and audio playlists with no master that ties them
//! together; the request then carries the audio playlist the way [`crate::pair`] does.

use crate::config::EngineConfig;
use crate::download::{ChunkView, Control, DownloadError, DownloadRequest, Progress, Status};
use crate::limiter::Limiter;
use crate::naming;
use crate::storage::TempDir;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use reqwest::Client;
use std::collections::HashMap;
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
    /// The playlist marks a change of source just before this segment.
    pub discontinuity: bool,
    /// The `EXT-X-MAP` header this segment follows.
    pub map: Option<(Url, Option<(u64, u64)>)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaPlaylist {
    pub init: Option<(Url, Option<(u64, u64)>)>,
    /// The key the init segment is encrypted with, when one was in force at its `EXT-X-MAP`.
    pub init_key: Option<Key>,
    pub segments: Vec<Segment>,
    pub live: bool,
    /// A live playlist that keeps everything since the stream began.
    pub event: bool,
    /// The longest a segment may be, in seconds.
    pub target: f64,
    /// Where this playlist came from.
    pub source: Option<Url>,
}

impl MediaPlaylist {
    /// Settle a finished playlist. Several init segments mean spliced-in content (an ad) that
    /// can't share a file with the main stream's header: the stream most of the playlist belongs
    /// to stays. Then come the ad breaks, unless they're to be kept.
    pub(crate) fn finish(&mut self, keep_ads: bool) {
        let main = self.init.clone();
        if main.is_some() {
            self.segments.retain(|s| s.map == main);
        }
        if !keep_ads {
            drop_ads(self);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub url: Url,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    /// `AUDIO` group id of a separate audio rendition.
    pub audio: Option<String>,
    /// `CODECS`, when the playlist says.
    pub codecs: Option<String>,
    /// `SUBTITLES` group id.
    pub subtitles: Option<String>,
}

/// `#rdm-kind=hls` / `#rdm-kind=dash`: the browser extension already knows the URL is a
/// stream (playlist URLs often have no telling extension).
pub fn kind_hint(url: &str) -> Option<&str> {
    url.split_once('#')?.1.split('&').find_map(|kv| kv.strip_prefix("rdm-kind="))
}

pub fn looks_like_hls(url: &str, content_type: Option<&str>) -> bool {
    if kind_hint(url) == Some("hls") {
        return true;
    }
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

pub const DRM: &str = "This stream is protected with DRM, so it can't be saved.";

/// Leave out ad breaks. Ads stitched into a stream come from another server, as separate
/// stretches between discontinuity marks, so a stretch that's wholly on a server other than the
/// main one goes. If that would take much of the playlist, it isn't ads: nothing is dropped.
pub(crate) fn drop_ads(pl: &mut MediaPlaylist) {
    let host = |s: &Segment| s.url.host_str().unwrap_or_default().to_string();
    let mut time: HashMap<String, f64> = HashMap::new();
    for s in &pl.segments {
        *time.entry(host(s)).or_default() += s.duration;
    }
    let Some(main) = time.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(h, _)| h.clone()) else { return };
    let total: f64 = time.values().sum();
    let mut stretches: Vec<Vec<usize>> = vec![];
    for (i, s) in pl.segments.iter().enumerate() {
        if s.discontinuity || stretches.is_empty() {
            stretches.push(vec![]);
        }
        if let Some(last) = stretches.last_mut() {
            last.push(i);
        }
    }
    let ads: Vec<usize> = stretches.iter().filter(|run| run.iter().all(|i| host(&pl.segments[*i]) != main)).flatten().copied().collect();
    let ad_time: f64 = ads.iter().map(|i| pl.segments[*i].duration).sum();
    if ads.is_empty() || ad_time > total * 0.4 {
        return;
    }
    let mut index = 0;
    pl.segments.retain(|_| {
        index += 1;
        !ads.contains(&(index - 1))
    });
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
                    codecs: a.get("CODECS").cloned(),
                    subtitles: a.get("SUBTITLES").cloned(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
pub fn parse_media(text: &str, base: &Url) -> Result<MediaPlaylist> {
    parse_stream(text, base, false)
}

/// Read a media playlist, leaving out ad breaks unless they're to be kept. Spliced-in content
/// with a header of its own can't stay either way: it can't share a file with the programme.
pub fn parse_stream(text: &str, base: &Url, keep_ads: bool) -> Result<MediaPlaylist> {
    let mut pl = MediaPlaylist { live: true, source: Some(base.clone()), ..Default::default() };
    let mut seq: u64 = 0;
    let mut key: Option<Key> = None;
    let mut duration = 0.0;
    let mut byterange: Option<(u64, u64)> = None;
    let mut discontinuity = false;
    let mut next_offset = 0u64;
    let mut current_map: Option<usize> = None;
    // Every EXT-X-MAP seen, and which of them each segment follows.
    let mut maps: Vec<((Url, Option<(u64, u64)>), Option<Key>)> = vec![];
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
                Some("AES-128") if a.get("KEYFORMAT").is_none_or(|f| f.eq_ignore_ascii_case("identity")) => {
                    let uri = a.get("URI").ok_or_else(|| DownloadError::Other("HLS key without URI".into()))?;
                    Some(Key { uri: base.join(uri).map_err(|e| DownloadError::Other(e.to_string()))?, iv: a.get("IV").and_then(|v| parse_iv(v)) })
                }
                Some(_) => return Err(DownloadError::Other(DRM.into())),
            };
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(v);
            if let Some(uri) = a.get("URI") {
                let br = a.get("BYTERANGE").map(|b| parse_byterange(b, 0));
                let map = (base.join(uri).map_err(|e| DownloadError::Other(e.to_string()))?, br);
                if !maps.iter().any(|(m, _)| *m == map) {
                    maps.push((map.clone(), key.clone()));
                }
                current_map = maps.iter().position(|(m, _)| *m == map);
            }
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            pl.target = v.trim().parse().unwrap_or(0.0);
        } else if line == "#EXT-X-PLAYLIST-TYPE:EVENT" {
            pl.event = true;
        } else if line == "#EXT-X-ENDLIST" || line == "#EXT-X-PLAYLIST-TYPE:VOD" {
            pl.live = false;
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = base.join(line).map_err(|e| DownloadError::Other(e.to_string()))?;
            pl.segments.push(Segment { url, byterange: byterange.take(), key: key.clone(), seq, duration, discontinuity, map: current_map.map(|m| maps[m].0.clone()) });
            discontinuity = false;
            seq += 1;
        }
    }
    if !maps.is_empty() {
        let main = (0..maps.len()).max_by_key(|m| pl.segments.iter().filter(|s| s.map.as_ref() == Some(&maps[*m].0)).count()).unwrap_or(0);
        let (map, key) = maps.swap_remove(main);
        pl.init = Some(map);
        pl.init_key = key;
    }
    // A live playlist shows only a moment of the stream: what looks like the odd one out now may
    // be the programme. A recording sorts that out when it can see the whole of it.
    if !pl.live {
        pl.finish(keep_ads);
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

/// Playlists are small and worth a retry: a hiccup here would lose the whole download.
pub(crate) async fn fetch_text(client: &Client, url: &Url, req: &DownloadRequest) -> Result<String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match fetch(client, url, None, req).await {
            Ok(b) => return Ok(String::from_utf8_lossy(&b).into_owned()),
            Err(DownloadError::Status(s)) if s.is_client_error() && s != reqwest::StatusCode::TOO_MANY_REQUESTS => return Err(DownloadError::Status(s)),
            Err(e) if attempt >= 4 => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(500 * attempt)).await,
        }
    }
}

/// The bytes of a `data:` URI (some players put the key in the playlist itself).
fn data_uri(url: &Url) -> Option<Vec<u8>> {
    let (meta, body) = url.as_str().strip_prefix("data:")?.split_once(',')?;
    let body = percent_encoding::percent_decode_str(body).collect::<Vec<u8>>();
    if !meta.ends_with(";base64") {
        return Some(body);
    }
    let mut out = vec![];
    let (mut bits, mut n) = (0u32, 0);
    for c in body {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        };
        bits = (bits << 6) | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    Some(out)
}

async fn fetch_key(client: &Client, uri: &Url, keys: &Mutex<HashMap<Url, [u8; 16]>>, req: &DownloadRequest) -> Result<[u8; 16]> {
    let mut cache = keys.lock().await;
    if let Some(k) = cache.get(uri) {
        return Ok(*k);
    }
    let raw = match data_uri(uri) {
        Some(b) => b,
        None => fetch(client, uri, None, req).await?,
    };
    let key: [u8; 16] = raw.get(..16).and_then(|s| s.try_into().ok()).ok_or_else(|| DownloadError::Other("bad HLS key".into()))?;
    cache.insert(uri.clone(), key);
    Ok(key)
}

fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| DownloadError::Other("segment decryption failed".into()))
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

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

/// The rendition this variant plays. `None` when there is no separate audio, which includes
/// a default rendition with no URI of its own: that one is muxed into the video.
fn pick_audio(master: &str, base: &Url, variant: &Variant) -> Option<Url> {
    let all: Vec<HashMap<String, String>> = master
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("#EXT-X-MEDIA:"))
        .map(attrs)
        .filter(|a| a.get("TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO")))
        .collect();
    let yes = |a: &HashMap<String, String>, k: &str| a.get(k).is_some_and(|d| d.eq_ignore_ascii_case("YES"));
    let group: Vec<&HashMap<String, String>> = match &variant.audio {
        Some(g) => all.iter().filter(|a| a.get("GROUP-ID") == Some(g)).collect(),
        // No group named, but the codecs say the video has no sound of its own.
        None if variant.codecs.as_deref().is_some_and(|c| !has_audio_codec(c)) => all.iter().collect(),
        None => return None,
    };
    let best = group.iter().find(|a| yes(a, "DEFAULT")).or(group.iter().find(|a| yes(a, "AUTOSELECT"))).or(group.first())?;
    base.join(best.get("URI")?).ok()
}

fn has_audio_codec(codecs: &str) -> bool {
    codecs.split(',').map(|c| c.trim().to_ascii_lowercase()).any(|c| ["mp4a", "ac-3", "ec-3", "ac-4", "mp3", "opus", "flac", "alac", "vorbis", "dts"].iter().any(|a| c.starts_with(a)))
}

/// Requested maximum video height, passed as a `#rdm-quality=720` URL fragment
/// (fragments never reach the server).
pub fn quality_hint(url: &Url) -> Option<u32> {
    url.fragment()?.split('&').find_map(|kv| kv.strip_prefix("rdm-quality=")?.trim_end_matches('p').parse().ok())
}

fn variant_height(v: &Variant) -> u32 {
    v.resolution.as_deref().and_then(|r| r.split_once(['x', 'X'])).and_then(|(_, h)| h.parse().ok()).unwrap_or(0)
}

/// Audio the built-in muxer can't take, which needs ffmpeg and which many players can't play.
fn awkward_audio(v: &Variant) -> bool {
    let codecs = v.codecs.as_deref().unwrap_or("").to_ascii_lowercase();
    ["ac-3", "ec-3", "ec+3", "ac-4", "dtsc", "dtse", "dtsh", "dtsl"].iter().any(|c| codecs.contains(c))
}

/// The variant to download: the best one, or the best at or below the requested height.
/// Sites often offer one picture with several audio formats, and those differ by little: take
/// the one that plays everywhere, unless that would cost real picture quality.
pub fn pick_variant(variants: &[Variant], max_height: Option<u32>) -> Option<&Variant> {
    let best = best_variant(variants, max_height)?;
    if !awkward_audio(best) {
        return Some(best);
    }
    let plain = variants.iter().filter(|v| variant_height(v) == variant_height(best) && !awkward_audio(v)).max_by_key(|v| v.bandwidth);
    Some(plain.filter(|v| v.bandwidth * 10 >= best.bandwidth * 7).unwrap_or(best))
}

fn best_variant(variants: &[Variant], max_height: Option<u32>) -> Option<&Variant> {
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

/// Download an HLS stream. Returns the final file path, or `None` if paused.
#[allow(clippy::too_many_arguments)]
/// An init segment, decrypted.
pub(crate) async fn fetch_init(client: &Client, map: &(Url, Option<(u64, u64)>), key: &Option<Key>, keys: &Mutex<HashMap<Url, [u8; 16]>>, req: &DownloadRequest) -> Result<Vec<u8>> {
    let mut data = fetch(client, &map.0, map.1, req).await?;
    if let Some(Key { uri, iv: Some(iv) }) = key {
        data = decrypt(&data, &fetch_key(client, uri, keys, req).await?, iv)?;
    }
    Ok(data)
}

/// Fetch one segment into `path`, decrypted, trying a few times.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn save_segment(client: &Client, seg: &Segment, keys: &Mutex<HashMap<Url, [u8; 16]>>, req: &DownloadRequest, path: &Path, limiter: &Limiter, global: &Limiter, counter: &std::sync::atomic::AtomicU64) -> Result<u64> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let r: Result<u64> = async {
            let mut data = fetch(client, &seg.url, seg.byterange, req).await?;
            if let Some(k) = &seg.key {
                let key = fetch_key(client, &k.uri, keys, req).await?;
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
            std::fs::rename(&tmp_path, path)?;
            counter.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(data.len() as u64)
        }
        .await;
        match r {
            Ok(b) => return Ok(b),
            Err(e) if attempt >= 8 || e.is_disk_full() => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(500 * attempt)).await,
        }
    }
}

/// Some servers sign only the first address and want that signature on everything after it.
/// `#rdm-keep-query` on the address asks for that.
fn signature(url: &Url) -> Option<String> {
    url.fragment()?.split('&').any(|k| k == "rdm-keep-query").then(|| url.query().map(str::to_string)).flatten()
}

fn sign(url: &mut Url, signature: Option<&str>) {
    if url.query().is_none() {
        url.set_query(signature);
    }
}

pub async fn run(
    req: &DownloadRequest,
    client: Client,
    cfg: &EngineConfig,
    tmp: TempDir,
    limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
    progress: &watch::Sender<Progress>,
    control: &mut watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    let connections = cfg.max_connections;
    let keep_ads = cfg.keep_ads;
    let master_url = Url::parse(&req.url).map_err(|e| DownloadError::Other(e.to_string()))?;
    let master = fetch_text(&client, &master_url, req).await?;
    let signed = signature(&master_url);
    let mut variants = parse_master(&master, &master_url);
    variants.iter_mut().for_each(|v| sign(&mut v.url, signed.as_deref()));
    // (kind, playlist): video first, then a separate audio rendition when the variant has one.
    let mut tracks: Vec<(&'static str, MediaPlaylist)> = vec![];
    let mut subtitle_group = None;
    match pick_variant(&variants, quality_hint(&master_url)) {
        Some(v) => {
            subtitle_group.clone_from(&v.subtitles);
            let text = fetch_text(&client, &v.url, req).await?;
            tracks.push(("video", parse_stream(&text, &v.url, keep_ads)?));
            if let Some(mut audio) = pick_audio(&master, &master_url, v) {
                sign(&mut audio, signed.as_deref());
                let text = fetch_text(&client, &audio, req).await?;
                tracks.push(("audio", parse_stream(&text, &audio, keep_ads)?));
            }
        }
        None => tracks.push(("video", parse_stream(&master, &master_url, keep_ads)?)),
    }
    // An audio playlist the player loaded on its own (it may be a master itself).
    if let Some(audio) = crate::pair::audio_url(&req.url).and_then(|a| Url::parse(&a).ok()) {
        let mut text = fetch_text(&client, &audio, req).await?;
        let mut url = audio;
        if let Some(v) = pick_variant(&parse_master(&text, &url), None) {
            url = v.url.clone();
            text = fetch_text(&client, &url, req).await?;
        }
        tracks.truncate(1);
        tracks.push(("audio", parse_stream(&text, &url, keep_ads)?));
    }
    for (_, playlist) in &mut tracks {
        playlist.segments.iter_mut().for_each(|s| sign(&mut s.url, signed.as_deref()));
        if let Some((url, _)) = playlist.init.as_mut() {
            sign(url, signed.as_deref());
        }
    }
    let mut live = tracks.iter().any(|(_, pl)| pl.live);
    if live && crate::live::is_finished(&client, req, &tracks[0].1, &tracks[0].1.source.clone().unwrap_or_else(|| master_url.clone())).await {
        live = false;
        tracks.iter_mut().for_each(|(_, pl)| pl.finish(keep_ads));
    }
    let base_name = req
        .file_name
        .clone()
        .map(|n| naming::sanitize(&n))
        .or_else(|| naming::from_url(&master_url).map(|n| naming::sanitize(&n)))
        .unwrap_or_else(|| "stream".into());
    let stem = base_name.rsplit_once('.').map_or(base_name.clone(), |(s, e)| if ["m3u8", "mp4", "m4a", "ts"].contains(&e.to_ascii_lowercase().as_str()) { s.to_string() } else { base_name.clone() });
    let stem = naming::stream_stem(stem, &master_url);
    let video_ext = segment_ext(&tracks[0].1);
    // A name given as audio stays audio.
    let file_name = format!("{stem}.{}", if base_name.to_ascii_lowercase().ends_with(".m4a") { "m4a" } else { "mp4" });
    // Flat job list over all tracks. A recording fetches its own as they appear.
    let jobs: Vec<(usize, usize)> = if live { vec![] } else { tracks.iter().enumerate().flat_map(|(t, (_, pl))| (0..pl.segments.len()).map(move |i| (t, i))).collect() };
    let n = jobs.len();
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.file_name = file_name.clone();
        p.resumable = !live;
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
                std::fs::write(&p, fetch_init(&client, &(init_url.clone(), *br), &pl.init_key, &keys, &req).await?)?;
            }
        }
    }
    let tracks = Arc::new(tracks);
    let mut files: Vec<Vec<usize>> = tracks.iter().map(|(_, pl)| (0..pl.segments.len()).collect()).collect();
    if live {
        let job = crate::live::Job { client: client.clone(), req: req.clone(), cfg, tmp: &tmp, limiter: limiter.clone(), global: global_limiter.clone(), progress, control };
        files = crate::live::record(job, &tracks, |t, i| seg_path((t, i)), init_path).await?;
    }

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
                (job, save_segment(&client, seg, &keys, &req, &path, &limiter, &global, &counter).await)
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
    // Joined off the async workers, with progress.
    let mut joined = vec![];
    let mut work = vec![];
    for (t, (kind, pl)) in tracks.iter().enumerate() {
        let path = tmp.path.join(format!("{kind}.{}", segment_ext(pl)));
        let mut parts = vec![];
        if init_path(t).exists() {
            parts.push(init_path(t));
        }
        // A recording can have a hole where a segment never came.
        parts.extend(files[t].iter().map(|i| seg_path((t, *i))).filter(|p| !live || p.exists()));
        work.push((parts, path.clone()));
        joined.push((*kind, path));
    }
    crate::join::concat(work, progress).await?;
    std::fs::create_dir_all(&req.dir)?;
    let dest = naming::unique_path(&req.dir, &file_name);
    let packaged = crate::join::package_stream(joined[0].1.clone(), joined.get(1).map(|j| j.1.clone()), tmp.path.join("package"), dest.clone(), progress).await?;
    let as_stream = || naming::unique_path(&req.dir, &format!("{stem}.{video_ext}"));
    let result = if let Some(done) = packaged {
        done
    } else if joined.len() == 1 && (video_ext == "mp4" || !ffmpeg_available()) {
        // A stream only ffmpeg could package is saved as it came (.ts plays everywhere).
        let dest = if video_ext == "mp4" { dest } else { as_stream() };
        move_file(&joined[0].1, &dest)?;
        dest
    } else if joined.len() == 1 {
        let (src, d2) = (joined[0].1.clone(), dest.clone());
        let ok = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(&src)
            .args(["-c", "copy"])
            .arg(&d2)
            .status()
            .await
            .is_ok_and(|s| s.success());
        if ok {
            dest
        } else {
            let _ = std::fs::remove_file(&dest);
            let ts = as_stream();
            move_file(&src, &ts)?;
            ts
        }
    } else {
        // Separate audio rendition: RDM's own joiner (fMP4), else ffmpeg, else side by side.
        crate::join::video_and_audio(joined[0].1.clone(), joined[1].1.clone(), dest, &stem, progress).await?
    };
    let name = stem.clone();
    let result = crate::join::finish(result, name, progress).await?;
    if let (true, Some(group)) = (cfg.save_subtitles, &subtitle_group) {
        let wanted = crate::subtitles::hls_tracks(&master, &master_url, group);
        crate::subtitles::save(&client, &req, wanted, &cfg.subtitle_languages, &result).await;
    }
    let total = std::fs::metadata(&result).map(|m| m.len()).unwrap_or(0);
    progress.send_modify(|p| {
        p.total = Some(total);
        p.downloaded = total;
    });
    let _ = tmp.remove();
    Ok(Some(result))
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
    fn ad_breaks_on_another_server_are_left_out() {
        let base = Url::parse("https://show.example/v/index.m3u8").unwrap();
        let names = |pl: &MediaPlaylist| pl.segments.iter().map(|s| s.url.path().rsplit('/').next().unwrap().to_string()).collect::<Vec<_>>();
        let stream = "#EXTM3U\n#EXTINF:6,\na.ts\n#EXTINF:6,\nb.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nhttps://ads.example/1.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nhttps://ads.example/2.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nc.ts\n#EXTINF:6,\nd.ts\n#EXTINF:6,\ne.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(names(&parse_media(stream, &base).unwrap()), ["a.ts", "b.ts", "c.ts", "d.ts", "e.ts"]);
        // Sequence numbers stay, because decryption can depend on them.
        assert_eq!(parse_media(stream, &base).unwrap().segments[2].seq, 4);
        // Without the marks, another server is just another server.
        let mirrors = "#EXTM3U\n#EXTINF:6,\na.ts\n#EXTINF:6,\nhttps://cdn2.example/b.ts\n#EXTINF:6,\nc.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(parse_media(mirrors, &base).unwrap().segments.len(), 3);
        // Half the playlist on another server isn't an ad break.
        let half = "#EXTM3U\n#EXTINF:6,\na.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6,\nhttps://cdn2.example/b.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(parse_media(half, &base).unwrap().segments.len(), 2);
        assert_eq!(parse_stream(stream, &base, true).unwrap().segments.len() > parse_stream(stream, &base, false).unwrap().segments.len(), true);
    }

    #[test]
    fn a_signature_is_carried_over_only_when_asked_for() {
        let plain = Url::parse("https://x/a.m3u8?Policy=p&Signature=s").unwrap();
        assert_eq!(signature(&plain), None);
        let asked = Url::parse("https://x/a.m3u8?Policy=p&Signature=s#rdm-quality=720&rdm-keep-query").unwrap();
        let s = signature(&asked);
        let mut segment = Url::parse("https://x/0.ts").unwrap();
        sign(&mut segment, s.as_deref());
        assert_eq!(segment.as_str(), "https://x/0.ts?Policy=p&Signature=s");
        let mut own = Url::parse("https://x/1.ts?t=1").unwrap();
        sign(&mut own, s.as_deref());
        assert_eq!(own.query(), Some("t=1"));
    }

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
    fn the_same_picture_goes_with_audio_that_plays_everywhere() {
        let base = Url::parse("https://cdn.example.com/m.m3u8").unwrap();
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1920x1080\naac.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=8200000,CODECS=\"avc1.64002a,ac-3\",RESOLUTION=1920x1080\nac3.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"avc1.64001e,mp4a.40.2\",RESOLUTION=640x360\nsd-aac.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1100000,CODECS=\"avc1.64001e,ec-3\",RESOLUTION=640x360\nsd-ec3.m3u8\n";
        let v = parse_master(m, &base);
        assert_eq!(pick_variant(&v, None).unwrap().url.path(), "/aac.m3u8");
        assert_eq!(pick_variant(&v, Some(360)).unwrap().url.path(), "/sd-aac.m3u8");
        // Far lower quality isn't worth it, and neither is a smaller picture.
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1920x1080\naac.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=8200000,CODECS=\"avc1.64002a,ac-3\",RESOLUTION=1920x1080\nac3.m3u8\n";
        assert_eq!(pick_variant(&parse_master(m, &base), None).unwrap().url.path(), "/ac3.m3u8");
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1280x720\naac.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=8200000,CODECS=\"avc1.64002a,ac-3\",RESOLUTION=1920x1080\nac3.m3u8\n";
        assert_eq!(pick_variant(&parse_master(m, &base), None).unwrap().url.path(), "/ac3.m3u8");
    }

    fn audio_for(m: &str) -> Option<String> {
        let base = Url::parse("https://cdn.example.com/m.m3u8").unwrap();
        let v = parse_master(m, &base);
        pick_audio(m, &base, &v[0]).map(|u| u.path().to_string())
    }

    #[test]
    fn audio_rendition_choice() {
        // The default of the variant's own group, even if it is listed last.
        let m = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"b\",DEFAULT=YES,URI=\"other.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"fr.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES,URI=\"en.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nv.m3u8\n";
        assert_eq!(audio_for(m).as_deref(), Some("/en.m3u8"));
        // Autoselect comes before just the first one.
        let m = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"fr.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",AUTOSELECT=YES,URI=\"en.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nv.m3u8\n";
        assert_eq!(audio_for(m).as_deref(), Some("/en.m3u8"));
        // A default without a URI is muxed into the video: the alternatives must not replace it.
        let m = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"fr.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nv.m3u8\n";
        assert_eq!(audio_for(m), None);
        // No group named: the codecs tell whether the video has its own sound.
        let silent = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"en.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"avc1.64001f\"\nv.m3u8\n";
        assert_eq!(audio_for(silent).as_deref(), Some("/en.m3u8"));
        assert_eq!(audio_for(&silent.replace("avc1.64001f", "avc1.64001f,mp4a.40.2")), None);
        assert_eq!(audio_for(&silent.replace(",CODECS=\"avc1.64001f\"", "")), None);
    }

    #[test]
    fn the_init_segment_most_of_the_stream_uses_wins() {
        let base = Url::parse("https://cdn.example.com/v/index.m3u8").unwrap();
        let t = "#EXTM3U\n#EXT-X-MAP:URI=\"ad.mp4\"\n#EXTINF:4,\nad0.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"main.mp4\"\n#EXTINF:4,\nm0.m4s\n#EXTINF:4,\nm1.m4s\n#EXT-X-MAP:URI=\"main.mp4\"\n#EXTINF:4,\nm2.m4s\n#EXT-X-ENDLIST\n";
        let pl = parse_media(t, &base).unwrap();
        assert_eq!(pl.init.as_ref().unwrap().0.path(), "/v/main.mp4");
        assert_eq!(pl.segments.iter().map(|s| s.url.path()).collect::<Vec<_>>(), ["/v/m0.m4s", "/v/m1.m4s", "/v/m2.m4s"]);
    }

    #[test]
    fn drm_is_named_and_inline_keys_work() {
        let base = Url::parse("https://cdn.example.com/index.m3u8").unwrap();
        for k in ["METHOD=SAMPLE-AES,URI=\"skd://x\"", "METHOD=AES-128,URI=\"k\",KEYFORMAT=\"com.apple.streamingkeydelivery\""] {
            let e = parse_media(&format!("#EXT-X-KEY:{k}\n#EXTINF:4,\na.ts\n"), &base).unwrap_err();
            assert!(e.to_string().contains("DRM"), "{e}");
        }
        assert_eq!(data_uri(&Url::parse("data:text/plain;base64,AAECAwQFBgcICQoLDA0ODw==").unwrap()).unwrap(), (0..16).collect::<Vec<u8>>());
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
