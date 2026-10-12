//! Separate video and audio streams (what adaptive web players fetch) downloaded side by
//! side with the segmented engine, then joined into one file by [`crate::mux`].
//!
//! The request URL is the video stream with the audio stream in its fragment:
//! `https://host/video?...#rdm-audio=<percent-encoded audio URL>` (fragments never
//! reach the server). Pausing keeps both parts' progress; resuming continues them.

use crate::config::EngineConfig;
use crate::download::{ChunkView, Control, Download, DownloadError, DownloadRequest, Progress, Status};
use crate::limiter::Limiter;
use crate::mux::{self, Container};
use crate::naming;
use crate::storage::TempDir;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;

const KEY: &str = "rdm-audio=";

/// The audio URL carried in a pair request, if this is one.
pub fn audio_url(url: &str) -> Option<String> {
    let frag = url.split_once('#')?.1;
    let enc = frag.split('&').find_map(|kv| kv.strip_prefix(KEY))?;
    let dec = percent_encoding::percent_decode_str(enc).decode_utf8().ok()?.into_owned();
    Url::parse(&dec).ok().map(|_| dec)
}

/// Build a pair request URL.
pub fn pair_url(video: &str, audio: &str) -> String {
    let video = video.split('#').next().unwrap_or(video);
    let enc = percent_encoding::utf8_percent_encode(audio, percent_encoding::NON_ALPHANUMERIC);
    format!("{video}#{KEY}{enc}")
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

fn ext_of(p: &Path, kind: Option<Container>, audio: bool) -> String {
    match kind {
        Some(Container::Mp4) => (if audio { "m4a" } else { "mp4" }).into(),
        Some(Container::WebM) => "webm".into(),
        None => p.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    req: &DownloadRequest,
    cfg: &EngineConfig,
    tmp: TempDir,
    global_limiter: Arc<Limiter>,
    limiter: Arc<Limiter>,
    progress: &watch::Sender<Progress>,
    control: &mut watch::Receiver<Control>,
) -> Result<Option<PathBuf>> {
    let audio = audio_url(&req.url).ok_or_else(|| DownloadError::Other("bad audio stream address".into()))?;
    let video = req.url.split('#').next().unwrap_or(&req.url).to_string();
    let stem = req
        .file_name
        .clone()
        .or_else(|| Url::parse(&video).ok().and_then(|u| naming::from_url(&u)))
        .map(|n| naming::sanitize(&n))
        .map(|n| n.rsplit_once('.').map_or(n.clone(), |(s, e)| if e.len() <= 4 { s.to_string() } else { n.clone() }))
        .unwrap_or_else(|| "video".into());
    let parts = tmp.path.join("parts");
    let half = limiter.limit().map(|l| (l / 2).max(1));
    let part = |url: &str, name: &str| DownloadRequest {
        url: url.to_string(),
        dir: parts.clone(),
        file_name: Some(name.to_string()),
        speed_limit: half,
        post_data: None,
        ..req.clone()
    };
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.file_name = format!("{stem}.mp4");
        p.resumable = true;
    });

    let vd = Download::start(part(&video, "video"), cfg.clone(), tmp.path.join("video"), global_limiter.clone());
    let ad = Download::start(part(&audio, "audio"), cfg.clone(), tmp.path.join("audio"), global_limiter.clone());
    let (vp, ap) = (vd.progress.clone(), ad.progress.clone());
    let (vh, ah) = (vd.handle(), ad.handle());
    let mut vt = tokio::spawn(vd.wait());
    let mut at = tokio::spawn(ad.wait());
    let (mut v_done, mut a_done): (Option<Option<PathBuf>>, Option<Option<PathBuf>>) = (None, None);
    let mut ui = tokio::time::interval(Duration::from_millis(200));
    let join_err = |e: tokio::task::JoinError| DownloadError::Other(e.to_string());

    while v_done.is_none() || a_done.is_none() {
        tokio::select! {
            r = &mut vt, if v_done.is_none() => {
                match r.map_err(join_err)? {
                    Ok(p) => v_done = Some(p),
                    Err(e) => { ah.cancel(); return Err(e); }
                }
            }
            r = &mut at, if a_done.is_none() => {
                match r.map_err(join_err)? {
                    Ok(p) => a_done = Some(p),
                    Err(e) => { vh.cancel(); return Err(e); }
                }
            }
            _ = control.changed() => {
                let c = *control.borrow();
                if c != Control::Run {
                    if c == Control::Cancel { vh.cancel(); ah.cancel(); } else { vh.pause(); ah.pause(); }
                    if v_done.is_none() { let _ = (&mut vt).await; }
                    if a_done.is_none() { let _ = (&mut at).await; }
                    if c == Control::Cancel {
                        let _ = tmp.remove();
                        return Err(DownloadError::Cancelled);
                    }
                    return Ok(None);
                }
            }
            _ = ui.tick() => {
                let (v, a) = (vp.borrow().clone(), ap.borrow().clone());
                let shift = v.total.unwrap_or(v.downloaded);
                let mut chunks = v.chunks.clone();
                chunks.extend(a.chunks.iter().map(|c| ChunkView { id: c.id + 10_000, start: c.start + shift, end: c.end + shift, pos: c.pos + shift, active: c.active }));
                progress.send_modify(|p| {
                    p.total = v.total.zip(a.total).map(|(x, y)| x + y);
                    p.downloaded = v.downloaded + a.downloaded;
                    p.speed = v.speed + a.speed;
                    p.connections = v.connections + a.connections;
                    p.chunks = chunks;
                });
            }
        }
    }
    // One part paused on its own (e.g. app shutdown): pause the other and stop.
    let (Some(Some(vpath)), Some(Some(apath))) = (v_done.clone(), a_done.clone()) else {
        vh.pause();
        ah.pause();
        return Ok(None);
    };

    progress.send_modify(|p| p.status = Status::Assembling);
    std::fs::create_dir_all(&req.dir)?;
    let (vk, ak) = (mux::sniff(&vpath)?, mux::sniff(&apath)?);
    let out_ext = match vk {
        Some(Container::WebM) => "webm",
        _ => "mp4",
    };
    let dest = naming::unique_path(&req.dir, &format!("{stem}.{out_ext}"));
    let (v2, a2, d2) = (vpath.clone(), apath.clone(), dest.clone());
    let merged = tokio::task::spawn_blocking(move || mux::merge(&v2, &a2, &d2)).await.map_err(join_err)?;
    let result = match merged {
        Ok(true) => dest,
        _ => {
            let _ = std::fs::remove_file(&dest);
            let mkv = naming::unique_path(&req.dir, &format!("{stem}.mkv"));
            let ok = ffmpeg_available()
                && tokio::process::Command::new("ffmpeg")
                    .args(["-y", "-loglevel", "error", "-i"])
                    .arg(&vpath)
                    .arg("-i")
                    .arg(&apath)
                    .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"])
                    .arg(&mkv)
                    .status()
                    .await
                    .is_ok_and(|s| s.success());
            if ok {
                mkv
            } else {
                // Keep both parts next to each other.
                let vdest = naming::unique_path(&req.dir, &format!("{stem}.video.{}", ext_of(&vpath, vk, false)));
                let adest = naming::unique_path(&req.dir, &format!("{stem}.audio.{}", ext_of(&apath, ak, true)));
                move_file(&vpath, &vdest)?;
                move_file(&apath, &adest)?;
                vdest
            }
        }
    };
    let size = std::fs::metadata(&result).map(|m| m.len()).unwrap_or(0);
    progress.send_modify(|p| {
        p.file_name = result.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        p.total = Some(size);
        p.downloaded = size;
    });
    let _ = tmp.remove();
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_url_round_trip() {
        let a = "https://r1.example.com/videoplayback?itag=140&mime=audio%2Fmp4&n=abc";
        let u = pair_url("https://r1.example.com/videoplayback?itag=137&n=abc", a);
        assert!(u.starts_with("https://r1.example.com/videoplayback?itag=137&n=abc#rdm-audio="));
        assert_eq!(audio_url(&u).as_deref(), Some(a));
        assert_eq!(audio_url("https://x/y#other=1"), None);
    }
}
