//! SoundCloud tracks. The site's own scripts hold the key its player uses with the API.
//! Encrypted streams (some paid tracks) are left alone.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const API: &str = "https://api-v2.soundcloud.com/";
/// First parts of an address that are site pages, not users.
const PAGES: &[&str] = &["discover", "you", "search", "stations", "charts", "upload", "pages"];
/// Second parts that are lists, not tracks.
const LISTS: &[&str] = &["sets", "tracks", "albums", "reposts", "likes", "popular-tracks", "spotlight", "toptracks", "followers", "following"];

pub fn family() -> Family {
    Family::single("SoundCloud", "soundcloud.com", true)
}

pub fn matches(url: &Url) -> bool {
    plain_host(url, &["www.", "m."]) == "soundcloud.com" && matches!(path_parts(url).as_slice(), [user, track, ..] if !PAGES.contains(user) && !LISTS.contains(track))
}

async fn client_id(page: &Page<'_>) -> Result<String> {
    let home = page.text(page.client.get("https://soundcloud.com/")).await?;
    let script = Regex::new(r#"<script[^>]+src="([^"]+)""#).map_err(|e| fail(e.to_string()))?;
    let key = Regex::new(r#"client_id\s*:\s*"([0-9a-zA-Z]{32})""#).map_err(|e| fail(e.to_string()))?;
    let scripts: Vec<_> = script.captures_iter(&home).map(|c| c[1].to_string()).collect();
    for src in scripts.iter().rev() {
        let Ok(body) = page.text(page.client.get(src)).await else { continue };
        if let Some(c) = key.captures(&body) {
            return Ok(c[1].to_string());
        }
    }
    Err(fail("SoundCloud's player key wasn't found"))
}

/// `/artist`, `/artist/tracks` or `/artist/sets/a-list`.
pub fn is_series(url: &Url) -> bool {
    plain_host(url, &["www.", "m."]) == "soundcloud.com"
        && match path_parts(url).as_slice() {
            [user] | [user, "tracks"] => !PAGES.contains(user),
            [user, "sets", _] => !PAGES.contains(user),
            _ => false,
        }
}

fn as_item(track: &Value, list: &str, number: Option<u64>) -> Option<Episode> {
    let url = track["permalink_url"].as_str()?;
    Some(Episode::item(url, list, track["title"].as_str()?, number, track["duration"].as_u64().map(|ms| ms / 1000), "m4a"))
}

/// The tracks of a set or an artist.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let key = client_id(page).await?;
    let mut address = page.url.clone();
    address.set_query(None);
    address.set_fragment(None);
    let tracks_page = address.path().ends_with("/tracks");
    if tracks_page {
        let path = address.path().trim_end_matches("/tracks").to_string();
        address.set_path(&path);
    }
    let item = page.json(page.client.get(format!("{API}resolve")).query(&[("url", address.as_str()), ("client_id", &key)])).await?;
    let mut out = vec![];
    match item["kind"].as_str() {
        Some("playlist") => {
            let name = item["title"].as_str().unwrap_or("SoundCloud set").to_string();
            let mut tracks: Vec<Value> = item["tracks"].as_array().cloned().unwrap_or_default();
            // A set only spells out its first tracks; the others come by id, in blocks.
            let bare: Vec<String> = tracks.iter().filter(|t| t["permalink_url"].is_null()).filter_map(|t| t["id"].as_u64().map(|i| i.to_string())).collect();
            let mut full = std::collections::HashMap::new();
            for block in bare.chunks(50) {
                let reply = page.json(page.client.get(format!("{API}tracks")).query(&[("ids", block.join(",")), ("client_id", key.clone())])).await?;
                for t in reply.as_array().into_iter().flatten() {
                    full.insert(t["id"].as_u64(), t.clone());
                }
            }
            for t in &mut tracks {
                if let Some(whole) = full.remove(&t["id"].as_u64()) {
                    *t = whole;
                }
            }
            out.extend(tracks.iter().enumerate().filter_map(|(i, t)| as_item(t, &name, Some(i as u64 + 1))));
        }
        Some("user") => {
            let name = item["username"].as_str().unwrap_or("SoundCloud artist").to_string();
            let id = item["id"].as_u64().ok_or_else(|| fail("This SoundCloud artist can't be read"))?;
            let mut next = Some(format!("{API}users/{id}/tracks?limit=200&linked_partitioning=1"));
            while let Some(url) = next.take() {
                let reply = page.json(page.client.get(url).query(&[("client_id", key.as_str())])).await?;
                out.extend(reply["collection"].as_array().into_iter().flatten().filter_map(|t| as_item(t, &name, None)));
                next = reply["next_href"].as_str().map(str::to_string);
            }
        }
        _ => return Err(fail("This SoundCloud page isn't a set or an artist")),
    }
    Ok(out)
}

/// The best stream without encryption, and the extension of its file.
fn pick(track: &Value) -> Option<(&str, &'static str)> {
    let list = track["media"]["transcodings"].as_array()?;
    let of = |protocol: &str, mime: &str| list.iter().find(|t| t["format"]["protocol"] == protocol && t["format"]["mime_type"].as_str().is_some_and(|m| m.contains(mime)));
    let (t, ext) = of("hls", "mp4").map(|t| (t, "m4a")).or_else(|| of("progressive", "mpeg").map(|t| (t, "mp3"))).or_else(|| of("hls", "mpeg").map(|t| (t, "m4a")))?;
    Some((t["url"].as_str()?, ext))
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let key = client_id(page).await?;
    let mut address = page.url.clone();
    address.set_query(None);
    address.set_fragment(None);
    let track = page.json(page.client.get(format!("{API}resolve")).query(&[("url", address.as_str()), ("client_id", &key)])).await?;
    if track["kind"] != "track" {
        return Err(fail("This SoundCloud page isn't a track"));
    }
    if track["policy"] == "BLOCK" {
        return Err(fail("SoundCloud doesn't offer this track in your country"));
    }
    let (stream, ext) = pick(&track).ok_or_else(|| fail("SoundCloud offers this track only in encrypted form, so it can't be saved."))?;
    let mut query = vec![("client_id", key.as_str())];
    if let Some(auth) = track["track_authorization"].as_str() {
        query.push(("track_authorization", auth));
    }
    let link = page.json(page.client.get(stream).query(&query)).await?;
    let url = link["url"].as_str().ok_or_else(|| fail("SoundCloud gave no stream for this track"))?;
    let title = track["title"].as_str().map(|t| match track["user"]["username"].as_str() {
        Some(user) if !t.contains(" - ") => format!("{user} - {t}"),
        _ => t.to_string(),
    });
    Ok(Found::new(url, title, track["duration"].as_u64().map(|ms| ms / 1000)).ext(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_are_recognised() {
        let is = |u: &str| is_series(&Url::parse(u).unwrap());
        assert!(is("https://soundcloud.com/ethmusic"));
        assert!(is("https://soundcloud.com/ethmusic/tracks"));
        assert!(is("https://soundcloud.com/ethmusic/sets/a-list"));
        assert!(!is("https://soundcloud.com/ethmusic/a-song"));
        assert!(!is("https://soundcloud.com/discover"));
    }

    #[test]
    fn track_pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy"));
        assert!(!is("https://soundcloud.com/ethmusic"));
        assert!(!is("https://soundcloud.com/ethmusic/sets/a-list"));
        assert!(!is("https://soundcloud.com/discover/new"));
    }

    #[test]
    fn encrypted_streams_are_left_out() {
        let t = |p: &str, m: &str, u: &str| json!({ "url": u, "format": { "protocol": p, "mime_type": m } });
        let track = json!({ "media": { "transcodings": [t("ctr-encrypted-hls", "audio/mp4", "x"), t("hls", "audio/mpeg", "b"), t("hls", "audio/mp4; codecs=\"mp4a.40.2\"", "a")] } });
        assert_eq!(pick(&track), Some(("a", "m4a")));
        assert_eq!(pick(&json!({ "media": { "transcodings": [t("ctr-encrypted-hls", "audio/mp4", "x")] } })), None);
    }
}
