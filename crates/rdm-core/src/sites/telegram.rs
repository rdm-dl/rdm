//! Telegram: public channels have web pages, one for each post and a running list of them.

use super::{Episode, Family, Found, Page, SiteInfo, clock, fail, path_parts, plain_host, unescape};
use crate::download::DownloadError;
use regex::Regex;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const MOST_PAGES: usize = 30;
/// Addresses that aren't channels.
const RESERVED: [&str; 6] = ["c", "joinchat", "addstickers", "addemoji", "share", "proxy"];

pub fn family() -> Family {
    let site = |domain| SiteInfo { name: "Telegram", domain, tested: true };
    Family { name: "Telegram", sites: vec![site("t.me"), site("telegram.me")], browser_only: false }
}

fn parts(url: &Url) -> Vec<&str> {
    if !matches!(plain_host(url, &["www."]), "t.me" | "telegram.me") {
        return vec![];
    }
    let parts = path_parts(url);
    if parts.first() == Some(&"s") { parts[1..].to_vec() } else { parts }
}

fn post_of(url: &Url) -> Option<(&str, &str)> {
    match parts(url).as_slice() {
        [channel, id] if !RESERVED.contains(channel) && id.bytes().all(|b| b.is_ascii_digit()) => Some((*channel, *id)),
        _ => None,
    }
}

fn channel_of(url: &Url) -> Option<&str> {
    match parts(url).as_slice() {
        [channel] if !RESERVED.contains(channel) => Some(*channel),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    post_of(url).is_some()
}

pub fn is_series(url: &Url) -> bool {
    channel_of(url).is_some()
}

/// A post's words without their markup, cut to fit a file name.
fn words(html: &str) -> Option<String> {
    let found = Regex::new(r#"(?s)<div class="tgme_widget_message_text[^"]*"[^>]*>(.*?)</div>"#).ok()?.captures(html)?;
    let plain = Regex::new(r"<[^>]+>").ok()?.replace_all(&found[1].replace("<br/>", " "), "").into_owned();
    let text: String = unescape(&plain).trim().chars().take(100).collect();
    (!text.is_empty()).then_some(text)
}

fn duration(html: &str) -> Option<u64> {
    let found = Regex::new(r"<time[^>]+duration[^>]*>([\d:]+)</time>").ok()?.captures(html)?;
    clock(&found[1])
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let (channel, id) = post_of(page.url).ok_or_else(|| fail("not a post"))?;
    let html = page.text(page.client.get(format!("https://t.me/{channel}/{id}?embed=1&single"))).await?;
    if !html.contains("tgme_widget_message_video_player") {
        return Err(fail("This post has no video that Telegram shows on the web."));
    }
    let source = Regex::new(r#"<video[^>]+src="([^"]+)""#).map_err(|e| fail(e.to_string()))?.captures(&html);
    let source = source.ok_or_else(|| fail("Telegram only shows this video in its app, because it's too big for the web page."))?;
    let title = words(&html).unwrap_or_else(|| format!("{channel} {id}"));
    Ok(Found::new(unescape(&source[1]), Some(title), duration(&html)))
}

/// The posts with videos on one page of a channel's list, and the oldest post there.
fn posts(html: &str, channel: &str) -> (Vec<(u64, Episode)>, Option<u64>) {
    let mut out = vec![];
    let mut oldest = None;
    for block in html.split("data-post=\"").skip(1) {
        let Some(id) = block.split('"').next().and_then(|p| p.rsplit('/').next()).and_then(|n| n.parse::<u64>().ok()) else { continue };
        oldest = Some(oldest.map_or(id, |o: u64| o.min(id)));
        if block.contains("tgme_widget_message_video_player") && block.contains("<video") {
            let title = words(block).unwrap_or_else(|| format!("{channel} {id}"));
            out.push((id, Episode::item(format!("https://t.me/{channel}/{id}"), channel, &format!("{id} {title}"), None, duration(block), "mp4")));
        }
    }
    (out, oldest)
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let channel = channel_of(page.url).ok_or_else(|| fail("not a channel page"))?;
    let mut found = vec![];
    let mut before: Option<u64> = None;
    for _ in 0..MOST_PAGES {
        let mut request = page.client.get(format!("https://t.me/s/{channel}"));
        if let Some(id) = before {
            request = request.query(&[("before", id)]);
        }
        let html = page.text(request).await?;
        let (list, oldest) = posts(&html, channel);
        found.extend(list);
        if oldest.is_none() || oldest == before || oldest == Some(1) {
            break;
        }
        before = oldest;
    }
    found.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    found.dedup_by_key(|(id, _)| *id);
    if found.is_empty() {
        return Err(fail("This channel has no videos that Telegram shows on the web."));
    }
    Ok(found.into_iter().map(|(_, e)| e).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posts_and_channels() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert_eq!(post_of(&url("https://t.me/europa_press/613")), Some(("europa_press", "613")));
        assert_eq!(post_of(&url("https://t.me/s/europa_press/613")), Some(("europa_press", "613")));
        assert!(!matches(&url("https://t.me/c/12345/613")));
        assert!(is_series(&url("https://t.me/s/europa_press")));
        assert!(is_series(&url("https://t.me/europa_press")));
        assert!(!is_series(&url("https://t.me/joinchat")));
    }

    #[test]
    fn a_channel_page_is_read() {
        let html = concat!(
            r#"<div data-post="news/7"><div class="tgme_widget_message_text js-message_text" dir="auto">No video here</div></div>"#,
            r#"<div data-post="news/8"><a class="tgme_widget_message_video_player"><video src="v.mp4"></video><time class="message_video_duration" duration>1:01</time></a>"#,
            r#"<div class="tgme_widget_message_text js-message_text" dir="auto">Lava &amp; <b>ash</b><br/>flow</div></div>"#,
        );
        let (list, oldest) = posts(html, "news");
        assert_eq!(oldest, Some(7));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].1.title, "8 Lava & ash flow");
        assert_eq!(list[0].1.duration, Some(61));
        assert_eq!(list[0].1.url, "https://t.me/news/8");
    }
}
