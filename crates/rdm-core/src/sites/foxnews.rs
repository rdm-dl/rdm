//! Fox News and Fox Business: the player's feed names the stream.

use super::{Family, Found, Page, SiteInfo, amp, fail, path_parts, plain_host};
use crate::download::DownloadError;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family { name: "Fox News", sites: vec![site("Fox News", "foxnews.com", true), site("Fox Business", "foxbusiness.com", false)], browser_only: false }
}

/// The number from `foxnews.com/video/123` or `video.foxnews.com/v/123`.
fn video_id(url: &Url) -> Option<&str> {
    if !matches!(plain_host(url, &["www.", "video.", "insider."]), "foxnews.com" | "foxbusiness.com") {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["v", "video-embed.html", id, ..] | ["video" | "v", id, ..] => *id,
        _ => return None,
    };
    id.bytes().all(|b| b.is_ascii_digit()).then_some(id)
}

pub fn matches(url: &Url) -> bool {
    video_id(url).is_some()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = video_id(page.url).ok_or_else(|| fail("not a video page"))?;
    let reply = page.text(page.client.get(format!("https://api.foxnews.com/v3/video-player/{id}?callback=uid_{id}"))).await?;
    // The answer comes wrapped in a function call.
    let inner = reply.find('(').zip(reply.rfind(')')).and_then(|(a, b)| reply.get(a + 1..b)).ok_or_else(|| fail("Fox's answer can't be read"))?;
    let feed = serde_json::from_str(inner).map_err(|e| fail(format!("Fox's answer can't be read: {e}")))?;
    amp::stream(&feed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let id = |u: &str| video_id(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("https://www.foxnews.com/video/6328632286112").as_deref(), Some("6328632286112"));
        assert_eq!(id("https://video.foxnews.com/v/6320653836112").as_deref(), Some("6320653836112"));
        assert_eq!(id("https://www.foxnews.com/politics/a-story"), None);
    }
}
