//! Spreaker podcasts: a public API with each episode's sound file.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const MOST_PAGES: usize = 50;

pub fn family() -> Family {
    Family::single("Spreaker", "spreaker.com", true)
}

/// The number at the end of `/episode/a-title--123` or `/episode/123`.
fn number(part: &str) -> Option<&str> {
    let id = part.rsplit("--").next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn id_of<'a>(url: &'a Url, kinds: &[&str]) -> Option<&'a str> {
    if plain_host(url, &["www."]) != "spreaker.com" {
        return None;
    }
    match path_parts(url).as_slice() {
        [kind, part, ..] if kinds.contains(kind) => number(part),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    id_of(url, &["episode"]).is_some()
}

pub fn is_series(url: &Url) -> bool {
    id_of(url, &["podcast", "show"]).is_some()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = id_of(page.url, &["episode"]).ok_or_else(|| fail("not an episode page"))?;
    let reply = page.json(page.client.get(format!("https://api.spreaker.com/v2/episodes/{id}"))).await?;
    let episode = &reply["response"]["episode"];
    let url = episode["playback_url"].as_str().ok_or_else(|| fail("Spreaker has no file for this episode"))?;
    Ok(Found::new(url, episode["title"].as_str().map(str::to_string), episode["duration"].as_u64().map(|ms| ms / 1000)).ext("mp3"))
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let id = id_of(page.url, &["podcast", "show"]).ok_or_else(|| fail("not a show page"))?;
    let show = page.json(page.client.get(format!("https://api.spreaker.com/v2/shows/{id}"))).await?;
    let name = show["response"]["show"]["title"].as_str().unwrap_or("Spreaker show").to_string();
    let mut next = Some(format!("https://api.spreaker.com/v2/shows/{id}/episodes?limit=100"));
    let mut out = vec![];
    for _ in 0..MOST_PAGES {
        let Some(address) = next.take() else { break };
        let reply = page.json(page.client.get(address)).await?;
        for e in reply["response"]["items"].as_array().into_iter().flatten() {
            let (Some(link), Some(title)) = (e["site_url"].as_str(), e["title"].as_str()) else { continue };
            out.push(Episode::item(link, &name, title, None, e["duration"].as_u64().map(|ms| ms / 1000), "mp3"));
        }
        next = reply["response"]["next_url"].as_str().map(str::to_string);
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
    fn pages_are_recognised() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(matches(&url("https://www.spreaker.com/episode/ep-15-music-marketing--12534508")));
        assert!(matches(&url("https://www.spreaker.com/episode/12534508")));
        assert!(!matches(&url("https://www.spreaker.com/episode/a-title")));
        assert!(is_series(&url("https://www.spreaker.com/podcast/success-with-music--2317431")));
        assert!(is_series(&url("https://www.spreaker.com/show/2317431")));
        assert!(!is_series(&url("https://www.spreaker.com/explore")));
    }
}
