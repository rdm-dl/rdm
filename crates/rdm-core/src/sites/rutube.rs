//! Rutube: its API names the HLS stream of a video.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Rutube", "rutube.ru", true)
}

/// The 32 character id from `/video/id/` or `/play/embed/id`.
fn video_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "rutube.ru" {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["video", "private", id, ..] | ["live", "video", id, ..] | ["play", "embed", id, ..] | ["video" | "embed", id, ..] => *id,
        _ => return None,
    };
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

/// The address of a list's pages, with `{page}` for the number.
fn list_api(url: &Url) -> Option<String> {
    if plain_host(url, &["www."]) != "rutube.ru" {
        return None;
    }
    let digits = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    match path_parts(url).as_slice() {
        ["video", "person", id, ..] | ["channel", id, ..] if digits(id) => Some(format!("https://rutube.ru/api/video/person/{id}/?page={{page}}&format=json")),
        ["plst", id, ..] if digits(id) => Some(format!("https://rutube.ru/api/playlist/custom/{id}/videos?page={{page}}&format=json")),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    list_api(url).is_some()
}

/// The videos of a channel or playlist.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let template = list_api(page.url).ok_or_else(|| fail("not a channel or playlist page"))?;
    let name = format!("Rutube {}", path_parts(page.url).iter().find(|p| p.bytes().all(|b| b.is_ascii_digit())).unwrap_or(&"list"));
    let mut out = vec![];
    for number in 1..=50 {
        let reply = page.json(page.client.get(template.replace("{page}", &number.to_string()))).await?;
        for v in reply["results"].as_array().into_iter().flatten() {
            let (Some(id), Some(title)) = (v["id"].as_str(), v["title"].as_str()) else { continue };
            out.push(Episode::item(format!("https://rutube.ru/video/{id}/"), &name, title, None, v["duration"].as_u64(), "mp4"));
        }
        if reply["has_next"] != true {
            break;
        }
    }
    Ok(out)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let options = page.json(page.client.get(format!("https://rutube.ru/api/play/options/{id}/")).query(&[("format", "json")])).await?;
    let url = options["video_balancer"]["m3u8"].as_str().ok_or_else(|| fail("Rutube has no stream for this video"))?;
    let video = page.json(page.client.get(format!("https://rutube.ru/api/video/{id}/")).query(&[("format", "json")])).await.unwrap_or_default();
    Ok(Found::new(url, video["title"].as_str().map(str::to_string), video["duration"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_recognised() {
        let is = |u: &str| is_series(&Url::parse(u).unwrap());
        assert!(is("https://rutube.ru/video/person/313878/"));
        assert!(is("https://rutube.ru/channel/39722070/videos/"));
        assert!(is("https://rutube.ru/plst/308547/"));
        assert!(!is("https://rutube.ru/video/3eac3b4561676c17df9132a9a1e62e3e/"));
    }

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://rutube.ru/video/3eac3b4561676c17df9132a9a1e62e3e/").as_deref(), Some("3eac3b4561676c17df9132a9a1e62e3e"));
        assert_eq!(id("https://rutube.ru/play/embed/3eac3b4561676c17df9132a9a1e62e3e").as_deref(), Some("3eac3b4561676c17df9132a9a1e62e3e"));
        assert_eq!(id("https://rutube.ru/channel/123/"), None);
    }
}
