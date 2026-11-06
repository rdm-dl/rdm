//! Flickr videos: the site's API lists the sizes, with a key the site hands to every visitor.

use super::{Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// Worst to best.
const SIZES: [&str; 10] = ["288p", "iphone_wifi", "100", "300", "700", "360p", "appletv", "720p", "1080p", "orig"];

pub fn family() -> Family {
    Family::single("Flickr", "flickr.com", true)
}

fn photo_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "secure."]) != "flickr.com" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["photos", _, id, ..] if id.bytes().all(|b| b.is_ascii_digit()) => Some(id),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    photo_id(url).is_some()
}

/// A number or text in Flickr's replies, which use both for the same thing.
fn as_text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string()))
}

/// The best of the stream list.
fn best(streams: &Value) -> Option<&str> {
    let rank = |s: &&Value| as_text(&s["type"]).and_then(|t| SIZES.iter().position(|k| *k == t));
    streams.as_array()?.iter().filter(|s| s["_content"].is_string()).max_by_key(rank)?["_content"].as_str()
}

async fn call(page: &Page<'_>, method: &str, key: &str, id: &str, secret: Option<&str>) -> Result<Value> {
    let mut query = vec![("method", method), ("api_key", key), ("photo_id", id), ("format", "json"), ("nojsoncallback", "1")];
    query.extend(secret.map(|s| ("secret", s)));
    let reply = page.json(page.client.get("https://api.flickr.com/services/rest").query(&query)).await?;
    if reply["stat"] != "ok" {
        return Err(fail(reply["message"].as_str().unwrap_or("Flickr refused this photo").to_string()));
    }
    Ok(reply)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = photo_id(page.url).ok_or_else(|| fail("not a photo page"))?;
    let visitor = page.json(page.client.get("https://www.flickr.com/hermes_error_beacon.gne")).await?;
    let key = visitor["site_key"].as_str().ok_or_else(|| fail("Flickr gave no key"))?;
    let info = call(page, "flickr.photos.getInfo", key, id, None).await?;
    let photo = &info["photo"];
    if photo["media"] != "video" {
        return Err(fail("This Flickr page is a photo, not a video."));
    }
    let streams = call(page, "flickr.video.getStreamInfo", key, id, photo["secret"].as_str()).await?;
    let url = best(&streams["streams"]["stream"]).ok_or_else(|| fail("Flickr has no file for this video"))?;
    let duration = as_text(&photo["video"]["duration"]).and_then(|d| d.parse().ok());
    Ok(Found::new(url, photo["title"]["_content"].as_str().map(str::to_string), duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_and_sizes() {
        assert!(matches(&Url::parse("https://www.flickr.com/photos/forestwander-nature-pictures/5645318632/in/photostream/").unwrap()));
        assert!(!matches(&Url::parse("https://www.flickr.com/photos/forestwander-nature-pictures/").unwrap()));
        let list = json!([{ "type": 700, "_content": "mid" }, { "type": "orig", "_content": "top" }, { "type": "360p", "_content": "low" }]);
        assert_eq!(best(&list), Some("top"));
    }
}
