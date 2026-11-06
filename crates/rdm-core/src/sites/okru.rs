//! OK.ru: the player's settings are in the page, and they name the stream.

use super::{Family, Found, Page, attribute, fail, path_parts, plain_host, tag_around};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("OK.ru", "ok.ru", true)
}

fn video_page(url: &Url) -> Option<(&str, &str)> {
    if !matches!(plain_host(url, &["www.", "m.", "mobile."]), "ok.ru" | "odnoklassniki.ru") {
        return None;
    }
    match path_parts(url).as_slice() {
        [kind @ ("video" | "videoembed" | "live"), id, ..] if id.bytes().all(|b| b.is_ascii_digit() || b == b'-') => Some((if *kind == "videoembed" { "videoembed" } else { "video" }, *id)),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    video_page(url).is_some()
}

/// The largest whole file.
fn best_file(metadata: &Value) -> Option<&str> {
    const ORDER: [&str; 8] = ["mobile", "lowest", "low", "sd", "hd", "full", "quad", "ultra"];
    let rank = |f: &&Value| f["name"].as_str().and_then(|n| ORDER.iter().position(|o| *o == n));
    metadata["videos"].as_array()?.iter().filter(|f| f["url"].is_string()).max_by_key(rank)?["url"].as_str()
}

async fn metadata(page: &Page<'_>, flashvars: &Value) -> Result<Value> {
    match &flashvars["metadata"] {
        Value::String(text) => serde_json::from_str(text).map_err(|e| fail(format!("OK.ru's page can't be read: {e}"))),
        Value::Null => {
            let address = flashvars["metadataUrl"].as_str().ok_or_else(|| fail("This video isn't available."))?;
            let address = percent_encoding::percent_decode_str(address).decode_utf8_lossy().into_owned();
            let form = [("st.location", flashvars["location"].as_str().unwrap_or_default())];
            page.json(page.client.post(address).form(&form)).await
        }
        inline => Ok(inline.clone()),
    }
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let (mode, id) = video_page(page.url).ok_or_else(|| fail("not a video page"))?;
    let html = page.text(page.client.get(format!("https://ok.ru/{mode}/{id}"))).await?;
    if html.contains("vp_video_stub_txt") {
        return Err(fail("OK.ru says this video isn't available."));
    }
    let tag = tag_around(&html, "data-options=\"{").ok_or_else(|| fail("Access to this video is restricted; sign in to OK.ru and play it in your browser."))?;
    let options = attribute(tag, "data-options").ok_or_else(|| fail("OK.ru's page has changed"))?;
    let player: Value = serde_json::from_str(&options).map_err(|e| fail(format!("OK.ru's page can't be read: {e}")))?;
    let data = metadata(page, &player["flashvars"]).await?;
    if data["provider"] == "USER_YOUTUBE" {
        return Err(fail("This is a YouTube video shown on OK.ru; open it on YouTube."));
    }
    let stream = data["hlsManifestUrl"].as_str().or_else(|| data["ondemandHls"].as_str()).or_else(|| data["hlsMasterPlaylistUrl"].as_str()).or_else(|| best_file(&data));
    let Some(url) = stream else {
        return Err(fail(if data["paymentInfo"].is_null() { "OK.ru has no file for this video." } else { "This video is paid, so it can't be saved." }));
    };
    let movie = &data["movie"];
    let duration = movie["duration"].as_u64().or_else(|| movie["duration"].as_str().and_then(|d| d.parse().ok()));
    Ok(Found::new(url, movie["title"].as_str().map(str::to_string), duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_and_files() {
        let page = |u: &str| video_page(&Url::parse(u).unwrap()).map(|(m, i)| format!("{m} {i}"));
        assert_eq!(page("https://ok.ru/video/20079905452").as_deref(), Some("video 20079905452"));
        assert_eq!(page("https://m.ok.ru/video/63567059965189-0?fromTime=5").as_deref(), Some("video 63567059965189-0"));
        assert_eq!(page("https://ok.ru/videoembed/20648036891").as_deref(), Some("videoembed 20648036891"));
        assert_eq!(page("https://ok.ru/video/c12345"), None);
        let data = json!({ "videos": [{ "name": "low", "url": "l" }, { "name": "hd", "url": "h" }, { "name": "sd", "url": "s" }] });
        assert_eq!(best_file(&data), Some("h"));
    }
}
