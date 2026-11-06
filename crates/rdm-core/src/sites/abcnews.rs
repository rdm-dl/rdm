//! ABC News: the player's feed names the stream.

use super::{Family, Found, Page, amp, fail, path_parts};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("ABC News", "abcnews.go.com", true)
}

/// The number at the end of `/.../video/some-title-123`, or from `/video/embed?id=123`.
fn video_id(url: &Url) -> Option<String> {
    if url.host_str()? != "abcnews.go.com" {
        return None;
    }
    let parts = path_parts(url);
    let at = parts.iter().position(|p| *p == "video")?;
    let id = match parts.get(at + 1)? {
        &"embed" | &"itemfeed" => url.query_pairs().find(|(k, _)| k == "id")?.1.into_owned(),
        slug => slug.rsplit('-').next()?.to_string(),
    };
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let feed = page.json(page.client.get("https://abcnews.go.com/video/itemfeed").query(&[("id", id.as_str())])).await?;
    amp::stream(&feed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap());
        assert_eq!(id("http://abcnews.go.com/ThisWeek/video/week-exclusive-zarif-20411932").as_deref(), Some("20411932"));
        assert_eq!(id("https://abcnews.go.com/video/embed?id=46979033").as_deref(), Some("46979033"));
        assert_eq!(id("https://abcnews.go.com/US/a-story/story?id=123"), None);
    }
}
