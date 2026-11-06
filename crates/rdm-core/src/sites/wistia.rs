//! Wistia, the video host behind many company sites: its embed data lists the files.

use super::{Family, Found, Page, SiteInfo, fail, path_parts};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family { name: "Wistia", sites: vec![site("Wistia", "wistia.com", false), site("Wistia player", "wistia.net", true)], browser_only: false }
}

/// The id from `/medias/id`, `/embed/medias/id` or `/embed/iframe/id`.
fn media_id(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    if !["wistia.com", "wistia.net"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))) {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["medias" | "iframe", id, ..] | ["embed", "medias" | "iframe", id, ..] => *id,
        _ => return None,
    };
    let id = id.strip_suffix(".json").unwrap_or(id);
    (id.len() >= 8 && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    media_id(url).is_some()
}

/// The largest ready MP4 file, or the upload itself. Their files end in `.bin` on the server.
fn best(assets: &[Value]) -> Option<String> {
    let mp4 = assets.iter().filter(|a| a["container"] == "mp4" && a["type"] != "original" && a["url"].is_string());
    let chosen = mp4.max_by_key(|a| a["width"].as_u64().unwrap_or(0)).or_else(|| assets.iter().find(|a| a["type"] == "original"))?;
    Some(chosen["url"].as_str()?.replace(".bin", ".mp4"))
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = media_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let address = format!("https://fast.wistia.net/embed/medias/{id}.json");
    let reply = page.json(page.client.get(&address).header("referer", address.as_str())).await?;
    let media = &reply["media"];
    let url = media["assets"].as_array().and_then(|a| best(a)).ok_or_else(|| fail("Wistia has no file for this video"))?;
    let title = media["name"].as_str().map(|n| n.strip_suffix(".mp4").unwrap_or(n).to_string());
    Ok(Found::new(url, title, media["duration"].as_f64().map(|d| d as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_and_files() {
        let id = |u: &str| media_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://fast.wistia.net/embed/iframe/a6ndpko1wg").as_deref(), Some("a6ndpko1wg"));
        assert_eq!(id("https://home.wistia.com/medias/a6ndpko1wg").as_deref(), Some("a6ndpko1wg"));
        assert_eq!(id("https://wistia.com/pricing"), None);
        let assets = [json!({ "type": "original", "url": "https://x/o.bin" }), json!({ "type": "md", "container": "mp4", "width": 640, "url": "https://x/m.bin" }), json!({ "type": "hd", "container": "mp4", "width": 1280, "url": "https://x/h.bin" })];
        assert_eq!(best(&assets).as_deref(), Some("https://x/h.mp4"));
    }
}
