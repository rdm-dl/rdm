//! A+E Global Media. Each episode is unlocked through the viewer's TV provider by the site's own
//! player, so only the browser can open a stream: a pasted episode link has nothing to look up.
//! What RDM does here is read a show's episodes from its page, for the extension to play one
//! after another and catch each stream.

use super::{Episode, Family, Found, Page, SiteInfo, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const DOMAINS: &[&str] = &["aetv.com", "history.com", "mylifetime.com", "fyi.tv"];

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family {
        name: "A+E",
        sites: vec![
            site("A&E", "aetv.com", true),
            site("History", "history.com", false),
            site("Lifetime", "mylifetime.com", false),
            site("LMN", "mylifetime.com", false),
            site("FYI", "fyi.tv", false),
            site("Vice TV", "vicetv.com", false),
            site("Biography", "biography.com", false),
            site("Crime + Investigation", "crimeandinvestigation.com", false),
            site("LRW", "mylifetime.com", false),
        ],
        browser_only: true,
    }
}

pub fn matches(_: &Url) -> bool {
    false
}

pub async fn find(_: &Page<'_>) -> Result<Found> {
    Err(fail("Play this video in your browser and download it from there."))
}

/// The show's slug, and the season when the page is for one.
fn show(url: &Url) -> Option<(&str, Option<u64>)> {
    if !DOMAINS.contains(&plain_host(url, &["www.", "play.", "watch."])) {
        return None;
    }
    match path_parts(url).as_slice() {
        ["shows", slug] => Some((slug, None)),
        ["shows", slug, season] => Some((slug, Some(season.strip_prefix("season-")?.parse().ok()?))),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    show(url).is_some()
}

/// The episodes in a show page's data, in order.
fn read_page(html: &str, host: &str, slug: &str, season: Option<u64>) -> Option<Vec<Episode>> {
    let found = Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).ok()?.captures(html)?;
    let data: Value = serde_json::from_str(&found[1]).ok()?;
    let state = data["props"]["apolloState"].as_object()?;
    let base = format!("/shows/{slug}");
    let name = state.values().find(|v| v["__typename"] == "Series" && v["canonical"] == base.as_str()).and_then(|v| v["title"].as_str()).unwrap_or(slug);
    let number = |v: &Value| v.as_str().and_then(|n| n.parse().ok()).or(v.as_u64());
    let prefix = format!("{base}/");
    let mut out: Vec<Episode> = state
        .values()
        .filter(|v| v["__typename"] == "Episode")
        .filter_map(|v| {
            let canonical = v["canonical"].as_str().filter(|c| c.starts_with(&prefix))?;
            Some(Episode {
                url: format!("https://{host}{canonical}"),
                show: name.to_string(),
                title: v["title"].as_str()?.trim().replace("&amp;", "&"),
                season: number(&v["tvSeasonNumber"]),
                number: number(&v["tvSeasonEpisodeNumber"]),
                duration: None,
                ext: "mp4",
            })
        })
        .filter(|e| season.is_none() || e.season == season)
        .collect();
    out.sort_by_key(|e| (e.season, e.number));
    Some(out)
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let (slug, season) = show(page.url).ok_or_else(|| fail("not a show page"))?;
    let host = format!("play.{}", plain_host(page.url, &["www.", "play.", "watch."]));
    let html = page.text(page.client.get(format!("https://{host}/shows/{slug}"))).await?;
    read_page(&html, &host, slug, season).ok_or_else(|| fail("This show's episodes can't be read"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_pages_are_recognised() {
        let s = |u: &str| show(&Url::parse(u).unwrap()).map(|(a, b)| (a.to_string(), b));
        assert_eq!(s("https://play.aetv.com/shows/60-days-in"), Some(("60-days-in".to_string(), None)));
        assert_eq!(s("https://play.aetv.com/shows/60-days-in/season-2"), Some(("60-days-in".to_string(), Some(2))));
        assert_eq!(s("https://www.history.com/shows/pawn-stars"), Some(("pawn-stars".to_string(), None)));
        assert_eq!(s("https://play.aetv.com/shows/60-days-in/season-1/episode-1"), None);
        assert_eq!(s("https://example.com/shows/60-days-in"), None);
    }

    #[test]
    fn episodes_are_read_in_order() {
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"apolloState":{
            "Series:1":{"__typename":"Series","canonical":"/shows/60-days-in","title":"60 Days In"},
            "Series:2":{"__typename":"Series","canonical":"/shows/other","title":"Other"},
            "Episode:3":{"__typename":"Episode","title":"Second","canonical":"/shows/60-days-in/season-1/episode-2","tvSeasonNumber":"1","tvSeasonEpisodeNumber":"2"},
            "Episode:4":{"__typename":"Episode","title":"First","canonical":"/shows/60-days-in/season-1/episode-1","tvSeasonNumber":"1","tvSeasonEpisodeNumber":"1"},
            "Episode:5":{"__typename":"Episode","title":"Later","canonical":"/shows/60-days-in/season-2/episode-1","tvSeasonNumber":"2","tvSeasonEpisodeNumber":"1"},
            "Episode:6":{"__typename":"Episode","title":"Elsewhere","canonical":"/shows/other/season-1/episode-1","tvSeasonNumber":"1","tvSeasonEpisodeNumber":"1"}
        }}}</script>"#;
        let all = read_page(html, "play.aetv.com", "60-days-in", None).unwrap();
        assert_eq!(all.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["First", "Second", "Later"]);
        assert_eq!(all[0].url, "https://play.aetv.com/shows/60-days-in/season-1/episode-1");
        assert_eq!(all[0].file_name(), "60 Days In - S1 E01 First.mp4");
        assert_eq!(read_page(html, "play.aetv.com", "60-days-in", Some(2)).unwrap().len(), 1);
    }
}
