//! PeerTube: the same software on many independent sites, each with the same API.

use super::{Episode, Family, Found, Page, SiteInfo, fail, path_parts};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// The sites whose channel pages can be read: a page like `/c/name` means nothing elsewhere.
const SITES: [&str; 5] = ["framatube.org", "tube.aquilenet.fr", "peertube.tv", "tilvids.com", "video.ploud.fr"];
const PAGE: u64 = 100;
const MOST_PAGES: u64 = 20;

pub fn family() -> Family {
    Family { name: "PeerTube", sites: SITES.iter().map(|d| SiteInfo { name: d, domain: d, tested: matches!(*d, "framatube.org" | "tube.aquilenet.fr") }).collect(), browser_only: false }
}

/// A long id with dashes, or the 22 characters of a short link.
fn is_id(id: &str) -> bool {
    let groups: Vec<_> = id.split('-').map(str::len).collect();
    (groups == [8, 4, 4, 4, 12] && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')) || (id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn video_id(url: &Url) -> Option<&str> {
    match path_parts(url).as_slice() {
        ["w", id] | ["videos", "watch" | "embed", id] if is_id(id) => Some(id),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

/// The HLS stream, which keeps picture and sound together, or else the tallest whole file.
fn stream(video: &Value) -> Option<String> {
    let hls = video["streamingPlaylists"].as_array().into_iter().flatten().find_map(|p| p["playlistUrl"].as_str());
    hls.map(str::to_string).or_else(|| {
        let files = video["files"].as_array().into_iter().flatten();
        let height = |f: &&Value| f["resolution"]["id"].as_u64().unwrap_or(0);
        files.filter(|f| f["fileUrl"].is_string()).max_by_key(height).and_then(|f| f["fileUrl"].as_str()).map(str::to_string)
    })
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let host = page.url.host_str().unwrap_or_default();
    let video = page.json(page.client.get(format!("https://{host}/api/v1/videos/{id}"))).await?;
    let url = stream(&video).ok_or_else(|| fail("This site has no file for this video"))?;
    Ok(Found::new(url, video["name"].as_str().map(str::to_string), video["duration"].as_u64()))
}

/// The API path of a list, and whether its items wrap each video in `{"video":...}`.
fn list_path(url: &Url) -> Option<(String, bool)> {
    let known = SITES.contains(&url.host_str()?);
    match path_parts(url).as_slice() {
        ["w", "p", id, ..] | ["videos", "watch", "playlist", id, ..] if is_id(id) => Some((format!("video-playlists/{id}/videos"), true)),
        ["c" | "video-channels", name, ..] if known => Some((format!("video-channels/{name}/videos"), false)),
        ["a" | "accounts", name, ..] if known => Some((format!("accounts/{name}/videos"), false)),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    list_path(url).is_some()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let (path, wrapped) = list_path(page.url).ok_or_else(|| fail("not a list page"))?;
    let host = page.url.host_str().unwrap_or_default();
    let name = path.split('/').nth(1).unwrap_or("PeerTube").to_string();
    let mut out = vec![];
    for number in 0..MOST_PAGES {
        let reply = page.json(page.client.get(format!("https://{host}/api/v1/{path}")).query(&[("start", number * PAGE), ("count", PAGE)])).await?;
        let items = reply["data"].as_array().cloned().unwrap_or_default();
        for item in &items {
            let video = if wrapped { &item["video"] } else { item };
            let (Some(id), Some(title)) = (video["uuid"].as_str(), video["name"].as_str()) else { continue };
            out.push(Episode::item(format!("https://{host}/w/{id}"), &name, title, None, video["duration"].as_u64(), "mp4"));
        }
        if (items.len() as u64) < PAGE {
            break;
        }
    }
    if out.is_empty() {
        return Err(fail("This list has no videos."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_on_any_site_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://framatube.org/w/814decea-237a-4d69-865b-be91e34cb6ae"));
        assert!(is("https://another.example/videos/watch/814decea-237a-4d69-865b-be91e34cb6ae"));
        assert!(is("https://another.example/w/gY6bDcv5eFcimRSF3SE5a5"));
        assert!(!is("https://another.example/w/short"));
        assert!(!is("https://another.example/w/p/gY6bDcv5eFcimRSF3SE5a5"));
    }

    #[test]
    fn channels_are_read_only_on_known_sites() {
        let series = |u: &str| is_series(&Url::parse(u).unwrap());
        assert!(series("https://framatube.org/c/name/videos"));
        assert!(series("https://framatube.org/a/name"));
        assert!(series("https://another.example/w/p/gY6bDcv5eFcimRSF3SE5a5"));
        assert!(!series("https://rumble.com/c/WMAR"));
        assert!(!series("https://another.example/a/name"));
    }

    #[test]
    fn hls_comes_first() {
        let v = json!({ "files": [{ "fileUrl": "low", "resolution": { "id": 240 } }, { "fileUrl": "high", "resolution": { "id": 720 } }], "streamingPlaylists": [] });
        assert_eq!(stream(&v).as_deref(), Some("high"));
        let v = json!({ "files": [{ "fileUrl": "a", "resolution": { "id": 0 } }], "streamingPlaylists": [{ "playlistUrl": "m.m3u8" }] });
        assert_eq!(stream(&v).as_deref(), Some("m.m3u8"));
    }
}
