//! Subtitles offered by HLS and DASH streams, saved next to the video as `name.en.vtt`, where
//! players find them on their own. A subtitle that can't be fetched never fails the download:
//! the video is what was asked for.

use crate::download::{DownloadError, DownloadRequest};
use crate::naming;
use reqwest::Client;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::task::JoinSet;
use url::Url;

const AT_ONCE: usize = 6;

/// One subtitle track of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub url: Url,
    pub language: Option<String>,
    /// The file's extension.
    pub ext: &'static str,
    /// A playlist of pieces rather than one file.
    pub pieces: bool,
}

/// Whether a track's language is on a list like `en, es`. An empty list takes every language,
/// and a track that doesn't name its language is kept: it may be the only one.
pub fn wanted(list: &str, language: Option<&str>) -> bool {
    let Some(language) = language else { return true };
    let main = language.split(['-', '_']).next().unwrap_or(language);
    let mut names = list.split([',', ' ']).map(str::trim).filter(|n| !n.is_empty()).peekable();
    names.peek().is_none() || names.any(|n| n.eq_ignore_ascii_case(language) || n.eq_ignore_ascii_case(main))
}

fn attribute(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!("{name}="))?;
    if at > 0 && !line[..at].ends_with([',', ':']) {
        return attribute(&line[at + 1..], name);
    }
    let value = &line[at + name.len() + 1..];
    let value = match value.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => value.split(',').next()?,
    };
    Some(value.to_string())
}

/// The subtitle tracks an HLS master playlist offers in `group`.
pub fn hls_tracks(master: &str, base: &Url, group: &str) -> Vec<Track> {
    let lines = master.lines().filter_map(|l| l.trim().strip_prefix("#EXT-X-MEDIA:"));
    let subtitles = lines.filter(|l| attribute(l, "TYPE").as_deref() == Some("SUBTITLES") && attribute(l, "GROUP-ID").as_deref() == Some(group));
    // A forced track only has the lines for foreign speech.
    let whole = subtitles.filter(|l| attribute(l, "FORCED").as_deref() != Some("YES"));
    whole.filter_map(|l| Some(Track { url: base.join(&attribute(l, "URI")?).ok()?, language: attribute(l, "LANGUAGE"), ext: "vtt", pieces: true })).collect()
}

/// `00:01:02.500` or `01:02.500` in milliseconds.
fn stamp(text: &str) -> Option<i64> {
    let (clock, millis) = text.trim().split_once('.')?;
    let mut total = 0i64;
    for part in clock.split(':') {
        total = total * 60 + part.parse::<i64>().ok()?;
    }
    Some(total * 1000 + millis.parse::<i64>().ok()?)
}

fn stamp_text(ms: i64) -> String {
    let ms = ms.max(0);
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// `X-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000`: which stream time a piece's cues start from,
/// in milliseconds.
fn piece_clock(header: &str) -> Option<i64> {
    let map = header.lines().find_map(|l| l.trim().strip_prefix("X-TIMESTAMP-MAP="))?;
    let (mut local, mut mpegts) = (None, None);
    for part in map.split(',') {
        if let Some(v) = part.trim().strip_prefix("LOCAL:") {
            local = stamp(v);
        } else if let Some(v) = part.trim().strip_prefix("MPEGTS:") {
            mpegts = v.parse::<i64>().ok();
        }
    }
    Some(mpegts? / 90 - local?)
}

/// Move a cue's times by `by` milliseconds.
fn moved(cue: &str, by: i64) -> String {
    let moved_line = |line: &str| {
        let Some((from, rest)) = line.split_once("-->") else { return line.to_string() };
        let mut rest = rest.trim_start().splitn(2, ' ');
        let to = rest.next().unwrap_or_default();
        let settings = rest.next().map(|s| format!(" {s}")).unwrap_or_default();
        match (stamp(from), stamp(to)) {
            (Some(a), Some(b)) => format!("{} --> {}{settings}", stamp_text(a + by), stamp_text(b + by)),
            _ => line.to_string(),
        }
    };
    cue.lines().map(moved_line).collect::<Vec<_>>().join("\n")
}

/// Pieces of WebVTT as one file: one header, each cue once (a cue crossing a piece's end is in
/// both pieces), and times on one clock.
pub fn join_vtt(pieces: &[String]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    let mut seen = HashSet::new();
    let mut first_clock = None;
    for piece in pieces {
        let text = piece.trim_start_matches('\u{feff}').replace("\r\n", "\n");
        let mut blocks = text.split("\n\n");
        let header = blocks.next().unwrap_or_default();
        let by = piece_clock(header).map_or(0, |clock| clock - *first_clock.get_or_insert(clock));
        for cue in blocks.map(str::trim).filter(|b| b.contains("-->")) {
            let cue = if by == 0 { cue.to_string() } else { moved(cue, by) };
            if seen.insert(cue.clone()) {
                out.push_str(&cue);
                out.push_str("\n\n");
            }
        }
    }
    out
}

async fn text(client: &Client, url: &Url, req: &DownloadRequest) -> Result<String, DownloadError> {
    crate::hls::fetch_text(client, url, req).await
}

/// Every piece, in order, a few at a time.
async fn fetch_all(client: &Client, req: &DownloadRequest, addresses: Vec<Url>) -> Result<Vec<String>, DownloadError> {
    let req = Arc::new(req.clone());
    let mut pieces = vec![String::new(); addresses.len()];
    let mut waiting = addresses.into_iter().enumerate();
    let mut set = JoinSet::new();
    loop {
        while set.len() < AT_ONCE {
            let Some((index, url)) = waiting.next() else { break };
            let (client, req) = (client.clone(), req.clone());
            set.spawn(async move { (index, text(&client, &url, &req).await) });
        }
        let Some(done) = set.join_next().await else { return Ok(pieces) };
        let (index, piece) = done.map_err(|e| DownloadError::Other(e.to_string()))?;
        pieces[index] = piece?;
    }
}

/// The whole subtitle text of a track. `None` when it's in a form that isn't plain text.
async fn read_track(client: &Client, track: &Track, req: &DownloadRequest) -> Result<Option<String>, DownloadError> {
    let first = text(client, &track.url, req).await?;
    if !track.pieces {
        return Ok(Some(first));
    }
    if first.trim_start_matches('\u{feff}').starts_with("WEBVTT") {
        return Ok(Some(first));
    }
    let addresses: Vec<Url> = first.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| track.url.join(l).ok()).collect();
    let pieces = fetch_all(client, req, addresses).await?;
    if pieces.is_empty() || !pieces.iter().all(|p| p.trim_start_matches('\u{feff}').starts_with("WEBVTT")) {
        return Ok(None);
    }
    Ok(Some(join_vtt(&pieces)))
}

/// Save the tracks on the `languages` list next to `video`: the first one of each language.
pub async fn save(client: &Client, req: &DownloadRequest, tracks: Vec<Track>, languages: &str, video: &Path) {
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str())) else { return };
    let mut done = HashSet::new();
    for track in tracks.iter().filter(|t| wanted(languages, t.language.as_deref())) {
        let language = track.language.clone().unwrap_or_else(|| "subtitles".into());
        if !done.insert(language.to_ascii_lowercase()) {
            continue;
        }
        match read_track(client, track, req).await {
            Ok(Some(body)) => {
                let path = naming::unique_path(dir, &format!("{stem}.{}.{}", naming::sanitize(&language), track.ext));
                if let Err(e) = std::fs::write(&path, body) {
                    tracing::warn!("subtitles not saved: {e}");
                }
            }
            Ok(None) => tracing::info!("subtitles for {language} aren't plain text; left out"),
            Err(e) => tracing::warn!("subtitles for {language} not fetched: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_on_the_list() {
        assert!(wanted("en", Some("en")));
        assert!(wanted("en", Some("en-US")));
        assert!(wanted("en, es", Some("ES")));
        assert!(wanted("pt-BR", Some("pt-BR")));
        assert!(!wanted("en", Some("fr")));
        assert!(wanted("", Some("fr")));
        assert!(wanted("en", None));
    }

    #[test]
    fn master_playlist_tracks() {
        let base = Url::parse("https://x/a/master.m3u8").unwrap();
        let master = concat!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"en\",NAME=\"English\",URI=\"en.m3u8\"\n",
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"en\",FORCED=YES,URI=\"forced.m3u8\"\n",
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"other\",LANGUAGE=\"fr\",URI=\"fr.m3u8\"\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"subs\",LANGUAGE=\"de\",URI=\"audio.m3u8\"\n",
        );
        let tracks = hls_tracks(master, &base, "subs");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].url.as_str(), "https://x/a/en.m3u8");
        assert_eq!(tracks[0].language.as_deref(), Some("en"));
    }

    #[test]
    fn pieces_become_one_file() {
        let one = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n00:00:01.000 --> 00:00:02.000\nHello\n\n00:00:05.000 --> 00:00:07.000\nAcross\n".to_string();
        let two = "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\n\n00:00:05.000 --> 00:00:07.000\nAcross\n\n00:00:08.000 --> 00:00:09.000 line:90%\nThere\n".to_string();
        assert_eq!(join_vtt(&[one, two]), "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\n\n00:00:05.000 --> 00:00:07.000\nAcross\n\n00:00:08.000 --> 00:00:09.000 line:90%\nThere\n\n");
    }

    #[test]
    fn pieces_on_their_own_clocks_are_lined_up() {
        let one = "\u{feff}WEBVTT\r\nX-TIMESTAMP-MAP=MPEGTS:90000,LOCAL:00:00:00.000\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nOne\r\n".to_string();
        let two = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:630000,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:02.000 align:start\nTwo\n".to_string();
        assert_eq!(join_vtt(&[one, two]), "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n00:00:07.000 --> 00:00:08.000 align:start\nTwo\n\n");
    }
}
