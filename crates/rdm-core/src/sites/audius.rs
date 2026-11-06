//! Audius: a public music service whose API resolves any page and streams any open track.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const API: &str = "https://api.audius.co/v1";
/// The service asks every program to name itself.
const APP: (&str, &str) = ("app_name", "rdm");
const PAGE: usize = 100;
const MOST_PAGES: usize = 20;
/// The site's own pages, which look like artists' names.
const RESERVED: [&str; 18] = ["trending", "explore", "feed", "library", "messages", "notifications", "settings", "upload", "search", "signin", "signup", "app", "legal", "rewards", "payouts", "purchases", "dashboard", "chat"];

pub fn family() -> Family {
    Family::single("Audius", "audius.co", true)
}

#[derive(Debug, PartialEq)]
enum Link<'a> {
    Track,
    List,
    Artist(&'a str),
}

fn link(url: &Url) -> Option<Link<'_>> {
    if plain_host(url, &["www."]) != "audius.co" {
        return None;
    }
    let parts = path_parts(url);
    match parts.as_slice() {
        [name, ..] if RESERVED.contains(name) => None,
        [_, "album" | "playlist", _] => Some(Link::List),
        [name] => Some(Link::Artist(name)),
        [_, _] => Some(Link::Track),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    link(url) == Some(Link::Track)
}

pub fn is_series(url: &Url) -> bool {
    matches!(link(url), Some(Link::List | Link::Artist(_)))
}

async fn call(page: &Page<'_>, path: &str, query: &[(&str, String)]) -> Result<Value> {
    let reply = page.json(page.client.get(format!("{API}/{path}")).query(&[APP]).query(query)).await?;
    match reply.get("data") {
        Some(data) if !data.is_null() => Ok(data.clone()),
        _ => Err(fail("Audius has nothing at this address.")),
    }
}

async fn resolve(page: &Page<'_>) -> Result<Value> {
    let data = call(page, "resolve", &[("url", page.url.as_str().to_string())]).await?;
    Ok(if data.is_array() { data[0].clone() } else { data })
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let track = resolve(page).await?;
    let id = track["id"].as_str().ok_or_else(|| fail("Audius has no such track"))?;
    if track["is_stream_gated"] == true {
        return Err(fail("This track is only for the artist's followers or buyers, so it can't be saved."));
    }
    let url = format!("{API}/tracks/{id}/stream?{}={}", APP.0, APP.1);
    Ok(Found::new(url, track["title"].as_str().map(str::to_string), track["duration"].as_u64()).ext("mp3"))
}

fn items(tracks: &Value, list: &str, numbered: bool, start: usize) -> Vec<Episode> {
    let open = tracks.as_array().into_iter().flatten().filter(|t| t["is_stream_gated"] != true);
    let found = open.filter_map(|t| Some((t["permalink"].as_str()?, t["title"].as_str()?, t["duration"].as_u64())));
    found.enumerate().map(|(i, (link, title, time))| Episode::item(format!("https://audius.co{link}"), list, title, numbered.then_some((start + i + 1) as u64), time, "mp3")).collect()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let mut out = vec![];
    match link(page.url) {
        Some(Link::Artist(name)) => {
            for number in 0..MOST_PAGES {
                let query = [("limit", PAGE.to_string()), ("offset", (number * PAGE).to_string())];
                let tracks = call(page, &format!("users/handle/{name}/tracks"), &query).await.unwrap_or_default();
                let count = tracks.as_array().map_or(0, Vec::len);
                out.extend(items(&tracks, name, false, 0));
                if count < PAGE {
                    break;
                }
            }
        }
        Some(Link::List) => {
            let list = resolve(page).await?;
            let id = list["id"].as_str().ok_or_else(|| fail("Audius has no such list"))?;
            let name = list["playlist_name"].as_str().unwrap_or("Audius");
            let tracks = call(page, &format!("playlists/{id}/tracks"), &[]).await?;
            out.extend(items(&tracks, name, true, 0));
        }
        _ => return Err(fail("not a list page")),
    }
    if out.is_empty() {
        return Err(fail("This page has no tracks to get."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_of_link() {
        let kind = |u: &str| link(&Url::parse(u).unwrap()).map(|k| format!("{k:?}"));
        assert_eq!(kind("https://audius.co/voltra/radar-103692").as_deref(), Some("Track"));
        assert_eq!(kind("https://audius.co/test_acc/playlist/test-playlist-22910").as_deref(), Some("List"));
        assert_eq!(kind("https://audius.co/someone/album/an-album").as_deref(), Some("List"));
        assert_eq!(kind("https://audius.co/pzl/").as_deref(), Some("Artist(\"pzl\")"));
        assert_eq!(kind("https://audius.co/trending"), None);
        assert_eq!(kind("https://audius.co/explore/premium"), None);
    }

    #[test]
    fn locked_tracks_are_left_out() {
        let tracks = json!([
            { "permalink": "/a/one", "title": "One", "duration": 10 },
            { "permalink": "/a/two", "title": "Two", "is_stream_gated": true },
            { "permalink": "/a/three", "title": "Three" },
        ]);
        let list = items(&tracks, "A", true, 0);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].url, "https://audius.co/a/three");
        assert_eq!(list[1].number, Some(2));
    }
}
