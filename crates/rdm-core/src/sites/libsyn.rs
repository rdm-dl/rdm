//! Libsyn podcasts: the episode player's page names the file, and each show has a feed.

use super::{Episode, Family, Found, Page, SiteInfo, applepodcasts, clock, fail, path_parts};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// Libsyn's own addresses, which aren't shows.
const RESERVED: [&str; 7] = ["www", "html5-player", "play", "traffic", "assets", "static", "api"];

pub fn family() -> Family {
    let site = |domain| SiteInfo { name: "Libsyn", domain, tested: true };
    Family { name: "Libsyn", sites: vec![site("html5-player.libsyn.com"), site("play.libsyn.com"), site("libsyn.com")], browser_only: false }
}

fn episode_of(url: &Url) -> Option<&str> {
    if !matches!(url.host_str()?, "html5-player.libsyn.com" | "play.libsyn.com") {
        return None;
    }
    match path_parts(url).as_slice() {
        ["embed", "episode", "id", id, ..] if id.bytes().all(|b| b.is_ascii_digit()) => Some(*id),
        _ => None,
    }
}

fn show_of(url: &Url) -> Option<&str> {
    let name = url.host_str()?.strip_suffix(".libsyn.com")?;
    (!name.contains('.') && !RESERVED.contains(&name) && matches!(path_parts(url).as_slice(), [] | ["rss"])).then_some(name)
}

pub fn matches(url: &Url) -> bool {
    episode_of(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    show_of(url).is_some()
}

fn player_item(html: &str) -> Option<Value> {
    let found = Regex::new(r"var\s+playlistItem\s*=\s*(\{.+?\});").ok()?.captures(html)?;
    serde_json::from_str(&found[1]).ok()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = episode_of(page.url).ok_or_else(|| fail("not an episode page"))?;
    let html = page.text(page.client.get(format!("https://html5-player.libsyn.com/embed/episode/id/{id}/"))).await?;
    let item = player_item(&html).ok_or_else(|| fail("Libsyn's player page has changed"))?;
    let url = ["media_url_libsyn", "media_url", "download_link"].iter().find_map(|k| item[*k].as_str().filter(|u| !u.is_empty()));
    let url = url.ok_or_else(|| fail("Libsyn has no file for this episode"))?;
    let duration = item["duration"].as_str().and_then(clock);
    let kind = if url.split('?').next().is_some_and(|p| p.ends_with(".m4a")) { "m4a" } else { "mp3" };
    Ok(Found::new(url, item["item_title"].as_str().map(|t| t.trim().to_string()), duration).ext(kind))
}

fn feed_title(xml: &str) -> Option<String> {
    let feed = roxmltree::Document::parse(xml).ok()?;
    let channel = feed.descendants().find(|n| n.has_tag_name("channel"))?;
    channel.children().find(|c| c.has_tag_name("title"))?.text().map(|t| t.trim().to_string())
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let name = show_of(page.url).ok_or_else(|| fail("not a show page"))?;
    let xml = page.text(page.client.get(format!("https://{name}.libsyn.com/rss"))).await?;
    let show = feed_title(&xml).unwrap_or_else(|| name.to_string());
    let out = applepodcasts::from_feed(&xml, &show);
    if out.is_empty() {
        return Err(fail("This show has no episodes to get."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn players_and_shows() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert_eq!(episode_of(&url("https://html5-player.libsyn.com/embed/episode/id/3727166/height/75/width/200/")), Some("3727166"));
        assert_eq!(episode_of(&url("https://play.libsyn.com/embed/episode/id/6385796/")), Some("6385796"));
        assert_eq!(show_of(&url("https://undergroundwellness.libsyn.com/")), Some("undergroundwellness"));
        assert_eq!(show_of(&url("https://undergroundwellness.libsyn.com/rss")), Some("undergroundwellness"));
        assert_eq!(show_of(&url("https://html5-player.libsyn.com/")), None);
        assert_eq!(show_of(&url("https://show.libsyn.com/an-episode")), None);
    }

    #[test]
    fn player_data_and_feed_title() {
        let html = r#"<script>var playlistItem = {"item_title":"One","duration":"00:13:54"};</script>"#;
        assert_eq!(player_item(html).unwrap()["item_title"], "One");
        let xml = r#"<rss><channel><title> A Show </title><item><title>One</title></item></channel></rss>"#;
        assert_eq!(feed_title(xml).as_deref(), Some("A Show"));
    }
}
