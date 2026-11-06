//! Dropbox shared links: the link itself gives the file when asked to.

use super::{Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use reqwest::header;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Dropbox", "dropbox.com", true)
}

pub fn matches(url: &Url) -> bool {
    plain_host(url, &["www."]) == "dropbox.com" && matches!(path_parts(url).as_slice(), ["s" | "sh", _, ..] | ["scl", "fi" | "fo", _, ..])
}

/// The same link, asking for the file instead of Dropbox's page for it.
fn file_link(url: &Url) -> Url {
    let keep: Vec<_> = url.query_pairs().filter(|(k, _)| k != "dl").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut out = url.clone();
    out.set_fragment(None);
    out.query_pairs_mut().clear().extend_pairs(keep).append_pair("dl", "1");
    out
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let link = file_link(page.url);
    let reply = page.client.get(link.as_str()).header(header::RANGE, "bytes=0-0").send().await.map_err(|e| fail(e.to_string()))?;
    let is_page = reply.headers().get(header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).is_some_and(|t| t.starts_with("text/html"));
    if !reply.status().is_success() || is_page {
        return Err(fail("Dropbox isn't offering this file for download; it may be private or have downloads turned off."));
    }
    Ok(Found::new(link.to_string(), None, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_ask_for_the_file() {
        let url = Url::parse("https://www.dropbox.com/scl/fi/abc/a.mp4?rlkey=xyz&dl=0").unwrap();
        assert!(matches(&url));
        assert_eq!(file_link(&url).as_str(), "https://www.dropbox.com/scl/fi/abc/a.mp4?rlkey=xyz&dl=1");
        assert!(matches(&Url::parse("https://www.dropbox.com/s/abc/a.mp4").unwrap()));
        assert!(!matches(&Url::parse("https://www.dropbox.com/home").unwrap()));
    }
}
