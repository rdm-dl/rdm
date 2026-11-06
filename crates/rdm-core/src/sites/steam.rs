//! Steam store: a game's page lists its trailers. A game has several, so its page is a list
//! and each trailer has its own address: the page's with `#trailer-N` added.

use super::{Episode, Family, Found, Page, attribute, fail, path_parts, plain_host, tag_around};
use crate::download::DownloadError;
use reqwest::header;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const AGE: &str = "Steam wants your age confirmed for this game. Open its page in your browser, answer, and download from there.";

pub fn family() -> Family {
    Family::single("Steam", "store.steampowered.com", true)
}

fn app_of(url: &Url) -> Option<&str> {
    if plain_host(url, &[]) != "store.steampowered.com" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["app", id, ..] | ["agecheck", "app", id, ..] if id.bytes().all(|b| b.is_ascii_digit()) => Some(*id),
        _ => None,
    }
}

fn trailer_of(url: &Url) -> Option<u64> {
    url.fragment()?.strip_prefix("trailer-")?.parse().ok()
}

pub fn matches(url: &Url) -> bool {
    app_of(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    app_of(url).is_some() && trailer_of(url).is_none()
}

/// The player's data on a game's page. The age question is answered in the browser, so its
/// answer only comes along with the cookies of a page opened there.
async fn trailers(page: &Page<'_>, id: &str) -> Result<Value> {
    let mut request = page.client.get(format!("https://store.steampowered.com/app/{id}/"));
    if let Some(cookies) = page.cookies {
        request = request.header(header::COOKIE, cookies);
    }
    let html = page.text(request).await?;
    let tag = tag_around(&html, "gamehighlight_desktopcarousel").ok_or_else(|| fail(AGE))?;
    let props = attribute(tag, "data-props").ok_or_else(|| fail(AGE))?;
    serde_json::from_str(&props).map_err(|e| fail(format!("Steam's page can't be read: {e}")))
}

/// The H.264 stream, which plays everywhere.
fn stream(trailer: &Value) -> Option<&str> {
    let dash = || trailer["dashManifests"].as_array()?.iter().filter_map(Value::as_str).find(|d| d.contains("h264") || d.contains("264"));
    trailer["hlsManifest"].as_str().or_else(dash)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = app_of(page.url).ok_or_else(|| fail("not a game page"))?;
    let data = trailers(page, id).await?;
    let list = data["trailers"].as_array().filter(|l| !l.is_empty()).ok_or_else(|| fail("This game has no trailers."))?;
    let number = trailer_of(page.url).unwrap_or(1) as usize;
    let trailer = list.get(number.saturating_sub(1)).ok_or_else(|| fail("This game has no such trailer."))?;
    let url = stream(trailer).ok_or_else(|| fail("Steam has no stream for this trailer."))?;
    let name = data["appName"].as_str().unwrap_or("Steam");
    Ok(Found::new(url, Some(format!("{name} - Trailer {number}")), None))
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let id = app_of(page.url).ok_or_else(|| fail("not a game page"))?;
    let data = trailers(page, id).await?;
    let name = data["appName"].as_str().unwrap_or("Steam");
    let list = data["trailers"].as_array().map_or(0, Vec::len) as u64;
    if list == 0 {
        return Err(fail("This game has no trailers."));
    }
    Ok((1..=list).map(|n| Episode::item(format!("https://store.steampowered.com/app/{id}/#trailer-{n}"), name, &format!("Trailer {n}"), Some(n), None, "mp4")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_game_is_a_list_and_a_trailer_is_not() {
        let game = Url::parse("https://store.steampowered.com/app/105600/Terraria/").unwrap();
        assert_eq!(app_of(&game), Some("105600"));
        assert!(matches(&game) && is_series(&game));
        let trailer = Url::parse("https://store.steampowered.com/app/105600/#trailer-3").unwrap();
        assert_eq!(trailer_of(&trailer), Some(3));
        assert!(matches(&trailer) && !is_series(&trailer));
        assert!(!matches(&Url::parse("https://store.steampowered.com/search/").unwrap()));
    }

    #[test]
    fn hls_comes_first() {
        assert_eq!(stream(&json!({ "hlsManifest": "a.m3u8", "dashManifests": ["b.mpd"] })), Some("a.m3u8"));
        assert_eq!(stream(&json!({ "dashManifests": ["dash_av1.mpd", "dash_h264.mpd"] })), Some("dash_h264.mpd"));
    }
}
