//! NBCUniversal's cable sites: Oxygen, Bravo and Syfy. The page names the video, and the video
//! service answers with a stream. Episodes behind a TV provider sign-in aren't handled.

use super::{Family, Found, Page, SiteInfo, attribute, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const DRM: &str = "This video is protected with DRM, so it can't be saved.";

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family { name: "NBCUniversal cable", sites: vec![site("Oxygen", "oxygen.com", false), site("Bravo", "bravotv.com", true), site("Syfy", "syfy.com", true)], browser_only: false }
}

pub fn matches(url: &Url) -> bool {
    let parts = path_parts(url);
    matches!(plain_host(url, &["www."]), "oxygen.com" | "bravotv.com" | "syfy.com") && parts.len() >= 2 && parts.iter().any(|p| *p == "videos" || p.starts_with("season-"))
}

/// The video's address in the service and its details, from the player on an episode page or
/// the playlist on a clip page. Fails for what needs a TV provider.
fn locate(html: &str) -> Result<(String, Value)> {
    let tag = Regex::new(r#"<[a-z]+[^>]*tve-video-deck-app[^>]*>"#).ok().and_then(|r| r.find(html).map(|m| m.as_str().to_string()));
    if let Some(tag) = tag {
        if attribute(&tag, "data-entitlement").as_deref() == Some("auth") {
            return Err(fail("This episode needs a TV provider sign-in, which RDM can't use yet."));
        }
        let pid = attribute(&tag, "data-mpx-media-account-pid").or_else(|| attribute(&tag, "data-mpx-account-pid"));
        let account = attribute(&tag, "data-mpx-media-account-id");
        let details: Value = attribute(&tag, "data-normalized-video").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let guid = attribute(&tag, "data-guid").or_else(|| details["guid"].as_str().map(str::to_string));
        if let (Some(pid), Some(account), Some(guid)) = (pid, account, guid) {
            return Ok((format!("{pid}/media/guid/{account}/{guid}"), details));
        }
    }
    let found = Regex::new(r#"(?s)data-drupal-selector="drupal-settings-json"[^>]*>(.*?)</script>"#).ok().and_then(|r| r.captures(html)).ok_or_else(|| fail("This page's data can't be read"))?;
    let settings: Value = serde_json::from_str(&found[1]).map_err(|e| fail(format!("This page's data can't be read: {e}")))?;
    let list = settings["ls_playlist"].as_object().map(|m| m.values().collect::<Vec<_>>()).or_else(|| settings["ls_playlist"].as_array().map(|a| a.iter().collect()));
    let playlist = list.into_iter().flatten().find(|p| p["defaultGuid"].is_string()).ok_or_else(|| fail("This page has no video"))?;
    let guid = playlist["defaultGuid"].as_str().unwrap_or_default();
    let pid = playlist["mpxMediaAccountPid"].as_str().or(playlist["mpxAccountPid"].as_str()).ok_or_else(|| fail("This page's data can't be read"))?;
    let account = playlist["mpxMediaAccountId"].as_str().map(str::to_string).or(playlist["mpxMediaAccountId"].as_u64().map(|n| n.to_string())).ok_or_else(|| fail("This page's data can't be read"))?;
    let details = playlist["videos"].as_array().and_then(|v| v.iter().find(|x| x["guid"] == guid)).cloned().unwrap_or_default();
    Ok((format!("{pid}/media/guid/{account}/{guid}"), details))
}

/// Ask the video service for an HLS address in the given formats.
async fn stream(page: &Page<'_>, path: &str, formats: &str) -> Result<String> {
    let query = [("format", "SMIL"), ("manifest", "m3u"), ("switch", "HLSServiceSecure"), ("formats", formats)];
    let smil = page.text(page.client.get(format!("https://link.theplatform.com/s/{path}")).query(&query)).await?;
    if smil.contains("GeoLocationBlocked") {
        return Err(fail("This video is only available in the USA."));
    }
    let found = Regex::new(r#"src="([^"]+\.m3u8[^"]*)""#).map_err(|e| fail(e.to_string()))?.captures(&smil).ok_or_else(|| fail("The video service gave no stream"))?;
    Ok(found[1].replace("&amp;", "&"))
}

/// The folders in a DRM address, which also hold the ordinary stream.
fn drm_folders(url: &str) -> Option<String> {
    let found = Regex::new(r"^https?://[^/?#]+/prod/[\w-]+/(.+/)cmaf/mpeg_(?:cbcs|cenc)\w*/master_cmaf\w*\.m3u").ok()?.captures(url)?;
    Some(found[1].to_string())
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let (path, details) = locate(&html)?;
    let mut url = stream(page, &path, "m3u+none,mpeg4").await?;
    if let Some(folders) = drm_folders(&url) {
        url = stream(page, &path, "mpeg4").await?.replace("{folders}", &folders);
    }
    if url.contains("/mpeg_cenc") || url.contains("/mpeg_cbcs") {
        return Err(fail(DRM));
    }
    let title = details["title"].as_str().map(|t| match (details["show"].as_str(), details["seasonNumber"].as_u64(), details["episodeNumber"].as_u64()) {
        (Some(show), Some(s), Some(e)) => format!("{show} - S{s} E{e} {t}"),
        (Some(show), ..) => format!("{show} - {t}"),
        _ => t.to_string(),
    });
    Ok(Found::new(url, title, details["durationInSeconds"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://www.oxygen.com/in-ice-cold-blood/season-1/closing-night"));
        assert!(is("https://www.bravotv.com/top-chef/season-16/episode-15/videos/the-winner"));
        assert!(is("https://www.syfy.com/face-off/videos/keyed-up"));
        assert!(!is("https://www.oxygen.com/about"));
    }

    #[test]
    fn provider_only_episodes_say_so_and_others_are_located() {
        let locked = r#"<div class="tve-video-deck-app" data-entitlement="auth" data-guid="g"></div>"#;
        assert!(locate(locked).err().unwrap().to_string().contains("TV provider"));
        let free = r#"<div class="tve-video-deck-app" data-entitlement="free" data-mpx-account-pid="pid" data-mpx-media-account-id="12" data-normalized-video="{&quot;guid&quot;:&quot;g1&quot;,&quot;title&quot;:&quot;Clip&quot;}"></div>"#;
        let (path, details) = locate(free).unwrap();
        assert_eq!(path, "pid/media/guid/12/g1");
        assert_eq!(details["title"], "Clip");
    }

    #[test]
    fn the_ordinary_stream_sits_in_the_same_folders() {
        let url = "https://x.akamaized.net/prod/video/a/b/cmaf/mpeg_cbcs/master_cmaf.m3u8?x=1";
        assert_eq!(drm_folders(url).as_deref(), Some("a/b/"));
        assert_eq!(drm_folders("https://x/prod/video/a/master_hls.m3u8"), None);
    }
}
