//! Medal game clips: the site's API names the stream.

use super::{Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// What a clip has instead of a video when it's only for signed-in viewers.
const GUEST_ONLY: &str = "video/privacy-protected-guest";

pub fn family() -> Family {
    Family::single("Medal", "medal.tv", true)
}

fn clip_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "medal.tv" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["games", _, "clips", id, ..] => Some(id),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    clip_id(url).is_some()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = clip_id(page.url).ok_or_else(|| fail("not a clip page"))?;
    let clip = page.json(page.client.get(format!("https://medal.tv/api/content/{id}")).header("accept", "application/json")).await?;
    let usable = |key: &str| clip[key].as_str().filter(|u| u.starts_with("http") && !u.contains(GUEST_ONLY));
    let url = usable("contentUrlHls").or(usable("contentUrl")).map(str::to_string).unwrap_or_else(|| format!("https://medal.tv/api/content/{id}/socialVideoUrl"));
    Ok(Found::new(url, clip["contentTitle"].as_str().map(str::to_string), clip["videoLengthSeconds"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_pages_are_recognised() {
        assert!(matches(&Url::parse("https://medal.tv/games/valorant/clips/jTBFnLKdLy15K").unwrap()));
        assert!(!matches(&Url::parse("https://medal.tv/games/valorant").unwrap()));
    }
}
