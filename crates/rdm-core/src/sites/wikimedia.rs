//! Wikimedia Commons: its API gives each file as it was uploaded.

use super::{Episode, Family, Found, Page, fail, path_parts, plain_host};
use crate::download::DownloadError;
use percent_encoding::percent_decode_str;
use reqwest::{RequestBuilder, header};
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const API: &str = "https://commons.wikimedia.org/w/api.php";
const MOST_PAGES: usize = 20;
/// The media kinds a category's list keeps.
const KINDS: [&str; 7] = ["webm", "ogv", "mp4", "mpg", "mpeg", "ogg", "oga"];

/// Wikimedia's rules ask programs to name themselves and say where they're from, and it refuses
/// anything that reads like a browser.
fn agent() -> String {
    format!("RDM/{} (https://rdm-dl.github.io)", env!("CARGO_PKG_VERSION"))
}

fn named(request: RequestBuilder) -> RequestBuilder {
    request.header(header::USER_AGENT, agent())
}

pub fn family() -> Family {
    Family::single("Wikimedia Commons", "commons.wikimedia.org", true)
}

/// `File:Name.webm` from a page's address, with its characters back as written.
fn title_after(url: &Url, start: &str) -> Option<String> {
    if plain_host(url, &[]) != "commons.wikimedia.org" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["wiki", name] => {
            let name = percent_decode_str(name).decode_utf8_lossy().replace('_', " ");
            name.starts_with(start).then_some(name)
        }
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    title_after(url, "File:").is_some()
}

pub fn is_series(url: &Url) -> bool {
    title_after(url, "Category:").is_some()
}

fn extension(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('.').next()?.to_ascii_lowercase();
    KINDS.into_iter().find(|k| *k == ext)
}

/// The name without `File:` and its extension.
fn plain_name(title: &str) -> String {
    let name = title.trim_start_matches("File:");
    name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let title = title_after(page.url, "File:").ok_or_else(|| fail("not a file page"))?;
    let query = [("action", "query"), ("format", "json"), ("titles", title.as_str()), ("prop", "videoinfo"), ("viprop", "url|size")];
    let reply = page.json(named(page.client.get(API)).query(&query)).await?;
    let info = reply["query"]["pages"].as_object().and_then(|p| p.values().next()).map(|p| &p["videoinfo"][0]).filter(|i| i.is_object());
    let info = info.ok_or_else(|| fail("Commons has no such file."))?;
    let url = info["url"].as_str().ok_or_else(|| fail("Commons has no such file."))?;
    let ext = title.rsplit('.').next().unwrap_or("mp4").to_ascii_lowercase();
    let mut found = Found::new(url, Some(plain_name(&title)), info["duration"].as_f64().map(|d| d.round() as u64)).ext(&ext);
    found.user_agent = Some(agent());
    Ok(found)
}

fn file_page(title: &str) -> Option<String> {
    let mut url = Url::parse("https://commons.wikimedia.org/wiki/").ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().push(&title.replace(' ', "_"));
    Some(url.to_string())
}

fn items(reply: &Value, list: &str) -> Vec<Episode> {
    let members = reply["query"]["categorymembers"].as_array().into_iter().flatten();
    members.filter_map(|m| m["title"].as_str()).filter_map(|t| Some(Episode::item(file_page(t)?, list, &plain_name(t), None, None, extension(t)?))).collect()
}

pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let title = title_after(page.url, "Category:").ok_or_else(|| fail("not a category page"))?;
    let name = title.trim_start_matches("Category:").to_string();
    let mut out = vec![];
    let mut next: Option<String> = None;
    for _ in 0..MOST_PAGES {
        let mut query = vec![("action", "query"), ("format", "json"), ("list", "categorymembers"), ("cmtype", "file"), ("cmlimit", "500"), ("cmtitle", title.as_str())];
        query.extend(next.as_deref().map(|n| ("cmcontinue", n)));
        let reply = page.json(named(page.client.get(API)).query(&query)).await?;
        out.extend(items(&reply, &name));
        next = reply["continue"]["cmcontinue"].as_str().map(str::to_string);
        if next.is_none() {
            break;
        }
    }
    if out.is_empty() {
        return Err(fail("This category has no videos or sound files."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_are_read_with_their_names() {
        let url = Url::parse("https://commons.wikimedia.org/wiki/File:Two-toed_sloth_%E4%BA%8C%E8%B6%BE_(HD).webm").unwrap();
        assert_eq!(title_after(&url, "File:").as_deref(), Some("File:Two-toed sloth 二趾 (HD).webm"));
        assert!(!is_series(&url));
        assert!(is_series(&Url::parse("https://commons.wikimedia.org/wiki/Category:Videos_of_sloths").unwrap()));
        assert!(!matches(&Url::parse("https://commons.wikimedia.org/wiki/Main_Page").unwrap()));
        assert_eq!(plain_name("File:A b.c.webm"), "A b.c");
    }

    #[test]
    fn a_category_keeps_only_media() {
        let reply = json!({ "query": { "categorymembers": [{ "title": "File:A clip?.webm" }, { "title": "File:A photo.jpg" }] } });
        let list = items(&reply, "Cats");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].url, "https://commons.wikimedia.org/wiki/File:A_clip%3F.webm");
        assert_eq!(list[0].ext, "webm");
    }
}
