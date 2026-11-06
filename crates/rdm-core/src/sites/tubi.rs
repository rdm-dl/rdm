//! Tubi: free films and shows. Each page carries the addresses of its own streams.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Tubi", "tubitv.com", true)
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The path without the language some links start with (`/es-mx/movies/...`).
fn parts(url: &Url) -> Vec<&str> {
    if plain_host(url, &["www."]) != "tubitv.com" {
        return vec![];
    }
    let parts = path_parts(url);
    match parts.first() {
        Some(first) if first.len() == 5 && first.as_bytes()[2] == b'-' => parts[1..].to_vec(),
        _ => parts,
    }
}

fn video_page(url: &Url) -> Option<(&str, &str)> {
    match parts(url).as_slice() {
        [kind @ ("movies" | "tv-shows" | "video"), id, ..] if digits(id) => Some((*kind, *id)),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    video_page(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    matches!(parts(url).as_slice(), ["series", id, _, ..] if digits(id))
}

/// `window.<name>={...};`, which is JSON apart from the word `undefined`.
fn state(html: &str, name: &str) -> Result<Value> {
    let mark = format!("window.{name}=");
    let start = html.find(&mark).ok_or_else(|| fail("Tubi's page has changed"))? + mark.len();
    let end = html[start..].find(";</script>").ok_or_else(|| fail("Tubi's page has changed"))? + start;
    let words = Regex::new(r"([:\[,])undefined").map_err(|e| fail(e.to_string()))?;
    serde_json::from_str(&words.replace_all(&html[start..end], "${1}null")).map_err(|e| fail(format!("Tubi's page can't be read: {e}")))
}

/// The tallest stream that isn't protected.
fn stream(video: &Value) -> Option<&str> {
    let height = |r: &&Value| r["resolution"].as_str().and_then(|h| h.trim_end_matches(['P', 'p', 'K']).parse::<u64>().ok()).unwrap_or(0);
    let clear = video["video_resources"].as_array()?.iter().filter(|r| matches!(r["type"].as_str(), Some("hlsv3" | "hlsv6")) && r["manifest"]["url"].is_string());
    clear.max_by_key(height)?["manifest"]["url"].as_str()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let (kind, id) = video_page(page.url).ok_or_else(|| fail("not a video page"))?;
    let html = page.text(page.client.get(format!("https://tubitv.com/{kind}/{id}/"))).await?;
    let data = state(&html, "__data")?;
    let video = &data["video"]["byId"][id];
    if !video.is_object() {
        return Err(fail("Tubi has no such video here; it may have left the service or not be offered in your country."));
    }
    let Some(url) = stream(video) else {
        let protected = video["video_resources"].as_array().is_some_and(|r| !r.is_empty());
        return Err(fail(if protected { "This video is protected with DRM, so it can't be saved." } else { "Tubi isn't offering this video now." }));
    };
    let title = video["title"].as_str().map(|t| t.trim().replacen(":E", " E", 1));
    Ok(Found::new(url, title, video["duration"].as_u64()))
}

/// `the-saddle-club` to `The Saddle Club`.
fn show_name(slug: &str) -> String {
    let word = |w: &str| {
        let mut letters = w.chars();
        letters.next().map(|f| f.to_uppercase().chain(letters).collect::<String>()).unwrap_or_default()
    };
    slug.split('-').map(word).collect::<Vec<_>>().join(" ")
}

/// `S01:E02 - Work Horses` to `Work Horses`.
fn episode_title(full: &str) -> &str {
    full.split_once(" - ").map_or(full, |(_, title)| title).trim()
}

fn list(seasons: &Value, titles: &Value, show: &str, only: Option<u64>) -> Vec<Episode> {
    let mut out = vec![];
    for season in seasons.as_array().into_iter().flatten() {
        let number = season["number"].as_str().and_then(|n| n.parse().ok()).or_else(|| season["number"].as_u64());
        if only.is_some() && only != number {
            continue;
        }
        for episode in season["episodes"].as_array().into_iter().flatten() {
            let Some(id) = episode["id"].as_str() else { continue };
            let num = episode["num"].as_u64();
            let known = &titles[id];
            let title = known["title"].as_str().map_or_else(|| format!("Episode {}", num.unwrap_or(0)), |t| episode_title(t).to_string());
            out.push(Episode { url: format!("https://tubitv.com/tv-shows/{id}/"), show: show.to_string(), title, season: number, number: num, duration: known["duration"].as_u64(), ext: "mp4" });
        }
    }
    out
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let path = parts(page.url);
    let (id, slug) = match path.as_slice() {
        ["series", id, slug, ..] => (*id, *slug),
        _ => return Err(fail("not a show page")),
    };
    let only = path.get(3).and_then(|p| p.strip_prefix("season-")).and_then(|n| n.parse().ok());
    let html = page.text(page.client.get(format!("https://tubitv.com/series/{id}/{slug}"))).await?;
    let queries = state(&html, "__REACT_QUERY_STATE__")?;
    let seasons = queries["queries"].as_array().into_iter().flatten().map(|q| &q["state"]["data"]["seasons"]).find(|s| s[0].is_object());
    let seasons = seasons.ok_or_else(|| fail("This show has no episodes to get."))?;
    let titles = state(&html, "__data").unwrap_or_default();
    let out = list(seasons, &titles["video"]["byId"], &show_name(slug), only);
    if out.is_empty() {
        return Err(fail("This show has no episodes to get."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_and_shows() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(matches(&url("https://tubitv.com/movies/100004539/the-39-steps")));
        assert!(matches(&url("https://tubitv.com/es-mx/tv-shows/477363/s01-e03-x")));
        assert!(!matches(&url("https://tubitv.com/series/3936/bob-ross")));
        assert!(is_series(&url("https://tubitv.com/series/300000435/the-saddle-club/season-2")));
        assert!(!is_series(&url("https://tubitv.com/movies/100004539/the-39-steps")));
    }

    #[test]
    fn page_data_with_the_word_undefined_can_be_read() {
        let html = r#"<script>window.__data={"a":undefined,"b":[undefined,1]};</script>"#;
        assert_eq!(state(html, "__data").unwrap(), json!({ "a": null, "b": [null, 1] }));
    }

    #[test]
    fn protected_streams_are_passed_over() {
        let video = json!({ "video_resources": [
            { "type": "hlsv6_widevine", "resolution": "1080P", "manifest": { "url": "locked" } },
            { "type": "hlsv6", "resolution": "480P", "manifest": { "url": "low" } },
            { "type": "hlsv6", "resolution": "720P", "manifest": { "url": "high" } },
        ] });
        assert_eq!(stream(&video), Some("high"));
    }

    #[test]
    fn a_season_can_be_picked() {
        let seasons = json!([
            { "number": "1", "episodes": [{ "id": "11", "num": 1 }] },
            { "number": "2", "episodes": [{ "id": "21", "num": 1 }, { "id": "22", "num": 2 }] },
        ]);
        let titles = json!({ "21": { "title": "S02:E01 - Work Horses", "duration": 1534 } });
        let all = list(&seasons, &titles, "Show", None);
        assert_eq!(all.len(), 3);
        let second = list(&seasons, &titles, "Show", Some(2));
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].title, "Work Horses");
        assert_eq!(second[1].title, "Episode 2");
        assert_eq!(second[0].url, "https://tubitv.com/tv-shows/21/");
        assert_eq!(show_name("the-saddle-club"), "The Saddle Club");
    }
}
