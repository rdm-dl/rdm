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

/// Package a downloaded MPEG-TS, AAC or MP3 stream (and a separate audio track, if any) as
/// MP4 without ffmpeg. `Ok(None)` means this isn't such a stream, or it has something
/// [`crate::ts`] can't carry over; the caller then goes on as if this had never been tried.
pub async fn package_stream(video: PathBuf, audio: Option<PathBuf>, work: PathBuf, dest: PathBuf, progress: &watch::Sender<Progress>) -> io::Result<Option<PathBuf>> {
    let total = [Some(&video), audio.as_ref()].into_iter().flatten().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let count = Arc::new(AtomicU64::new(0));
    let c2 = count.clone();
    let packaged = with_progress(progress, total, move || c2.load(Ordering::Relaxed), move || {
        std::fs::create_dir_all(&work)?;
        let result = package_blocking(&video, audio.as_deref(), &work, &dest, &count);
        let _ = std::fs::remove_dir_all(&work);
        result
    })
    .await;
    Ok(match packaged {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            tracing::info!("stream left to the fallback: {e}");
            None
        }
        Err(e) => return Err(e),
    })
}

fn package_blocking(video: &Path, audio: Option<&Path>, work: &Path, dest: &Path, read: &AtomicU64) -> io::Result<Option<PathBuf>> {
    use crate::{mux, ts};
    let is_ts = |p: &Path| ts::sniff(p).map(|k| k.is_some() && !matches!(mux::sniff(p), Ok(Some(_))));
    let video_ts = is_ts(video)?;
    let audio_ts = audio.map(is_ts).transpose()?.unwrap_or(false);
    if !video_ts && !audio_ts {
        return Ok(None);
    }
    let (v_out, a_out) = (work.join("video.mp4"), work.join("audio.m4a"));
    let muxed = audio.is_none();
    // The tracks of one stream share a clock, also when they come from different files.
    let mut base = None;
    if video_ts {
        base = ts::start_time(video, true, muxed)?;
    }
    if let (Some(a), true) = (audio, audio_ts) {
        base = base.into_iter().chain(ts::start_time(a, false, true)?).min();
    }
    let base = base.unwrap_or(0);
    let (mut have_v, mut have_a) = (false, false);
    if video_ts {
        (have_v, have_a) = ts::to_fmp4(video, Some(&v_out), muxed.then_some(a_out.as_path()), base, read)?;
    }
    if let (Some(a), true) = (audio, audio_ts) {
        have_a = ts::to_fmp4(a, None, Some(&a_out), base, read)?.1;
    }
    // One side may already be fragmented MP4.
    let v_file = if video_ts { have_v.then_some(v_out.as_path()) } else { Some(video) };
    let a_file = match audio {
        Some(a) if !audio_ts => Some(a),
        _ => have_a.then_some(a_out.as_path()),
    };
    match (v_file, a_file) {
        (Some(v), Some(a)) => Ok(mux::mp4::merge(v, a, dest)?.then(|| dest.to_path_buf())),
        (Some(v), None) if audio.is_none() => {
            move_file(v, dest)?;
            Ok(Some(dest.to_path_buf()))
        }
        (None, Some(a)) if audio.is_none() => {
            let m4a = crate::naming::unique_path(dest.parent().unwrap_or(Path::new(".")), &format!("{}.m4a", dest.file_stem().and_then(|s| s.to_str()).unwrap_or("audio")));
            move_file(a, &m4a)?;
            Ok(Some(m4a))
        }
        // A separate track came out empty: don't quietly lose it.
        _ => Ok(None),
    }
}

/// The last step for a stream: audio on its own becomes an ordinary `.m4a`, and a fragmented
/// video an ordinary `.mp4`. `name` is the file's name without its extension.
pub async fn finish(result: PathBuf, name: String, progress: &watch::Sender<Progress>) -> io::Result<PathBuf> {
    let total = std::fs::metadata(&result).map(|m| m.len()).unwrap_or(0);
    let work = result.with_extension("mp4.part");
    with_progress(progress, total, file_size(work), move || {
        if crate::mux::mp4::is_audio_only(&result) {
            Ok(crate::mux::mp4::finish_audio(result, &name))
        } else {
            Ok(crate::mux::mp4::finish_video(result, &name))
        }
    })
    .await
}

/// Put a separate video and audio track together as `dest`: RDM's own MP4/WebM joiner
/// first, then ffmpeg if installed, and only if neither can, the two files side by side
/// (`stem.video.ext` + `stem.audio.ext`). Returns the file to show as the download.
pub async fn video_and_audio(video: PathBuf, audio: PathBuf, dest: PathBuf, stem: &str, progress: &watch::Sender<Progress>) -> io::Result<PathBuf> {
    // Sound that came as a bare MP3 or AAC stream gets a container first.
    let work = dest.with_extension("package");
    if let Some(done) = package_stream(video.clone(), Some(audio.clone()), work, dest.clone(), progress).await? {
        return Ok(done);
    }
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
        None if std::fs::read(p).is_ok_and(|b| b.starts_with(b"ID3") || (b.len() > 1 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0)) => "mp3".to_string(),
        None => p.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_string(),
    };
    let vdest = crate::naming::unique_path(&dir, &format!("{stem}.video.{}", ext(&video, false)));
    let adest = crate::naming::unique_path(&dir, &format!("{stem}.audio.{}", ext(&audio, true)));
    move_file(&video, &vdest)?;
    move_file(&audio, &adest)?;
    Ok(vdest)
}
