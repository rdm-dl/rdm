//! Odysee and LBRY: the claim in the address resolves to a stream through their public API.

use super::{Episode, Family, Found, Page, SiteInfo, fail, path_parts, plain_host};
use crate::download::DownloadError;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const API: &str = "https://api.lbry.tv/api/v1/proxy";

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family { name: "Odysee", sites: vec![site("Odysee", "odysee.com", true), site("LBRY", "lbry.tv", false)], browser_only: false }
}

/// `lbry://@channel#1/video#2` from `/@channel:1/video:2`, or `lbry://video#2` from `/video:2`.
fn claim(url: &Url) -> Option<String> {
    if !matches!(plain_host(url, &["www."]), "odysee.com" | "lbry.tv") {
        return None;
    }
    let path = match path_parts(url).as_slice() {
        [channel, video] if channel.starts_with('@') => format!("{channel}/{video}"),
        [video] if !video.starts_with(['@', '$']) => (*video).to_string(),
        _ => return None,
    };
    Some(format!("lbry://{}", percent_decode_str(&path).decode_utf8_lossy().replace(':', "#")))
}

pub fn matches(url: &Url) -> bool {
    claim(url).is_some()
}

async fn call(page: &Page<'_>, method: &str, params: Value) -> Result<Value> {
    let mut request = page.client.post(API).header("content-type", "application/json-rpc").body(json!({ "method": method, "params": params }).to_string());
    if let Some(token) = page.cookie("auth_token") {
        request = request.header("x-lbry-auth-token", token);
    }
    let reply = page.json(request).await?;
    if let Some(e) = reply["error"]["message"].as_str() {
        return Err(fail(format!("Odysee says: {e}")));
    }
    Ok(reply["result"].clone())
}

/// `lbry://@channel#1` from the page of a channel.
fn channel(url: &Url) -> Option<String> {
    if !matches!(plain_host(url, &["www."]), "odysee.com" | "lbry.tv") {
        return None;
    }
    match path_parts(url).as_slice() {
        [name] if name.starts_with('@') => Some(format!("lbry://{}", percent_decode_str(name).decode_utf8_lossy().replace(':', "#"))),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    channel(url).is_some()
}

/// A channel's videos, newest first.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let uri = channel(page.url).ok_or_else(|| fail("not a channel page"))?;
    let resolved = call(page, "resolve", json!({ "urls": uri })).await?;
    let item = &resolved[uri.as_str()];
    let id = item["claim_id"].as_str().ok_or_else(|| fail("This Odysee channel wasn't found"))?;
    let name = item["value"]["title"].as_str().or(item["name"].as_str()).unwrap_or("Odysee channel").to_string();
    let mut out = vec![];
    for number in 1..=20u64 {
        let params = json!({ "channel_ids": [id], "claim_type": "stream", "stream_types": ["video", "audio"], "order_by": ["release_time"], "fee_amount": ">=0", "page": number, "page_size": 50 });
        let reply = call(page, "claim_search", params).await?;
        for v in reply["items"].as_array().into_iter().flatten() {
            let (Some(slug), Some(claim)) = (v["name"].as_str(), v["claim_id"].as_str()) else { continue };
            let value = &v["value"];
            let audio = value["stream_type"] == "audio";
            let length = value["video"]["duration"].as_u64().or(value["audio"]["duration"].as_u64());
            out.push(Episode::item(format!("https://odysee.com/{slug}:{claim}"), &name, value["title"].as_str().unwrap_or(slug), None, length, if audio { "mp3" } else { "mp4" }));
        }
        if number >= reply["total_pages"].as_u64().unwrap_or(1) {
            break;
        }
    }
    Ok(out)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let uri = claim(page.url).ok_or_else(|| fail("not a video page"))?;
    let resolved = call(page, "resolve", json!({ "urls": uri })).await?;
    let item = &resolved[uri.as_str()];
    if let Some(e) = item["error"]["text"].as_str() {
        return Err(fail(format!("Odysee says: {e}")));
    }
    let value = &item["value"];
    if !matches!(value["stream_type"].as_str(), Some("video" | "audio")) {
        return Err(fail("This Odysee page isn't a video"));
    }
    let got = call(page, "get", json!({ "uri": uri })).await?;
    let stream = got["streaming_url"].as_str().ok_or_else(|| fail("Odysee gave no stream for this video"))?;
    // The third version of the address gives the uploaded file itself, when there is one.
    let url = regex::Regex::new(r"/api/v\d+/").map_err(|e| fail(e.to_string()))?.replace(stream, "/api/v3/").into_owned();
    let ext = value["source"]["name"].as_str().and_then(|n| n.rsplit_once('.')).map_or("mp4", |(_, e)| e);
    let duration = value["video"]["duration"].as_u64().or(value["audio"]["duration"].as_u64());
    let mut found = Found::new(url, value["title"].as_str().map(str::to_string), duration).ext(ext);
    found.referer = Some("https://odysee.com/".into());
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_recognised() {
        assert_eq!(channel(&Url::parse("https://odysee.com/@Mantega:1").unwrap()).as_deref(), Some("lbry://@Mantega#1"));
        assert_eq!(channel(&Url::parse("https://odysee.com/@Mantega:1/First-day:1").unwrap()), None);
        assert_eq!(channel(&Url::parse("https://odysee.com/$/settings").unwrap()), None);
    }

    #[test]
    fn claims_are_read_from_addresses() {
        let c = |u: &str| claim(&Url::parse(u).unwrap());
        assert_eq!(c("https://odysee.com/@Mantega:1/First-day-LBRY:1").as_deref(), Some("lbry://@Mantega#1/First-day-LBRY#1"));
        assert_eq!(c("https://lbry.tv/Episode-1:e7").as_deref(), Some("lbry://Episode-1#e7"));
        assert_eq!(c("https://odysee.com/@Mantega:1"), None);
        assert_eq!(c("https://odysee.com/$/settings"), None);
    }
}
