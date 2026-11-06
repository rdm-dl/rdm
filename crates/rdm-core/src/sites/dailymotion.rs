//! Dailymotion: its player's metadata names an HLS stream for every video.

use super::{Episode, Family, Found, Page, fail, path_parts};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Dailymotion", "dailymotion.com", true)
}

/// `x8u4owg` from `/video/x8u4owg_some-title`, `/embed/video/x8u4owg` or `dai.ly/x8u4owg`.
fn video_id(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    let host = ["www.", "touch.", "geo."].iter().find_map(|p| host.strip_prefix(p)).unwrap_or(host);
    let parts: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match (host, parts.as_slice()) {
        ("dai.ly", [id, ..]) => *id,
        ("dailymotion.com", ["video", id, ..] | ["embed" | "crawler", "video", id, ..]) => *id,
        _ => return None,
    };
    id.split('_').next().filter(|i| !i.is_empty())
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

/// The public API's address for a playlist or a channel, and the list's name.
fn list_api(url: &Url) -> Option<(String, &str)> {
    if !url.host_str()?.ends_with("dailymotion.com") {
        return None;
    }
    match path_parts(url).as_slice() {
        ["playlist", id, ..] => {
            let id = id.split('_').next()?;
            Some((format!("https://api.dailymotion.com/playlist/{id}/videos"), id))
        }
        [name] if !["embed", "swf", "video", "playlist", "search", "crawler", "signin", "library"].contains(name) => Some((format!("https://api.dailymotion.com/user/{name}/videos"), name)),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    list_api(url).is_some()
}

/// The videos of a playlist or channel.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let (api, name) = list_api(page.url).ok_or_else(|| fail("not a channel or playlist page"))?;
    let mut out = vec![];
    for number in 1..=20u32 {
        let query = [("fields", "id,title,duration"), ("limit", "100"), ("page", &number.to_string())];
        let reply = page.json(page.client.get(&api).query(&query)).await?;
        for v in reply["list"].as_array().into_iter().flatten() {
            let (Some(id), Some(title)) = (v["id"].as_str(), v["title"].as_str()) else { continue };
            out.push(Episode::item(format!("https://www.dailymotion.com/video/{id}"), name, title, None, v["duration"].as_u64(), "mp4"));
        }
        if reply["has_more"] != true {
            break;
        }
    }
    Ok(out)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| DownloadError::Other("not a video page".into()))?;
    let meta_url = format!("https://www.dailymotion.com/player/metadata/video/{id}");
    let meta = page.json(page.client.get(meta_url).query(&[("app", "com.dailymotion.neon")])).await?;
    if let Some(e) = meta["error"]["title"].as_str().or(meta["error"]["raw_message"].as_str()) {
        return Err(DownloadError::Other(format!("Dailymotion says: {e}")));
    }
    let qualities = meta["qualities"].as_object().ok_or_else(|| DownloadError::Other("Dailymotion has no streams for this video".into()))?;
    let stream = qualities.values().filter_map(|q| q.as_array()).flatten().find(|m| m["type"] == "application/x-mpegURL");
    let url = stream.and_then(|m| m["url"].as_str()).ok_or_else(|| DownloadError::Other("Dailymotion has no streams for this video".into()))?;
    Ok(Found::new(url, meta["title"].as_str().map(str::to_string), meta["duration"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_recognised() {
        let is = |u: &str| is_series(&Url::parse(u).unwrap());
        assert!(is("https://www.dailymotion.com/playlist/x7wdsj"));
        assert!(is("https://www.dailymotion.com/nqtv"));
        assert!(!is("https://www.dailymotion.com/video/x8u4owg"));
        assert!(!is("https://www.example.com/nqtv"));
    }

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://www.dailymotion.com/video/x8u4owg").as_deref(), Some("x8u4owg"));
        assert_eq!(id("https://www.dailymotion.com/video/x2iuewm_steam-machine-models").as_deref(), Some("x2iuewm"));
        assert_eq!(id("https://www.dailymotion.com/embed/video/x8u4owg?autoplay=1").as_deref(), Some("x8u4owg"));
        assert_eq!(id("https://dai.ly/x94cnnk").as_deref(), Some("x94cnnk"));
        assert_eq!(id("https://www.dailymotion.com/playlist/x7wdsj"), None);
        assert_eq!(id("https://www.example.com/video/x8u4owg"), None);
    }
}
