//! Streamable: its player's API lists plain MP4 files.

use super::{Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Streamable", "streamable.com", true)
}

/// `dnd1` from `/dnd1`, `/e/dnd1` or `/s/dnd1`.
fn video_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "streamable.com" {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["e" | "s", id, ..] | [id] => *id,
        _ => return None,
    };
    (id.len() <= 12 && id.chars().all(|c| c.is_ascii_alphanumeric()) && !["login", "signup", "pricing", "about", "terms", "privacy", "blog"].contains(&id)).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let video = page.json(page.client.get(format!("https://ajax.streamable.com/videos/{id}"))).await?;
    let files = video["files"].as_object().ok_or_else(|| fail("Streamable has no file for this video yet"))?;
    let best = files.values().filter(|f| f["url"].is_string()).max_by_key(|f| f["height"].as_u64().unwrap_or(0));
    let url = best.and_then(|f| f["url"].as_str()).ok_or_else(|| fail("Streamable has no file for this video yet"))?;
    let url = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    let title = video["reddit_title"].as_str().or(video["title"].as_str()).map(str::to_string);
    Ok(Found::new(url, title, video["duration"].as_f64().or(best.and_then(|f| f["duration"].as_f64())).map(|d| d as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://streamable.com/dnd1").as_deref(), Some("dnd1"));
        assert_eq!(id("https://streamable.com/e/dnd1").as_deref(), Some("dnd1"));
        assert_eq!(id("https://streamable.com/pricing"), None);
        assert_eq!(id("https://example.com/dnd1"), None);
    }
}
