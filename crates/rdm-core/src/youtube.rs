//! YouTube videos, the way the reference download manager handles them:
//!
//! 1. The browser extension hands over the page config (visitor data, player script URL,
//!    client version, login state) and the session's proof-of-origin token taken from the
//!    player's own streaming request.
//! 2. The player script is fetched (cached per version) and its `signatureTimestamp` read.
//! 3. RDM makes its own player request with that session and timestamp, getting the full
//!    format list.
//! 4. Each format link is unlocked by running the player script's own signature and `n`
//!    functions ([`crate::ytsolve`]), and the token is added.
//! 5. Video and audio are downloaded with the segmented engine and joined ([`crate::pair`]).
//!
//! A job is a request whose URL is `https://www.youtube.com/watch?v=ID#rdm-yt` with the
//! job details as JSON in `post_data` (never sent anywhere as a body).

use crate::config::EngineConfig;
use crate::download::{DownloadError, DownloadRequest};
use crate::ytsolve::{Solved, Solver};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

type Result<T> = std::result::Result<T, DownloadError>;

pub const MARK: &str = "#rdm-yt";
const ORIGIN: &str = "https://www.youtube.com";
/// The player client whose response carries a link for every format.
const CLIENT_NAME: &str = "MWEB";
const CLIENT_ID: &str = "2";
const CLIENT_VERSION: &str = "2.20261009.01.00";
pub const USER_AGENT: &str = "Mozilla/5.0 (iPad; CPU OS 16_7_10 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1,gzip(gfe)";
const PAGE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

fn err(e: impl std::fmt::Display) -> DownloadError {
    DownloadError::Other(format!("YouTube: {e}"))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Job {
    pub video_id: String,
    pub itag_video: u32,
    pub itag_audio: u32,
    pub title: String,
    pub visitor_data: String,
    pub player_js_url: String,
    pub client_version: String,
    pub session_index: String,
    pub logged_in: bool,
    pub pot: String,
}

pub fn is_job(url: &str) -> bool {
    url.ends_with(MARK) && url.contains("youtube.com/watch")
}

/// One format from the player response, unlocked.
#[derive(Debug, Clone)]
pub struct Format {
    pub itag: u32,
    pub mime: String,
    pub height: u32,
    pub bitrate: u64,
    pub clen: u64,
    pub url: String,
}

impl Format {
    fn video_only(&self) -> bool {
        self.mime.starts_with("video/") && !self.mime.contains("mp4a") && !self.mime.contains("opus")
    }
    fn audio(&self) -> bool {
        self.mime.starts_with("audio/")
    }
    fn container(&self) -> &'static str {
        if self.mime.contains("webm") { "webm" } else { "mp4" }
    }
}

fn cache_dir() -> PathBuf {
    crate::settings::data_dir().join("youtube")
}

fn solvers() -> &'static Mutex<HashMap<String, Solver>> {
    static S: OnceLock<Mutex<HashMap<String, Solver>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The solver for a player script URL (memory cache, then disk cache, then a fresh search).
async fn solver(client: &Client, js_url: &str) -> Result<Solver> {
    if let Some(s) = solvers().lock().unwrap().get(js_url) {
        return Ok(s.clone());
    }
    let dir = cache_dir();
    std::fs::create_dir_all(&dir)?;
    let key: String = js_url.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let js_path = dir.join(format!("{key}.js"));
    let src = match std::fs::read_to_string(&js_path) {
        Ok(s) => s,
        Err(_) => {
            let r = client.get(js_url).header("user-agent", PAGE_UA).send().await?;
            if !r.status().is_success() {
                return Err(DownloadError::Status(r.status()));
            }
            let s = r.text().await?;
            let _ = std::fs::write(&js_path, &s);
            s
        }
    };
    let solved_path = dir.join("solved.json");
    let mut known: HashMap<String, Solved> = std::fs::read(&solved_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let cached = known.get(js_url).cloned();
    let s = tokio::task::spawn_blocking(move || Solver::open(src, cached)).await.map_err(err)?.map_err(err)?;
    known.insert(js_url.to_string(), s.solved.clone());
    let _ = std::fs::write(&solved_path, serde_json::to_vec_pretty(&known).unwrap_or_default());
    solvers().lock().unwrap().insert(js_url.to_string(), s.clone());
    Ok(s)
}

/// Fill in what the extension didn't send (direct use from the app / command line) from the
/// watch page itself: player script URL and visitor data.
async fn page_config(client: &Client, job: &mut Job, cookies: Option<&str>) -> Result<()> {
    if !job.player_js_url.is_empty() && !job.visitor_data.is_empty() {
        return Ok(());
    }
    let mut rb = client.get(format!("{ORIGIN}/watch?v={}", job.video_id)).header("user-agent", PAGE_UA).header("accept-language", "en-US,en");
    if let Some(c) = cookies {
        rb = rb.header("cookie", c);
    }
    let html = rb.send().await?.text().await?;
    let grab = |re: &str| regex::Regex::new(re).ok().and_then(|r| r.captures(&html)).and_then(|c| c.get(1)).map(|m| m.as_str().replace("\\/", "/"));
    if job.player_js_url.is_empty() {
        job.player_js_url = grab(r#""(?:jsUrl|PLAYER_JS_URL)"\s*:\s*"([^"]+)""#).map(|u| if u.starts_with('/') { format!("{ORIGIN}{u}") } else { u }).unwrap_or_default();
    }
    if job.visitor_data.is_empty() {
        job.visitor_data = grab(r#""VISITOR_DATA"\s*:\s*"([^"]+)""#).unwrap_or_default();
    }
    if job.title.is_empty() {
        job.title = grab(r#"<title>([^<]*?)(?: - YouTube)?</title>"#).unwrap_or_default();
    }
    if job.player_js_url.is_empty() {
        return Err(err("couldn't find the player script on the page"));
    }
    Ok(())
}

/// `SAPISIDHASH` authorization for signed-in requests (the page sends the same header).
fn sapisid_hash(cookies: &str) -> Option<String> {
    use sha1::{Digest, Sha1};
    let sapisid = cookies.split(';').map(str::trim).find_map(|c| c.strip_prefix("SAPISID=").or_else(|| c.strip_prefix("__Secure-3PAPISID=")))?;
    let ts = chrono::Utc::now().timestamp();
    let digest = Sha1::digest(format!("{ts} {sapisid} {ORIGIN}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("SAPISIDHASH {ts}_{hex}"))
}

/// Own player request, the way the page makes it.
async fn player_request(client: &Client, job: &Job, sts: u64, cookies: Option<&str>) -> Result<Value> {
    let mut client_ctx = json!({ "clientName": CLIENT_NAME, "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US" });
    if !job.visitor_data.is_empty() {
        client_ctx["visitorData"] = json!(job.visitor_data);
    }
    let body = json!({
        "context": { "client": client_ctx },
        "videoId": job.video_id,
        "playbackContext": { "contentPlaybackContext": { "signatureTimestamp": sts, "html5Preference": "HTML5_PREF_WANTS" } },
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let mut rb = client
        .post(format!("{ORIGIN}/youtubei/v1/player?prettyPrint=false"))
        .header("content-type", "application/json")
        .header("x-youtube-client-name", CLIENT_ID)
        .header("x-youtube-client-version", CLIENT_VERSION)
        .header("origin", ORIGIN)
        .header("user-agent", USER_AGENT)
        .body(serde_json::to_vec(&body).map_err(err)?);
    if !job.visitor_data.is_empty() {
        rb = rb.header("x-goog-visitor-id", &job.visitor_data);
    }
    if let Some(c) = cookies {
        rb = rb.header("cookie", c);
        if let Some(auth) = sapisid_hash(c) {
            rb = rb.header("authorization", auth).header("x-origin", ORIGIN).header("x-goog-authuser", if job.session_index.is_empty() { "0" } else { job.session_index.as_str() });
        }
    }
    let r = rb.send().await?;
    if !r.status().is_success() {
        return Err(DownloadError::Status(r.status()));
    }
    serde_json::from_slice(&r.bytes().await?).map_err(err)
}

fn playable(v: &Value) -> std::result::Result<(), String> {
    let ps = &v["playabilityStatus"];
    match ps["status"].as_str() {
        Some("OK") => Ok(()),
        s => Err(format!("{} {}", s.unwrap_or("no status"), ps["reason"].as_str().unwrap_or("")).trim().to_string()),
    }
}

/// Unlock every format of a player response.
fn unlock_all(resp: &Value, solver: &Solver, pot: &str) -> Vec<Format> {
    let sd = &resp["streamingData"];
    let list = sd["adaptiveFormats"].as_array().into_iter().flatten().chain(sd["formats"].as_array().into_iter().flatten());
    let mut out = vec![];
    for f in list {
        if f.get("drmFamilies").is_some() {
            continue;
        }
        let raw = match (f["url"].as_str(), f["signatureCipher"].as_str().or(f["cipher"].as_str())) {
            (Some(u), _) => url::Url::parse(u).ok(),
            (None, Some(c)) => {
                let p: HashMap<String, String> = url::form_urlencoded::parse(c.as_bytes()).into_owned().collect();
                let (Some(u), Some(s)) = (p.get("url"), p.get("s")) else { continue };
                let Some(sig) = solver.sig(s) else { continue };
                url::Url::parse(u).ok().map(|mut u| {
                    u.query_pairs_mut().append_pair(p.get("sp").map(String::as_str).unwrap_or("signature"), &sig);
                    u
                })
            }
            _ => None,
        };
        let Some(mut u) = raw else { continue };
        let pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
        let mut q = u.query_pairs_mut();
        q.clear();
        for (k, v) in &pairs {
            if k == "n" {
                let Some(n) = solver.n(v) else { continue };
                q.append_pair("n", &n);
            } else {
                q.append_pair(k, v);
            }
        }
        if !pot.is_empty() && !pairs.iter().any(|(k, _)| k == "pot") {
            q.append_pair("pot", pot);
        }
        drop(q);
        out.push(Format {
            itag: f["itag"].as_u64().unwrap_or(0) as u32,
            mime: f["mimeType"].as_str().unwrap_or("").to_string(),
            height: f["height"].as_u64().unwrap_or(0) as u32,
            bitrate: f["bitrate"].as_u64().unwrap_or(0),
            clen: f["contentLength"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
            url: u.to_string(),
        });
    }
    out
}

/// Pick the requested video/audio formats (or the closest available).
fn choose(formats: &[Format], job: &Job) -> Option<(Option<Format>, Format)> {
    let audio = |container: Option<&str>| {
        formats
            .iter()
            .filter(|f| f.audio() && container.is_none_or(|c| f.container() == c))
            .max_by_key(|f| f.bitrate)
            .cloned()
    };
    let wanted_a = formats.iter().find(|f| f.itag == job.itag_audio && f.audio()).cloned();
    if job.itag_video == 0 {
        return Some((None, wanted_a.or_else(|| audio(Some("mp4"))).or_else(|| audio(None))?));
    }
    let wanted_h = formats.iter().find(|f| f.itag == job.itag_video).map(|f| f.height);
    let video = formats.iter().find(|f| f.itag == job.itag_video && f.video_only()).cloned().or_else(|| {
        let h = wanted_h.unwrap_or(u32::MAX);
        formats.iter().filter(|f| f.video_only() && f.height <= h).max_by_key(|f| (f.height, f.container() == "mp4", f.bitrate)).cloned()
    })?;
    let a = wanted_a.filter(|a| a.container() == video.container()).or_else(|| audio(Some(video.container()))).or_else(|| audio(None))?;
    Some((Some(video), a))
}

/// Turn a YouTube job into a plain download request (one stream, or a video + audio pair).
pub async fn resolve(req: &DownloadRequest, cfg: &EngineConfig) -> Result<DownloadRequest> {
    let mut job: Job = match req.post_data.as_deref() {
        Some(j) => serde_json::from_str(j).map_err(err)?,
        // No details (link pasted into the app): best video + audio.
        None => Job { itag_video: u32::MAX, ..Default::default() },
    };
    if job.video_id.is_empty() {
        job.video_id = url::Url::parse(&req.url).ok().and_then(|u| u.query_pairs().find(|(k, _)| k == "v").map(|(_, v)| v.into_owned())).unwrap_or_default();
    }
    if job.video_id.is_empty() {
        return Err(err("no video id"));
    }
    let mut builder = Client::builder().timeout(cfg.timeout).gzip(true);
    if let Some(p) = cfg.proxy.as_deref().filter(|p| !p.is_empty() && *p != "none" && *p != "system") {
        builder = builder.proxy(reqwest::Proxy::all(p).map_err(err)?);
    }
    let client = builder.build()?;
    let cookies = req.cookies.as_deref().filter(|c| !c.is_empty());
    page_config(&client, &mut job, cookies).await?;
    let solver = solver(&client, &job.player_js_url).await?;
    // Anonymous with the page's visitor session first; with the signed-in cookies if needed.
    let mut resp = player_request(&client, &job, solver.solved.sts, None).await?;
    if let (Err(_), Some(c)) = (playable(&resp), cookies) {
        resp = player_request(&client, &job, solver.solved.sts, Some(c)).await?;
    }
    playable(&resp).map_err(err)?;
    if job.title.is_empty() {
        job.title = resp["videoDetails"]["title"].as_str().unwrap_or("video").to_string();
    }
    let pot = job.pot.clone();
    let s2 = solver.clone();
    let resp2 = resp.clone();
    let formats = tokio::task::spawn_blocking(move || unlock_all(&resp2, &s2, &pot)).await.map_err(err)?;
    let (video, audio) = choose(&formats, &job).ok_or_else(|| err("no downloadable formats"))?;
    if std::env::var_os("RDM_YT_DEBUG").is_some() {
        eprintln!("[yt] player {} sts {} sig {:?} n {:?}", job.player_js_url, solver.solved.sts, solver.solved.sig, solver.solved.n);
        eprintln!("[yt] playability {:?}, {} formats unlocked, pot {} chars", playable(&resp), formats.len(), job.pot.len());
        for f in [video.as_ref(), Some(&audio)].into_iter().flatten() {
            eprintln!("[yt] itag {} {} clen {} url {}", f.itag, f.mime, f.clen, f.url);
        }
    }
    let ext = match &video {
        Some(v) => v.container(),
        None if audio.container() == "mp4" => "m4a",
        None => "webm",
    };
    let stem = req
        .file_name
        .clone()
        .map(|n| n.rsplit_once('.').map_or(n.clone(), |(s, e)| if e.len() <= 4 { s.to_string() } else { n.clone() }))
        .unwrap_or_else(|| job.title.clone());
    let url = match &video {
        Some(v) => crate::pair::pair_url(&v.url, &audio.url),
        None => audio.url.clone(),
    };
    Ok(DownloadRequest {
        url,
        file_name: Some(crate::naming::sanitize(&format!("{stem}.{ext}"))),
        referer: Some(format!("{ORIGIN}/")),
        user_agent: Some(USER_AGENT.into()),
        cookies: None,
        post_data: None,
        ..req.clone()
    })
}

/// What the add dialog shows for a job: the name and the size the extension sent (video +
/// audio of the chosen quality), without contacting the site.
pub fn probe_info(req: &DownloadRequest) -> crate::download::ProbeInfo {
    let job: Value = req.post_data.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
    crate::download::ProbeInfo {
        file_name: req.file_name.clone().unwrap_or_else(|| job["title"].as_str().map(|t| format!("{t}.mp4")).unwrap_or_default()),
        total: job["size"].as_u64().filter(|s| *s > 0),
        content_type: Some("video/mp4".into()),
        resumable: true,
        final_url: req.url.clone(),
        last_modified: None,
        etag: None,
    }
}

/// Stream links change on every request; for resuming, two links name the same stream if
/// they are the same format of the same video.
pub fn same_stream(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let key = |s: &str| -> Option<(String, String, String)> {
        let u = url::Url::parse(s).ok()?;
        if !u.host_str()?.ends_with("googlevideo.com") {
            return None;
        }
        let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
        Some((q.get("id")?.clone(), q.get("itag")?.clone(), q.get("clen").cloned().unwrap_or_default()))
    };
    matches!((key(a), key(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_and_stream_identity() {
        assert!(is_job("https://www.youtube.com/watch?v=abcdefghijk#rdm-yt"));
        assert!(!is_job("https://www.youtube.com/watch?v=abcdefghijk"));
        let a = "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1&id=o-AB&itag=137&clen=500&n=aaa&sig=1";
        let b = "https://rr5---sn-y.googlevideo.com/videoplayback?expire=2&id=o-AB&itag=137&clen=500&n=bbb&sig=2";
        let c = "https://rr5---sn-y.googlevideo.com/videoplayback?expire=2&id=o-AB&itag=140&clen=90&n=bbb&sig=2";
        assert!(same_stream(a, b));
        assert!(!same_stream(a, c));
        assert!(!same_stream("https://x/a", "https://x/b"));
    }

    #[test]
    fn picks_requested_or_closest_formats() {
        let f = |itag, mime: &str, height, bitrate| Format { itag, mime: mime.into(), height, bitrate, clen: 0, url: String::new() };
        let all = vec![
            f(137, "video/mp4; codecs=\"avc1\"", 1080, 4_000_000),
            f(248, "video/webm; codecs=\"vp9\"", 1080, 3_000_000),
            f(136, "video/mp4; codecs=\"avc1\"", 720, 2_000_000),
            f(140, "audio/mp4; codecs=\"mp4a.40.2\"", 0, 130_000),
            f(251, "audio/webm; codecs=\"opus\"", 0, 140_000),
        ];
        let job = |v, a| Job { itag_video: v, itag_audio: a, ..Default::default() };
        let (v, a) = choose(&all, &job(137, 140)).unwrap();
        assert_eq!((v.unwrap().itag, a.itag), (137, 140));
        // A requested itag that isn't offered falls back to the best at or below its height.
        let (v, a) = choose(&all, &job(999, 0)).unwrap();
        assert_eq!((v.unwrap().itag, a.itag), (137, 140));
        let (v, a) = choose(&all, &job(248, 140)).unwrap();
        assert_eq!((v.unwrap().itag, a.itag), (248, 251));
        let (v, a) = choose(&all, &job(0, 0)).unwrap();
        assert!(v.is_none());
        assert_eq!(a.itag, 140);
    }
}
