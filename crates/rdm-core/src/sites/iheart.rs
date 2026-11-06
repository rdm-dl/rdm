//! iHeart podcasts: a public API with each episode's sound file.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const API: &str = "https://api.iheart.com/api/v3/podcast";
const PAGE: usize = 100;
const MOST_PAGES: usize = 100;

pub fn family() -> Family {
    Family::single("iHeart", "iheart.com", true)
}

/// The number at the end of `a-title-123`.
fn number(part: &str) -> Option<&str> {
    let id = part.rsplit('-').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn parts(url: &Url) -> Vec<&str> {
    if matches!(plain_host(url, &["www."]), "iheart.com" | "iheartpodcastnetwork.com") { path_parts(url) } else { vec![] }
}

fn episode_of(url: &Url) -> Option<&str> {
    match parts(url).as_slice() {
        ["podcast", _, "episode", part, ..] => number(part),
        _ => None,
    }
}

fn show_of(url: &Url) -> Option<&str> {
    match parts(url).as_slice() {
        ["podcast", part] => number(part),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    episode_of(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    show_of(url).is_some()
}

fn kind(file: &str) -> &'static str {
    if file.split('?').next().is_some_and(|p| p.ends_with(".m4a")) { "m4a" } else { "mp3" }
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = episode_of(page.url).ok_or_else(|| fail("not an episode page"))?;
    let reply = page.json(page.client.get(format!("{API}/episodes/{id}"))).await?;
    let episode = &reply["episode"];
    let url = episode["mediaUrl"].as_str().ok_or_else(|| fail("iHeart has no file for this episode"))?;
    Ok(Found::new(url, episode["title"].as_str().map(str::to_string), episode["duration"].as_u64()).ext(kind(url)))
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let id = show_of(page.url).ok_or_else(|| fail("not a show page"))?;
    let show = page.json(page.client.get(format!("{API}/podcasts/{id}"))).await?;
    let name = show["title"].as_str().unwrap_or("iHeart").to_string();
    let mut out = vec![];
    let mut next: Option<String> = None;
    for _ in 0..MOST_PAGES {
        let mut request = page.client.get(format!("{API}/podcasts/{id}/episodes")).query(&[("limit", PAGE)]);
        if let Some(after) = &next {
            request = request.query(&[("pageKey", after)]);
        }
        let reply = page.json(request).await?;
        let list = reply["data"].as_array().cloned().unwrap_or_default();
        for e in &list {
            let (Some(episode), Some(title)) = (e["id"].as_u64(), e["title"].as_str()) else { continue };
            out.push(Episode::item(format!("https://www.iheart.com/podcast/{id}/episode/{episode}/"), &name, title, None, e["duration"].as_u64(), "mp3"));
        }
        next = reply["links"]["next"].as_str().map(str::to_string);
        if next.is_none() || list.is_empty() {
            break;
        }
    }
    if out.is_empty() {
        return Err(fail("This show has no episodes to get."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episodes_and_shows() {
        let url = |u: &str| Url::parse(u).unwrap();
        let episode = url("https://www.iheart.com/podcast/105-behind-the-bastards-29236323/episode/part-one-the-dictator-70346499/?embed=true");
        assert_eq!(episode_of(&episode), Some("70346499"));
        assert!(!is_series(&episode));
        let show = url("https://www.iheart.com/podcast/1119-it-could-happen-here-30717896/");
        assert_eq!(show_of(&show), Some("30717896"));
        assert!(is_series(&url("https://www.iheartpodcastnetwork.com/podcast/105-stuff-you-should-know-26940277")));
        assert!(matches(&url("https://www.iheart.com/podcast/30717896/episode/70346499/")));
        assert_eq!(kind("https://x/a.m4a?t=1"), "m4a");
    }
}
