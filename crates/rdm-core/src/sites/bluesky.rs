//! Bluesky: a public API gives each post, and the poster's own server keeps the file as uploaded.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const API: &str = "https://public.api.bsky.app/xrpc";
const MOST_PAGES: usize = 20;

pub fn family() -> Family {
    Family::single("Bluesky", "bsky.app", true)
}

fn post_of(url: &Url) -> Option<(&str, &str)> {
    if plain_host(url, &["www."]) != "bsky.app" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["profile", who, "post", id] => Some((*who, *id)),
        _ => None,
    }
}

fn profile_of(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "bsky.app" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["profile", who] | ["profile", who, "media"] => Some(*who),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    post_of(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    profile_of(url).is_some()
}

/// The video of a post, and whether it's the poster's own rather than one in a quoted post.
fn video_of(post: &Value) -> Option<(&Value, bool)> {
    let embed = &post["embed"];
    let own = [embed, &embed["media"]].into_iter().find(|e| e["playlist"].is_string());
    let quoted = || [&embed["record"]["record"]["embeds"][0], &embed["record"]["embeds"][0], &embed["media"]["record"]["record"]["embeds"][0]].into_iter().find(|e| e["playlist"].is_string());
    own.map(|e| (e, true)).or_else(|| quoted().map(|e| (e, false)))
}

/// The first line of a post, short enough for a file name.
fn title_of(post: &Value, id: &str) -> String {
    let text = post["record"]["text"].as_str().unwrap_or_default().lines().next().unwrap_or_default().trim();
    let short: String = text.chars().take(72).collect();
    if short.is_empty() { format!("Bluesky post {id}") } else { short }
}

/// The address of the file on the poster's own server.
async fn original(page: &Page<'_>, did: &str, cid: &str) -> Option<String> {
    let address = match did.strip_prefix("did:web:") {
        Some(host) => format!("https://{host}/.well-known/did.json"),
        None => format!("https://plc.directory/{did}"),
    };
    let about = page.json(page.client.get(address)).await.ok()?;
    let service = about["service"].as_array()?.iter().find(|s| s["type"] == "AtprotoPersonalDataServer")?;
    let mut file = Url::parse(service["serviceEndpoint"].as_str()?).ok()?.join("/xrpc/com.atproto.sync.getBlob").ok()?;
    file.query_pairs_mut().append_pair("did", did).append_pair("cid", cid);
    Some(file.to_string())
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let (who, id) = post_of(page.url).ok_or_else(|| fail("not a post"))?;
    let uri = format!("at://{who}/app.bsky.feed.post/{id}");
    let query = [("uri", uri.as_str()), ("depth", "0"), ("parentHeight", "0")];
    let reply = page.json(page.client.get(format!("{API}/app.bsky.feed.getPostThread")).query(&query)).await?;
    let post = &reply["thread"]["post"];
    let (video, own) = video_of(post).ok_or_else(|| fail("This post has no video."))?;
    let playlist = video["playlist"].as_str().ok_or_else(|| fail("This post has no video."))?;
    let file = match (own, post["author"]["did"].as_str(), video["cid"].as_str()) {
        (true, Some(did), Some(cid)) => original(page, did, cid).await,
        _ => None,
    };
    Ok(Found::new(file.as_deref().unwrap_or(playlist), Some(title_of(post, id)), None))
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let who = profile_of(page.url).ok_or_else(|| fail("not a profile"))?;
    let mut out = vec![];
    let mut cursor: Option<String> = None;
    let mut name = who.to_string();
    for _ in 0..MOST_PAGES {
        let mut request = page.client.get(format!("{API}/app.bsky.feed.getAuthorFeed")).query(&[("actor", who), ("filter", "posts_with_video"), ("limit", "100")]);
        if let Some(after) = &cursor {
            request = request.query(&[("cursor", after)]);
        }
        let reply = page.json(request).await?;
        for item in reply["feed"].as_array().into_iter().flatten() {
            let post = &item["post"];
            let (Some(handle), Some(id)) = (post["author"]["handle"].as_str(), post["uri"].as_str().and_then(|u| u.rsplit('/').next())) else { continue };
            if !item["reason"].is_null() || video_of(post).is_none() {
                continue;
            }
            name = post["author"]["displayName"].as_str().filter(|n| !n.is_empty()).unwrap_or(handle).to_string();
            out.push(Episode::item(format!("https://bsky.app/profile/{handle}/post/{id}"), &name, &title_of(post, id), None, None, "mp4"));
        }
        cursor = reply["cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    if out.is_empty() {
        return Err(fail("This profile has no videos."));
    }
    for episode in &mut out {
        episode.show = name.clone();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn posts_and_profiles() {
        let url = Url::parse("https://bsky.app/profile/bsky.app/post/3l3vgf77uco2g").unwrap();
        assert_eq!(post_of(&url), Some(("bsky.app", "3l3vgf77uco2g")));
        assert!(!is_series(&url));
        assert!(is_series(&Url::parse("https://bsky.app/profile/bsky.app").unwrap()));
        assert!(!matches(&Url::parse("https://bsky.app/profile/bsky.app").unwrap()));
    }

    #[test]
    fn videos_of_every_kind_of_post() {
        let plain = json!({ "embed": { "playlist": "a" } });
        assert_eq!(video_of(&plain).map(|(v, own)| (v["playlist"].clone(), own)), Some((json!("a"), true)));
        let with_media = json!({ "embed": { "media": { "playlist": "b" } } });
        assert_eq!(video_of(&with_media).map(|(_, own)| own), Some(true));
        let quoted = json!({ "embed": { "record": { "record": { "embeds": [{ "playlist": "c" }] } } } });
        assert_eq!(video_of(&quoted).map(|(_, own)| own), Some(false));
        assert!(video_of(&json!({ "embed": { "images": [] } })).is_none());
    }

    #[test]
    fn titles_are_short() {
        let post = json!({ "record": { "text": "First line\nsecond" } });
        assert_eq!(title_of(&post, "x"), "First line");
        assert_eq!(title_of(&json!({}), "x"), "Bluesky post x");
    }
}
