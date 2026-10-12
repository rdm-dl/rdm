//! Site grabber: crawl a website and collect links to files matching chosen types.

use crate::settings::wildcard_match;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrabberProject {
    pub start_url: String,
    /// How many link levels to follow from the start page (0 = start page only).
    pub depth: u32,
    /// Only explore pages on the start page's host.
    pub same_host: bool,
    /// Only explore pages under the start URL's path.
    pub same_path: bool,
    /// File patterns to collect, e.g. `*.jpg`, `*.zip`.
    pub include: Vec<String>,
    /// URL patterns to skip entirely.
    pub exclude: Vec<String>,
    pub max_pages: usize,
    pub cookies: Option<String>,
    pub user_agent: String,
    pub respect_robots: bool,
}

impl Default for GrabberProject {
    fn default() -> Self {
        Self {
            start_url: String::new(),
            depth: 2,
            same_host: true,
            same_path: false,
            include: preset("images"),
            exclude: vec![],
            max_pages: 500,
            cookies: None,
            user_agent: format!("Mozilla/5.0 (compatible; RDM-Grabber/{})", env!("CARGO_PKG_VERSION")),
            respect_robots: true,
        }
    }
}

/// Ready-made file patterns.
pub fn preset(name: &str) -> Vec<String> {
    let exts = match name {
        "images" => "jpg jpeg png gif webp bmp svg tif tiff avif",
        "video" => "mp4 mkv webm avi mov wmv flv m4v mpg mpeg",
        "audio" => "mp3 m4a aac flac wav ogg opus wma",
        "documents" => "pdf doc docx xls xlsx ppt pptx txt rtf odt epub csv",
        "archives" => "zip rar 7z gz tar bz2 xz iso",
        "programs" => "exe msi apk dmg pkg deb rpm appimage",
        _ => "",
    };
    exts.split_whitespace().map(|e| format!("*.{e}")).collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FoundFile {
    pub url: String,
    pub referer: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct GrabberStatus {
    pub pages_explored: usize,
    pub pages_queued: usize,
    pub files: Vec<FoundFile>,
    pub current: String,
    pub done: bool,
    pub error: Option<String>,
}

/// Extract href/src values from HTML without a full parser.
pub fn extract_links(html: &str, base: &Url) -> Vec<Url> {
    let mut out = vec![];
    let lower = html.to_ascii_lowercase();
    let mut base = base.clone();
    if let Some(i) = lower.find("<base ") {
        if let Some(href) = attr_value(&html[i..], &lower[i..], "href") {
            if let Ok(b) = base.join(&href) {
                base = b;
            }
        }
    }
    for key in ["href", "src", "data-src", "data-original"] {
        let pat = format!("{key}=");
        let mut from = 0;
        while let Some(pos) = lower[from..].find(&pat) {
            let at = from + pos;
            // Must be preceded by whitespace (an attribute, not part of another word).
            let ok = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
            from = at + pat.len();
            if !ok {
                continue;
            }
            if let Some(v) = read_value(&html[from..]) {
                let v = html_unescape(v.trim());
                if v.is_empty() || v.starts_with('#') || v.starts_with("javascript:") || v.starts_with("mailto:") || v.starts_with("data:") {
                    continue;
                }
                if let Ok(mut u) = base.join(&v) {
                    u.set_fragment(None);
                    if matches!(u.scheme(), "http" | "https" | "ftp") {
                        out.push(u);
                    }
                }
            }
        }
    }
    out
}

fn attr_value(orig: &str, lower: &str, name: &str) -> Option<String> {
    let end = lower.find('>')?;
    let p = lower[..end].find(&format!("{name}="))?;
    read_value(&orig[p + name.len() + 1..]).map(str::to_string)
}

fn read_value(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let q = s.chars().next()?;
    if q == '"' || q == '\'' {
        let rest = &s[1..];
        rest.find(q).map(|e| &rest[..e])
    } else {
        let e = s.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(s.len());
        Some(&s[..e])
    }
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

fn matches_any(patterns: &[String], url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    let last = path.rsplit('/').next().unwrap_or("");
    patterns.iter().any(|p| wildcard_match(p, last) || wildcard_match(p, url.as_str()))
}

/// Parse `Disallow:` rules for `User-agent: *`.
pub fn robots_disallows(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut applies = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(v) = line.strip_prefix("User-agent:").or_else(|| line.strip_prefix("user-agent:")) {
            applies = v.trim() == "*";
        } else if applies {
            if let Some(v) = line.strip_prefix("Disallow:").or_else(|| line.strip_prefix("disallow:")) {
                let v = v.trim();
                if !v.is_empty() {
                    out.push(v.to_string());
                }
            }
        }
    }
    out
}

/// Crawl the site, publishing progress to `status`. Stops when `cancel` becomes true.
pub async fn run(project: GrabberProject, status: watch::Sender<GrabberStatus>, cancel: watch::Receiver<bool>) {
    let fail = |e: String| {
        status.send_modify(|s| {
            s.error = Some(e);
            s.done = true;
        })
    };
    let Ok(start) = Url::parse(&project.start_url) else {
        return fail("invalid start address".into());
    };
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(c) = &project.cookies {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(c) {
            headers.insert(reqwest::header::COOKIE, v);
        }
    }
    let client = match reqwest::Client::builder()
        .user_agent(project.user_agent.clone())
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail(e.to_string()),
    };
    let disallow = if project.respect_robots {
        match start.join("/robots.txt") {
            Ok(r) => match client.get(r).send().await {
                Ok(resp) if resp.status().is_success() => robots_disallows(&resp.text().await.unwrap_or_default()),
                _ => vec![],
            },
            Err(_) => vec![],
        }
    } else {
        vec![]
    };
    let allowed = |u: &Url| -> bool {
        if matches_any(&project.exclude, u) {
            return false;
        }
        if project.same_host && u.host_str() != start.host_str() {
            return false;
        }
        if project.same_path {
            let dir = start.path().rsplit_once('/').map_or("", |(d, _)| d);
            if !u.path().starts_with(dir) {
                return false;
            }
        }
        !(u.host_str() == start.host_str() && disallow.iter().any(|d| u.path().starts_with(d.as_str())))
    };

    let seen = Arc::new(Mutex::new(HashSet::<String>::new()));
    let mut queue: VecDeque<(Url, u32)> = VecDeque::from([(start.clone(), 0)]);
    seen.lock().unwrap().insert(start.to_string());
    let mut files_seen = HashSet::new();
    let mut explored = 0;

    while let Some((page, level)) = queue.pop_front() {
        if *cancel.borrow() || explored >= project.max_pages {
            break;
        }
        status.send_modify(|s| {
            s.current = page.to_string();
            s.pages_queued = queue.len();
        });
        let resp = match client.get(page.clone()).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("html"));
        if !is_html {
            continue;
        }
        let final_url = resp.url().clone();
        let Ok(body) = resp.text().await else { continue };
        explored += 1;
        for link in extract_links(&body, &final_url) {
            if matches_any(&project.include, &link) {
                if files_seen.insert(link.to_string()) {
                    let f = FoundFile { url: link.to_string(), referer: final_url.to_string(), size: None };
                    status.send_modify(|s| s.files.push(f));
                }
                continue;
            }
            if level < project.depth && allowed(&link) && seen.lock().unwrap().insert(link.to_string()) {
                queue.push_back((link, level + 1));
            }
        }
        status.send_modify(|s| {
            s.pages_explored = explored;
            s.pages_queued = queue.len();
        });
    }
    status.send_modify(|s| {
        s.done = true;
        s.current.clear();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_extracted_and_resolved() {
        let base = Url::parse("https://site.com/gallery/index.html").unwrap();
        let html = r#"<a href="page2.html">2</a> <img src='/img/a.JPG'> <a href=../dl/file.zip#x>z</a>
            <a href="javascript:void(0)">no</a> <a class="x" href="https://other.com/b.png">o</a> <p data-src="c.webp"></p>"#;
        let l: Vec<String> = extract_links(html, &base).into_iter().map(|u| u.to_string()).collect();
        assert!(l.contains(&"https://site.com/gallery/page2.html".to_string()));
        assert!(l.contains(&"https://site.com/img/a.JPG".to_string()));
        assert!(l.contains(&"https://site.com/dl/file.zip".to_string()));
        assert!(l.contains(&"https://other.com/b.png".to_string()));
        assert!(l.contains(&"https://site.com/gallery/c.webp".to_string()));
        assert!(!l.iter().any(|x| x.contains("javascript")));
    }

    #[test]
    fn presets_match_case_insensitively() {
        let p = preset("images");
        assert!(matches_any(&p, &Url::parse("https://a/b/PHOTO.JPG").unwrap()));
        assert!(!matches_any(&p, &Url::parse("https://a/b/page.html").unwrap()));
    }

    #[test]
    fn robots() {
        let r = "User-agent: Googlebot\nDisallow: /g\n\nUser-agent: *\nDisallow: /private\nDisallow:\n";
        assert_eq!(robots_disallows(r), vec!["/private".to_string()]);
    }
}
