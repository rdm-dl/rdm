//! NBC News, TODAY and MSNBC: the page's data lists the streams.

use super::{Family, Found, Page, SiteInfo, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family { name: "NBC News", sites: vec![site("NBC News", "nbcnews.com", true), site("TODAY", "today.com", false), site("MSNBC", "msnbc.com", false)], browser_only: false }
}

pub fn matches(url: &Url) -> bool {
    let is_video = path_parts(url).iter().any(|p| matches!(*p, "video" | "watch"));
    let ends_in_id = path_parts(url).last().is_some_and(|l| l.rsplit('-').next().is_some_and(|n| n.len() >= 9 && n.bytes().all(|b| b.is_ascii_digit())));
    matches!(plain_host(url, &["www."]), "nbcnews.com" | "today.com" | "msnbc.com") && is_video && ends_in_id
}

/// The video in a page's data: `(video, duration field left to the caller)`.
fn page_video(html: &str) -> Option<Value> {
    let found = Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).ok()?.captures(html)?;
    let page: Value = serde_json::from_str(&found[1]).ok()?;
    let state = &page["props"]["initialState"];
    let current = &state["video"]["current"];
    let video = if current.is_object() { current } else { &state["article"]["content"][0]["primaryMedia"]["video"] };
    video.is_object().then(|| video.clone())
}

fn stream(video: &Value) -> Option<String> {
    let list = video["videoAssets"].as_array()?;
    let asset = list.iter().find(|a| a["format"] == "M3U" && a["publicUrl"].is_string()).or_else(|| list.iter().find(|a| a["publicUrl"].is_string()))?;
    let url = asset["publicUrl"].as_str()?;
    Some(if url.contains("://link.theplatform.com/") { format!("{url}{}format=redirect", if url.contains('?') { '&' } else { '?' }) } else { url.to_string() })
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let video = page_video(&html).ok_or_else(|| fail("This page's data can't be read"))?;
    let url = stream(&video).ok_or_else(|| fail("The site has no stream for this video"))?;
    Ok(Found::new(url, video["headline"]["primary"].as_str().map(str::to_string), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn video_pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("http://www.nbcnews.com/watch/nbcnews-com/how-twitter-reacted-269389891880"));
        assert!(is("https://www.today.com/video/some-title-212345678"));
        assert!(!is("https://www.nbcnews.com/news/a-story-rcna123"));
    }

    #[test]
    fn hls_is_preferred_and_redirects_are_asked_for() {
        let v = json!({ "videoAssets": [{ "format": "MPEG4", "publicUrl": "https://x/a.mp4" }, { "format": "M3U", "publicUrl": "https://link.theplatform.com/s/a" }] });
        assert_eq!(stream(&v).as_deref(), Some("https://link.theplatform.com/s/a?format=redirect"));
    }
}
