//! One TV network's sites: ID GO, TLC, Discovery, HGTV and others.
//!
//! Their player says it can use DRM, so the stream it receives is encrypted. The same service
//! gives an ordinary HLS stream to a player that says it can't, so that's what is asked for.

use super::{Episode, Family, Found, Page, SiteInfo, fail, path_parts};
use crate::download::DownloadError;
use reqwest::RequestBuilder;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, REFERER};
use serde_json::{Value, json};
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// Its name, domain, the service's name for it, and its API server.
const SITES: &[(&str, &str, &str, &str)] = &[
    ("ID GO", "investigationdiscovery.com", "ids", "us1-prod-direct.investigationdiscovery.com"),
    ("Discovery GO", "discovery.com", "dsc", "us1-prod-direct.go.discovery.com"),
    ("TLC GO", "tlc.com", "tlc", "us1-prod-direct.tlc.com"),
    ("Animal Planet GO", "animalplanet.com", "apl", "us1-prod-direct.animalplanet.com"),
    ("Science Channel GO", "sciencechannel.com", "sci", "us1-prod-direct.sciencechannel.com"),
    ("Discovery Life GO", "discoverylife.com", "dlf", "us1-prod-direct.discoverylife.com"),
    ("American Heroes Channel GO", "ahctv.com", "ahc", "us1-prod-direct.ahctv.com"),
    ("Destination America GO", "destinationamerica.com", "dam", "us1-prod-direct.destinationamerica.com"),
    ("HGTV GO", "hgtv.com", "hgtv", "us1-prod-direct.watch.hgtv.com"),
    ("Food Network GO", "foodnetwork.com", "food", "us1-prod-direct.watch.foodnetwork.com"),
    ("Cooking Channel GO", "cookingchanneltv.com", "cook", "us1-prod-direct.watch.cookingchanneltv.com"),
    ("Travel Channel GO", "travelchannel.com", "trav", "us1-prod-direct.watch.travelchannel.com"),
];
const CLIENT_VERSION: &str = "27.43.0";

pub fn family() -> Family {
    let sites = SITES.iter().map(|s| SiteInfo { name: s.0, domain: s.1, tested: s.1 == "investigationdiscovery.com" }).collect();
    Family { name: "Discovery", sites, browser_only: false }
}

fn site(url: &Url) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    let host = url.host_str()?;
    let host = ["www.", "go.", "watch."].iter().find_map(|p| host.strip_prefix(p)).unwrap_or(host);
    SITES.iter().find(|s| s.1 == host)
}

/// `show/episode` from `/video/show/episode`.
fn video_path(url: &Url) -> Option<String> {
    let mut parts = url.path_segments()?.filter(|s| !s.is_empty());
    (parts.next()? == "video").then_some(())?;
    Some(format!("{}/{}", parts.next()?, parts.next()?))
}

pub fn matches(url: &Url) -> bool {
    site(url).is_some() && video_path(url).is_some()
}

fn title(reply: &Value) -> Option<String> {
    let a = &reply["data"]["attributes"];
    let name = a["name"].as_str()?.trim();
    let included = reply["included"].as_array().into_iter().flatten();
    let show = included.filter(|i| i["type"] == "show").find_map(|i| i["attributes"]["name"].as_str());
    Some(match (show, a["seasonNumber"].as_u64(), a["episodeNumber"].as_u64()) {
        (Some(show), Some(s), Some(e)) => format!("{show} - S{s} E{e} {name}"),
        (Some(show), ..) => format!("{show} - {name}"),
        _ => name.to_string(),
    })
}

/// The first stream without DRM.
fn ordinary_stream(playback: &Value) -> Option<&str> {
    let list = playback["data"]["attributes"]["streaming"].as_array()?;
    let plain = list.iter().find(|s| s["type"] == "hls" && s["protection"]["drmEnabled"] != json!(true));
    plain?["url"].as_str().filter(|u| !u.is_empty())
}

/// Signed in, the browser's session; otherwise a guest one, which gets what a guest can watch.
async fn session(page: &Page<'_>, base: &str) -> Result<String> {
    if let Some(t) = page.cookie("st") {
        return Ok(t.to_string());
    }
    let device = format!("{:032x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_nanos()));
    let reply = page.json(page.client.get(format!("{base}token")).query(&[("realm", "go"), ("deviceId", &device)])).await?;
    reply["data"]["attributes"]["token"].as_str().map(str::to_string).ok_or_else(|| fail("the site gave no session"))
}

fn signed(r: RequestBuilder, page: &Page<'_>, token: &str, product: &str) -> RequestBuilder {
    r.header(AUTHORIZATION, format!("Bearer {token}"))
        .header(REFERER, page.url.as_str())
        .header("x-disco-params", format!("realm=go,siteLookupKey={product}"))
        .header("x-disco-client", format!("WEB:UNKNOWN:{product}:{CLIENT_VERSION}"))
}

/// The slug of a show's page: `/show/some-show`.
fn show_slug(url: &Url) -> Option<&str> {
    site(url)?;
    match path_parts(url).as_slice() {
        ["show" | "shows", slug] => Some(slug),
        _ => None,
    }
}

pub fn is_series(url: &Url) -> bool {
    show_slug(url).is_some()
}

/// The show's id and season numbers, from what the show page is built from.
fn seasons(route: &Value) -> Option<(String, Vec<String>)> {
    let parts = route["included"].as_array()?;
    let list = parts.iter().map(|p| &p["attributes"]["component"]).find(|c| c["filters"][0]["options"].is_array() && c["mandatoryParams"].is_string())?;
    let id = list["mandatoryParams"].as_str()?.rsplit('=').next()?.to_string();
    let options = list["filters"][0]["options"].as_array()?;
    Some((id, options.iter().filter_map(|o| o["id"].as_str().map(str::to_string).or(o["id"].as_u64().map(|n| n.to_string()))).collect()))
}

fn show_name(route: &Value) -> Option<String> {
    let parts = route["included"].as_array()?;
    parts.iter().find(|p| p["type"] == "show").and_then(|p| p["attributes"]["name"].as_str()).map(str::to_string)
}

/// Every episode of a show, season by season.
pub async fn episodes(page: &Page<'_>) -> Result<Vec<Episode>> {
    let (Some(&(_, _, product, server)), Some(slug)) = (site(page.url), show_slug(page.url)) else {
        return Err(fail("not a show page"));
    };
    let base = format!("https://{server}/");
    let token = session(page, &base).await?;
    let route = page.json(signed(page.client.get(format!("{base}cms/routes/show/{slug}")).query(&[("include", "default")]), page, &token, product)).await?;
    let (show_id, numbers) = seasons(&route).ok_or_else(|| fail("This show's seasons can't be read"))?;
    let show = show_name(&route).unwrap_or_else(|| slug.split('-').collect::<Vec<_>>().join(" "));
    let host = page.url.host_str().unwrap_or_default();
    let mut out = vec![];
    for season in numbers {
        let (mut number, mut pages) = (1u64, 1u64);
        while number <= pages {
            let query = [("sort", "episodeNumber"), ("filter[seasonNumber]", season.as_str()), ("filter[show.id]", show_id.as_str()), ("page[size]", "100"), ("page[number]", &number.to_string())];
            let reply = page.json(signed(page.client.get(format!("{base}content/videos")).query(&query), page, &token, product)).await?;
            pages = reply["meta"]["totalPages"].as_u64().unwrap_or(1);
            for video in reply["data"].as_array().into_iter().flatten() {
                let a = &video["attributes"];
                let (Some(path), Some(name)) = (a["path"].as_str(), a["name"].as_str()) else { continue };
                if a["videoType"].as_str().is_some_and(|t| t != "EPISODE") {
                    continue;
                }
                out.push(Episode {
                    url: format!("https://{host}/video/{path}"),
                    show: show.clone(),
                    title: name.trim().to_string(),
                    season: a["seasonNumber"].as_u64(),
                    number: a["episodeNumber"].as_u64(),
                    duration: a["videoDuration"].as_u64().map(|ms| ms / 1000),
                    ext: "mp4",
                });
            }
            number += 1;
        }
    }
    out.sort_by_key(|e| (e.season, e.number));
    Ok(out)
}

pub async fn find(page: &Page<'_>) -> Result<Found> {
    let (Some(&(_, _, product, server)), Some(path)) = (site(page.url), video_path(page.url)) else {
        return Err(DownloadError::Other("not a video page".into()));
    };
    let base = format!("https://{server}/");
    let token = session(page, &base).await?;
    let signed = |r: RequestBuilder| signed(r, page, &token, product);
    let query = [("include", "show"), ("fields[show]", "name"), ("fields[video]", "name,episodeNumber,seasonNumber,videoDuration")];
    let video = page.json(signed(page.client.get(format!("{base}content/videos/{path}")).query(&query))).await?;
    let id = video["data"]["id"].as_str().ok_or_else(|| DownloadError::Other("the site doesn't know this video".into()))?;
    let body = json!({ "deviceInfo": { "adBlocker": false, "drmSupported": false }, "videoId": id, "wisteriaProperties": {} });
    let request = page.client.post(format!("{base}playback/v3/videoPlaybackInfo")).header(CONTENT_TYPE, "application/json").body(body.to_string());
    let playback = page.json(signed(request)).await.map_err(|e| match e {
        DownloadError::Other(m) if m.contains("403") => DownloadError::Other("This video needs an account: sign in on the site in your browser, then try again.".into()),
        other => other,
    })?;
    let url = ordinary_stream(&playback).ok_or_else(|| DownloadError::Other("This site only offers this video with DRM, so it can't be saved.".into()))?;
    Ok(Found::new(url, title(&video), video["data"]["attributes"]["videoDuration"].as_u64().map(|ms| ms / 1000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pages_are_recognised() {
        let is = |u: &str| matches(&Url::parse(u).unwrap());
        assert!(is("https://www.investigationdiscovery.com/video/a-show/an-episode"));
        assert!(is("https://go.tlc.com/video/a-show/an-episode?x=1"));
        assert!(is("https://watch.hgtv.com/video/a-show/an-episode"));
        assert!(!is("https://www.investigationdiscovery.com/video/a-show"));
        assert!(!is("https://www.investigationdiscovery.com/shows/a-show/an-episode"));
        assert!(!is("https://www.example.com/video/a-show/an-episode"));
    }

    #[test]
    fn show_pages_and_their_seasons() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(is_series(&url("https://www.investigationdiscovery.com/show/body-cam-investigation-discovery")));
        assert!(!is_series(&url("https://www.investigationdiscovery.com/video/body-cam/an-episode")));
        assert!(!matches(&url("https://www.investigationdiscovery.com/show/body-cam-investigation-discovery")));
        let route = json!({ "included": [
            { "type": "show", "attributes": { "name": "Body Cam" } },
            { "attributes": { "component": { "mandatoryParams": "pf[show.id]=1234", "filters": [{ "options": [{ "id": "1" }, { "id": 2 }] }] } } },
        ] });
        assert_eq!(seasons(&route), Some(("1234".to_string(), vec!["1".to_string(), "2".to_string()])));
        assert_eq!(show_name(&route).as_deref(), Some("Body Cam"));
    }

    #[test]
    fn titles_read_like_the_site() {
        let reply = json!({
            "data": { "attributes": { "name": " Collecting Trophies ", "seasonNumber": 3, "episodeNumber": 1 } },
            "included": [{ "type": "image" }, { "type": "show", "attributes": { "name": "A Body in the Basement" } }],
        });
        assert_eq!(title(&reply).as_deref(), Some("A Body in the Basement - S3 E1 Collecting Trophies"));
        assert_eq!(title(&json!({ "data": { "attributes": { "name": "Film" } } })).as_deref(), Some("Film"));
    }

    #[test]
    fn only_a_stream_without_drm_counts() {
        let reply = |list: Value| json!({ "data": { "attributes": { "streaming": list } } });
        let plain = json!({ "type": "hls", "url": "https://cdn/m.m3u8", "protection": { "drmEnabled": false } });
        let drm = json!({ "type": "hls", "url": "https://cdn/x.m3u8", "protection": { "drmEnabled": true } });
        let dash = json!({ "type": "dash", "url": "https://cdn/m.mpd" });
        assert_eq!(ordinary_stream(&reply(json!([dash, drm, plain]))), Some("https://cdn/m.m3u8"));
        assert_eq!(ordinary_stream(&reply(json!([dash, drm]))), None);
    }
}
