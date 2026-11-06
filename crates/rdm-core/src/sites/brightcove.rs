//! Brightcove, the video platform behind several TV sites. Not a site itself: a family that
//! finds a Brightcove video on its page calls this.

use super::{Found, Page, fail};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;

type Result<T> = std::result::Result<T, DownloadError>;

/// The player's key to the video API, from its settings or else its script.
async fn policy_key(page: &Page<'_>, account: &str, player: &str) -> Result<String> {
    let base = format!("https://players.brightcove.net/{account}/{player}_default/");
    if let Ok(config) = page.json(page.client.get(format!("{base}config.json"))).await {
        if let Some(key) = config["video_cloud"]["policy_key"].as_str() {
            return Ok(key.to_string());
        }
    }
    let script = page.text(page.client.get(format!("{base}index.min.js"))).await?;
    let found = Regex::new(r#"policyKey\s*:\s*["']([^"']+)["']"#).map_err(|e| fail(e.to_string()))?.captures(&script);
    found.map(|c| c[1].to_string()).ok_or_else(|| fail("The player's key wasn't found"))
}

/// An HLS source without DRM, or else an MP4 file.
fn source(video: &Value) -> Option<&str> {
    let list = video["sources"].as_array()?;
    let plain = |s: &&Value| s["key_systems"].is_null() && s["src"].as_str().is_some_and(|u| u.starts_with("https"));
    let hls = list.iter().filter(plain).find(|s| s["type"] == "application/x-mpegURL");
    hls.or_else(|| list.iter().filter(plain).find(|s| s["container"] == "MP4"))?["src"].as_str()
}

pub async fn find(page: &Page<'_>, account: &str, player: &str, video: &str) -> Result<Found> {
    let key = policy_key(page, account, player).await?;
    let address = format!("https://edge.api.brightcove.com/playback/v1/accounts/{account}/videos/{video}");
    let info = page.json(page.client.get(address).header("accept", format!("application/json;pk={key}"))).await?;
    let url = source(&info).ok_or_else(|| fail("This video is protected with DRM, so it can't be saved."))?;
    Ok(Found::new(url, info["name"].as_str().map(str::to_string), info["duration"].as_u64().map(|ms| ms / 1000)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sources_with_drm_are_left_out() {
        let drm = json!({ "type": "application/x-mpegURL", "src": "https://x/d.m3u8", "key_systems": { "com.widevine.alpha": {} } });
        let plain = json!({ "type": "application/x-mpegURL", "src": "https://x/a.m3u8" });
        assert_eq!(source(&json!({ "sources": [drm, plain] })), Some("https://x/a.m3u8"));
        assert_eq!(source(&json!({ "sources": [drm] })), None);
    }
}
