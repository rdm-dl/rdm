//! Recording a live HLS stream. The playlist only ever lists the last few segments, so it's read
//! again and again and each new segment is saved as it appears. Pausing a recording ends it: what
//! was caught so far is put together as usual, and the same happens when the stream ends. The
//! pieces stay in the temporary folder until then, so a crash loses nothing already saved.
//!
//! A stream can change hands while it runs: an ad break often has a header of its own, and the
//! two can't share a file. So the recording is kept in stretches, one per header, and the one
//! with the most in it is the programme. Ad breaks without a header of their own are found the
//! way they are for any stream, once the whole recording can be seen.

use crate::config::EngineConfig;
use crate::download::{Control, DownloadError, DownloadRequest, Progress, Status};
use crate::hls::{Key, MediaPlaylist, Segment, drop_ads, fetch_init, fetch_text, parse_stream, save_segment};
use crate::limiter::Limiter;
use crate::storage::TempDir;
use reqwest::Client;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use url::Url;

type Result<T> = std::result::Result<T, DownloadError>;
type Map = Option<(Url, Option<(u64, u64)>)>;
type Keys = Arc<Mutex<HashMap<Url, [u8; 16]>>>;

/// A playlist longer than this reaches into the past: recording starts near its end.
const LONG_WINDOW: f64 = 120.0;
/// How much of a long playlist's past comes along.
const LOOK_BACK: f64 = 60.0;
/// Failed playlist reads in a row before the stream counts as over.
const MOST_FAILURES: u32 = 5;

/// The first segment worth recording. A playlist of type EVENT keeps everything since the start,
/// and is taken whole.
fn start_of(pl: &MediaPlaylist) -> usize {
    let total: f64 = pl.segments.iter().map(|s| s.duration).sum();
    if pl.event || total <= LONG_WINDOW {
        return 0;
    }
    let mut back = 0.0;
    for (i, s) in pl.segments.iter().enumerate().rev() {
        back += s.duration;
        if back >= LOOK_BACK {
            return i;
        }
    }
    0
}

/// How long to wait between readings of the playlist.
fn pause_for(target: f64) -> Duration {
    Duration::from_secs_f64((target / 2.0).clamp(1.0, 10.0))
}

/// How long a stream may stay silent before it's over.
fn quiet_limit(target: f64) -> Duration {
    Duration::from_secs_f64((target * 4.0).max(30.0))
}

/// A playlist without an end mark may still be finished: a video whose server left the mark off.
/// It's read a second time to see. Only long playlists need this: a short one is taken whole
/// either way, and recording it just ends when nothing new comes.
pub(crate) async fn is_finished(client: &Client, req: &DownloadRequest, pl: &MediaPlaylist, url: &Url) -> bool {
    let total: f64 = pl.segments.iter().map(|s| s.duration).sum();
    if total <= LONG_WINDOW {
        return false;
    }
    tokio::time::sleep(Duration::from_secs_f64((pl.target * 2.0).clamp(4.0, 30.0))).await;
    let Ok(text) = fetch_text(client, url, req).await else { return false };
    let Ok(again) = parse_stream(&text, url, true) else { return false };
    again.segments.last().map(|s| s.seq) == pl.segments.last().map(|s| s.seq)
}

/// A part of the recording that shares one header.
#[derive(Default)]
struct Stretch {
    init: Option<Vec<u8>>,
    segments: Vec<Segment>,
    /// Which file each segment went to, by its number in the playlist.
    files: HashMap<u64, usize>,
    /// What has arrived, in seconds.
    seconds: f64,
}

/// The stretch with the most in it, and which of its files to keep. Ad breaks in it go too,
/// unless they're to be kept.
fn programme(stretches: &[Stretch], keep_ads: bool) -> Option<(Option<&[u8]>, Vec<usize>)> {
    let best = stretches.iter().max_by(|a, b| a.seconds.total_cmp(&b.seconds))?;
    let mut pl = MediaPlaylist { segments: best.segments.clone(), ..Default::default() };
    if !keep_ads {
        drop_ads(&mut pl);
    }
    Some((best.init.as_deref(), pl.segments.iter().filter_map(|s| best.files.get(&s.seq).copied()).collect()))
}

struct Track {
    url: Url,
    /// The next segment number to take from the playlist.
    next_seq: u64,
    /// The next file to write.
    next_file: usize,
    /// Files already there when this recording began.
    earlier: usize,
    ended: bool,
    failures: u32,
    target: f64,
    stretches: Vec<Stretch>,
    /// Which stretch each header address belongs to.
    known: HashMap<Map, usize>,
}

impl Track {
    /// The stretch for a header. Two addresses can give the same header, so it's the bytes that
    /// decide. `None` if the header can't be fetched.
    async fn stretch_for(&mut self, map: &Map, key: &Option<Key>, client: &Client, req: &DownloadRequest, keys: &Keys) -> Option<usize> {
        if let Some(found) = self.known.get(map) {
            return Some(*found);
        }
        let init = match map {
            Some(map) => match fetch_init(client, map, key, keys, req).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    tracing::warn!("live header not fetched: {e}");
                    return None;
                }
            },
            None => None,
        };
        let index = self.stretches.iter().position(|s| s.init == init).unwrap_or_else(|| {
            self.stretches.push(Stretch { init, ..Default::default() });
            self.stretches.len() - 1
        });
        self.known.insert(map.clone(), index);
        Some(index)
    }

    /// Note a segment and give it its file. Its place in the queue is the caller's.
    async fn take(&mut self, segment: Segment, key: &Option<Key>, client: &Client, req: &DownloadRequest, keys: &Keys) -> Option<(usize, usize, Segment)> {
        let stretch = self.stretch_for(&segment.map, key, client, req, keys).await?;
        let file = self.next_file;
        self.next_file += 1;
        self.stretches[stretch].files.insert(segment.seq, file);
        self.stretches[stretch].segments.push(segment.clone());
        Some((stretch, file, segment))
    }
}

/// Number of the first file not yet there, so a recording cut off by a crash goes on after it.
fn first_free(path: impl Fn(usize) -> PathBuf) -> usize {
    (0..).find(|i| !path(*i).exists()).unwrap_or(0)
}

/// Everything `record` needs from the download it's part of.
pub(crate) struct Job<'a> {
    pub client: Client,
    pub req: Arc<DownloadRequest>,
    pub cfg: &'a EngineConfig,
    pub tmp: &'a TempDir,
    pub limiter: Arc<Limiter>,
    pub global: Arc<Limiter>,
    pub progress: &'a watch::Sender<Progress>,
    pub control: &'a mut watch::Receiver<Control>,
}

/// Record until the stream ends or the user stops it. Returns the segment files to join for each
/// track, in order, and leaves each track's header in `init_file`.
pub(crate) async fn record(job: Job<'_>, playlists: &[(&'static str, MediaPlaylist)], segment_file: impl Fn(usize, usize) -> PathBuf, init_file: impl Fn(usize) -> PathBuf) -> Result<Vec<Vec<usize>>> {
    let Job { client, req, cfg, tmp, limiter, global, progress, control } = job;
    let keys: Keys = Arc::new(Mutex::new(HashMap::new()));
    let mut tracks = vec![];
    let mut queue: VecDeque<(usize, usize, usize, Segment)> = VecDeque::new();
    for (t, (_, pl)) in playlists.iter().enumerate() {
        let url = pl.source.clone().ok_or_else(|| DownloadError::Other("the stream has no playlist address".into()))?;
        let earlier = first_free(|i| segment_file(t, i));
        let next_seq = pl.segments.last().map_or(0, |s| s.seq + 1);
        let mut track = Track { url, next_seq, next_file: earlier, earlier, ended: false, failures: 0, target: pl.target.max(1.0), stretches: vec![], known: HashMap::new() };
        for segment in &pl.segments[start_of(pl)..] {
            if let Some((stretch, file, segment)) = track.take(segment.clone(), &pl.init_key, &client, &req, &keys).await {
                queue.push_back((t, stretch, file, segment));
            }
        }
        tracks.push(track);
    }
    progress.send_modify(|p| {
        p.status = Status::Downloading;
        p.resumable = false;
        p.total = None;
        p.recording = Some(0);
    });

    let saved = Arc::new(AtomicU64::new(0));
    let mut set: JoinSet<(usize, usize, f64, Result<u64>)> = JoinSet::new();
    let mut files = 0usize;
    let mut last_new = Instant::now();
    let mut poll_at = tokio::time::Instant::now() + pause_for(tracks[0].target);
    let mut ui = tokio::time::interval(Duration::from_millis(250));
    let (mut speed_t, mut speed_b, mut speed) = (Instant::now(), 0u64, 0.0f64);
    let limit = cfg.record_for.map(|s| s as f64);
    // The length of a recording is the length of its programme.
    let length = |tracks: &[Track]| tracks[0].stretches.iter().map(|s| s.seconds).fold(0.0, f64::max);
    loop {
        while set.len() < cfg.max_connections.max(1) {
            let Some((t, stretch, file, segment)) = queue.pop_front() else { break };
            let (client, req, keys, limiter, global, saved, path) = (client.clone(), req.clone(), keys.clone(), limiter.clone(), global.clone(), saved.clone(), segment_file(t, file));
            set.spawn(async move {
                let result = save_segment(&client, &segment, &keys, &req, &path, &limiter, &global, &saved).await;
                (t, stretch, segment.duration, result)
            });
        }
        let idle = set.is_empty() && queue.is_empty();
        let ended = tracks.iter().all(|t| t.ended) && idle;
        let silent = last_new.elapsed() >= quiet_limit(tracks[0].target) && idle;
        let lost = tracks.iter().any(|t| t.failures >= MOST_FAILURES) && idle;
        if ended || silent || lost || limit.is_some_and(|l| length(&tracks) >= l) {
            break;
        }
        tokio::select! {
            finished = set.join_next(), if !set.is_empty() => {
                if let Some(finished) = finished {
                    let (t, stretch, seconds, result) = finished.map_err(|e| DownloadError::Other(e.to_string()))?;
                    match result {
                        Ok(_) => {
                            files += 1;
                            tracks[t].stretches[stretch].seconds += seconds;
                        }
                        Err(e) if e.is_disk_full() => return Err(e),
                        // A live segment that won't come is gone: the recording goes on without it.
                        Err(e) => tracing::warn!("live segment lost: {e}"),
                    }
                }
            }
            _ = tokio::time::sleep_until(poll_at) => {
                for (t, track) in tracks.iter_mut().enumerate() {
                    if track.ended {
                        continue;
                    }
                    let read = match fetch_text(&client, &track.url, &req).await {
                        Ok(text) => parse_stream(&text, &track.url, true),
                        Err(e) => Err(e),
                    };
                    let pl = match read {
                        Ok(pl) => pl,
                        Err(e) => {
                            tracing::info!("live playlist not read: {e}");
                            track.failures += 1;
                            continue;
                        }
                    };
                    track.failures = 0;
                    track.target = pl.target.max(1.0);
                    track.ended = !pl.live;
                    let from = track.next_seq;
                    for segment in pl.segments.into_iter().filter(|s| s.seq >= from) {
                        track.next_seq = segment.seq + 1;
                        last_new = Instant::now();
                        if let Some((stretch, file, segment)) = track.take(segment, &pl.init_key, &client, &req, &keys).await {
                            queue.push_back((t, stretch, file, segment));
                        }
                    }
                }
                poll_at = tokio::time::Instant::now() + pause_for(tracks[0].target);
            }
            _ = control.changed() => {
                match *control.borrow() {
                    Control::Run => {}
                    Control::Cancel => {
                        set.abort_all();
                        let _ = tmp.clone().remove();
                        return Err(DownloadError::Cancelled);
                    }
                    // Stopping a recording is how it's meant to end: the file gets made.
                    Control::Pause => break,
                }
            }
            _ = ui.tick() => {
                let bytes = saved.load(Ordering::Relaxed);
                let elapsed = speed_t.elapsed().as_secs_f64();
                if elapsed >= 1.0 {
                    let now = bytes.saturating_sub(speed_b) as f64 / elapsed;
                    speed = if speed == 0.0 { now } else { speed * 0.6 + now * 0.4 };
                    speed_t = Instant::now();
                    speed_b = bytes;
                }
                progress.send_modify(|p| {
                    p.downloaded = bytes;
                    p.speed = speed;
                    p.connections = set.len();
                    p.recording = Some(length(&tracks) as u64);
                });
            }
        }
    }
    set.abort_all();
    progress.send_modify(|p| p.recording = None);
    if files == 0 {
        return Err(DownloadError::Other("Nothing could be recorded from this live stream.".into()));
    }
    let mut out = vec![];
    for (t, track) in tracks.iter().enumerate() {
        let (init, kept) = programme(&track.stretches, cfg.keep_ads).unwrap_or((None, vec![]));
        match init {
            Some(init) => std::fs::write(init_file(t), init)?,
            None => {
                let _ = std::fs::remove_file(init_file(t));
            }
        }
        out.push((0..track.earlier).chain(kept).collect());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(segments: u64, seconds: f64, event: bool) -> MediaPlaylist {
        let url = Url::parse("https://x/a.ts").unwrap();
        let segments = (0..segments).map(|seq| Segment { url: url.clone(), byterange: None, key: None, seq, duration: seconds, discontinuity: false, map: None }).collect();
        MediaPlaylist { segments, live: true, event, ..Default::default() }
    }

    fn stretch(init: &[u8], first: u64, count: u64, host: &str, from_file: usize) -> Stretch {
        let url = Url::parse(&format!("https://{host}/a.ts")).unwrap();
        let segments: Vec<_> = (first..first + count).map(|seq| Segment { url: url.clone(), byterange: None, key: None, seq, duration: 2.0, discontinuity: false, map: None }).collect();
        let files = segments.iter().enumerate().map(|(i, s)| (s.seq, from_file + i)).collect();
        Stretch { init: Some(init.to_vec()), seconds: count as f64 * 2.0, segments, files }
    }

    #[test]
    fn a_short_window_is_taken_whole() {
        assert_eq!(start_of(&playlist(6, 6.0, false)), 0);
    }

    #[test]
    fn a_long_window_starts_near_its_end() {
        assert_eq!(start_of(&playlist(1000, 6.0, false)), 990);
    }

    #[test]
    fn an_event_keeps_everything_since_the_start() {
        assert_eq!(start_of(&playlist(1000, 6.0, true)), 0);
    }

    #[test]
    fn waiting_stays_sensible() {
        assert_eq!(pause_for(6.0), Duration::from_secs(3));
        assert_eq!(pause_for(0.5), Duration::from_secs(1));
        assert_eq!(pause_for(60.0), Duration::from_secs(10));
        assert_eq!(quiet_limit(2.0), Duration::from_secs(30));
        assert_eq!(quiet_limit(10.0), Duration::from_secs(40));
    }

    #[test]
    fn an_ad_with_its_own_header_is_not_the_programme() {
        // Fifteen segments of ad first, then twenty of the show.
        let stretches = [stretch(b"ad", 0, 15, "ads.example", 0), stretch(b"show", 15, 20, "show.example", 15)];
        let (init, files) = programme(&stretches, false).unwrap();
        assert_eq!(init, Some(&b"show"[..]));
        assert_eq!(files, (15..35).collect::<Vec<_>>());
    }

    #[test]
    fn a_broken_off_recording_goes_on_after_its_last_file() {
        let dir = std::env::temp_dir().join(format!("rdm-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            std::fs::write(dir.join(format!("{i}.bin")), b"x").unwrap();
        }
        assert_eq!(first_free(|i| dir.join(format!("{i}.bin"))), 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
