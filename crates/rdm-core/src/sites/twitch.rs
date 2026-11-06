//! Twitch channels on air and past broadcasts: a signed access token opens the playlist.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::json;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const CLIENT_ID: &str = "ue6666qo983tsx6so1t0vnawi233wa";
/// The site's stored request for a channel's videos.
const CHANNEL_VIDEOS: &str = "67004f7881e65c297936f32c75246470629557a393788fb5a69d6d9a25a8fd5f";
/// A channel can have thousands; this many newest ones is a sensible list.
const MOST_VIDEOS: usize = 500;
/// The site's own pages, which look like channels.
const RESERVED: [&str; 16] = ["directory", "downloads", "drops", "friends", "inventory", "jobs", "login", "messages", "p", "payments", "search", "settings", "signup", "subscriptions", "turbo", "wallet"];

pub fn family() -> Family {
    Family::single("Twitch", "twitch.tv", true)
}

/// The number from `/videos/123` or `/channel/v/123`.
fn video_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "go.", "m."]) != "twitch.tv" {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["videos", id, ..] | [_, "v" | "video", id, ..] => *id,
        _ => return None,
    };
    let id = id.strip_prefix('v').unwrap_or(id);
    let at = url.as_str().find(id)?;
    id.bytes().all(|b| b.is_ascii_digit()).then(|| &url.as_str()[at..at + id.len()])
}

/// The channel from `/name`, which is its live page.
fn live_channel(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "go.", "m."]) != "twitch.tv" {
        return None;
    }
    match path_parts(url).as_slice() {
        [name] if !RESERVED.contains(name) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => Some(*name),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some() || live_channel(url).is_some()
}

/// The channel from `/channel/videos`.
fn channel(url: &Url) -> Option<&str> {
    if plain_host(url, &["www.", "go.", "m."]) != "twitch.tv" {
        return None;
    }
    match path_parts(url).as_slice() {
        [name, "videos"] if *name != "directory" => Some(name),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    channel(url).is_some()
}

/// A channel's past broadcasts, newest first.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let name = channel(page.url).ok_or_else(|| fail("not a channel's videos page"))?;
    let mut out = vec![];
    let mut cursor: Option<String> = None;
    loop {
        let body = json!([{
            "operationName": "FilterableVideoTower_Videos",
            "variables": { "channelOwnerLogin": name, "broadcastType": "ARCHIVE", "videoSort": "TIME", "limit": 100, "cursor": cursor },
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": CHANNEL_VIDEOS } },
        }]);
        let reply = page.json(page.client.post("https://gql.twitch.tv/gql").header("Client-ID", CLIENT_ID).header("content-type", "text/plain;charset=UTF-8").body(body.to_string())).await?;
        let videos = &reply[0]["data"]["user"]["videos"];
        let edges = videos["edges"].as_array().ok_or_else(|| fail("This Twitch channel wasn't found"))?;
        for edge in edges {
            let node = &edge["node"];
            let (Some(id), Some(title)) = (node["id"].as_str(), node["title"].as_str()) else { continue };
            out.push(Episode::item(format!("https://www.twitch.tv/videos/{id}"), name, title, None, node["lengthSeconds"].as_u64(), "mp4"));
            cursor = edge["cursor"].as_str().map(str::to_string);
        }
        if videos["pageInfo"]["hasNextPage"] != true || edges.is_empty() || out.len() >= MOST_VIDEOS {
            return Ok(out);
        }
    }
}

fn stream_address(path: &str, token: &str, signature: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("https://usher.ttvnw.net/{path}.m3u8")).map_err(|e| fail(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("allow_source", "true")
        .append_pair("allow_audio_only", "true")
        .append_pair("platform", "web")
        .append_pair("player", "twitchweb")
        .append_pair("supported_codecs", "h264")
        .append_pair("playlist_include_framerate", "true")
        .append_pair("sig", signature)
        .append_pair("token", token);
    Ok(url)
}

/// A channel on air.
async fn find_live(page: &Page<'_>, name: &str) -> Result<Found> {
    let name = name.to_ascii_lowercase();
    let query = format!(
        r#"{{ streamPlaybackAccessToken(channelName: "{name}", params: {{ platform: "web", playerBackend: "mediaplayer", playerType: "site" }}) {{ value signature }} user(login: "{name}") {{ stream {{ id }} broadcastSettings {{ title }} }} }}"#
    );
    let mut request = page.client.post("https://gql.twitch.tv/gql").header("Client-ID", CLIENT_ID).header("content-type", "text/plain;charset=UTF-8").body(json!({ "query": query }).to_string());
    if let Some(token) = page.cookie("auth-token") {
        request = request.header("Authorization", format!("OAuth {token}"));
    }
    let reply = page.json(request).await?;
    let user = &reply["data"]["user"];
    if user["stream"].is_null() {
        return Err(fail("This Twitch channel isn't live right now."));
    }
    let access = &reply["data"]["streamPlaybackAccessToken"];
    let (Some(token), Some(signature)) = (access["value"].as_str(), access["signature"].as_str()) else {
        return Err(fail("Twitch has no stream for this channel"));
    };
    let title = user["broadcastSettings"]["title"].as_str().map_or_else(|| format!("{name} live"), |t| format!("{name} - {}", t.trim()));
    Ok(Found::new(stream_address(&format!("api/channel/hls/{name}"), token, signature)?, Some(title), None))
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    if let Some(name) = live_channel(page.url) {
        return find_live(page, name).await;
    }
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let query = format!(
        r#"{{ videoPlaybackAccessToken(id: "{id}", params: {{ platform: "web", playerBackend: "mediaplayer", playerType: "site" }}) {{ value signature }} video(id: "{id}") {{ title lengthSeconds }} }}"#
    );
    let mut request = page.client.post("https://gql.twitch.tv/gql").header("Client-ID", CLIENT_ID).header("content-type", "text/plain;charset=UTF-8").body(json!({ "query": query }).to_string());
    if let Some(token) = page.cookie("auth-token") {
        request = request.header("Authorization", format!("OAuth {token}"));
    }
    let reply = page.json(request).await?;
    let access = &reply["data"]["videoPlaybackAccessToken"];
    let (Some(token), Some(signature)) = (access["value"].as_str(), access["signature"].as_str()) else {
        return Err(fail("Twitch has no stream for this video"));
    };
    let url = stream_address(&format!("vod/{id}"), token, signature)?;
    let video = &reply["data"]["video"];
    Ok(Found::new(url, video["title"].as_str().map(str::to_string), video["lengthSeconds"].as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_pages_are_told_from_the_rest() {
        let live = |u: &str| live_channel(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(live("https://www.twitch.tv/shroud").as_deref(), Some("shroud"));
        assert_eq!(live("https://www.twitch.tv/directory"), None);
        assert_eq!(live("https://www.twitch.tv/shroud/videos"), None);
        assert_eq!(live("https://www.twitch.tv/videos/123"), None);
        assert!(!is_series(&Url::parse("https://www.twitch.tv/shroud").unwrap()));
    }

    #[test]
    fn channel_pages_are_recognised() {
        assert!(is_series(&Url::parse("https://www.twitch.tv/riotgames/videos").unwrap()));
        assert!(!is_series(&Url::parse("https://www.twitch.tv/riotgames").unwrap()));
        assert!(!is_series(&Url::parse("https://www.twitch.tv/videos/123").unwrap()));
    }

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://www.twitch.tv/videos/1234567890").as_deref(), Some("1234567890"));
        assert_eq!(id("http://www.twitch.tv/riotgames/v/6528877?t=5m10s").as_deref(), Some("6528877"));
        assert_eq!(id("https://www.twitch.tv/riotgames"), None);
        assert_eq!(id("https://www.twitch.tv/videos/latest"), None);
    }
}
