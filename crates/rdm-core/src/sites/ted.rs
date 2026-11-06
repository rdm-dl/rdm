//! TED talks: the page's data names the HLS stream.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("TED", "ted.com", true)
}

pub fn matches(url: &Url) -> bool {
    plain_host(url, &["www.", "embed."]) == "ted.com" && matches!(path_parts(url).as_slice(), ["talks", _, ..])
}

pub fn is_series(url: &Url) -> bool {
    plain_host(url, &["www."]) == "ted.com" && matches!(path_parts(url).as_slice(), ["playlists", _, ..])
}

/// The talks of a playlist.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let found = Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).map_err(|e| fail(e.to_string()))?.captures(&html).ok_or_else(|| fail("This TED page's data can't be read"))?;
    let data: Value = serde_json::from_str(&found[1]).map_err(|e| fail(format!("This TED page's data can't be read: {e}")))?;
    let list = &data["props"]["pageProps"]["playlist"];
    let name = list["title"].as_str().unwrap_or("TED playlist");
    let mut out = vec![];
    for (i, t) in list["videos"]["nodes"].as_array().into_iter().flatten().enumerate() {
        let (Some(url), Some(title)) = (t["canonicalUrl"].as_str(), t["title"].as_str()) else { continue };
        out.push(Episode::item(url, name, title, Some(i as u64 + 1), t["duration"].as_u64(), "mp4"));
    }
    Ok(out)
}

/// The talk's data in a page.
fn talk(html: &str) -> Option<Value> {
    let found = Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).ok()?.captures(html)?;
    let page: Value = serde_json::from_str(&found[1]).ok()?;
    Some(page["props"]["pageProps"]["videoData"].clone()).filter(Value::is_object)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let video = talk(&html).ok_or_else(|| fail("This TED page's data can't be read"))?;
    let url = video["videoPlayerData"]["resources"]["hls"]["stream"].as_str().ok_or_else(|| fail("TED has no stream for this talk"))?;
    Ok(Found::new(url, video["title"].as_str().map(str::to_string), video["duration"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_data_is_read() {
        let html = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"videoData":{"title":"Talk","duration":7,"videoPlayerData":{"resources":{"hls":{"stream":"https://x/m.m3u8"}}}}}}}</script>"#;
        let video = talk(html).unwrap();
        assert_eq!(video["title"], "Talk");
        assert_eq!(video["videoPlayerData"]["resources"]["hls"]["stream"], "https://x/m.m3u8");
    }

    #[test]
    fn talk_pages_are_recognised() {
        assert!(matches(&Url::parse("https://www.ted.com/talks/some_talk").unwrap()));
        assert!(!matches(&Url::parse("https://www.ted.com/about").unwrap()));
    }
}
