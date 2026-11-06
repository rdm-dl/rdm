//! Kick: channels on air, finished streams and clips. Its API names the HLS stream or the
//! clip's file.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// The site's own pages, which look like channels.
const RESERVED: [&str; 10] = ["browse", "categories", "category", "following", "dashboard", "settings", "search", "video", "clips", "terms-of-service"];

pub fn family() -> Family {
    Family::single("Kick", "kick.com", true)
}

/// The channel from `/name`.
fn live_channel(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "kick.com" || url.query_pairs().any(|(k, _)| k == "clip") {
        return None;
    }
    match path_parts(url).as_slice() {
        [name] if !RESERVED.contains(name) => Some(*name),
        _ => None,
    }
}

fn is_uuid(id: &str) -> bool {
    let groups: Vec<_> = id.split('-').map(str::len).collect();
    groups == [8, 4, 4, 4, 12] && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// `uuid` from `/name/videos/uuid` or `/video/uuid`.
fn video_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "kick.com" {
        return None;
    }
    match path_parts(url).as_slice() {
        [_, "videos", id, ..] | ["video", id, ..] if is_uuid(id) => Some(id),
        _ => None,
    }
}

/// `clip_abc` from `/name/clips/clip_abc` or `/name?clip=clip_abc`.
fn clip_id(url: &Url) -> Option<String> {
    if plain_host(url, &["www."]) != "kick.com" {
        return None;
    }
    let by_path = match path_parts(url).as_slice() {
        [_, "clips", id, ..] if id.starts_with("clip_") => Some(id.to_string()),
        _ => None,
    };
    by_path.or_else(|| url.query_pairs().find(|(k, v)| k == "clip" && v.starts_with("clip_")).map(|(_, v)| v.into_owned()))
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some() || clip_id(url).is_some() || live_channel(url).is_some()
}

fn title(stream: &Value) -> Option<String> {
    stream["session_title"].as_str().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    if let Some(id) = clip_id(page.url) {
        let clip = page.json(page.client.get(format!("https://kick.com/api/v2/clips/{id}"))).await?;
        let clip = &clip["clip"];
        let url = ["clip_url", "video_url"].iter().find_map(|k| clip[*k].as_str()).ok_or_else(|| fail("Kick has no file for this clip"))?;
        return Ok(Found::new(url, clip["title"].as_str().map(str::to_string), clip["duration"].as_u64()));
    }
    if let Some(name) = live_channel(page.url) {
        let channel = page.json(page.client.get(format!("https://kick.com/api/v2/channels/{name}"))).await?;
        let url = channel["playback_url"].as_str().filter(|_| channel["livestream"].is_object());
        let url = url.ok_or_else(|| fail("This Kick channel isn't live right now."))?;
        return Ok(Found::new(url, title(&channel["livestream"]).or_else(|| Some(format!("{name} live"))), None));
    }
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let video = page.json(page.client.get(format!("https://kick.com/api/v1/video/{id}"))).await?;
    if video["status"] == "sub_only" || video["status"] == "subscriber_only" {
        return Err(fail("This Kick video is for the channel's subscribers."));
    }
    let url = video["source"].as_str().ok_or_else(|| fail("Kick has no stream for this video"))?;
    let stream = &video["livestream"];
    Ok(Found::new(url, title(stream), stream["duration"].as_u64().map(|ms| ms / 1000)))
}

/// `name` from `/name/videos`.
fn channel(url: &Url) -> Option<&str> {
    match path_parts(url).as_slice() {
        [name, "videos"] if plain_host(url, &["www."]) == "kick.com" => Some(name),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    channel(url).is_some()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let name = channel(page.url).ok_or_else(|| fail("not a channel's videos page"))?;
    let list = page.json(page.client.get(format!("https://kick.com/api/v2/channels/{name}/videos"))).await?;
    let open = |v: &&Value| v["is_live"] != true && v["video"]["status"] != "sub_only" && v["video"]["status"] != "subscriber_only";
    let out: Vec<_> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter(open)
        .filter_map(|v| {
            let id = v["video"]["uuid"].as_str()?;
            Some(Episode::item(format!("https://kick.com/{name}/videos/{id}"), name, &title(v).unwrap_or_else(|| id.to_string()), None, v["duration"].as_u64().map(|ms| ms / 1000), "mp4"))
        })
        .collect();
    if out.is_empty() {
        return Err(fail("This Kick channel has no saved videos."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://kick.com/xqc/videos/907b0587-bdb7-468a-832b-1154369568bb"));
        assert!(is("https://kick.com/video/907b0587-bdb7-468a-832b-1154369568bb"));
        assert!(is("https://kick.com/xqc/clips/clip_01ABC"));
        assert!(is("https://kick.com/xqc?clip=clip_01ABC"));
        assert!(is("https://kick.com/xqc"));
        assert!(!is("https://kick.com/xqc/videos"));
        assert!(is_series(&Url::parse("https://kick.com/xqc/videos").unwrap()));
        assert!(!is_series(&Url::parse("https://kick.com/xqc").unwrap()));
    }
}
