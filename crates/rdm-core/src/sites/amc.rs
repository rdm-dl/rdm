//! AMC Networks: AMC, BBC America, IFC, Sundance TV and WE tv. Free videos are on Brightcove;
//! locked ones have no video id on the page because they're protected with DRM.

use super::{Family, Found, Page, SiteInfo, brightcove, fail, path_parts, plain_host};
use crate::download::DownloadError;
use regex::Regex;
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

pub fn family() -> Family {
    let site = |name, domain, tested| SiteInfo { name, domain, tested };
    Family {
        name: "AMC Networks",
        sites: vec![site("AMC", "amc.com", true), site("BBC America", "bbcamerica.com", false), site("IFC", "ifc.com", false), site("Sundance TV", "sundancetv.com", false), site("WE tv", "wetv.com", false)],
        browser_only: false,
    }
}

pub fn matches(url: &Url) -> bool {
    let parts = path_parts(url);
    matches!(plain_host(url, &["www."]), "amc.com" | "bbcamerica.com" | "ifc.com" | "sundancetv.com" | "wetv.com") && (matches!(parts.first(), Some(&"movies")) && parts.len() >= 2 || matches!(parts.first(), Some(&"shows")) && parts.len() >= 3)
}

/// The data a page keeps in `window.initialData = JSON.parse(String.raw`...`)`.
fn page_data(html: &str) -> Option<Value> {
    let found = Regex::new(r"(?s)window\.initialData\s*=\s*JSON\.parse\(String\.raw`(.*?)`\)").ok()?.captures(html)?;
    serde_json::from_str(&found[1]).ok()
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let html = page.text(page.client.get(page.url.as_str())).await?;
    let data = page_data(&html).ok_or_else(|| fail("This page's data can't be read"))?;
    let video = data["initialData"]["properties"]["videoId"].as_str().ok_or_else(|| fail("This video is protected with DRM, so it can't be saved."))?;
    let brightcove = &data["config"]["brightcove"];
    let text = |v: &Value| v.as_str().map(str::to_string).or(v.as_u64().map(|n| n.to_string()));
    let (Some(account), Some(player)) = (text(&brightcove["accountId"]), text(&brightcove["playerId"])) else {
        return Err(fail("This page's data can't be read"));
    };
    brightcove::find(page, &account, &player, video).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_and_their_data() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://www.amc.com/shows/dark-winds/videos/dark-winds-a-look-at-season-3--1072027"));
        assert!(is("https://www.ifc.com/movies/a-film"));
        assert!(!is("https://www.amc.com/shows/dark-winds"));
        let html = "<script>window.initialData = JSON.parse(String.raw`{\"initialData\":{\"properties\":{\"videoId\":\"6\"}}}`);</script>";
        assert_eq!(page_data(html).unwrap()["initialData"]["properties"]["videoId"], "6");
    }
}
