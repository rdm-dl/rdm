//! BitChute: its site API names the media file.

use super::{Family, Found, Page, clock, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::{Value, json};
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("BitChute", "bitchute.com", true)
}

fn video_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "old."]) != "bitchute.com" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["video" | "embed", id, ..] => Some(id),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

async fn call(page: &Page<'_>, endpoint: &str, id: &str) -> Result<Value> {
    let request = page.client.post(format!("https://api.bitchute.com/api/beta/{endpoint}")).header("accept", "application/json").header("content-type", "application/json").body(json!({ "video_id": id }).to_string());
    page.json(request).await
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let media = call(page, "video/media", id).await?;
    let url = media["media_url"].as_str().ok_or_else(|| fail("BitChute has no file for this video"))?;
    // Only the name and length come from here, so a failure doesn't matter.
    let video = call(page, "video", id).await.unwrap_or_default();
    Ok(Found::new(url, video["video_name"].as_str().map(str::to_string), video["duration"].as_str().and_then(clock)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://www.bitchute.com/video/UGlrF9o9b-Q/").as_deref(), Some("UGlrF9o9b-Q"));
        assert_eq!(id("https://www.bitchute.com/channel/abc/"), None);
    }
}
