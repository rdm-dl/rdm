//! Offline copies of websites for the site grabber: where each URL is saved, which
//! references a page needs (stylesheets, scripts, images), and rewriting links between
//! saved files into relative local links so the copy can be browsed without a network.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use url::Url;

/// One URL reference inside HTML: an attribute value's byte range.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrRef {
    pub tag: String,
    pub attr: String,
    pub start: usize,
    pub end: usize,
}

impl AttrRef {
    /// A resource the page needs to display (not a link to another page).
    pub fn is_resource(&self) -> bool {
        matches!(self.attr.as_str(), "src" | "data-src" | "poster") && !matches!(self.tag.as_str(), "iframe" | "frame")
            || (self.tag == "link" && self.attr == "href")
    }
}

const URL_ATTRS: &[&str] = &["href", "src", "data-src", "poster"];

/// Scan HTML tags for URL-valued attributes (href, src, data-src, poster). Script and style
/// contents and comments are skipped.
pub fn scan_attrs(html: &str) -> Vec<AttrRef> {
    let b = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if lower[i..].starts_with("<!--") {
            i = lower[i..].find("-->").map_or(b.len(), |e| i + e + 3);
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_alphanumeric() {
            j += 1;
        }
        if j == i + 1 {
            i += 1;
            continue;
        }
        let tag = lower[i + 1..j].to_string();
        // Attributes until '>'.
        let mut k = j;
        while k < b.len() && b[k] != b'>' {
            while k < b.len() && (b[k].is_ascii_whitespace() || b[k] == b'/') {
                k += 1;
            }
            let name_start = k;
            while k < b.len() && !b[k].is_ascii_whitespace() && b[k] != b'=' && b[k] != b'>' {
                k += 1;
            }
            let name = lower[name_start..k].to_string();
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < b.len() && b[k] == b'=' {
                k += 1;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                let (vs, ve);
                if k < b.len() && (b[k] == b'"' || b[k] == b'\'') {
                    let q = b[k];
                    vs = k + 1;
                    ve = html[vs..].find(q as char).map_or(b.len(), |e| vs + e);
                    k = (ve + 1).min(b.len());
                } else {
                    vs = k;
                    while k < b.len() && !b[k].is_ascii_whitespace() && b[k] != b'>' {
                        k += 1;
                    }
                    ve = k;
                }
                if URL_ATTRS.contains(&name.as_str()) && ve > vs {
                    out.push(AttrRef { tag: tag.clone(), attr: name, start: vs, end: ve });
                }
            } else if k == name_start {
                k += 1;
            }
        }
        i = k + 1;
        // Skip the body of script/style elements.
        if tag == "script" || tag == "style" {
            let close = format!("</{tag}");
            i = lower[i.min(lb.len())..].find(&close).map_or(b.len(), |e| i + e);
        }
    }
    out
}

/// `url(...)` references in CSS, as byte ranges of the URL text.
pub fn css_urls(css: &str) -> Vec<(usize, usize)> {
    let lower = css.to_ascii_lowercase();
    let mut out = vec![];
    let mut from = 0;
    while let Some(p) = lower[from..].find("url(") {
        let mut s = from + p + 4;
        let Some(close) = css[s..].find(')') else { break };
        let mut e = s + close;
        from = e + 1;
        while s < e && css.as_bytes()[s].is_ascii_whitespace() {
            s += 1;
        }
        while e > s && css.as_bytes()[e - 1].is_ascii_whitespace() {
            e -= 1;
        }
        if e > s && (css.as_bytes()[s] == b'"' || css.as_bytes()[s] == b'\'') {
            s += 1;
            e -= 1;
        }
        if e > s && !css[s..e].starts_with("data:") {
            out.push((s, e));
        }
    }
    // @import "file.css";
    let mut from = 0;
    while let Some(p) = lower[from..].find("@import") {
        let s0 = from + p + 7;
        from = s0;
        let rest = &css[s0..];
        let t = rest.trim_start();
        let off = s0 + (rest.len() - t.len());
        if let Some(q) = t.chars().next().filter(|c| *c == '"' || *c == '\'') {
            if let Some(e) = t[1..].find(q) {
                out.push((off + 1, off + 1 + e));
            }
        }
    }
    out
}

fn sanitize(seg: &str) -> String {
    let s: String = seg
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c })
        .collect();
    let s = s.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() || s == "." || s == ".." { "_".into() } else { s }
}

/// Where a URL is saved inside the mirror folder: `<root>/<host>/<path>`, with
/// `index.html` for folders, the query folded into the file name, and `.html` added to
/// pages whose names don't already say so.
pub fn local_path(root: &Path, url: &Url, is_html: bool) -> PathBuf {
    let mut p = root.to_path_buf();
    let host = match url.port() {
        Some(port) => format!("{}_{port}", url.host_str().unwrap_or("site")),
        None => url.host_str().unwrap_or("site").to_string(),
    };
    p.push(sanitize(&host));
    let path = url.path();
    let segs: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| sanitize(&percent_encoding::percent_decode_str(s).decode_utf8_lossy()))
        .collect();
    let dir_like = path.ends_with('/') || segs.is_empty();
    let (dirs, mut file) = if dir_like { (&segs[..], "index.html".to_string()) } else { (&segs[..segs.len() - 1], segs[segs.len() - 1].clone()) };
    for d in dirs {
        p.push(d);
    }
    if let Some(q) = url.query().filter(|q| !q.is_empty()) {
        let q: String = sanitize(q).chars().take(80).collect();
        file = match file.rsplit_once('.') {
            Some((stem, ext)) if !is_html || ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm") => format!("{stem}_{q}.{ext}"),
            _ => format!("{file}_{q}"),
        };
    }
    if is_html {
        let lower = file.to_ascii_lowercase();
        if !lower.ends_with(".html") && !lower.ends_with(".htm") {
            file.push_str(".html");
        }
    }
    p.push(file);
    p
}

/// Relative link from the folder containing `from_file` to `to_file`, with `/` separators
/// and characters that would break a URL encoded.
pub fn relative_link(from_file: &Path, to_file: &Path) -> String {
    let from: Vec<Component> = from_file.parent().map(|d| d.components().collect()).unwrap_or_default();
    let to: Vec<Component> = to_file.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), from.len() - common).collect();
    parts.extend(to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    const UNSAFE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ').add(b'#').add(b'?').add(b'%').add(b'"');
    percent_encoding::utf8_percent_encode(&parts.join("/"), UNSAFE).to_string()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

/// Resolve a reference against a page, ignoring non-web links.
pub fn resolve(base: &Url, raw: &str) -> Option<Url> {
    let v = html_unescape(raw.trim());
    if v.is_empty() || v.starts_with('#') || v.starts_with("javascript:") || v.starts_with("mailto:") || v.starts_with("data:") || v.starts_with("tel:") {
        return None;
    }
    let u = base.join(&v).ok()?;
    matches!(u.scheme(), "http" | "https").then_some(u)
}

fn key(u: &Url) -> String {
    let mut u = u.clone();
    u.set_fragment(None);
    u.to_string()
}

/// Rewrite references in a saved HTML page to the local copies in `saved` (URL without
/// fragment → local file). References to things that weren't saved become absolute URLs,
/// so they still work online.
pub fn rewrite_html(html: &str, page: &Url, page_file: &Path, saved: &HashMap<String, PathBuf>) -> String {
    let mut base = page.clone();
    let refs = scan_attrs(html);
    if let Some(b) = refs.iter().find(|r| r.tag == "base" && r.attr == "href").and_then(|r| resolve(page, &html[r.start..r.end])) {
        base = b;
    }
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    for r in &refs {
        if r.tag == "base" {
            // A <base href> would send the now-relative links back to the site.
            out.push_str(&html[last..r.start]);
            out.push_str("./");
            last = r.end;
            continue;
        }
        let raw = &html[r.start..r.end];
        let Some(u) = resolve(&base, raw) else { continue };
        let replacement = match saved.get(&key(&u)) {
            Some(local) => {
                let mut rel = relative_link(page_file, local);
                if let Some(f) = u.fragment() {
                    rel.push('#');
                    rel.push_str(f);
                }
                rel
            }
            None => u.to_string().replace('"', "%22"),
        };
        out.push_str(&html[last..r.start]);
        out.push_str(&replacement);
        last = r.end;
    }
    out.push_str(&html[last..]);
    out
}

/// Rewrite `url(...)` / `@import` references in a saved stylesheet.
pub fn rewrite_css(css: &str, sheet: &Url, sheet_file: &Path, saved: &HashMap<String, PathBuf>) -> String {
    let mut out = String::with_capacity(css.len());
    let mut last = 0;
    let mut refs = css_urls(css);
    refs.sort();
    for (s, e) in refs {
        if s < last {
            continue;
        }
        let Some(u) = resolve(sheet, &css[s..e]) else { continue };
        if let Some(local) = saved.get(&key(&u)) {
            out.push_str(&css[last..s]);
            out.push_str(&relative_link(sheet_file, local));
            last = e;
        }
    }
    out.push_str(&css[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_tags_and_kinds() {
        let html = r#"<!-- <a href="no.html"> --><a class=x href="p2.html">2</a><img src='i/a.png'>
            <link rel=stylesheet href=style.css><script src="app.js">var s = "<a href='fake.html'>";</script>
            <iframe src="frame.html"></iframe><video poster="v.jpg"></video>"#;
        let refs = scan_attrs(html);
        let vals: Vec<(&str, bool)> = refs.iter().map(|r| (&html[r.start..r.end], r.is_resource())).collect();
        assert_eq!(vals, [("p2.html", false), ("i/a.png", true), ("style.css", true), ("app.js", true), ("frame.html", false), ("v.jpg", true)]);
    }

    #[test]
    fn local_paths() {
        let root = Path::new("/m");
        let p = |u: &str, h| local_path(root, &Url::parse(u).unwrap(), h);
        assert_eq!(p("https://s.com/", true), Path::new("/m/s.com/index.html"));
        assert_eq!(p("https://s.com/a/b/", true), Path::new("/m/s.com/a/b/index.html"));
        assert_eq!(p("https://s.com/a/page.php?id=2", true), Path::new("/m/s.com/a/page.php_id=2.html"));
        assert_eq!(p("https://s.com/img/My%20Pic.png", false), Path::new("/m/s.com/img/My Pic.png"));
        assert_eq!(p("http://s.com:8080/x.html", true), Path::new("/m/s.com_8080/x.html"));
    }

    #[test]
    fn relative_links() {
        let a = Path::new("/m/s.com/a/b/index.html");
        assert_eq!(relative_link(a, Path::new("/m/s.com/a/b/c.html")), "c.html");
        assert_eq!(relative_link(a, Path::new("/m/s.com/img/x y.png")), "../../img/x%20y.png");
        assert_eq!(relative_link(a, Path::new("/m/other.com/z.css")), "../../../other.com/z.css");
    }

    #[test]
    fn rewrites_saved_links_and_absolutizes_the_rest() {
        let page = Url::parse("https://s.com/docs/index.html").unwrap();
        let root = Path::new("/m");
        let mut saved = HashMap::new();
        for (u, h) in [("https://s.com/docs/p2.html", true), ("https://s.com/img/a.png", false), ("https://s.com/docs/index.html", true)] {
            saved.insert(u.to_string(), local_path(root, &Url::parse(u).unwrap(), h));
        }
        let me = local_path(root, &page, true);
        let html = r#"<a href="p2.html#top">2</a><img src="/img/a.png"><a href="https://other.com/x">x</a><a href="missing.html">m</a>"#;
        let out = rewrite_html(html, &page, &me, &saved);
        assert_eq!(out, r#"<a href="p2.html#top">2</a><img src="../img/a.png"><a href="https://other.com/x">x</a><a href="https://s.com/docs/missing.html">m</a>"#);
        let css = "body{background:url( '../img/a.png' )} @import \"other.css\";";
        let sheet = Url::parse("https://s.com/css/site.css").unwrap();
        let out = rewrite_css(css, &sheet, &local_path(root, &sheet, false), &saved);
        assert_eq!(out, "body{background:url( '../img/a.png' )} @import \"other.css\";");
    }
}
