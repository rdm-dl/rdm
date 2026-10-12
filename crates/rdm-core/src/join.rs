//! The last step of a download: joining pieces into the final file. Always runs on a
//! blocking-pool thread (never on the async workers that drive downloads and the UI
//! updates), and reports how far it got through `Progress::joining`.

use crate::download::Progress;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// Run `work` on a blocking thread; while it runs, publish `done() / total` as the joining
/// progress every quarter second.
pub async fn with_progress<T: Send + 'static>(
    progress: &watch::Sender<Progress>,
    total: u64,
    done: impl Fn() -> u64,
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    progress.send_modify(|p| p.joining = Some(0.0));
    let mut task = tokio::task::spawn_blocking(work);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let out = loop {
        tokio::select! {
            r = &mut task => break r.map_err(io::Error::other)?,
            _ = tick.tick() => {
                let f = if total == 0 { 0.0 } else { (done() as f64 / total as f64).min(1.0) as f32 };
                progress.send_modify(|p| p.joining = Some(f));
            }
        }
    };
    progress.send_modify(|p| p.joining = None);
    out
}

/// Progress of a job that writes `file` up to roughly `total` bytes.
pub fn file_size(file: PathBuf) -> impl Fn() -> u64 {
    move || std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0)
}

struct Counting<W> {
    inner: W,
    count: Arc<AtomicU64>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Concatenate each track's parts (in order) into its output file.
pub async fn concat(tracks: Vec<(Vec<PathBuf>, PathBuf)>, progress: &watch::Sender<Progress>) -> io::Result<()> {
    let total: u64 = tracks.iter().flat_map(|(parts, _)| parts).filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let count = Arc::new(AtomicU64::new(0));
    let c2 = count.clone();
    with_progress(progress, total, move || c2.load(Ordering::Relaxed), move || {
        for (parts, out) in tracks {
            let mut w = Counting { inner: BufWriter::with_capacity(1 << 20, File::create(&out)?), count: count.clone() };
            for p in parts {
                let mut r = BufReader::with_capacity(1 << 20, File::open(&p)?);
                io::copy(&mut r, &mut w)?;
            }
            w.flush()?;
            w.inner.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        Ok(())
    })
    .await
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

/// Put a separate video and audio track together as `dest`: RDM's own MP4/WebM joiner
/// first, then ffmpeg if installed, and only if neither can, the two files side by side
/// (`stem.video.ext` + `stem.audio.ext`). Returns the file to show as the download.
pub async fn video_and_audio(video: PathBuf, audio: PathBuf, dest: PathBuf, stem: &str, progress: &watch::Sender<Progress>) -> io::Result<PathBuf> {
    let total = [&video, &audio].iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let (v, a, d) = (video.clone(), audio.clone(), dest.clone());
    let merged = with_progress(progress, total, file_size(dest.clone()), move || crate::mux::merge(&v, &a, &d)).await;
    if matches!(merged, Ok(true)) {
        return Ok(dest);
    }
    let _ = std::fs::remove_file(&dest);
    let dir = dest.parent().map(Path::to_path_buf).unwrap_or_default();
    if ffmpeg_available() {
        let mkv = crate::naming::unique_path(&dir, &format!("{stem}.mkv"));
        let ok = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(&video)
            .arg("-i")
            .arg(&audio)
            .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"])
            .arg(&mkv)
            .status()
            .await
            .is_ok_and(|s| s.success());
        if ok {
            return Ok(mkv);
        }
    }
    let ext = |p: &Path, audio: bool| match crate::mux::sniff(p).ok().flatten() {
        Some(crate::mux::Container::Mp4) => (if audio { "m4a" } else { "mp4" }).to_string(),
        Some(crate::mux::Container::WebM) => "webm".to_string(),
        None => p.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_string(),
    };
    let vdest = crate::naming::unique_path(&dir, &format!("{stem}.video.{}", ext(&video, false)));
    let adest = crate::naming::unique_path(&dir, &format!("{stem}.audio.{}", ext(&audio, true)));
    move_file(&video, &vdest)?;
    move_file(&audio, &adest)?;
    Ok(vdest)
}
