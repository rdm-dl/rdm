//! The video feed format several news sites share (Fox News, ABC News). Not a site itself.

use super::{Found, clock, fail};
use crate::download::DownloadError;
use serde_json::Value;

/// A named part of a feed item, which sits in a `media-group` or directly in the item.
fn node<'a>(item: &'a Value, name: &str) -> &'a Value {
    let group = if item["media-group"].is_object() { &item["media-group"] } else { item };
    let prefixed = &group[format!("media-{name}").as_str()];
    if prefixed.is_null() { &group[name] } else { prefixed }
}

/// The stream of a feed: HLS when there is any, then an MP4 file.
pub fn stream(feed: &Value) -> Result<Found, DownloadError> {
    let item = &feed["channel"]["item"];
    let content = node(item, "content");
    let list = content.as_array().map_or_else(|| vec![content], |l| l.iter().collect());
    let urls: Vec<&Value> = list.iter().map(|c| &c["@attributes"]).collect();
    let of = |ext: &str| urls.iter().find(|a| a["url"].as_str().is_some_and(|u| u.split('?').next().unwrap_or(u).ends_with(ext)));
    let chosen = of(".m3u8").or_else(|| of(".mp4")).ok_or_else(|| fail("The site has no stream for this video"))?;
    let url = chosen["url"].as_str().unwrap_or_default();
    let duration = urls.iter().find_map(|a| a["duration"].as_str()).and_then(|d| d.parse::<f64>().ok().map(|s| s as u64).or_else(|| clock(d)));
    let title = node(item, "title").as_str().or(item["title"].as_str()).map(str::to_string);
    Ok(Found::new(url, title, duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hls_is_preferred_and_either_layout_is_read() {
        let content = json!([
            { "@attributes": { "url": "https://x/a.f4m", "duration": "155" } },
            { "@attributes": { "url": "https://x/a.mp4" } },
            { "@attributes": { "url": "https://x/a.m3u8?t=1" } },
        ]);
        let feed = json!({ "channel": { "item": { "media-group": { "media-title": "Clip", "media-content": content } } } });
        let found = stream(&feed).unwrap();
        assert_eq!(found.url, "https://x/a.m3u8?t=1");
        assert_eq!(found.title.as_deref(), Some("Clip"));
        assert_eq!(found.duration, Some(155));
        let single = json!({ "channel": { "item": { "title": "One", "media-content": { "@attributes": { "url": "https://x/b.mp4" } } } } });
        assert_eq!(stream(&single).unwrap().url, "https://x/b.mp4");
        assert!(stream(&json!({})).is_err());
    }
}
