//! Loom shared recordings: the session API gives a signed stream address. Videos with a
//! password aren't handled.

use super::{Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::{Value, json};
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Loom", "loom.com", true)
}

fn video_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "loom.com" {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["share" | "embed", id, ..] => *id,
        _ => return None,
    };
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

async fn session_url(page: &Page<'_>, id: &str, endpoint: &str) -> Option<String> {
    let visitor = format!("{:032x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos());
    let body = json!({ "anonID": visitor, "deviceID": null, "force_original": false, "password": null });
    let request = page.client.post(format!("https://www.loom.com/api/campaigns/sessions/{id}/{endpoint}")).header("accept", "application/json").header("content-type", "application/json").body(body.to_string());
    let reply = page.json(request).await.ok()?;
    reply["url"].as_str().filter(|u| !u.is_empty()).map(str::to_string)
}

async fn details(page: &Page<'_>, id: &str) -> Option<Value> {
    let query = "query GetVideoSSR($videoId: ID!, $password: String) { getVideo(id: $videoId, password: $password) { __typename ... on RegularUserVideo { name video_properties { duration } } } }";
    let body = json!({ "operationName": "GetVideoSSR", "variables": { "videoId": id, "password": null }, "query": query });
    let request = page.client.post("https://www.loom.com/graphql").header("content-type", "application/json").header("x-loom-request-source", "loom_web_45a5bd4").header("apollographql-client-name", "web").header("apollographql-client-version", "45a5bd4").body(body.to_string());
    let reply = page.json(request).await.ok()?;
    Some(reply["data"]["getVideo"].clone())
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let info = details(page, id).await.unwrap_or_default();
    if info["__typename"] == "VideoPasswordMissingOrIncorrect" {
        return Err(fail("This Loom video has a password, which RDM can't send yet"));
    }
    let url = match session_url(page, id, "raw-url").await {
        Some(u) => u,
        None => session_url(page, id, "transcoded-url").await.ok_or_else(|| fail("Loom gave no stream for this video"))?,
    };
    let duration = info["video_properties"]["duration"].as_f64().map(|d| d as u64);
    let title = info["name"].as_str().map(str::to_string);
    let path = url.split('?').next().unwrap_or(&url);
    if path.ends_with(".m3u8") {
        // Its signature has to go along on every request after the first.
        let whole = url.replace("-split.m3u8", ".m3u8");
        return Ok(Found::new(format!("{whole}#rdm-keep-query"), title, duration));
    }
    let found = Found::new(url.as_str(), title, duration);
    Ok(if path.ends_with(".webm") { found.ext("webm") } else { found })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://www.loom.com/share/43d05f362f734614a2e81b4694a3a523"));
        assert!(is("https://www.loom.com/embed/43d05f362f734614a2e81b4694a3a523?t=1"));
        assert!(!is("https://www.loom.com/share/folder/abc"));
    }
}
