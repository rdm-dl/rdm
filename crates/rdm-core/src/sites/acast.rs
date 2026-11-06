//! Acast podcasts: a public API with each episode's sound file.

use super::{Episode, Family, Found, Page, fail, path_parts};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Acast", "acast.com", true)
}

/// The show, and the episode when the address names one.
fn parts(url: &Url) -> Option<(&str, Option<&str>)> {
    match (url.host_str()?, path_parts(url).as_slice()) {
        ("shows.acast.com", [show]) | ("play.acast.com", ["s", show]) => Some((show, None)),
        ("shows.acast.com", [show, "episodes", episode]) | ("play.acast.com", ["s", show, episode]) => Some((show, Some(episode))),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    parts(url).is_some_and(|(_, episode)| episode.is_some())
}

pub fn is_series(url: &Url) -> bool {
    parts(url).is_some_and(|(_, episode)| episode.is_none())
}

fn length(episode: &Value) -> Option<u64> {
    episode["duration"].as_u64()
}

/// Acast's files are MP3 unless the address says otherwise.
fn extension(url: &str) -> &'static str {
    if url.split('?').next().is_some_and(|p| p.ends_with(".m4a")) { "m4a" } else { "mp3" }
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let (show, episode) = parts(page.url).ok_or_else(|| fail("not an episode page"))?;
    let episode = episode.ok_or_else(|| fail("not an episode page"))?;
    let reply = page.json(page.client.get(format!("https://feeder.acast.com/api/v1/shows/{show}/episodes/{episode}"))).await?;
    let url = reply["url"].as_str().ok_or_else(|| fail("Acast has no file for this episode"))?;
    Ok(Found::new(url, reply["title"].as_str().map(str::to_string), length(&reply)).ext(extension(url)))
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let (show, _) = parts(page.url).ok_or_else(|| fail("not a show page"))?;
    let reply = page.json(page.client.get(format!("https://feeder.acast.com/api/v1/shows/{show}"))).await?;
    let name = reply["title"].as_str().unwrap_or(show).to_string();
    let out: Vec<_> = reply["episodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let url = e["url"].as_str()?;
            Some(Episode::item(url, &name, e["title"].as_str()?, None, length(e), extension(url)))
        })
        .collect();
    if out.is_empty() {
        return Err(fail("This show has no episodes to get."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_recognised() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(matches(&url("https://shows.acast.com/sparpodcast/episodes/2.raggarmordet")));
        assert!(matches(&url("https://play.acast.com/s/rattegangspodden/s04e09")));
        assert!(is_series(&url("https://shows.acast.com/sparpodcast")));
        assert!(is_series(&url("https://play.acast.com/s/sparpodcast")));
        assert!(!matches(&url("https://shows.acast.com/sparpodcast")));
        assert!(!is_series(&url("https://www.acast.com/about")));
    }

    #[test]
    fn file_kinds() {
        assert_eq!(extension("https://a/b.mp3?x=1"), "mp3");
        assert_eq!(extension("https://a/b.m4a"), "m4a");
    }
}
