//! Apple Podcasts: Apple's public lookup names a show's feed, which lists every episode and its
//! sound file; the lookup's own shorter list is the fallback.

use super::{Episode, Family, Found, Page, clock, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// The most Apple gives at once.
const MOST: &str = "200";

pub fn family() -> Family {
    Family::single("Apple Podcasts", "podcasts.apple.com", true)
}

/// The show's number, from `/us/podcast/name/id123`.
fn show_id(url: &Url) -> Option<&str> {
    if plain_host(url, &[]) != "podcasts.apple.com" {
        return None;
    }
    let parts = path_parts(url);
    if !parts.contains(&"podcast") {
        return None;
    }
    parts.last()?.strip_prefix("id").filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

fn episode_id(url: &Url) -> Option<String> {
    show_id(url)?;
    url.query_pairs().find(|(k, _)| k == "i").map(|(_, v)| v.into_owned())
}

pub fn matches(url: &Url) -> bool {
    episode_id(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    show_id(url).is_some() && episode_id(url).is_none()
}

async fn lookup(page: &Page<'_>) -> Result<Vec<Value>> {
    let id = show_id(page.url).ok_or_else(|| fail("not a podcast page"))?;
    let reply = page.json(page.client.get("https://itunes.apple.com/lookup").query(&[("id", id), ("entity", "podcastEpisode"), ("limit", MOST)])).await?;
    Ok(reply["results"].as_array().cloned().unwrap_or_default())
}

fn extension(episode: &Value) -> &'static str {
    match episode["episodeFileExtension"].as_str() {
        Some("m4a") => "m4a",
        Some("mp4") => "mp4",
        _ => "mp3",
    }
}

/// Every `EpisodeLockup` inside `value`.
fn lockups<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if map.get("modelType").is_some_and(|m| m == "EpisodeLockup") {
                out.push(value);
            }
            map.values().for_each(|v| lockups(v, out));
        }
        Value::Array(list) => list.iter().for_each(|v| lockups(v, out)),
        _ => {}
    }
}

/// The page of an episode outside the newest 200 still names its file.
fn from_page(html: &str) -> Option<(String, Option<String>, Option<u64>)> {
    let start = html.find(r#"id="serialized-server-data""#)?;
    let open = start + html[start..].find('>')? + 1;
    let data: Value = serde_json::Deserializer::from_str(&html[open..]).into_iter().next()?.ok()?;
    let mut found = vec![];
    lockups(&data, &mut found);
    let model = &found.first()?["model"];
    let url = model["playAction"]["episodeOffer"]["streamUrl"].as_str()?.to_string();
    Some((url, model["title"].as_str().map(str::to_string), model["duration"].as_u64()))
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = episode_id(page.url).ok_or_else(|| fail("not an episode page"))?;
    let list = lookup(page).await?;
    if let Some(episode) = list.iter().find(|e| e["trackId"].as_u64().is_some_and(|t| t.to_string() == id)) {
        let url = episode["episodeUrl"].as_str().ok_or_else(|| fail("Apple has no file for this episode"))?;
        return Ok(Found::new(url, episode["trackName"].as_str().map(str::to_string), episode["trackTimeMillis"].as_u64().map(|ms| ms / 1000)).ext(extension(episode)));
    }
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let (url, title, duration) = from_page(&html).ok_or_else(|| fail("Apple has no file for this episode"))?;
    let ext = if url.split('?').next().is_some_and(|p| p.ends_with(".m4a")) { "m4a" } else { "mp3" };
    Ok(Found::new(url, title, duration).ext(ext))
}

/// The episodes of a podcast's feed.
pub(super) fn from_feed(xml: &str, show: &str) -> Vec<Episode> {
    let Ok(feed) = roxmltree::Document::parse(xml) else { return vec![] };
    let episode = |item: roxmltree::Node| {
        let child = |name: &str| item.children().find(|c| c.tag_name().name() == name);
        let file = child("enclosure")?;
        let url = file.attribute("url")?;
        let m4a = file.attribute("type").is_some_and(|t| t.contains("mp4") || t.contains("m4a")) || url.split('?').next().is_some_and(|p| p.ends_with(".m4a"));
        let time = child("duration").and_then(|d| d.text()).and_then(clock);
        Some(Episode::item(url, show, child("title")?.text()?, None, time, if m4a { "m4a" } else { "mp3" }))
    };
    feed.descendants().filter(|n| n.has_tag_name("item")).filter_map(episode).collect()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let list = lookup(page).await?;
    let show = list.iter().find_map(|e| e["collectionName"].as_str()).unwrap_or("Podcast").to_string();
    if let Some(feed) = list.iter().find_map(|e| e["feedUrl"].as_str()) {
        let read = page.text(page.client.get(feed)).await.map(|xml| from_feed(&xml, &show));
        if let Some(out) = read.ok().filter(|o| !o.is_empty()) {
            return Ok(out);
        }
    }
    let out: Vec<_> = list
        .iter()
        .filter(|e| e["wrapperType"] == "podcastEpisode" || e["kind"] == "podcast-episode")
        .filter_map(|e| Some(Episode::item(e["episodeUrl"].as_str()?, &show, e["trackName"].as_str()?, None, e["trackTimeMillis"].as_u64().map(|ms| ms / 1000), extension(e))))
        .collect();
    if out.is_empty() {
        return Err(fail("This podcast has no episodes to get."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn episode_and_show_pages() {
        let episode = Url::parse("https://podcasts.apple.com/us/podcast/the-voters/id1200361736?i=1000800028507&uo=4").unwrap();
        let show = Url::parse("https://podcasts.apple.com/us/podcast/the-daily/id1200361736").unwrap();
        assert!(matches(&episode) && !is_series(&episode));
        assert!(!matches(&show) && is_series(&show));
        assert!(!is_series(&Url::parse("https://podcasts.apple.com/us/charts").unwrap()));
    }

    #[test]
    fn a_feed_gives_every_episode() {
        let xml = r#"<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Show</title>
            <item><title><![CDATA[First & best]]></title><itunes:duration>01:01</itunes:duration><enclosure url="https://a/1.mp3?x=1" type="audio/mpeg"/></item>
            <item><title>Second</title><itunes:duration>90</itunes:duration><enclosure url="https://a/2.m4a" type="audio/x-m4a"/></item>
            <item><title>No file</title></item></channel></rss>"#;
        let list = from_feed(xml, "Show");
        assert_eq!(list.iter().map(|e| (e.title.as_str(), e.duration, e.ext)).collect::<Vec<_>>(), [("First & best", Some(61), "mp3"), ("Second", Some(90), "m4a")]);
        assert_eq!(list[0].url, "https://a/1.mp3?x=1");
    }

    #[test]
    fn file_kinds() {
        assert_eq!(extension(&json!({ "episodeFileExtension": "m4a" })), "m4a");
        assert_eq!(extension(&json!({})), "mp3");
    }

    #[test]
    fn an_older_episode_is_read_from_its_page() {
        let html = r#"<script type="application/json" id="serialized-server-data">{"data":[{"headerButtonItems":[{"modelType":"EpisodeLockup","model":{"title":"Old One","duration":61,"playAction":{"episodeOffer":{"streamUrl":"https://a/b.mp3"}}}}]}]}</script>"#;
        assert_eq!(from_page(html), Some(("https://a/b.mp3".to_string(), Some("Old One".to_string()), Some(61))));
    }
}
