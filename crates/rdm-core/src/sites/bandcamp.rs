//! Bandcamp tracks: the page carries the file its player streams, usually MP3 at 128 kbit/s.
//! An album's page lists its tracks.

use super::{Episode, Family, Found, Page, fail, path_parts};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Bandcamp", "bandcamp.com", true)
}

pub fn matches(url: &Url) -> bool {
    url.host_str().is_some_and(|h| h.ends_with(".bandcamp.com")) && matches!(path_parts(url).as_slice(), ["track", _, ..])
}

pub fn is_series(url: &Url) -> bool {
    url.host_str().is_some_and(|h| h.ends_with(".bandcamp.com")) && matches!(path_parts(url).as_slice(), ["album", _, ..])
}

/// An album's tracks: only those with a length can be played.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let data = track_data(&html).ok_or_else(|| fail("This Bandcamp page's data can't be read"))?;
    let album = data["current"]["title"].as_str().unwrap_or("Bandcamp album");
    let name = data["artist"].as_str().map_or(album.to_string(), |a| format!("{a} - {album}"));
    let tracks = data["trackinfo"].as_array().ok_or_else(|| fail("This album lists no tracks"))?;
    let mut out = vec![];
    for t in tracks.iter().filter(|t| t["duration"].as_f64().is_some_and(|d| d > 0.0)) {
        let (Some(link), Some(title)) = (t["title_link"].as_str(), t["title"].as_str()) else { continue };
        let url = page.url.join(link).map_err(|e| fail(e.to_string()))?;
        out.push(Episode::item(url, &name, title, t["track_num"].as_u64(), t["duration"].as_f64().map(|d| d as u64), "mp3"));
    }
    Ok(out)
}

/// The JSON a page keeps in `data-tralbum="..."`, with its HTML escapes undone.
fn track_data(html: &str) -> Option<Value> {
    let found = Regex::new(r#"data-tralbum="([^"]+)""#).ok()?.captures(html)?;
    let text = found[1].replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    serde_json::from_str(&text).ok()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let data = track_data(&html).ok_or_else(|| fail("This Bandcamp page's data can't be read"))?;
    let track = &data["trackinfo"][0];
    let files = track["file"].as_object().ok_or_else(|| fail("Bandcamp doesn't stream this track"))?;
    let (_, url) = files.iter().find(|(k, v)| k.starts_with("mp3") && v.is_string()).ok_or_else(|| fail("Bandcamp doesn't stream this track"))?;
    let url = url.as_str().unwrap_or_default();
    let url = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    let title = track["title"].as_str().map(|t| data["artist"].as_str().map_or(t.to_string(), |a| format!("{a} - {t}")));
    Ok(Found::new(url, title, track["duration"].as_f64().map(|d| d as u64)).ext("mp3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_data_is_read() {
        let html = r#"<script data-tralbum="{&quot;artist&quot;:&quot;A &amp; B&quot;,&quot;trackinfo&quot;:[{&quot;title&quot;:&quot;Song&quot;}]}" data-x="1">"#;
        let data = track_data(html).unwrap();
        assert_eq!(data["artist"], "A & B");
        assert_eq!(data["trackinfo"][0]["title"], "Song");
    }

    #[test]
    fn track_pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://artist.bandcamp.com/track/a-song"));
        assert!(!is("https://artist.bandcamp.com/album/a-record"));
        assert!(is_series(&Url::parse("https://artist.bandcamp.com/album/a-record").unwrap()));
        assert!(!is("https://example.com/track/a-song"));
    }
}
