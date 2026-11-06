//! Video sites that need their own code. Most sites need none: the stream the browser plays
//! is the one to download. These are for sites where it isn't, and for pasting a page's link.
//!
//! Each family of sites (one service behind one or several sites) is one file here with:
//! - `family()`: its name and sites, for the Options window and the browser extension,
//! - `matches(url)`: whether this is a page of a video,
//! - `find(page)`: the stream to download.
//!
//! To add a family, add its file and put its name in the `families!` list below. The result is
//! an ordinary address, so HLS, DASH and joining work as for any other download.

use crate::config::EngineConfig;
use crate::download::{DownloadError, DownloadRequest, build_client};
use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

/// Marks a stream that came from a site's code. Its address can look like a page of that same
/// site (`i.imgur.com/x.mp4`), and asking again would never end.
const RESOLVED: &str = "rdm-site=done";

/// A site in front of a check that only a person in a browser can pass.
const BROWSER_CHECK: &str = "This site is asking for a browser check right now. Play the video in your browser and download it from there.";

macro_rules! families {
    ($($name:ident),* $(,)?) => {
        $(mod $name;)*

        /// Every family with its own code here.
        pub fn families() -> Vec<Family> {
            vec![$($name::family()),*]
        }

        /// Whether this address is a video page of a site with its own code.
        pub fn matches(url: &str) -> bool {
            if url.contains(RESOLVED) {
                return false;
            }
            let Ok(url) = Url::parse(url) else { return false };
            false $(|| $name::matches(&url))*
        }

        /// Ask the site for the stream of a page.
        pub async fn find(url: &str, cookies: Option<&str>, client: &Client) -> Result<Found> {
            let url = Url::parse(url).map_err(|e| fail(format!("bad url: {e}")))?;
            let page = Page { url: &url, cookies, client };
            $(if $name::matches(&url) {
                return $name::find(&page).await;
            })*
            Err(fail("RDM has no code for this site"))
        }
    };
}

mod amp;
mod brightcove;
/// The families that can list a whole show, channel, album or playlist. Each has `is_series(url)`
/// and `episodes(page)`.
macro_rules! lists {
    ($($name:ident),* $(,)?) => {
        /// Whether this is a page with a list of videos to read.
        pub fn is_series(url: &str) -> bool {
            Url::parse(url).is_ok_and(|u| false $(|| $name::is_series(&u))*)
        }

        /// Every video of a show's, channel's or album's page.
        pub async fn episodes(url: &str, cookies: Option<String>, cfg: &EngineConfig) -> Result<Vec<Episode>> {
            let parsed = Url::parse(url).map_err(|e| fail(format!("bad url: {e}")))?;
            let cookies = cookies.or_else(|| sign_in_for(url));
            let req = DownloadRequest { url: url.to_string(), cookies, ..Default::default() };
            let client = build_client(cfg, &req)?;
            let page = Page { url: &parsed, cookies: req.cookies.as_deref(), client: &client };
            $(if $name::is_series(&parsed) {
                return $name::episodes(&page).await;
            })*
            Err(fail("RDM can't list this page"))
        }
    };
}

lists!(discovery, twitch, soundcloud, bandcamp, rutube, odysee, ted, dailymotion, imgur, aenetworks, ytlists, rumble, kick, peertube, applepodcasts, spreaker, acast, tubi, bluesky, audius, wikimedia, telegram, steam, iheart, libsyn);

families!(
    // Batch 1
    discovery, dailymotion, twitch, soundcloud, streamable, odysee, archive, bitchute, bandcamp, ted, rutube,
    // Batch 2
    imgur, coub, loom, wistia, vidyard, pinterest, medal, nbcnews, abcnews, foxnews,
    // Batch 3
    nbcu, amc, aenetworks, ytlists,
    // Batch 4
    rumble, kick, peertube, flickr, applepodcasts, spreaker, acast, jwplayer, dropbox, googledrive,
    // Batch 5
    tubi, cnn, bluesky, audius, wikimedia, telegram, okru, steam, iheart, libsyn,
);

/// What a site offers for a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub url: String,
    pub title: Option<String>,
    /// In seconds.
    pub duration: Option<u64>,
    /// File extension of the result.
    pub ext: String,
    /// Some stream servers want to see where the request came from.
    pub referer: Option<String>,
    /// Some servers refuse anything that isn't named the way their rules ask.
    pub user_agent: Option<String>,
}

impl Found {
    pub fn new(url: impl Into<String>, title: Option<String>, duration: Option<u64>) -> Self {
        Self { url: url.into(), title, duration, ext: "mp4".into(), referer: None, user_agent: None }
    }

    pub fn ext(mut self, ext: &str) -> Self {
        self.ext = ext.to_string();
        self
    }

    /// A slash in a title (`AC/DC`) would otherwise read as a folder and cut the name short.
    pub fn file_name(&self) -> Option<String> {
        self.title.as_ref().map(|t| format!("{}.{}", t.replace(['/', '\\'], "-"), self.ext))
    }
}

/// One episode of a show page.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    /// The episode's own page, which downloads like any other.
    pub url: String,
    pub show: String,
    pub title: String,
    pub season: Option<u64>,
    pub number: Option<u64>,
    /// In seconds.
    pub duration: Option<u64>,
    /// File extension of the result.
    pub ext: &'static str,
}

impl Episode {
    /// An item of a list with no seasons: a clip, or an album's track when it has a number.
    pub fn item(url: impl Into<String>, list: &str, title: &str, number: Option<u64>, duration: Option<u64>, ext: &'static str) -> Self {
        Self { url: url.into(), show: list.to_string(), title: title.trim().to_string(), season: None, number, duration, ext }
    }

    /// The folder under the download folder: `Show/Season 2`.
    pub fn folder(&self) -> std::path::PathBuf {
        let clean = |t: &str| crate::naming::sanitize(&t.replace(['/', '\\'], "-"));
        let mut dir = std::path::PathBuf::from(clean(&self.show));
        if let Some(s) = self.season {
            dir.push(format!("Season {s}"));
        }
        dir
    }

    /// `Show - S2 E5 Title.mp4`
    pub fn file_name(&self) -> String {
        let name = match (self.season, self.number) {
            (Some(s), Some(n)) => format!("{} - S{s} E{n:02} {}", self.show, self.title),
            (None, Some(n)) => format!("{n:02} - {}", self.title),
            _ => self.title.clone(),
        };
        format!("{name}.{}", self.ext).replace(['/', '\\'], "-")
    }
}

/// One site of a family.
#[derive(Debug, Clone, PartialEq)]
pub struct SiteInfo {
    pub name: &'static str,
    pub domain: &'static str,
    /// Tried on the real site, not just written.
    pub tested: bool,
}

/// One entry in the Options window: a service and the sites it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    pub name: &'static str,
    pub sites: Vec<SiteInfo>,
    /// Works only by playing the video in the browser: RDM has nothing to look up for a page.
    pub browser_only: bool,
}

impl Family {
    /// A family of one site.
    pub fn single(name: &'static str, domain: &'static str, tested: bool) -> Self {
        Self { name, sites: vec![SiteInfo { name, domain, tested }], browser_only: false }
    }
}

/// The page being asked about, and how to reach the site.
pub struct Page<'a> {
    pub url: &'a Url,
    pub cookies: Option<&'a str>,
    pub client: &'a Client,
}

impl Page<'_> {
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies?.split(';').filter_map(|c| c.trim().split_once('=')).find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    /// Send a request and read the reply as text.
    pub async fn text(&self, request: RequestBuilder) -> Result<String> {
        let reply = request.send().await.map_err(|e| fail(e.to_string()))?;
        let status = reply.status();
        if reply.headers().contains_key("cf-mitigated") {
            return Err(fail(BROWSER_CHECK));
        }
        if !status.is_success() {
            return Err(fail(format!("the site answered {status}")));
        }
        reply.text().await.map_err(|e| fail(e.to_string()))
    }

    /// Send a request and read the reply as JSON.
    pub async fn json(&self, request: RequestBuilder) -> Result<Value> {
        let text = self.text(request).await?;
        serde_json::from_str(&text).map_err(|e| fail(format!("the site's answer can't be read: {e}")))
    }
}

/// Text from a page with its character codes turned back: `&amp;`, `&#x27;`.
pub fn unescape(text: &str) -> String {
    let Ok(codes) = regex::Regex::new(r"&(#[xX][0-9a-fA-F]+|#[0-9]+|amp|lt|gt|quot|apos|nbsp);") else { return text.to_string() };
    let code = |found: &regex::Captures| match &found[1] {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        number => {
            let digits = number.trim_start_matches('#');
            let value = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse().ok(),
            };
            value.and_then(char::from_u32).map_or_else(|| found[0].to_string(), String::from)
        }
    };
    codes.replace_all(text, code).into_owned()
}

/// The whole tag that has `mark` in it. Attribute values can't hold a raw `>` on the pages this
/// reads: they write it as a character code.
pub fn tag_around<'a>(html: &'a str, mark: &str) -> Option<&'a str> {
    let at = html.find(mark)?;
    let start = html[..at].rfind('<')?;
    let end = at + html[at..].find('>')?;
    Some(&html[start..=end])
}

/// The value of `name="value"` in a tag, with its character codes turned back.
pub fn attribute(tag: &str, name: &str) -> Option<String> {
    let found = regex::Regex::new(&format!(r#"\b{name}="([^"]*)""#)).ok()?.captures(tag)?;
    Some(unescape(&found[1]))
}

pub fn fail(message: impl Into<String>) -> DownloadError {
    DownloadError::Other(message.into())
}

/// The path of an address without its empty parts.
pub fn path_parts(url: &Url) -> Vec<&str> {
    url.path_segments().map(|p| p.filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

/// The host without a `www.`-like start.
pub fn plain_host<'a>(url: &'a Url, starts: &[&str]) -> &'a str {
    let host = url.host_str().unwrap_or_default();
    starts.iter().find_map(|s| host.strip_prefix(s)).unwrap_or(host)
}

/// Seconds from `1:02:03` or `02:03`.
pub fn clock(text: &str) -> Option<u64> {
    text.trim().split(':').try_fold(0u64, |total, part| Some(total * 60 + part.parse::<u64>().ok()?))
}

/// The domains where only the browser's player can open a stream, so the extension plays a
/// show's episodes itself.
pub fn browser_hosts() -> Vec<&'static str> {
    families().iter().filter(|f| f.browser_only).flat_map(|f| f.sites.iter().map(|s| s.domain)).collect()
}

/// Whether a page's list has to be started from the browser.
pub fn needs_browser(url: &str) -> bool {
    Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).is_some_and(|host| browser_hosts().iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))))
}

/// The domains with their own code here.
pub fn hosts() -> Vec<&'static str> {
    families().iter().filter(|f| !f.browser_only).flat_map(|f| f.sites.iter().map(|s| s.domain)).collect()
}

/// Sign-ins by site. A show is added from RDM's own window, where there's no browser to ask, so
/// the extension hands over the one sign-in cookie of each site that has code here. It's kept
/// across restarts in the data folder (the browser may not speak up again for a while) and only
/// ever sent to that same site.
static SIGN_INS: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> = std::sync::Mutex::new(None);
const SIGN_IN_FILE: &str = "site-sign-ins.json";

/// The sites whose sign-in RDM can use, and the cookie that holds it.
pub fn sign_in_cookies() -> Vec<(&'static str, &'static str)> {
    discovery::family().sites.iter().map(|s| (s.domain, "st")).collect()
}

fn site_of(url: &str) -> Option<&'static str> {
    let host = Url::parse(url).ok()?.host_str()?.to_string();
    sign_in_cookies().into_iter().map(|(d, _)| d).find(|d| host == *d || host.ends_with(&format!(".{d}")))
}

fn with_sign_ins<T>(use_them: impl FnOnce(&mut std::collections::HashMap<String, String>) -> T) -> Option<T> {
    let mut slot = SIGN_INS.lock().ok()?;
    let all = slot.get_or_insert_with(|| {
        let saved = if cfg!(test) { None } else { std::fs::read(crate::settings::data_dir().join(SIGN_IN_FILE)).ok() };
        saved.and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    });
    Some(use_them(all))
}

/// Keep what the browser sent for a site, as long as it's a site with a sign-in here.
pub fn remember_sign_in(domain: &str, cookie: &str) {
    if cookie.is_empty() || !sign_in_cookies().iter().any(|(d, name)| *d == domain && cookie.starts_with(&format!("{name}="))) {
        return;
    }
    let changed = with_sign_ins(|all| {
        let changed = all.get(domain).map(String::as_str) != Some(cookie);
        all.insert(domain.to_string(), cookie.to_string());
        changed.then(|| serde_json::to_vec(all).unwrap_or_default())
    });
    if let (Some(Some(bytes)), false) = (changed, cfg!(test)) {
        let _ = std::fs::write(crate::settings::data_dir().join(SIGN_IN_FILE), bytes);
    }
}

/// The browser's sign-in for this site, if the extension has handed one over.
pub fn sign_in_for(url: &str) -> Option<String> {
    let site = site_of(url)?;
    with_sign_ins(|all| all.get(site).cloned()).flatten()
}

/// The same as [`find`], for callers that only have the page and its cookies.
pub async fn find_for_page(url: &str, cookies: Option<String>, cfg: &EngineConfig) -> Result<Found> {
    let req = DownloadRequest { url: url.to_string(), cookies, ..Default::default() };
    find(url, req.cookies.as_deref(), &build_client(cfg, &req)?).await
}

/// The download a page turns into. The site's cookies stay behind: the stream is on other servers.
pub fn stream_request(req: &DownloadRequest, found: &Found) -> DownloadRequest {
    let mut out = req.clone();
    // Hints on the page's address (a quality) join any the stream carries (a separate audio file).
    out.url = match req.url.split_once('#') {
        Some((_, hints)) if found.url.contains('#') => format!("{}&{hints}", found.url),
        Some((_, hints)) => format!("{}#{hints}", found.url),
        None => found.url.clone(),
    };
    out.url.push(if out.url.contains('#') { '&' } else { '#' });
    out.url.push_str(RESOLVED);
    out.cookies = None;
    out.referer = found.referer.clone();
    out.user_agent = found.user_agent.clone().or(out.user_agent);
    if out.file_name.is_none() {
        out.file_name = found.file_name();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_becomes_a_stream_request_without_its_cookies() {
        let req = DownloadRequest { url: "https://site.example/video/a/b#rdm-quality=720".into(), cookies: Some("st=secret".into()), referer: Some("https://site.example/".into()), ..Default::default() };
        let found = Found::new("https://cdn.example/master.m3u8?sig=1", Some("Show".into()), None);
        let out = stream_request(&req, &found);
        assert_eq!(out.url, "https://cdn.example/master.m3u8?sig=1#rdm-quality=720&rdm-site=done");
        assert_eq!(out.cookies, None);
        assert_eq!(out.referer, None);
        assert_eq!(out.file_name.as_deref(), Some("Show.mp4"));
    }

    #[test]
    fn a_stream_is_never_looked_up_again() {
        let page = "https://imgur.com/A61SaA1";
        assert!(matches(page));
        let req = DownloadRequest { url: page.into(), ..Default::default() };
        // Imgur's file looks like one of its pages.
        let found = Found::new("https://i.imgur.com/A61SaA1.mp4", Some("Clip".into()), None);
        assert!(matches(&found.url));
        assert!(!matches(&stream_request(&req, &found).url));
    }

    #[test]
    fn slashes_in_titles_stay_in_the_name() {
        let name = Found::new("https://x/a.mp3", Some("AC/DC - Thunder".into()), None).ext("mp3").file_name().unwrap();
        assert_eq!(crate::naming::sanitize(&name), "AC-DC - Thunder.mp3");
    }

    #[test]
    fn every_family_lists_named_sites() {
        for f in families() {
            assert!(!f.name.is_empty() && !f.sites.is_empty());
            assert!(f.sites.iter().all(|s| !s.name.is_empty() && !s.domain.is_empty()));
        }
        assert!(hosts().contains(&"investigationdiscovery.com"));
    }

    #[test]
    fn episodes_get_a_tidy_place() {
        let e = Episode { url: "https://x/video/a/b".into(), show: "Body Cam".into(), title: "Shots/Fired".into(), season: Some(2), number: Some(5), duration: None, ext: "mp4" };
        assert_eq!(e.folder(), std::path::Path::new("Body Cam").join("Season 2"));
        assert_eq!(e.file_name(), "Body Cam - S2 E05 Shots-Fired.mp4");
        let track = Episode::item("https://x/t", "An Album", " A Song ", Some(3), None, "mp3");
        assert_eq!(track.folder(), std::path::Path::new("An Album"));
        assert_eq!(track.file_name(), "03 - A Song.mp3");
        assert_eq!(Episode::item("https://x/c", "A Channel", "A Clip", None, None, "mp4").file_name(), "A Clip.mp4");
    }

    #[test]
    fn a_sign_in_is_kept_for_its_own_site_only() {
        remember_sign_in("investigationdiscovery.com", "st=abc");
        assert_eq!(sign_in_for("https://www.investigationdiscovery.com/show/x").as_deref(), Some("st=abc"));
        assert_eq!(sign_in_for("https://www.tlc.com/video/a/b"), None);
        assert_eq!(sign_in_for("https://example.com/"), None);
        // Nothing but the site's own sign-in cookie, for a site that has one.
        remember_sign_in("example.com", "st=abc");
        remember_sign_in("tlc.com", "other=abc");
        assert_eq!(sign_in_for("https://www.tlc.com/video/a/b"), None);
    }

    #[test]
    fn cookies_are_read_by_name() {
        let url = Url::parse("https://x.example/").unwrap();
        let client = Client::new();
        let page = Page { url: &url, cookies: Some("a=1; st=tok=en; b=2"), client: &client };
        assert_eq!(page.cookie("st"), Some("tok=en"));
        assert_eq!(page.cookie("nope"), None);
    }

    #[test]
    fn clock_times_become_seconds() {
        assert_eq!(clock("00:16"), Some(16));
        assert_eq!(clock("1:02:03"), Some(3723));
        assert_eq!(clock("soon"), None);
    }
}
