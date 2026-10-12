//! The download manager: list of downloads, queues, scheduler, quota, events.
//!
//! The UI (GUI, CLI, browser bridge) talks only to [`Manager`]. State is saved to
//! `downloads.json` and `settings.json` in the data folder.

use crate::download::{ChunkView, Download, DownloadHandle, DownloadRequest, Progress, Status};
use crate::limiter::Limiter;
use crate::naming;
use crate::settings::{MAIN_QUEUE, OnQueueDone, SYNC_QUEUE, Settings};
use chrono::{Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};
use url::Url;

pub type ItemId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ItemState {
    /// Waiting in a queue.
    Queued,
    /// Not started / stopped by the user.
    Stopped,
    Running,
    Complete,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub request: DownloadRequest,
    pub file_name: String,
    pub category: Option<String>,
    pub queue: u32,
    pub state: ItemState,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub resumable: bool,
    pub description: String,
    /// Unix seconds.
    pub added: i64,
    pub finished: Option<i64>,
    pub last_try: Option<i64>,
    pub final_path: Option<PathBuf>,
    #[serde(skip)]
    pub speed: f64,
    #[serde(skip)]
    pub connections: usize,
    #[serde(skip)]
    pub chunks: Vec<ChunkView>,
}

impl Item {
    pub fn progress_fraction(&self) -> Option<f32> {
        self.total.filter(|&t| t > 0).map(|t| (self.downloaded as f64 / t as f64).min(1.0) as f32)
    }
    pub fn eta(&self) -> Option<Duration> {
        let t = self.total?;
        (self.speed >= 1.0).then(|| Duration::from_secs_f64(t.saturating_sub(self.downloaded) as f64 / self.speed))
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Added(ItemId),
    Changed(ItemId),
    Removed(ItemId),
    Completed(ItemId),
    Failed(ItemId, String),
    /// A queue finished; the app should perform `action`.
    QueueDone(u32, OnQueueDone),
    /// Quota reached; downloads resume at the given unix time.
    QuotaExceeded(i64),
    SettingsChanged,
}

/// Options when adding a download.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub start_now: bool,
    pub queue: Option<u32>,
    pub category: Option<String>,
    pub description: String,
}

struct Running {
    handle: DownloadHandle,
    progress: watch::Receiver<Progress>,
    /// Bytes counted so far for the quota.
    counted: u64,
}

struct State {
    settings: Settings,
    items: BTreeMap<ItemId, Item>,
    next_id: ItemId,
    running: HashMap<ItemId, Running>,
    /// Queues currently processing.
    active_queues: HashSet<u32>,
    /// (time, bytes) for the quota window.
    quota_log: VecDeque<(i64, u64)>,
    quota_until: Option<i64>,
    /// Last scheduler minute processed per queue (avoid double triggers).
    sched_fired: HashMap<(u32, bool), i64>,
    /// Completed items already checked in the current synchronization run.
    synced: HashSet<ItemId>,
    dirty: bool,
    last_save: Instant,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedList {
    next_id: ItemId,
    items: Vec<Item>,
}

struct Inner {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    limiter: Arc<Limiter>,
    data_dir: PathBuf,
    /// The runtime downloads run on; methods may be called from threads outside it (the UI).
    rt: tokio::runtime::Handle,
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Manager {
    /// Load state from `data_dir` and start the background loop (needs a tokio runtime).
    pub fn open(data_dir: &Path) -> Self {
        let settings = Settings::load(&data_dir.join("settings.json"));
        let saved: SavedList = std::fs::read(data_dir.join("downloads.json"))
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        let mut items = BTreeMap::new();
        for mut it in saved.items {
            if it.state == ItemState::Running {
                it.state = ItemState::Stopped;
            }
            items.insert(it.id, it);
        }
        let next_id = saved.next_id.max(items.keys().max().map_or(1, |m| m + 1));
        let limiter = Arc::new(Limiter::new(settings.speed_limit));
        let (events, _) = broadcast::channel(1024);
        let m = Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    settings,
                    items,
                    next_id,
                    running: HashMap::new(),
                    active_queues: HashSet::new(),
                    quota_log: VecDeque::new(),
                    quota_until: None,
                    sched_fired: HashMap::new(),
                    synced: HashSet::new(),
                    dirty: false,
                    last_save: Instant::now(),
                }),
                events,
                limiter,
                data_dir: data_dir.to_path_buf(),
                rt: tokio::runtime::Handle::current(),
            }),
        };
        let bg = m.clone();
        tokio::spawn(async move { bg.background().await });
        m
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    fn emit(&self, e: Event) {
        let _ = self.inner.events.send(e);
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub fn settings(&self) -> Settings {
        self.inner.state.lock().unwrap().settings.clone()
    }

    pub fn set_settings(&self, s: Settings) {
        self.inner.limiter.set_limit(s.speed_limit);
        let _ = s.save(&self.inner.data_dir.join("settings.json"));
        self.inner.state.lock().unwrap().settings = s;
        self.emit(Event::SettingsChanged);
    }

    pub fn set_speed_limit(&self, limit: Option<u64>) {
        let mut s = self.settings();
        s.speed_limit = limit;
        self.set_settings(s);
    }

    pub fn items(&self) -> Vec<Item> {
        self.inner.state.lock().unwrap().items.values().cloned().collect()
    }

    pub fn item(&self, id: ItemId) -> Option<Item> {
        self.inner.state.lock().unwrap().items.get(&id).cloned()
    }

    pub fn is_queue_active(&self, q: u32) -> bool {
        self.inner.state.lock().unwrap().active_queues.contains(&q)
    }

    /// Add a download. Returns its id.
    pub fn add(&self, mut req: DownloadRequest, opts: AddOptions) -> ItemId {
        let id;
        {
            let mut st = self.inner.state.lock().unwrap();
            id = st.next_id;
            st.next_id += 1;
            let guess = req.file_name.clone().unwrap_or_else(|| {
                Url::parse(&req.url).ok().and_then(|u| naming::from_url(&u)).map(|n| naming::sanitize(&n)).unwrap_or_else(|| "download".into())
            });
            let category = opts.category.clone().or_else(|| st.settings.category_for(&guess).map(|c| c.name.clone()));
            if req.dir.as_os_str().is_empty() {
                req.dir = st.settings.folder_for(category.as_deref());
            }
            let item = Item {
                id,
                file_name: guess,
                category,
                queue: opts.queue.unwrap_or(MAIN_QUEUE),
                state: if opts.start_now { ItemState::Stopped } else { ItemState::Queued },
                total: None,
                downloaded: 0,
                resumable: false,
                description: opts.description.clone(),
                added: now_unix(),
                finished: None,
                last_try: None,
                final_path: None,
                speed: 0.0,
                connections: 0,
                chunks: vec![],
                request: req,
            };
            st.items.insert(id, item);
            st.dirty = true;
        }
        self.emit(Event::Added(id));
        if opts.start_now {
            self.start(id);
        }
        id
    }

    /// Find an existing item with the same URL.
    pub fn find_by_url(&self, url: &str) -> Option<ItemId> {
        self.inner.state.lock().unwrap().items.values().find(|i| i.request.url == url).map(|i| i.id)
    }

    /// Start (or resume) a download immediately.
    pub fn start(&self, id: ItemId) {
        let _rt = self.inner.rt.enter();
        let mut st = self.inner.state.lock().unwrap();
        if st.running.contains_key(&id) || st.quota_until.is_some() {
            return;
        }
        let settings = st.settings.clone();
        let Some(item) = st.items.get_mut(&id) else { return };
        if item.state == ItemState::Complete {
            return;
        }
        let mut req = item.request.clone();
        if req.username.is_none() {
            if let Some(l) = settings.login_for(&req.url) {
                req.username = Some(l.username.clone());
                req.password = Some(l.password.clone());
            }
        }
        let mut cfg = settings.engine.clone();
        cfg.proxy = Some(settings.proxy.mode.clone()).filter(|m| !m.is_empty());
        cfg.proxy_user = settings.proxy.username.clone();
        cfg.proxy_pass = settings.proxy.password.clone();
        let temp = settings.temp_dir.join(id.to_string());
        let dl = Download::start(req, cfg, temp, self.inner.limiter.clone());
        item.state = ItemState::Running;
        item.last_try = Some(now_unix());
        item.speed = 0.0;
        let handle = dl.handle();
        let progress = dl.progress.clone();
        st.running.insert(id, Running { handle, progress, counted: 0 });
        st.dirty = true;
        drop(st);
        self.emit(Event::Changed(id));
        let me = self.clone();
        tokio::spawn(async move {
            let res = dl.wait().await;
            me.finished(id, res);
        });
    }

    /// Pause a running download (keeps its progress) or take it out of the queue.
    pub fn stop(&self, id: ItemId) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(r) = st.running.get(&id) {
            r.handle.pause();
        } else if let Some(it) = st.items.get_mut(&id) {
            if it.state == ItemState::Queued {
                it.state = ItemState::Stopped;
                st.dirty = true;
            }
        }
        drop(st);
        self.emit(Event::Changed(id));
    }

    pub fn stop_all(&self) {
        let ids: Vec<ItemId> = self.inner.state.lock().unwrap().running.keys().copied().collect();
        for id in ids {
            self.stop(id);
        }
    }

    /// Resume every unfinished download.
    pub fn start_all(&self) {
        let ids: Vec<ItemId> = self
            .inner
            .state
            .lock()
            .unwrap()
            .items
            .values()
            .filter(|i| matches!(i.state, ItemState::Stopped | ItemState::Failed(_) | ItemState::Queued))
            .map(|i| i.id)
            .collect();
        for id in ids {
            self.start(id);
        }
    }

    /// Remove from the list; optionally delete the downloaded file.
    pub fn remove(&self, id: ItemId, delete_file: bool) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(r) = st.running.remove(&id) {
            r.handle.cancel();
        }
        let temp = st.settings.temp_dir.join(id.to_string());
        if let Some(it) = st.items.remove(&id) {
            if delete_file {
                if let Some(p) = it.final_path {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        let _ = std::fs::remove_dir_all(temp);
        st.dirty = true;
        drop(st);
        self.emit(Event::Removed(id));
    }

    /// Re-download from scratch (e.g. expired link replaced by a new URL).
    pub fn redownload(&self, id: ItemId, new_url: Option<String>) {
        {
            let mut st = self.inner.state.lock().unwrap();
            if st.running.contains_key(&id) {
                return;
            }
            let temp = st.settings.temp_dir.join(id.to_string());
            let _ = std::fs::remove_dir_all(temp);
            let Some(it) = st.items.get_mut(&id) else { return };
            if let Some(u) = new_url {
                it.request.url = u;
            }
            it.state = ItemState::Stopped;
            it.downloaded = 0;
            it.final_path = None;
            st.dirty = true;
        }
        self.start(id);
    }

    /// Update the URL of an unfinished download (link refresh) keeping its progress.
    pub fn set_url(&self, id: ItemId, url: String) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(it) = st.items.get_mut(&id) {
            it.request.url = url.clone();
            st.dirty = true;
        }
        // The saved state is keyed by URL; rewrite it so resume continues.
        let p = st.settings.temp_dir.join(id.to_string()).join("state.json");
        if let Ok(data) = std::fs::read(&p) {
            if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&data) {
                v["url"] = serde_json::Value::String(url);
                let _ = std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap_or_default());
            }
        }
    }

    /// Per-download speed limit; applies immediately if running.
    pub fn set_item_speed_limit(&self, id: ItemId, limit: Option<u64>) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(r) = st.running.get(&id) {
            r.handle.set_speed_limit(limit);
        }
        if let Some(it) = st.items.get_mut(&id) {
            it.request.speed_limit = limit;
            st.dirty = true;
        }
    }

    pub fn set_description(&self, id: ItemId, text: String) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(it) = st.items.get_mut(&id) {
            it.description = text;
            st.dirty = true;
        }
    }

    pub fn global_limiter(&self) -> Arc<Limiter> {
        self.inner.limiter.clone()
    }

    pub fn set_queue(&self, id: ItemId, queue: u32) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(it) = st.items.get_mut(&id) {
            it.queue = queue;
            if it.state == ItemState::Stopped {
                it.state = ItemState::Queued;
            }
            st.dirty = true;
        }
    }

    pub fn start_queue(&self, q: u32) {
        let mut st = self.inner.state.lock().unwrap();
        st.active_queues.insert(q);
        if q == SYNC_QUEUE {
            st.synced.clear();
        }
        for it in st.items.values_mut() {
            if it.queue == q && matches!(it.state, ItemState::Stopped | ItemState::Failed(_)) {
                it.state = ItemState::Queued;
            }
        }
        st.dirty = true;
    }

    pub fn stop_queue(&self, q: u32) {
        let ids: Vec<ItemId> = {
            let mut st = self.inner.state.lock().unwrap();
            st.active_queues.remove(&q);
            st.items.values().filter(|i| i.queue == q && i.state == ItemState::Running).map(|i| i.id).collect()
        };
        for id in ids {
            self.stop(id);
        }
    }

    fn finished(&self, id: ItemId, res: Result<Option<PathBuf>, crate::DownloadError>) {
        let mut ev = vec![Event::Changed(id)];
        let mut scan: Option<(String, PathBuf)> = None;
        {
            let mut st = self.inner.state.lock().unwrap();
            let last = st.running.remove(&id).map(|r| r.progress.borrow().clone());
            let scan_cmd = st.settings.virus_scan_command.clone();
            if let Some(it) = st.items.get_mut(&id) {
                if let Some(p) = &last {
                    apply_progress(it, p);
                }
                it.speed = 0.0;
                it.connections = 0;
                match res {
                    Ok(Some(path)) => {
                        it.state = ItemState::Complete;
                        it.finished = Some(now_unix());
                        it.downloaded = it.total.unwrap_or(it.downloaded);
                        if let Some(n) = path.file_name() {
                            it.file_name = n.to_string_lossy().into_owned();
                        }
                        it.final_path = Some(path.clone());
                        ev.push(Event::Completed(id));
                        if let Some(cmd) = scan_cmd {
                            scan = Some((cmd, path));
                        }
                    }
                    Ok(None) => it.state = ItemState::Stopped,
                    Err(crate::DownloadError::Cancelled) => it.state = ItemState::Stopped,
                    Err(e) => {
                        it.state = ItemState::Failed(e.to_string());
                        ev.push(Event::Failed(id, e.to_string()));
                    }
                }
            }
            st.dirty = true;
        }
        if let Some((cmd, path)) = scan {
            run_virus_scan(&cmd, &path);
        }
        for e in ev {
            self.emit(e);
        }
    }

    /// Background loop: progress polling, queues, scheduler, quota, saving.
    async fn background(self) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut sched_tick = Instant::now();
        loop {
            tick.tick().await;
            self.poll_progress();
            if sched_tick.elapsed() >= Duration::from_secs(1) {
                sched_tick = Instant::now();
                self.run_scheduler();
                self.check_quota();
            }
            self.run_queues();
            self.save_if_dirty(false);
        }
    }

    fn poll_progress(&self) {
        let mut changed = vec![];
        {
            let mut st = self.inner.state.lock().unwrap();
            let st = &mut *st;
            let now = now_unix();
            for (id, r) in st.running.iter_mut() {
                let p = r.progress.borrow().clone();
                if let Some(it) = st.items.get_mut(id) {
                    apply_progress(it, &p);
                    changed.push(*id);
                }
                if p.downloaded > r.counted {
                    st.quota_log.push_back((now, p.downloaded - r.counted));
                    r.counted = p.downloaded;
                }
            }
        }
        for id in changed {
            self.emit(Event::Changed(id));
        }
    }

    /// Has the server copy of a completed download changed? (newer Last-Modified or other size)
    pub async fn check_update(&self, id: ItemId) -> Result<bool, crate::DownloadError> {
        let (req, cfg, path, total) = {
            let st = self.inner.state.lock().unwrap();
            let it = st.items.get(&id).ok_or_else(|| crate::DownloadError::Other("no such item".into()))?;
            (it.request.clone(), st.settings.engine.clone(), it.final_path.clone(), it.total)
        };
        let info = crate::download::probe_info(&req, &cfg).await?;
        let Some(path) = path.filter(|p| p.exists()) else { return Ok(true) };
        if let (Some(server), Some(local)) = (info.total, total) {
            if server != local {
                return Ok(true);
            }
        }
        let local_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok().map(chrono::DateTime::<chrono::Utc>::from);
        let server_mtime = info
            .last_modified
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc2822(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        Ok(matches!((server_mtime, local_mtime), (Some(s), Some(l)) if s > l))
    }

    /// Synchronization queue: re-check completed items and re-download changed ones.
    fn run_sync(&self) {
        let ids: Vec<ItemId> = {
            let mut st = self.inner.state.lock().unwrap();
            if !st.active_queues.contains(&SYNC_QUEUE) {
                return;
            }
            let ids: Vec<ItemId> = st
                .items
                .values()
                .filter(|i| i.queue == SYNC_QUEUE && i.state == ItemState::Complete && !st.synced.contains(&i.id))
                .map(|i| i.id)
                .collect();
            st.synced.extend(ids.iter().copied());
            ids
        };
        for id in ids {
            let me = self.clone();
            tokio::spawn(async move {
                if let Ok(true) = me.check_update(id).await {
                    {
                        let mut st = me.inner.state.lock().unwrap();
                        if let Some(it) = st.items.get_mut(&id) {
                            it.state = ItemState::Queued;
                            it.downloaded = 0;
                            it.request.file_name = it.final_path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                            if let Some(dir) = it.final_path.as_ref().and_then(|p| p.parent()) {
                                it.request.dir = dir.to_path_buf();
                            }
                            st.dirty = true;
                        }
                    }
                    me.emit(Event::Changed(id));
                }
            });
        }
    }

    fn run_queues(&self) {
        self.run_sync();
        let mut to_start = vec![];
        let mut done = vec![];
        {
            let mut st = self.inner.state.lock().unwrap();
            if st.quota_until.is_some() {
                return;
            }
            let queues = st.settings.queues.clone();
            for q in &queues {
                if !st.active_queues.contains(&q.id) {
                    continue;
                }
                let running = st.items.values().filter(|i| i.queue == q.id && i.state == ItemState::Running).count();
                let waiting: Vec<ItemId> = st.items.values().filter(|i| i.queue == q.id && i.state == ItemState::Queued).map(|i| i.id).collect();
                if running == 0 && waiting.is_empty() {
                    done.push((q.id, q.schedule.on_done));
                    continue;
                }
                for id in waiting.into_iter().take(q.max_concurrent.saturating_sub(running)) {
                    to_start.push(id);
                }
            }
            for (q, _) in &done {
                st.active_queues.remove(q);
            }
        }
        for id in to_start {
            self.start(id);
        }
        for (q, action) in done {
            self.emit(Event::QueueDone(q, action));
        }
    }

    fn run_scheduler(&self) {
        let now = Local::now();
        let hhmm = format!("{:02}:{:02}", now.hour(), now.minute());
        let day = now.weekday().num_days_from_monday() as usize;
        let minute_key = now.timestamp() / 60;
        let mut start = vec![];
        let mut stop = vec![];
        {
            let mut st = self.inner.state.lock().unwrap();
            let queues = st.settings.queues.clone();
            for q in queues {
                let s = &q.schedule;
                if !s.enabled {
                    continue;
                }
                let day_ok = !s.days.iter().any(|d| *d) || s.days[day];
                if !day_ok {
                    continue;
                }
                for (is_start, at) in [(true, &s.start_at), (false, &s.stop_at)] {
                    if at.as_deref() == Some(hhmm.as_str()) && st.sched_fired.get(&(q.id, is_start)) != Some(&minute_key) {
                        st.sched_fired.insert((q.id, is_start), minute_key);
                        if is_start { start.push(q.id) } else { stop.push(q.id) }
                    }
                }
                if s.once && start.contains(&q.id) {
                    if let Some(qd) = st.settings.queues.iter_mut().find(|x| x.id == q.id) {
                        qd.schedule.enabled = false;
                    }
                }
            }
        }
        for q in start {
            self.start_queue(q);
        }
        for q in stop {
            self.stop_queue(q);
        }
    }

    fn check_quota(&self) {
        let mut exceeded = None;
        let mut resume = false;
        {
            let mut st = self.inner.state.lock().unwrap();
            let q = st.settings.quota.clone();
            let now = now_unix();
            if let Some(until) = st.quota_until {
                if now >= until {
                    st.quota_until = None;
                    resume = true;
                }
            } else if q.enabled && q.hours > 0 {
                let window = i64::from(q.hours) * 3600;
                while st.quota_log.front().is_some_and(|(t, _)| *t < now - window) {
                    st.quota_log.pop_front();
                }
                let used: u64 = st.quota_log.iter().map(|(_, b)| b).sum();
                if used > q.megabytes * 1024 * 1024 {
                    let until = st.quota_log.front().map_or(now, |(t, _)| *t) + window;
                    st.quota_until = Some(until);
                    exceeded = Some(until);
                }
            }
        }
        if let Some(until) = exceeded {
            self.stop_all();
            self.emit(Event::QuotaExceeded(until));
        }
        if resume {
            self.start_all();
        }
    }

    pub fn save_if_dirty(&self, force: bool) {
        let mut st = self.inner.state.lock().unwrap();
        if !(st.dirty && (force || st.last_save.elapsed() >= Duration::from_secs(2))) {
            return;
        }
        let list = SavedList { next_id: st.next_id, items: st.items.values().cloned().collect() };
        st.dirty = false;
        st.last_save = Instant::now();
        drop(st);
        let dir = &self.inner.data_dir;
        let _ = std::fs::create_dir_all(dir);
        let tmp = dir.join("downloads.json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&list).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, dir.join("downloads.json"));
        }
    }

    /// Pause everything and save; call before exiting.
    pub async fn shutdown(&self) {
        self.stop_all();
        for _ in 0..50 {
            if self.inner.state.lock().unwrap().running.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.inner.state.lock().unwrap().dirty = true;
        self.save_if_dirty(true);
    }
}

fn apply_progress(it: &mut Item, p: &Progress) {
    if !p.file_name.is_empty() {
        it.file_name = p.file_name.clone();
    }
    it.total = p.total.or(it.total);
    it.downloaded = p.downloaded.max(if p.status == Status::Connecting { it.downloaded } else { 0 });
    it.speed = p.speed;
    it.connections = p.connections;
    it.resumable = p.resumable;
    it.chunks = p.chunks.clone();
}

fn run_virus_scan(cmd: &str, path: &Path) {
    let line = cmd.replace("{file}", &path.to_string_lossy());
    let mut parts = shell_words(&line).into_iter();
    if let Some(prog) = parts.next() {
        let _ = std::process::Command::new(prog).args(parts).spawn();
    }
}

/// Minimal command-line splitting with double quotes.
fn shell_words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut q) = (vec![], String::new(), false);
    for c in s.chars() {
        match c {
            '"' => q = !q,
            ' ' if !q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
