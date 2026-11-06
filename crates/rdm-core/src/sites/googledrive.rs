//! Google Drive shared files: the download address gives the original file.

use super::{Family, Found, Page, fail, path_parts};
use crate::download::DownloadError;
use reqwest::header;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    Family::single("Google Drive", "drive.google.com", true)
}

fn file_id(url: &Url) -> Option<String> {
    if !matches!(url.host_str()?, "drive.google.com" | "docs.google.com") {
        return None;
    }
    let id = match path_parts(url).as_slice() {
        ["file", "d", id, ..] => Some(id.to_string()),
        ["open" | "uc"] => url.query_pairs().find(|(k, _)| k == "id").map(|(_, v)| v.into_owned()),
        _ => None,
    };
    id.filter(|id| id.len() >= 25)
}

pub fn matches(url: &Url) -> bool {
    file_id(url).is_some()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let id = file_id(page.url).ok_or_else(|| fail("not a file page"))?;
    let address = format!("https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t");
    let reply = page.client.get(&address).header(header::RANGE, "bytes=0-0").send().await.map_err(|e| fail(e.to_string()))?;
    // Google answers with a page instead of the file when it's private or over its daily limit.
    if !reply.headers().contains_key(header::CONTENT_DISPOSITION) {
        return Err(fail("Google Drive isn't offering this file now; it may be private or have reached its download limit."));
    }
    Ok(Found::new(address, None, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_pages_are_recognised() {
        let id = |u: &str| file_id(&Url::parse(u).unwrap());
        let long = "0ByeS4oOUV-49Zzh4R1J6R09zazQ";
        assert_eq!(id(&format!("https://drive.google.com/file/d/{long}/view")).as_deref(), Some(long));
        assert_eq!(id(&format!("https://docs.google.com/file/d/{long}/edit?pli=1")).as_deref(), Some(long));
        assert_eq!(id(&format!("https://drive.google.com/open?id={long}")).as_deref(), Some(long));
        assert_eq!(id("https://drive.google.com/drive/my-drive"), None);
    }
}
