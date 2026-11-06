//! Vidyard, the video host behind many company sites: its player data names the stream.

use super::{Family, Found, Page, fail, path_parts};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Vidyard", "vidyard.com", true)
}

/// The id from `share.vidyard.com/watch/id`, `x.hubs.vidyard.com/watch/id` or `play.vidyard.com/id`.
fn video_id(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    if !host.ends_with(".vidyard.com") {
        return None;
    }
    let id = match (host, path_parts(url).as_slice()) {
        ("play.vidyard.com", ["player", id, ..] | [id, ..]) => *id,
        (_, ["watch" | "share", id, ..]) => *id,
        _ => return None,
    };
    let id = id.split('.').next()?;
    (id.len() >= 10).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

/// The stream of a chapter: HLS, or else the largest MP4 file.
fn stream(chapter: &Value) -> Option<&str> {
    let sources = &chapter["sources"];
    if let Some(u) = sources["hls"][0]["url"].as_str() {
        return Some(u);
    }
    let mp4 = sources["mp4"].as_array().or(sources["h264"].as_array())?;
    mp4.iter().filter_map(|m| m["url"].as_str()).next()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let reply = page.json(page.client.get(format!("https://play.vidyard.com/player/{id}.json"))).await?;
    let chapter = &reply["payload"]["chapters"][0];
    let url = stream(chapter).ok_or_else(|| fail("Vidyard has no stream for this video"))?;
    let mut found = Found::new(url, chapter["name"].as_str().map(str::to_string), chapter["seconds"].as_u64());
    found.referer = Some("https://play.vidyard.com/".into());
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://vyexample03.hubs.vidyard.com/watch/oTDMPlUv--51Th455G5u7Q").as_deref(), Some("oTDMPlUv--51Th455G5u7Q"));
        assert_eq!(id("https://play.vidyard.com/player/7GAApnNNbcZZ46k6JqJQSh.json").as_deref(), Some("7GAApnNNbcZZ46k6JqJQSh"));
        assert_eq!(id("https://www.vidyard.com/pricing"), None);
    }
}
