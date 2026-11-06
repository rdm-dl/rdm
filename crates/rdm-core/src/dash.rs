//! MPEG-DASH (`.mpd`) downloading: best video + best audio representation, segment
//! download in parallel, then merge with ffmpeg (or keep separate tracks without it).

use crate::download::{Control, DownloadError, DownloadRequest, Progress, Status};
use crate::limiter::Limiter;
use crate::naming;
use crate::storage::TempDir;
use reqwest::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

#[derive(Debug, Clone, PartialEq)]
pub struct SegRef {
    pub url: Url,
    /// Inclusive byte range.
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub kind: String,
    /// The `Representation` id, which a stream keeps from period to period.
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub init: Option<SegRef>,
    pub segments: Vec<SegRef>,
}

pub fn looks_like_dash(url: &str, content_type: Option<&str>) -> bool {
    crate::hls::kind_hint(url) == Some("dash")
        || Url::parse(url).map(|u| u.path().to_ascii_lowercase().ends_with(".mpd")).unwrap_or(false)
        || content_type.is_some_and(|t| t.to_ascii_lowercase().contains("dash+xml"))
}

/// ISO-8601 duration like `PT1H2M3.5S` -> seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim().strip_prefix('P')?;
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    let mut num = String::new();
    for c in date.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().ok()?;
            num.clear();
            total += v * match c {
                'Y' => 365.0 * 86400.0,
                'M' => 30.0 * 86400.0,
                'W' => 7.0 * 86400.0,
                'D' => 86400.0,
                _ => return None,
            };
        }
    }
    for c in time.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().ok()?;
            num.clear();
            total += v * match c {
                'H' => 3600.0,
                'M' => 60.0,
                'S' => 1.0,
                _ => return None,
            };
        }
    }
    Some(total)
}

fn parse_range(r: &str) -> Option<(u64, u64)> {
    let (a, b) = r.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Fill `$RepresentationID$`, `$Number$`, `$Time$`, `$Bandwidth$` (with `%0Nd` widths).
fn fill_template(t: &str, rep_id: &str, number: u64, time: u64, bandwidth: u64) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &after[..end];
        if token.is_empty() {
            out.push('$');
        } else {
            let (name, fmt) = token.split_once('%').map_or((token, None), |(n, f)| (n, Some(f)));
            let width: usize = fmt.and_then(|f| f.trim_start_matches('0').trim_end_matches('d').parse().ok()).unwrap_or(0);
            let v = match name {
                "RepresentationID" => Some(rep_id.to_string()),
                "Number" => Some(format!("{number:0width$}")),
                "Time" => Some(format!("{time:0width$}")),
                "Bandwidth" => Some(format!("{bandwidth:0width$}")),
                _ => None,
            };
            match v {
                Some(v) => out.push_str(&v),
                None => {
                    out.push('$');
                    out.push_str(token);
                    out.push('$');
                }
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn child<'a, 'b>(n: roxmltree::Node<'a, 'b>, name: &str) -> Option<roxmltree::Node<'a, 'b>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn base_url(n: roxmltree::Node, parent: &Url) -> Url {
    child(n, "BaseURL").and_then(|b| b.text()).and_then(|t| parent.join(t.trim()).ok()).unwrap_or_else(|| parent.clone())
}

/// One `Period`: a stretch of the programme, or an ad break.
#[derive(Debug, Clone, PartialEq)]
pub struct Period {
    pub tracks: Vec<Track>,
    /// In seconds.
    pub length: f64,
}

/// Parse an MPD (static / VOD) into the longest period's tracks.
#[cfg(test)]
pub fn parse_mpd(xml: &str, mpd_url: &Url) -> Result<Vec<Track>> {
    let (mut periods, main) = parse_periods(xml, mpd_url)?;
    Ok(periods.swap_remove(main).tracks)
}

/// Parse an MPD (static / VOD) into its periods, and say which is the longest.
pub fn parse_periods(xml: &str, mpd_url: &Url) -> Result<(Vec<Period>, usize)> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| DownloadError::Other(format!("bad MPD: {e}")))?;
    let mpd = doc.root_element();
    if mpd.attribute("type") == Some("dynamic") {
        return Err(DownloadError::Other("live DASH streams are not supported yet".into()));
    }
    if doc.descendants().any(|n| n.is_element() && n.tag_name().name() == "ContentProtection") {
        return Err(DownloadError::Other(crate::hls::DRM.into()));
    }
    let total_dur = mpd.attribute("mediaPresentationDuration").and_then(parse_duration).unwrap_or(0.0);
    let mpd_base = base_url(mpd, mpd_url);
    // Ad breaks are periods of their own and often come first: the longest one is the programme.
    let periods: Vec<_> = mpd.children().filter(|c| c.is_element() && c.tag_name().name() == "Period").collect();
    let length = |i: usize| {
        periods[i].attribute("duration").and_then(parse_duration).unwrap_or_else(|| {
            let start = periods[i].attribute("start").and_then(parse_duration).unwrap_or(0.0);
            let end = periods.get(i + 1).and_then(|p| p.attribute("start")).and_then(parse_duration).unwrap_or(total_dur);
            (end - start).max(0.0)
        })
    };
    if periods.is_empty() {
        return Err(DownloadError::Other("MPD has no Period".into()));
    }
    let out: Vec<Period> = periods.iter().enumerate().map(|(i, p)| Period { tracks: period_tracks(*p, &mpd_base, length(i)), length: length(i) }).collect();
    // A period can be only a link to one kept elsewhere, usually an ad: it has no tracks here.
    let main = (0..out.len()).rev().filter(|i| !out[*i].tracks.is_empty()).max_by(|a, b| out[*a].length.total_cmp(&out[*b].length));
    let main = main.ok_or_else(|| DownloadError::Other("no playable tracks in MPD".into()))?;
    Ok((out, main))
}

/// The tracks of one period.
fn period_tracks(period: roxmltree::Node, mpd_base: &Url, period_dur: f64) -> Vec<Track> {
    let mut tracks = vec![];
    let period_base = base_url(period, mpd_base);
    for aset in period.children().filter(|c| c.tag_name().name() == "AdaptationSet") {
        let aset_base = base_url(aset, &period_base);
        let aset_tmpl = child(aset, "SegmentTemplate");
        for rep in aset.children().filter(|c| c.tag_name().name() == "Representation") {
            let mime = rep.attribute("mimeType").or(aset.attribute("mimeType")).unwrap_or("");
            let ctype = aset.attribute("contentType").unwrap_or("");
            let kind = if mime.starts_with("video") || ctype == "video" {
                "video"
            } else if mime.starts_with("audio") || ctype == "audio" {
                "audio"
            } else {
                continue;
            };
            let rep_id = rep.attribute("id").unwrap_or("");
            let bandwidth: u64 = rep.attribute("bandwidth").and_then(|b| b.parse().ok()).unwrap_or(0);
            let base = base_url(rep, &aset_base);
            let mut init = None;
            let mut segments = vec![];
            if let Some(t) = child(rep, "SegmentTemplate").or(aset_tmpl) {
                let timescale: f64 = t.attribute("timescale").and_then(|v| v.parse().ok()).unwrap_or(1.0);
                let start: u64 = t.attribute("startNumber").and_then(|v| v.parse().ok()).unwrap_or(1);
                if let Some(i) = t.attribute("initialization") {
                    init = base.join(&fill_template(i, rep_id, 0, 0, bandwidth)).ok().map(|url| SegRef { url, range: None });
                }
                let media = t.attribute("media").unwrap_or("");
                if let Some(tl) = child(t, "SegmentTimeline") {
                    let (mut time, mut number) = (0u64, start);
                    for s in tl.children().filter(|c| c.tag_name().name() == "S") {
                        if let Some(tt) = s.attribute("t").and_then(|v| v.parse().ok()) {
                            time = tt;
                        }
                        let d: u64 = s.attribute("d").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let r: i64 = s.attribute("r").and_then(|v| v.parse().ok()).unwrap_or(0);
                        for _ in 0..=r.max(0) {
                            if let Ok(url) = base.join(&fill_template(media, rep_id, number, time, bandwidth)) {
                                segments.push(SegRef { url, range: None });
                            }
                            time += d;
                            number += 1;
                        }
                    }
                } else {
                    let dur: f64 = t.attribute("duration").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    if dur > 0.0 && period_dur > 0.0 {
                        let count = (period_dur / (dur / timescale)).ceil() as u64;
                        for k in 0..count {
                            let n = start + k;
                            let time = (k as f64 * dur) as u64;
                            if let Ok(url) = base.join(&fill_template(media, rep_id, n, time, bandwidth)) {
                                segments.push(SegRef { url, range: None });
                            }
                        }
                    }
                }
            } else if let Some(list) = child(rep, "SegmentList") {
                if let Some(i) = child(list, "Initialization") {
                    let url = i.attribute("sourceURL").and_then(|s| base.join(s).ok()).unwrap_or_else(|| base.clone());
                    init = Some(SegRef { url, range: i.attribute("range").and_then(parse_range) });
                }
                for s in list.children().filter(|c| c.tag_name().name() == "SegmentURL") {
                    let url = s.attribute("media").and_then(|m| base.join(m).ok()).unwrap_or_else(|| base.clone());
                    segments.push(SegRef { url, range: s.attribute("mediaRange").and_then(parse_range) });
                }
            } else {
                // SegmentBase or plain BaseURL: one file per representation.
                segments.push(SegRef { url: base.clone(), range: None });
            }
            if segments.is_empty() {
                continue;
            }
            tracks.push(Track {
                kind: kind.into(),
                id: rep_id.to_string(),
                bandwidth,
                width: rep.attribute("width").and_then(|v| v.parse().ok()),
                height: rep.attribute("height").and_then(|v| v.parse().ok()),
                codecs: rep.attribute("codecs").or(aset.attribute("codecs")).map(str::to_string),
                init,
                segments,
            });
        }
    }
    tracks
}

/// Whether two segments come from the same place. An ad break is served from somewhere else,
/// even when it's on the same servers.
fn same_place(a: &SegRef, b: &SegRef) -> bool {
    a.url.host_str() == b.url.host_str() && a.url.path_segments().and_then(|mut s| s.next()) == b.url.path_segments().and_then(|mut s| s.next())
}

/// The track of another period that carries on from `track`.
fn counterpart<'a>(track: &Track, others: &'a [Track]) -> Option<&'a Track> {
    let same_kind = || others.iter().filter(|o| o.kind == track.kind);
    let closest = || same_kind().min_by_key(|o| (o.height != track.height, o.bandwidth.abs_diff(track.bandwidth)));
    same_kind().find(|o| o.id == track.id).or_else(closest)
}

/// What it takes to put several periods together.
#[derive(Debug, Clone, PartialEq)]
struct Joined {
    /// Where each period starts in the finished video, in seconds.
    starts: Vec<f64>,
    /// For each track: the segment each period starts at.
    marks: Vec<Vec<usize>>,
}

/// The periods that go together with `main`: the same stream carried on. One served from
/// elsewhere is an ad break, and one with a different header can't share a file.
async fn join_periods(client: &Client, req: &DownloadRequest, periods: &[Period], main: usize, chosen: &[(&'static str, Track)], keep_ads: bool) -> Option<(Vec<(&'static str, Track)>, Joined)> {
    if periods.len() < 2 {
        return None;
    }
    let mut known: HashMap<Url, Option<Vec<u8>>> = HashMap::new();
    let mut identity = async |init: &SegRef| {
        if let Some(found) = known.get(&init.url) {
            return found.clone();
        }
        let bytes = fetch(client, init, req).await.ok();
        let found = bytes.and_then(|b| crate::mux::mp4::stream_identity(&b));
        known.insert(init.url.clone(), found.clone());
        found
    };
    let mut wanted = vec![];
    for (_, track) in chosen {
        wanted.push(identity(track.init.as_ref()?).await?);
    }
    let mut kept = vec![];
    'periods: for (index, period) in periods.iter().enumerate() {
        if index == main {
            kept.push(index);
            continue;
        }
        for ((_, track), want) in chosen.iter().zip(&wanted) {
            let Some(other) = counterpart(track, &period.tracks) else { continue 'periods };
            let (Some(init), Some(first), Some(ours)) = (&other.init, other.segments.first(), track.segments.first()) else { continue 'periods };
            if (!keep_ads && !same_place(first, ours)) || identity(init).await.as_ref() != Some(want) {
                continue 'periods;
            }
        }
        kept.push(index);
    }
    if kept.len() < 2 {
        return None;
    }
    let mut starts = vec![];
    let mut at = 0.0;
    for index in &kept {
        starts.push(at);
        at += periods[*index].length;
    }
    let mut joined = vec![];
    let mut marks = vec![];
    for (kind, track) in chosen {
        let mut whole = track.clone();
        whole.segments.clear();
        let mut starts_at = vec![];
        for index in &kept {
            let part = if *index == main { track } else { counterpart(track, &periods[*index].tracks)? };
            starts_at.push(whole.segments.len());
            whole.segments.extend(part.segments.iter().cloned());
        }
        marks.push(starts_at);
        joined.push((*kind, whole));
    }
    Some((joined, Joined { starts, marks }))
}

/// Periods keep their own clocks, and ones left out leave holes: put each on the finished video's
/// clock. The tracks of a period keep their distance from each other, which is the sync. A track
/// can start before its period does, because segments rarely end exactly there: so a period
/// begins where the last of its tracks begins, and what's before that overlaps the part before.
/// Only the first period has nothing to overlap.
fn line_up(files: &[PathBuf], places: &[Vec<u64>], starts: &[f64]) -> std::io::Result<()> {
    let opening: Vec<Vec<Option<f64>>> = files.iter().zip(places).map(|(f, at)| crate::mux::mp4::first_times(f, at)).collect::<std::io::Result<_>>()?;
    let first = |p: usize| opening.iter().filter_map(move |o| o[p]);
    let origin: Vec<f64> = (0..starts.len()).map(|p| if p == 0 { first(p).fold(f64::INFINITY, f64::min) } else { first(p).fold(f64::NEG_INFINITY, f64::max) }).collect();
    for (file, at) in files.iter().zip(places) {
        let moves: Vec<f64> = starts.iter().zip(&origin).map(|(start, first)| if first.is_finite() { start - first } else { 0.0 }).collect();
        crate::mux::mp4::move_times(file, at, &moves)?;
    }
    Ok(())
}

/// Subtitle tracks that are one whole file. Ones cut into pieces come in forms RDM can't read.
pub fn text_tracks(xml: &str, mpd_url: &Url) -> Vec<crate::subtitles::Track> {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return vec![] };
    let mpd = doc.root_element();
    let mpd_base = base_url(mpd, mpd_url);
    let mut out = vec![];
    for period in mpd.children().filter(|c| c.tag_name().name() == "Period") {
        let period_base = base_url(period, &mpd_base);
        for aset in period.children().filter(|c| c.tag_name().name() == "AdaptationSet") {
            let aset_base = base_url(aset, &period_base);
            if child(aset, "SegmentTemplate").is_some() {
                continue;
            }
            for rep in aset.children().filter(|c| c.tag_name().name() == "Representation") {
                let mime = rep.attribute("mimeType").or(aset.attribute("mimeType")).unwrap_or("");
                let ext = match mime {
                    "text/vtt" => "vtt",
                    "application/ttml+xml" => "ttml",
                    _ => continue,
                };
                if child(rep, "BaseURL").is_none() && child(aset, "BaseURL").is_none() {
                    continue;
                }
                out.push(crate::subtitles::Track { url: base_url(rep, &aset_base), language: aset.attribute("lang").map(str::to_string), ext, pieces: false });
            }
        }
    }
    out
}

/// Highest-bandwidth video and audio tracks.
pub fn pick_best(tracks: &[Track]) -> (Option<&Track>, Option<&Track>) {
    pick(tracks, None)
}

/// Best video at or below `max_height` (lowest available if none fits) and best audio.
pub fn pick(tracks: &[Track], max_height: Option<u32>) -> (Option<&Track>, Option<&Track>) {
    let key = |t: &&Track| (t.height.unwrap_or(0), t.bandwidth);
    let videos = || tracks.iter().filter(|t| t.kind == "video");
    let video = match max_height {
        Some(h) => videos().filter(|t| t.height.unwrap_or(0) <= h).max_by_key(key).or_else(|| videos().min_by_key(key)),
        None => videos().max_by_key(key),
    };
    (video, tracks.iter().filter(|t| t.kind == "audio").max_by_key(key))
}

async fn fetch(client: &Client, s: &SegRef, req: &DownloadRequest) -> Result<Vec<u8>> {
    let mut rb = client.get(s.url.clone());
    if let Some((a, b)) = s.range {
        rb = rb.header(reqwest::header::RANGE, format!("bytes={a}-{b}"));
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    req: &DownloadRequest,
    client: Client,
    cfg: &crate::config::EngineConfig,
    tmp: TempDir,
    limiter: Arc<Limiter>,
    global_limiter: Arc<Limiter>,
    progress: &watch::Sender<Progress>,
    control: &mut watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    let connections = cfg.max_connections;
    let url = Url::parse(&req.url).map_err(|e| DownloadError::Other(e.to_string()))?;
    let xml = String::from_utf8_lossy(&fetch(&client, &SegRef { url: url.clone(), range: None }, req).await?).into_owned();
    let (periods, main) = parse_periods(&xml, &url)?;
    let (video, audio) = pick(&periods[main].tracks, crate::hls::quality_hint(&url));
    let chosen: Vec<(&'static str, Track)> = [("video", video), ("audio", audio)].into_iter().filter_map(|(n, t)| t.map(|t| (n, t.clone()))).collect();
    // A show cut up by ad breaks is several periods: the parts go back together.
    let (chosen, joined) = match join_periods(&client, req, &periods, main, &chosen, cfg.keep_ads).await {
        Some((whole, joined)) => (whole, Some(joined)),
        None => (chosen, None),
    };

    let stem = req
        .file_name
        .clone()
        .or_else(|| naming::from_url(&url))
        .map(|n| naming::sanitize(&n))
        .map(|n| n.rsplit_once('.').map_or(n.clone(), |(s, e)| if ["mpd", "mp4", "mkv", "webm"].contains(&e.to_ascii_lowercase().as_str()) { s.to_string() } else { n.clone() }))
        .unwrap_or_else(|| "stream".into());
    let stem = naming::stream_stem(stem, &url);
    let file_name = format!("{stem}.mp4");
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.file_name = file_name.clone();
        p.resumable = true;
    });

    // Flat job list: (track index, segment index or usize::MAX for init, SegRef).
    let mut jobs = vec![];
    for (ti, (_, t)) in chosen.iter().enumerate() {
        if let Some(i) = &t.init {
            jobs.push((ti, usize::MAX, i.clone()));
        }
        for (si, s) in t.segments.iter().enumerate() {
            jobs.push((ti, si, s.clone()));
        }
    }
    let path_of = |ti: usize, si: usize| -> PathBuf {
        if si == usize::MAX { tmp.path.join(format!("t{ti}_init.bin")) } else { tmp.path.join(format!("t{ti}_{si:06}.bin")) }
    };
    let total_jobs = jobs.len();
    let done_bytes = Arc::new(AtomicU64::new(0));
    let mut finished = 0usize;
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    let mut pending = jobs.into_iter();
    let req_arc = Arc::new(req.clone());
    let mut ui = tokio::time::interval(Duration::from_millis(200));
    let (mut st, mut sb, mut speed) = (Instant::now(), 0u64, 0.0f64);
    loop {
        while set.len() < connections.max(1) {
            let Some((ti, si, seg)) = pending.next() else { break };
            let path = path_of(ti, si);
            if path.exists() {
                done_bytes.fetch_add(std::fs::metadata(&path).map_or(0, |m| m.len()), Ordering::Relaxed);
                finished += 1;
                continue;
            }
            let (client, req, counter, lim, glim) = (client.clone(), req_arc.clone(), done_bytes.clone(), limiter.clone(), global_limiter.clone());
            set.spawn(async move {
                let mut attempt = 0u64;
                loop {
                    attempt += 1;
                    match fetch(&client, &seg, &req).await {
                        Ok(data) => {
                            lim.throttle(data.len() as u64).await;
                            glim.throttle(data.len() as u64).await;
                            let part = path.with_extension("part");
                            std::fs::write(&part, &data)?;
                            std::fs::rename(&part, &path)?;
                            counter.fetch_add(data.len() as u64, Ordering::Relaxed);
                            return Ok(());
                        }
                        Err(e) if attempt >= 8 => return Err(e),
                        Err(_) => tokio::time::sleep(Duration::from_millis(500 * attempt)).await,
                    }
                }
            });
        }
        if set.is_empty() {
            break;
        }
        tokio::select! {
            j = set.join_next() => {
                if let Some(j) = j {
                    j.map_err(|e| DownloadError::Other(e.to_string()))??;
                    finished += 1;
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
                let b = done_bytes.load(Ordering::Relaxed);
                let el = st.elapsed().as_secs_f64();
                if el >= 0.5 {
                    let inst = b.saturating_sub(sb) as f64 / el;
                    speed = if speed == 0.0 { inst } else { speed * 0.6 + inst * 0.4 };
                    st = Instant::now();
                    sb = b;
                }
                let est = (finished > 0).then(|| b / finished as u64 * total_jobs as u64);
                progress.send_modify(|p| {
                    p.downloaded = b;
                    p.total = est;
                    p.speed = speed;
                    p.connections = set.len();
                });
            }
        }
    }

    progress.send_modify(|p| p.status = Status::Assembling);
    // Concatenate each track (init + segments = valid fragmented MP4 / WebM), off the
    // async workers, then put video and audio together.
    let mut tracks = vec![];
    let mut track_files = vec![];
    for (ti, (kind, t)) in chosen.iter().enumerate() {
        let out = tmp.path.join(format!("{kind}.bin"));
        let mut parts = vec![];
        if t.init.is_some() {
            parts.push(path_of(ti, usize::MAX));
        }
        parts.extend((0..t.segments.len()).map(|si| path_of(ti, si)));
        tracks.push((parts, out.clone()));
        track_files.push((kind.to_string(), out));
    }
    // Where each period starts in each joined file, in bytes.
    let places: Option<Vec<Vec<u64>>> = joined.as_ref().map(|j| {
        let size = |p: PathBuf| std::fs::metadata(p).map_or(0, |m| m.len());
        chosen.iter().enumerate().map(|(ti, (_, t))| {
            let header = if t.init.is_some() { size(path_of(ti, usize::MAX)) } else { 0 };
            j.marks[ti].iter().map(|m| header + (0..*m).map(|si| size(path_of(ti, si))).sum::<u64>()).collect()
        }).collect()
    });
    crate::join::concat(tracks, progress).await?;
    if let (Some(joined), Some(places)) = (&joined, places) {
        let (files, starts): (Vec<PathBuf>, Vec<f64>) = (track_files.iter().map(|(_, f): &(String, PathBuf)| f.clone()).collect(), joined.starts.clone());
        tokio::task::spawn_blocking(move || line_up(&files, &places, &starts)).await.map_err(|e| DownloadError::Other(e.to_string()))??;
    }
    std::fs::create_dir_all(&req.dir)?;
    let dest = naming::unique_path(&req.dir, &file_name);
    let result = if track_files.len() == 1 {
        move_file(&track_files[0].1, &dest)?;
        dest
    } else {
        let video = track_files.iter().find(|(k, _)| k == "video").map(|(_, f)| f.clone()).unwrap_or_else(|| track_files[0].1.clone());
        let audio = track_files.iter().find(|(k, _)| k == "audio").map(|(_, f)| f.clone()).unwrap_or_else(|| track_files[1].1.clone());
        crate::join::video_and_audio(video, audio, dest, &stem, progress).await?
    };
    let name = stem.clone();
    let result = crate::join::finish(result, name, progress).await?;
    if cfg.save_subtitles {
        crate::subtitles::save(&client, &req, text_tracks(&xml, &url), &cfg.subtitle_languages, &result).await;
    }
    let size = std::fs::metadata(&result).map(|m| m.len()).unwrap_or(0);
    progress.send_modify(|p| {
        p.total = Some(size);
        p.downloaded = size;
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

    fn track(id: &str, bandwidth: u64, height: Option<u32>) -> Track {
        let url = Url::parse("https://x/a.mp4").unwrap();
        Track { kind: "video".into(), id: id.into(), bandwidth, width: None, height, codecs: None, init: None, segments: vec![SegRef { url, range: None }] }
    }

    #[test]
    fn a_stream_is_followed_from_period_to_period() {
        let others = [track("720", 3000, Some(720)), track("480", 1000, Some(480))];
        assert_eq!(counterpart(&track("480", 1200, Some(480)), &others).unwrap().id, "480");
        // The name changed, the picture size didn't.
        assert_eq!(counterpart(&track("v2", 1200, Some(480)), &others).unwrap().id, "480");
    }

    #[test]
    fn an_ad_is_served_from_elsewhere() {
        let at = |u: &str| SegRef { url: Url::parse(u).unwrap(), range: None };
        assert!(same_place(&at("https://x/show/v/1.m4s"), &at("https://x/show/v/9.m4s")));
        assert!(!same_place(&at("https://x/show/v/1.m4s"), &at("https://x/ads/v/1.m4s")));
        assert!(!same_place(&at("https://x/show/1.m4s"), &at("https://y/show/1.m4s")));
    }

    #[test]
    fn whole_subtitle_files_are_found() {
        let xml = r#"<MPD><BaseURL>https://x/a/</BaseURL><Period>
          <AdaptationSet mimeType="text/vtt" lang="en"><Representation id="s"><BaseURL>en.vtt</BaseURL></Representation></AdaptationSet>
          <AdaptationSet contentType="text" lang="fr"><Representation id="t" mimeType="application/ttml+xml"><BaseURL>fr.ttml</BaseURL></Representation></AdaptationSet>
          <AdaptationSet mimeType="text/vtt" lang="de"><SegmentTemplate media="$Number$.vtt"/><Representation id="p"/></AdaptationSet>
          <AdaptationSet mimeType="video/mp4"><Representation id="v"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet>
        </Period></MPD>"#;
        let tracks = text_tracks(xml, &Url::parse("https://x/m.mpd").unwrap());
        let got: Vec<_> = tracks.iter().map(|t| (t.url.as_str(), t.language.as_deref(), t.ext)).collect();
        assert_eq!(got, [("https://x/a/en.vtt", Some("en"), "vtt"), ("https://x/a/fr.ttml", Some("fr"), "ttml")]);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_duration("PT0.5S"), Some(0.5));
        assert_eq!(parse_duration("P1DT1S"), Some(86401.0));
    }

    #[test]
    fn templates() {
        assert_eq!(fill_template("v/$RepresentationID$/seg-$Number%05d$.m4s", "720p", 7, 0, 0), "v/720p/seg-00007.m4s");
        assert_eq!(fill_template("t_$Time$_$Bandwidth$.m4s", "x", 0, 9000, 128000), "t_9000_128000.m4s");
        assert_eq!(fill_template("a$$b", "x", 0, 0, 0), "a$b");
    }

    #[test]
    fn the_longest_period_is_taken_not_the_ad_in_front_of_it() {
        let period = |attrs: &str, dir: &str| {
            format!(r#"<Period {attrs}><BaseURL>{dir}/</BaseURL><AdaptationSet contentType="video"><Representation id="v" bandwidth="1"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period>"#)
        };
        let url = Url::parse("https://cdn.example.com/m.mpd").unwrap();
        let wrap = |body: String| format!(r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10M15S">{body}</MPD>"#);
        let got = |mpd: String| parse_mpd(&mpd, &url).unwrap()[0].segments[0].url.path().to_string();
        let stated = period(r#"duration="PT15S""#, "ad") + &period(r#"duration="PT10M""#, "show");
        assert_eq!(got(wrap(stated)), "/show/v.mp4");
        // Without durations, the gaps between the starts tell the same thing.
        let started = period(r#"start="PT0S""#, "ad") + &period(r#"start="PT15S""#, "show");
        assert_eq!(got(wrap(started)), "/show/v.mp4");
        assert_eq!(got(wrap(period("", "only"))), "/only/v.mp4");
    }

    #[test]
    fn protected_streams_are_refused_with_a_clear_message() {
        let mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S"><Period>
  <AdaptationSet contentType="video"><ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
   <Representation id="v" bandwidth="1"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet>
</Period></MPD>"#;
        let url = Url::parse("https://cdn.example.com/m.mpd").unwrap();
        let e = parse_mpd(mpd, &url).err().unwrap().to_string();
        assert!(e.contains("protected with DRM"), "{e}");
    }

    #[test]
    fn parses_template_and_timeline() {
        let mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
 <Period>
  <AdaptationSet mimeType="video/mp4">
   <SegmentTemplate timescale="1000" duration="4000" startNumber="1" initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/$Number$.m4s"/>
   <Representation id="360" bandwidth="500000" width="640" height="360"/>
   <Representation id="1080" bandwidth="4000000" width="1920" height="1080"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4">
   <Representation id="aud" bandwidth="128000">
    <SegmentTemplate timescale="48000" initialization="a/init.mp4" media="a/$Time$.m4s">
     <SegmentTimeline><S t="0" d="96000" r="2"/><S d="48000"/></SegmentTimeline>
    </SegmentTemplate>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>"#;
        let base = Url::parse("https://cdn.example.com/show/manifest.mpd").unwrap();
        let tracks = parse_mpd(mpd, &base).unwrap();
        let (v, a) = pick_best(&tracks);
        let v = v.unwrap();
        assert_eq!(v.height, Some(1080));
        assert_eq!(v.segments.len(), 3); // ceil(10 / 4)
        assert_eq!(v.segments[2].url.as_str(), "https://cdn.example.com/show/1080/3.m4s");
        assert_eq!(v.init.as_ref().unwrap().url.as_str(), "https://cdn.example.com/show/1080/init.mp4");
        let a = a.unwrap();
        let urls: Vec<&str> = a.segments.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec![
            "https://cdn.example.com/show/a/0.m4s",
            "https://cdn.example.com/show/a/96000.m4s",
            "https://cdn.example.com/show/a/192000.m4s",
            "https://cdn.example.com/show/a/288000.m4s",
        ]);
    }

    #[test]
    fn parses_segment_list_and_base_url() {
        let mpd = r#"<MPD type="static"><BaseURL>https://media.example.com/v/</BaseURL><Period>
  <AdaptationSet contentType="video"><Representation id="r" bandwidth="1">
   <SegmentList><Initialization sourceURL="file.mp4" range="0-999"/>
    <SegmentURL media="file.mp4" mediaRange="1000-4999"/><SegmentURL media="file.mp4" mediaRange="5000-8999"/></SegmentList>
  </Representation></AdaptationSet>
  <AdaptationSet contentType="audio"><Representation id="x" bandwidth="2"><BaseURL>audio.m4a</BaseURL><SegmentBase indexRange="0-100"/></Representation></AdaptationSet>
 </Period></MPD>"#;
        let tracks = parse_mpd(mpd, &Url::parse("https://other/x.mpd").unwrap()).unwrap();
        let (v, a) = pick_best(&tracks);
        let v = v.unwrap();
        assert_eq!(v.init.as_ref().unwrap().range, Some((0, 999)));
        assert_eq!(v.segments[1].range, Some((5000, 8999)));
        assert_eq!(v.segments[0].url.as_str(), "https://media.example.com/v/file.mp4");
        assert_eq!(a.unwrap().segments[0].url.as_str(), "https://media.example.com/v/audio.m4a");
    }
}
