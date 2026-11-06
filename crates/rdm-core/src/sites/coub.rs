//! Coub: short looping clips, with the picture and the sound in separate files. Their sound is
//! MP3, which RDM's joiner can't put in an MP4, so without ffmpeg they stay two files.

use super::{Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Coub", "coub.com", false)
}

fn clip_id(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "coub.com" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["view" | "embed" | "coubs", id, ..] => Some(id),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    clip_id(url).is_some()
}

/// The best quality of one kind of file.
fn best(kind: &Value) -> Option<&str> {
    ["higher", "high", "med"].iter().find_map(|q| kind[*q]["url"].as_str())
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = clip_id(page.url).ok_or_else(|| fail("not a clip page"))?;
    let clip = page.json(page.client.get(format!("https://coub.com/api/v2/coubs/{id}.json"))).await?;
    let files = &clip["file_versions"]["html5"];
    let video = best(&files["video"]).ok_or_else(|| fail("Coub has no file for this clip"))?;
    // The phone version is one file; the others can't be joined, their sound being MP3.
    let whole = clip["file_versions"]["iphone"]["url"].as_str().map(str::to_string);
    let url = whole.unwrap_or_else(|| best(&files["audio"]).map_or_else(|| video.to_string(), |audio| crate::pair::pair_url(video, audio)));
    Ok(Found::new(url, clip["title"].as_str().map(str::to_string), clip["duration"].as_f64().map(|d| d as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clip_pages_and_qualities() {
        assert!(matches(&Url::parse("http://coub.com/view/5u5n1").unwrap()));
        assert!(!matches(&Url::parse("https://coub.com/community/fun").unwrap()));
        assert_eq!(best(&json!({ "med": { "url": "m" }, "high": { "url": "h" } })), Some("h"));
    }
}
