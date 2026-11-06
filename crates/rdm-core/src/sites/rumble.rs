//! Rumble: its player's data lists plain MP4 files, or HLS.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host, unescape};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// Channels show 25 videos a page.
const MOST_PAGES: u32 = 40;

pub fn family() -> Family {
    Family::single("Rumble", "rumble.com", true)
}

fn is_rumble(url: &Url) -> bool {
    plain_host(url, &["www."]) == "rumble.com"
}

/// `v5pv5f` from `/embed/v5pv5f` or `/embed/ufe9n.v5pv5f`.
fn embed_id(url: &Url) -> Option<&str> {
    match path_parts(url).as_slice() {
        ["embed", id, ..] if is_rumble(url) => id.rsplit('.').next(),
        _ => None,
    }
}

/// A video's own page: `/v1234-a-title.html`.
fn is_video_page(url: &Url) -> bool {
    is_rumble(url) && matches!(path_parts(url).as_slice(), [name] if name.starts_with('v') && name.ends_with(".html") && name.contains('-'))
}

pub fn matches(url: &Url) -> bool {
    embed_id(url).is_some() || is_video_page(url)
}

/// The best file of the player's data: the tallest MP4, or else the HLS stream.
fn best(video: &Value) -> Option<String> {
    let files = |kind: &str| video["ua"][kind].as_object().map(|m| m.iter().collect::<Vec<_>>()).unwrap_or_default();
    let height = |(key, file): &(&String, &Value)| file["meta"]["h"].as_u64().or(key.parse().ok()).unwrap_or(0);
    let tallest = files("mp4").into_iter().filter(|(_, f)| f["url"].is_string()).max_by_key(|f| height(f));
    tallest.or_else(|| files("hls").into_iter().find(|(_, f)| f["url"].is_string())).and_then(|(_, f)| f["url"].as_str()).map(str::to_string)
}

/// The id in a player's address.
fn player_id(html: &str) -> Option<String> {
    Regex::new(r"rumble\.com/embed/(?:[0-9a-z]+\.)?([0-9a-z]+)").ok()?.captures(html).map(|c| c[1].to_string())
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = match embed_id(page.url) {
        Some(id) => id.to_string(),
        None => {
            // The site's public lookup for pages; it's the one that's open to programs.
            let lookup = page.json(page.client.get("https://rumble.com/api/Media/oembed.json").query(&[("url", page.url.as_str())])).await?;
            player_id(lookup["html"].as_str().unwrap_or_default()).ok_or_else(|| fail("This Rumble page has no video"))?
        }
    };
    let video = page.json(page.client.get("https://rumble.com/embedJS/u3/").query(&[("request", "video"), ("ver", "2"), ("v", id.as_str())])).await?;
    let url = best(&video).ok_or_else(|| fail("Rumble has no file for this video"))?;
    Ok(Found::new(url, video["title"].as_str().map(unescape), video["duration"].as_u64()))
}

/// The name of a channel, from `/c/Name` or `/user/name`.
fn channel(url: &Url) -> Option<&str> {
    match path_parts(url).as_slice() {
        ["c" | "user", name, ..] if is_rumble(url) => Some(name),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    channel(url).is_some()
}

/// `/v1234-a-title.html` to `A title`: the list pages only have the address.
fn title_of(path: &str) -> String {
    let words = path.trim_start_matches('/').trim_end_matches(".html").split_once('-').map_or("", |(_, t)| t).replace('-', " ");
    let mut letters = words.chars();
    letters.next().map_or(String::new(), |first| first.to_uppercase().chain(letters).collect())
}

/// The videos' addresses on a channel's page, once each.
fn links(html: &str) -> Vec<String> {
    let Ok(link) = Regex::new(r#""relative_url":"(/v[0-9a-z]+-[^"\\]*\.html)"|href="(/v[0-9a-z]+-[^"\\]*\.html)""#) else { return vec![] };
    let mut seen = std::collections::HashSet::new();
    link.captures_iter(html).filter_map(|c| c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())).filter(|l| seen.insert(l.clone())).collect()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let name = channel(page.url).ok_or_else(|| fail("not a channel page"))?;
    let base = format!("https://rumble.com{}", page.url.path().trim_end_matches('/'));
    let mut all: Vec<String> = vec![];
    for number in 1..=MOST_PAGES {
        // The page after the last one is "not found".
        let Ok(html) = page.text(page.client.get(&base).query(&[("page", number)])).await else { break };
        let before = all.len();
        for link in links(&html) {
            if !all.contains(&link) {
                all.push(link);
            }
        }
        if all.len() == before {
            break;
        }
    }
    if all.is_empty() {
        return Err(fail("This Rumble channel has no videos."));
    }
    Ok(all.iter().map(|l| Episode::item(format!("https://rumble.com{l}"), name, &title_of(l), None, None, "mp4")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://rumble.com/embed/v5pv5f"));
        assert!(is("https://rumble.com/embed/ufe9n.v5pv5f"));
        assert!(is("https://rumble.com/vdmum1-moose-the-dog.html"));
        assert!(!is("https://rumble.com/videos?page=2"));
        assert!(!is("https://rumble.com/c/WMAR"));
        assert_eq!(embed_id(&Url::parse("https://rumble.com/embed/ufe9n.v5pv5f").unwrap()), Some("v5pv5f"));
        assert_eq!(channel(&Url::parse("https://rumble.com/c/WMAR/videos").unwrap()), Some("WMAR"));
    }

    #[test]
    fn the_tallest_file_is_taken() {
        let v = json!({ "ua": { "mp4": { "360": { "url": "low", "meta": { "h": 360 } }, "1080": { "url": "high" }, "720": { "url": "mid", "meta": { "h": 720 } } }, "hls": { "auto": { "url": "h" } } } });
        assert_eq!(best(&v).as_deref(), Some("high"));
        assert_eq!(best(&json!({ "ua": { "hls": { "auto": { "url": "h" } } } })).as_deref(), Some("h"));
        assert_eq!(best(&json!({ "ua": {} })), None);
        assert_eq!(player_id(r#"<iframe src="https://rumble.com/embed/vb0ofn/" width="1"></iframe>"#).as_deref(), Some("vb0ofn"));
    }

    #[test]
    fn list_pages_give_each_video_once() {
        let html = r#"{"url":"https://rumble.com/v3fb317-a-b.html","relative_url":"/v3fb317-a-b.html","by":{"relative_url":"/c/WMAR"}} <a href="/v3fb317-a-b.html"><a href="/v2-c.html">"#;
        assert_eq!(links(html), ["/v3fb317-a-b.html", "/v2-c.html"]);
        assert_eq!(title_of("/v3fb317-archdiocese-considering-bankruptcy.html"), "Archdiocese considering bankruptcy");
    }
}
