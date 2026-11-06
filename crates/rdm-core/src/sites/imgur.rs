//! Imgur videos and animated posts. Albums and plain pictures aren't handled.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const CLIENT_ID: &str = "546c25a59c58ad7";

pub fn family() -> Family {
    Family::single("Imgur", "imgur.com", true)
}

/// `A61SaA1` from `/A61SaA1`, `/some-title-A61SaA1` or `i.imgur.com/A61SaA1.gifv`.
fn media_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "i.", "m."]) != "imgur.com" {
        return None;
    }
    let [last] = path_parts(url)[..] else { return None };
    let id = last.split('.').next()?.rsplit('-').next()?;
    (id.len() >= 5 && id.len() <= 8 && id.bytes().all(|b| b.is_ascii_alphanumeric()) && !["about", "upload", "signin", "search"].contains(&id)).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    media_id(url).is_some()
}

/// The album from `/a/id`, `/gallery/id` or `/a/some-title-id`.
fn album_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "m."]) != "imgur.com" {
        return None;
    }
    let ["a" | "gallery", id, ..] = path_parts(url)[..] else { return None };
    id.rsplit('-').next().filter(|i| i.bytes().all(|b| b.is_ascii_alphanumeric()))
}

pub fn is_series(url: &Url) -> bool {
    album_id(url).is_some()
}

/// The videos in an album; its pictures are left out.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let id = album_id(page.url).ok_or_else(|| fail("not an Imgur album"))?;
    let reply = page.json(page.client.get(format!("https://api.imgur.com/post/v1/albums/{id}")).query(&[("client_id", CLIENT_ID), ("include", "media,account")])).await?;
    let name = reply["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(id).to_string();
    let mut out = vec![];
    let videos = reply["media"].as_array().into_iter().flatten().filter(|m| m["type"] == "video" || m["metadata"]["is_animated"] == true);
    for (i, m) in videos.enumerate() {
        let Some(item) = m["id"].as_str() else { continue };
        let number = i as u64 + 1;
        let title = m["metadata"]["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(item);
        out.push(Episode::item(format!("https://imgur.com/{item}"), &name, title, Some(number), m["metadata"]["duration"].as_f64().map(|d| d as u64), "mp4"));
    }
    Ok(out)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = media_id(page.url).ok_or_else(|| fail("not an Imgur post"))?;
    let reply = page.json(page.client.get(format!("https://api.imgur.com/post/v1/media/{id}")).query(&[("client_id", CLIENT_ID), ("include", "media,account")])).await?;
    let media = &reply["media"][0];
    if media["type"] != "video" && media["metadata"]["is_animated"] != true {
        return Err(fail("This Imgur post has no video"));
    }
    let url = media["url"].as_str().ok_or_else(|| fail("Imgur has no file for this post"))?;
    // The same clip is there as a far smaller video.
    let gif = media["ext"] == "gif" || url.ends_with(".gif");
    let (url, ext) = if gif { (format!("https://i.imgur.com/{id}.mp4"), "mp4") } else { (url.to_string(), media["ext"].as_str().unwrap_or("mp4")) };
    let title = reply["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(id).to_string();
    let duration = media["metadata"]["duration"].as_f64().map(|d| d as u64);
    Ok(Found::new(url, Some(title), duration).ext(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn albums_are_recognised() {
        assert!(is_series(&Url::parse("https://imgur.com/a/abc1234").unwrap()));
        assert!(is_series(&Url::parse("https://imgur.com/gallery/some-title-abc1234").unwrap()));
        assert!(!is_series(&Url::parse("https://imgur.com/A61SaA1").unwrap()));
    }

    #[test]
    fn posts_are_recognised() {
        let id = |u: &str| media_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://imgur.com/A61SaA1").as_deref(), Some("A61SaA1"));
        assert_eq!(id("https://i.imgur.com/A61SaA1.gifv").as_deref(), Some("A61SaA1"));
        assert_eq!(id("https://imgur.com/some-title-A61SaA1").as_deref(), Some("A61SaA1"));
        assert_eq!(id("https://imgur.com/a/abc1234"), None);
        assert_eq!(id("https://imgur.com/gallery/abc1234"), None);
    }
}
