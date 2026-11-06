//! The Internet Archive: an item's metadata lists its files; the largest playable one is taken.

use super::{Family, Found, Page, clock, fail, path_parts, plain_host};
use crate::download::DownloadError;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// File types worth saving, best first.
const TYPES: &[&str] = &["mp4", "mkv", "webm", "ogv", "flac", "m4a", "mp3", "ogg"];

pub fn family() -> Family {
    Family::single("Internet Archive", "archive.org", true)
}

fn item(url: &Url) -> Option<&str> {
    if plain_host(url, &["www."]) != "archive.org" {
        return None;
    }
    match path_parts(url).as_slice() {
        ["details" | "embed", id, ..] => Some(id),
        _ => None,
    }
}

pub fn matches(url: &Url) -> bool {
    item(url).is_some()
}

fn size(f: &Value) -> u64 {
    f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// The file to save: the best type there is, and the largest of those.
fn best(files: &[Value]) -> Option<(&Value, &'static str)> {
    TYPES.iter().find_map(|ext| {
        let dotted = format!(".{ext}");
        let of_type = files.iter().filter(|f| f["name"].as_str().is_some_and(|n| n.to_ascii_lowercase().ends_with(&dotted)));
        of_type.max_by_key(|f| size(f)).map(|f| (f, *ext))
    })
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = item(page.url).ok_or_else(|| fail("not an item page"))?;
    let meta = page.json(page.client.get(format!("https://archive.org/metadata/{id}"))).await?;
    let files = meta["files"].as_array().ok_or_else(|| fail("The Archive has no files for this item"))?;
    let (file, ext) = best(files).ok_or_else(|| fail("This Archive item has no video or audio"))?;
    let name = file["name"].as_str().unwrap_or_default();
    let mut url = Url::parse("https://archive.org/download/").map_err(|e| fail(e.to_string()))?;
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().push(id).extend(name.split('/'));
    }
    let length = file["length"].as_str().and_then(|l| l.parse::<f64>().ok().map(|s| s as u64).or_else(|| clock(l)));
    Ok(Found::new(url, meta["metadata"]["title"].as_str().map(str::to_string), length).ext(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_best_file_is_chosen() {
        let f = |n: &str, s: &str| json!({ "name": n, "size": s });
        let files = [f("a.ogv", "900"), f("b.mp4", "100"), f("c.mp4", "300"), f("d.jpg", "999")];
        assert_eq!(best(&files).map(|(f, e)| (f["name"].as_str().unwrap().to_string(), e)), Some(("c.mp4".into(), "mp4")));
        assert!(best(&[f("d.jpg", "1")]).is_none());
    }

    #[test]
    fn item_pages_are_recognised() {
        let id = |u: &str| item(&Url::parse(u).unwrap()).map(str::to_string);
        assert_eq!(id("http://archive.org/details/XD300-23").as_deref(), Some("XD300-23"));
        assert_eq!(id("https://archive.org/embed/XD300-23").as_deref(), Some("XD300-23"));
        assert_eq!(id("https://archive.org/search?q=a"), None);
    }
}
