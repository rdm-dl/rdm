//! JW Player: the video host behind many sites' players. Its media API lists the files.

use super::{Family, Found, Page, SiteInfo, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    let site = |domain| SiteInfo { name: "JW Player", domain, tested: true };
    Family { name: "JW Player", sites: vec![site("cdn.jwplayer.com"), site("content.jwplatform.com")], browser_only: false }
}

/// The 8 character media id from a player's or preview's address.
fn media_id(url: &Url) -> Option<&str> {
    if !matches!(plain_host(url, &[]), "cdn.jwplayer.com" | "content.jwplatform.com") {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["players" | "previews", name] | ["v2", "media", name] => name.split(['-', '.']).next()?,
        _ => return None,
    };
    (id.len() == 8 && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    media_id(url).is_some()
}

/// The HLS stream, or else the tallest MP4.
fn best(item: &Value) -> Option<&str> {
    let sources = item["sources"].as_array()?;
    let hls = sources.iter().find(|s| s["type"] == "application/vnd.apple.mpegurl" && s["file"].is_string());
    let tallest = || sources.iter().filter(|s| s["type"] == "video/mp4" && s["file"].is_string()).max_by_key(|s| s["height"].as_u64().unwrap_or(0));
    hls.or_else(tallest)?["file"].as_str()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = media_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let media = page.json(page.client.get(format!("https://cdn.jwplayer.com/v2/media/{id}"))).await?;
    let item = &media["playlist"][0];
    if item["sources"].as_array().is_some_and(|s| s.iter().any(|f| f["drm"].is_object())) {
        return Err(fail("This video is protected with DRM, so it can't be saved."));
    }
    let url = best(item).ok_or_else(|| fail("This player has no file for this video"))?;
    Ok(Found::new(url, item["title"].as_str().map(str::to_string), item["duration"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn players_and_sources() {
        let id = |u: &str| media_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://content.jwplatform.com/players/nPripu9l-ALJ3XQCI.html").as_deref(), Some("nPripu9l"));
        assert_eq!(id("https://cdn.jwplayer.com/previews/nPripu9l").as_deref(), Some("nPripu9l"));
        assert_eq!(id("https://cdn.jwplayer.com/videos/nPripu9l-1ahmry41.mp4"), None);
        let item = json!({ "sources": [{ "type": "video/mp4", "height": 270, "file": "low" }, { "type": "video/mp4", "height": 608, "file": "high" }] });
        assert_eq!(best(&item), Some("high"));
    }
}
