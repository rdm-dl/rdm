//! Pinterest video pins: the site's resource API lists the files.

use super::{Family, Found, Page, fail, path_parts};
use crate::download::DownloadError;
use serde_json::{Value, json};
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Pinterest", "pinterest.com", true)
}

/// The number from `/pin/123/` or `/pin/some-title--123/`, on any of its country domains.
fn pin_id(url: &Url) -> Option<&str> {
    if !url.host_str()?.split('.').any(|p| p == "pinterest") {
        return None;
    }
    let ["pin", id, ..] = path_parts(url)[..] else { return None };
    let id = id.rsplit("--").next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    pin_id(url).is_some()
}

/// The videos of a pin, wherever this kind of pin keeps them.
fn video_list(data: &Value) -> Option<&serde_json::Map<String, Value>> {
    data["videos"]["video_list"].as_object().or_else(|| {
        let pages = data["story_pin_data"]["pages"].as_array()?;
        pages.iter().flat_map(|p| p["blocks"].as_array().into_iter().flatten()).find_map(|b| b["video"]["video_list"].as_object())
    })
}

/// HLS when there is any, otherwise the widest file.
fn best(list: &serde_json::Map<String, Value>) -> Option<&Value> {
    let hls = list.values().find(|v| v["url"].as_str().is_some_and(|u| u.contains(".m3u8")));
    hls.or_else(|| list.values().filter(|v| v["url"].is_string()).max_by_key(|v| v["width"].as_u64().unwrap_or(0)))
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = pin_id(page.url).ok_or_else(|| fail("not a pin"))?;
    let options = json!({ "options": { "field_set_key": "unauth_react_main_pin", "id": id } }).to_string();
    let request = page.client.get("https://www.pinterest.com/resource/PinResource/get/").query(&[("data", options.as_str())]).header("X-Pinterest-PWS-Handler", "www/[username].js");
    let reply = page.json(request).await?;
    let data = &reply["resource_response"]["data"];
    let video = video_list(data).and_then(best).ok_or_else(|| fail("This pin has no video"))?;
    let url = video["url"].as_str().unwrap_or_default();
    let title = [&data["title"], &data["grid_title"]].iter().find_map(|t| t.as_str().map(str::trim).filter(|t| !t.is_empty())).map(str::to_string);
    Ok(Found::new(url, title.or(Some(format!("pin {id}"))), video["duration"].as_u64().map(|ms| ms / 1000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_files() {
        let id = |u: &str| pin_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://www.pinterest.com/pin/664281013778109217/").as_deref(), Some("664281013778109217"));
        assert_eq!(id("https://de.pinterest.de/pin/some-title--664281013778109217/").as_deref(), Some("664281013778109217"));
        assert_eq!(id("https://www.pinterest.com/someone/"), None);
        let data = json!({ "videos": { "video_list": { "V_720P": { "url": "https://x/a.mp4", "width": 720 }, "V_HLSV4": { "url": "https://x/a.m3u8" } } } });
        assert_eq!(video_list(&data).and_then(best).unwrap()["url"], "https://x/a.m3u8");
    }
}
