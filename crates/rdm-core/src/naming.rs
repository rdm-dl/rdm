//! Choosing the local file name for a download.
//!
//! Order: server `Content-Disposition` (RFC 6266 / RFC 5987 `filename*`), then the last
//! URL path segment, then a `file=` / `name=` query parameter when the path looks like a
//! script, then a fallback. Names are sanitized for every OS, invisible bidirectional
//! control characters are removed (so `invoice\u{202E}fdp.exe` can't pose as a PDF), and
//! existing files are never overwritten silently.

use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};
use url::Url;

/// Extensions treated as "scripts" whose real file name is usually in the query string.
const SCRIPT_EXTS: &[&str] = &["php", "asp", "aspx", "cgi", "jsp", "pl", "py", "do", "action"];
const QUERY_NAME_KEYS: &[&str] = &["file", "filename", "name", "fn", "f"];

/// Parse a `Content-Disposition` header value and return the file name, if any.
pub fn from_content_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;
    for part in split_params(value) {
        let Some((key, val)) = part.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        match key.as_str() {
            "filename*" => extended = decode_ext_value(val),
            "filename" => plain = Some(unquote(val)),
            _ => {}
        }
    }
    extended.or(plain).filter(|s| !s.trim().is_empty())
}

/// Split on `;` while respecting double quotes.
fn split_params(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut esc) = (Vec::new(), String::new(), false, false);
    for ch in s.chars() {
        if esc {
            cur.push(ch);
            esc = false;
        } else if ch == '\\' && quoted {
            cur.push(ch);
            esc = true;
        } else if ch == '"' {
            quoted = !quoted;
            cur.push(ch);
        } else if ch == ';' && !quoted {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let inner = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    // Many servers send raw UTF-8 or percent-encoded names in the plain parameter.
    if out.contains('%') {
        if let Ok(d) = percent_decode_str(&out).decode_utf8() {
            return d.into_owned();
        }
    }
    out
}

/// RFC 5987 `charset'lang'percent-encoded`.
fn decode_ext_value(v: &str) -> Option<String> {
    let v = v.trim().trim_matches('"');
    let mut it = v.splitn(3, '\'');
    let charset = it.next()?.to_ascii_lowercase();
    let _lang = it.next()?;
    let data = it.next()?;
    let bytes: Vec<u8> = percent_decode_str(data).collect();
    match charset.as_str() {
        "utf-8" | "" => String::from_utf8(bytes).ok(),
        "iso-8859-1" | "latin1" => Some(bytes.iter().map(|&b| b as char).collect()),
        _ => String::from_utf8(bytes).ok(),
    }
}

/// Name derived from the URL: last path segment, or a query parameter for script URLs.
pub fn from_url(url: &Url) -> Option<String> {
    let segment = url
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_string))
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(&s).decode_utf8_lossy().into_owned());

    let looks_like_script = match &segment {
        None => true,
        Some(s) => match s.rsplit_once('.') {
            None => true,
            Some((_, ext)) => SCRIPT_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        },
    };
    if looks_like_script {
        for (k, v) in url.query_pairs() {
            if QUERY_NAME_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
                let candidate = v.rsplit(['/', '\\']).next().unwrap_or("").to_string();
                if candidate.contains('.') {
                    return Some(candidate);
                }
            }
        }
    }
    segment
}

/// Full decision: server name, then URL name, then a fallback using the host.
pub fn choose(url: &Url, content_disposition: Option<&str>, content_type: Option<&str>) -> String {
    let raw = content_disposition
        .and_then(from_content_disposition)
        .or_else(|| from_url(url))
        .unwrap_or_else(|| {
            let host = url.host_str().unwrap_or("download");
            if content_type.is_some_and(|t| t.starts_with("text/html")) {
                format!("{host}_index.html")
            } else {
                format!("{host}_download")
            }
        });
    let mut name = sanitize(&raw);
    if !name.contains('.') {
        if let Some(ext) = content_type.and_then(ext_for_mime) {
            name = format!("{name}.{ext}");
        }
    }
    name
}

/// Remove path separators, OS-reserved characters, control and bidi characters.
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out: String = base
        .chars()
        .filter(|c| !is_bidi_control(*c) && !c.is_control())
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    out = out.trim().trim_end_matches(['.', ' ']).to_string();
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    if out.is_empty() || out == "." || out == ".." {
        out = "download".to_string();
    }
    // Keep names within common filesystem limits (255 bytes).
    while out.len() > 240 {
        let cut = out.char_indices().nth_back(0).map(|(i, _)| i).unwrap_or(0);
        if let Some((stem, ext)) = out.rsplit_once('.') {
            if ext.len() < 16 && stem.len() > 1 {
                let mut s = stem.to_string();
                s.pop();
                out = format!("{s}.{ext}");
                continue;
            }
        }
        out.truncate(cut);
    }
    out
}

/// Unicode bidirectional formatting characters that can disguise a file extension.
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `name.ext` -> `name (1).ext`, `name (2).ext`, ... until the path is free.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded search")
}

/// A reasonable extension for common MIME types when the name has none.
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let m = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match m.as_str() {
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/x-7z-compressed" => "7z",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/pdf" => "pdf",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" => "msi",
        "application/x-iso9660-image" => "iso",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" => "wav",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "text/html" => "html",
        "text/plain" => "txt",
        "application/json" => "json",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn rfc5987_preferred_over_plain() {
        let cd = "attachment; filename=\"fallback.bin\"; filename*=UTF-8''r%C3%A9sum%C3%A9.bin";
        assert_eq!(from_content_disposition(cd).unwrap(), "résumé.bin");
    }

    #[test]
    fn plain_quoted_with_semicolon() {
        assert_eq!(from_content_disposition("attachment; filename=\"a;b.zip\"").unwrap(), "a;b.zip");
    }

    #[test]
    fn url_path_and_query() {
        assert_eq!(from_url(&u("http://h/files/archive.zip")).unwrap(), "archive.zip");
        assert_eq!(from_url(&u("http://h/get.php?file=report.pdf")).unwrap(), "report.pdf");
        assert_eq!(from_url(&u("http://h/dl?name=x%20y.iso")).unwrap(), "x y.iso");
    }

    #[test]
    fn bidi_and_reserved_removed() {
        assert_eq!(sanitize("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(sanitize("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
    }

    #[test]
    fn mime_extension_added() {
        assert_eq!(choose(&u("http://h/download"), None, Some("application/zip")), "download.zip");
    }

    #[test]
    fn unique_numbering() {
        let dir = std::env::temp_dir().join(format!("rdm_naming_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.bin"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "f.bin"), dir.join("f (1).bin"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
