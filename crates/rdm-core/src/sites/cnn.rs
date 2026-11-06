//! CNN: each page has a player tag naming its video, and a public service lists the files.

use super::{Family, Found, Page, SiteInfo, attribute, clock, fail, path_parts, plain_host, tag_around};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    let site = |domain| SiteInfo { name: "CNN", domain, tested: true };
    Family { name: "CNN", sites: vec![site("cnn.com"), site("edition.cnn.com"), site("cnnespanol.cnn.com")], browser_only: false }
}

pub fn matches(url: &Url) -> bool {
    let parts = path_parts(url);
    matches!(plain_host(url, &["www.", "us."]), "cnn.com" | "edition.cnn.com" | "cnnespanol.cnn.com") && parts.len() >= 2 && parts[0] != "audio"
}

/// The widest of the files, which are named `...-1280x720_3500k.mp4`.
fn tallest(files: &Value) -> Option<&str> {
    let size = Regex::new(r"-(\d+)x\d+_(\d+)k\.mp4").ok()?;
    let width = |f: &&str| size.captures(f).map_or((0, 0), |c| (c[1].parse::<u64>().unwrap_or(0), c[2].parse::<u64>().unwrap_or(0)));
    files.as_array()?.iter().filter_map(|f| f["fileUri"].as_str()).max_by_key(width)
}

/// The HLS stream the player uses, for videos with no files listed.
async fn stream(page: &Page<'_>, html: &str, media: &str) -> Option<String> {
    let found = Regex::new(r#"TOP_AUTH_SERVICE_APP_ID["\s:]+([^",]+)"#).ok()?.captures(html)?;
    let address = format!("https://medium.ngtv.io/v2/media/{media}/desktop");
    let reply = page.json(page.client.get(address).query(&[("appId", &found[1])])).await.ok()?;
    reply["media"]["desktop"]["unprotected"]["unencrypted"]["url"].as_str().map(str::to_string)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let tag = tag_around(&html, r#"data-component-name="video-player""#).ok_or_else(|| fail("This CNN page has no video."))?;
    let media = attribute(tag, "data-media-id").ok_or_else(|| fail("This CNN page has no video."))?;
    if attribute(tag, "data-live").is_some_and(|l| l == "true") {
        return Err(fail("This video is live; it can be saved once it has ended."));
    }
    let mut info = Value::Null;
    if let Some(parent) = attribute(tag, "data-video-resource-parent-uri") {
        let query = [("id", media.as_str()), ("stellarUri", parent.as_str())];
        info = page.json(page.client.get("https://fave.api.cnn.io/v1/video").query(&query)).await.unwrap_or_default();
    }
    let url = match tallest(&info["files"]) {
        Some(file) => file.to_string(),
        None => stream(page, &html, &media).await.ok_or_else(|| fail("CNN has no file for this video"))?,
    };
    let title = info["headline"].as_str().map(str::to_string).or_else(|| attribute(tag, "data-headline"));
    let duration = info["trt"].as_u64().or_else(|| attribute(tag, "data-duration").and_then(|d| clock(&d)));
    Ok(Found::new(url, title, duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_and_files() {
        assert!(matches(&Url::parse("https://www.cnn.com/2024/05/31/sport/video/a-title").unwrap()));
        assert!(matches(&Url::parse("https://cnnespanol.cnn.com/video/a-title").unwrap()));
        assert!(!matches(&Url::parse("https://www.cnn.com/").unwrap()));
        assert!(!matches(&Url::parse("https://www.cnn.com/audio/podcasts/x").unwrap()));
        let files = json!([{ "fileUri": "a-640x360_800k.mp4" }, { "fileUri": "a-1920x1080_5000k.mp4" }, { "fileUri": "a-1280x720_3500k.mp4" }]);
        assert_eq!(tallest(&files), Some("a-1920x1080_5000k.mp4"));
    }
}
