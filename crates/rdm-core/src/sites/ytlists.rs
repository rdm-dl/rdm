//! YouTube and YouTube Music playlists and albums. A video's stream needs what only the browser's
//! player has, so RDM reads the list here and the extension plays each item to catch it.

use super::{Episode, Family, Found, Page, clock, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// A long list is read in blocks of 100; this many covers any album and most playlists.
const MOST_BLOCKS: usize = 50;

pub fn family() -> Family {
    let mut family = Family::single("YouTube", "youtube.com", true);
    family.browser_only = true;
    family
}

/// A video's page, or a list's: without this, a pasted link would save the web page itself.
pub fn matches(url: &Url) -> bool {
    match (plain_host(url, &["www.", "m.", "music."]), path_parts(url).as_slice()) {
        ("youtube.com", ["watch" | "playlist", ..] | ["shorts" | "live", _, ..]) | ("youtu.be", [_, ..]) => true,
        _ => false,
    }
}

pub async fn find(_: &Page<'_>) -> Result<Found> {
    Err(fail("Play this video in your browser and download it from there."))
}

fn list_id(url: &Url) -> Option<String> {
    if !matches!(plain_host(url, &["www.", "m.", "music."]), "youtube.com") || path_parts(url).as_slice() != ["playlist"] {
        return None;
    }
    url.query_pairs().find(|(k, _)| k == "list").map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty())
}

pub fn is_series(url: &Url) -> bool {
    list_id(url).is_some()
}

/// Everything of one kind anywhere inside `value`.
fn collect<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    out.push(v);
                } else {
                    collect(v, key, out);
                }
            }
        }
        Value::Array(list) => list.iter().for_each(|v| collect(v, key, out)),
        _ => {}
    }
}

/// The text of a `{"runs":[{"text":...}]}` or `{"simpleText":...}` field.
fn text(v: &Value) -> Option<String> {
    v["simpleText"].as_str().map(str::to_string).or_else(|| Some(v["runs"].as_array()?.iter().filter_map(|r| r["text"].as_str()).collect())).filter(|t: &String| !t.is_empty())
}

/// The items in a block of playlist data: id, title, length.
fn items(data: &Value) -> Vec<(String, String, Option<u64>)> {
    let mut found = vec![];
    collect(data, "playlistVideoRenderer", &mut found);
    let mut out: Vec<_> = found
        .iter()
        .filter(|r| r["isPlayable"] != false)
        .filter_map(|r| Some((r["videoId"].as_str()?.to_string(), text(&r["title"])?, r["lengthSeconds"].as_str().and_then(|s| s.parse().ok()))))
        .collect();
    // The newer layout.
    let mut lockups = vec![];
    collect(data, "lockupViewModel", &mut lockups);
    out.extend(lockups.iter().filter(|l| l["contentType"] == "LOCKUP_CONTENT_TYPE_VIDEO").filter_map(|l| {
        let title = l["metadata"]["lockupMetadataViewModel"]["title"]["content"].as_str()?.to_string();
        let mut badges = vec![];
        collect(l, "thumbnailBadgeViewModel", &mut badges);
        Some((l["contentId"].as_str()?.to_string(), title, badges.iter().find_map(|b| b["text"].as_str().and_then(clock))))
    }));
    out
}

fn next_block(data: &Value) -> Option<String> {
    let mut found = vec![];
    collect(data, "continuationCommand", &mut found);
    found.iter().find_map(|c| c["token"].as_str()).map(str::to_string)
}

fn list_title(data: &Value) -> Option<String> {
    let named = data["metadata"]["playlistMetadataRenderer"]["title"].as_str();
    // YouTube Music names its albums this way.
    named.map(|t| t.strip_prefix("Album - ").unwrap_or(t).to_string()).or_else(|| {
        let mut found = vec![];
        collect(data, "playlistHeaderRenderer", &mut found);
        found.first().and_then(|h| text(&h["title"]))
    })
}

/// The data a page keeps in `var ytInitialData = {...};`.
fn page_data(html: &str) -> Option<Value> {
    let start = html.find("ytInitialData = ")? + "ytInitialData = ".len();
    let mut reader = serde_json::Deserializer::from_str(&html[start..]).into_iter::<Value>();
    reader.next()?.ok()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let id = list_id(page.url).ok_or_else(|| fail("not a playlist page"))?;
    let music = page.url.host_str().is_some_and(|h| h.starts_with("music."));
    // The ordinary site shows the same list, and its page is plainer to read.
    let html = page.text(page.client.get("https://www.youtube.com/playlist").query(&[("list", id.as_str()), ("hl", "en")])).await?;
    let data = page_data(&html).ok_or_else(|| fail("This playlist's data can't be read"))?;
    let name = list_title(&data).unwrap_or_else(|| format!("YouTube playlist {id}"));
    let version = Regex::new(r#""INNERTUBE_CONTEXT_CLIENT_VERSION":"([^"]+)""#).ok().and_then(|r| r.captures(&html)).map_or("2.20260101.00.00".to_string(), |c| c[1].to_string());
    let mut all = items(&data);
    let mut next = next_block(&data);
    for _ in 0..MOST_BLOCKS {
        let Some(token) = next.take() else { break };
        let body = json!({ "context": { "client": { "clientName": "WEB", "clientVersion": version, "hl": "en" } }, "continuation": token });
        let block = page.json(page.client.post("https://www.youtube.com/youtubei/v1/browse?prettyPrint=false").header("content-type", "application/json").body(body.to_string())).await?;
        all.extend(items(&block));
        next = next_block(&block);
    }
    if all.is_empty() {
        return Err(fail("This playlist is empty, private, or needs a sign-in."));
    }
    // Music's own site doesn't show the extension its player's data, and every track is also an
    // ordinary YouTube video: those pages are the ones to open, keeping just the sound.
    let ext = if music { "m4a" } else { "mp4" };
    Ok(all.iter().enumerate().map(|(i, (video, title, length))| Episode::item(format!("https://www.youtube.com/watch?v={video}"), &name, title, Some(i as u64 + 1), *length, ext)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_link_never_saves_the_web_page() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://www.youtube.com/watch?v=jNQXAC9IVRw"));
        assert!(is("https://music.youtube.com/watch?v=abc"));
        assert!(is("https://youtu.be/jNQXAC9IVRw"));
        assert!(is("https://www.youtube.com/shorts/abc"));
        assert!(!is("https://www.youtube.com/feed/history"));
    }

    #[test]
    fn playlist_links_are_recognised() {
        let id = |u: &str| list_id(&Url::parse(u).unwrap());
        assert_eq!(id("https://music.youtube.com/playlist?list=OLAK5uy_m10&si=abc").as_deref(), Some("OLAK5uy_m10"));
        assert_eq!(id("https://www.youtube.com/playlist?list=PLabc").as_deref(), Some("PLabc"));
        assert_eq!(id("https://www.youtube.com/watch?v=abc&list=PLabc"), None);
        assert_eq!(id("https://example.com/playlist?list=PLabc"), None);
    }

    #[test]
    fn the_newer_layout_is_read_too() {
        let data = json!({ "metadata": { "playlistMetadataRenderer": { "title": "Album - Kobe Brray 8" } }, "contents": [
            { "lockupViewModel": { "contentId": "abc", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
              "metadata": { "lockupMetadataViewModel": { "title": { "content": "A Track" } } },
              "contentImage": { "thumbnailBadgeViewModel": { "text": "3:42" } } } },
            { "lockupViewModel": { "contentId": "list", "contentType": "LOCKUP_CONTENT_TYPE_PLAYLIST" } },
        ] });
        assert_eq!(items(&data), [("abc".to_string(), "A Track".to_string(), Some(222))]);
        assert_eq!(list_title(&data).as_deref(), Some("Kobe Brray 8"));
    }

    #[test]
    fn a_page_gives_its_items_title_and_next_block() {
        let html = r#"<script>var ytInitialData = {"metadata":{"playlistMetadataRenderer":{"title":"An Album"}},"contents":[{"playlistVideoRenderer":{"videoId":"a1","title":{"runs":[{"text":"First"}]},"lengthSeconds":"61"}},{"playlistVideoRenderer":{"videoId":"b2","isPlayable":false,"title":{"runs":[{"text":"Gone"}]}}},{"continuationItemRenderer":{"continuationEndpoint":{"continuationCommand":{"token":"more"}}}}]};</script>"#;
        let data = page_data(html).unwrap();
        assert_eq!(items(&data), [("a1".to_string(), "First".to_string(), Some(61))]);
        assert_eq!(list_title(&data).as_deref(), Some("An Album"));
        assert_eq!(next_block(&data).as_deref(), Some("more"));
    }
}
