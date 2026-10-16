//! Updates in the app: looks for a newer RDM on GitHub, downloads it with RDM's own engine
//! and puts the new files in place; the user restarts when it suits them. On macOS the new
//! disk image is opened instead (drag RDM to Applications).

use rdm_core::update::{self, Release};
use rdm_core::{Download, DownloadRequest, EngineConfig, Limiter, Progress};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// First automatic check after start-up, then this often.
const FIRST_CHECK: Duration = Duration::from_secs(8);
const EVERY: Duration = Duration::from_secs(12 * 3600);

#[derive(Clone)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release, watch::Receiver<Progress>),
    Installing(Release),
    /// The new files are in place; restarting RDM starts the new version.
    Ready(Release),
    /// macOS: the new disk image was opened.
    Opened(Release),
    Failed(String, Option<Release>),
}

/// This program's version. RDM_PRETEND_VERSION overrides it to try the update steps.
pub fn current_version() -> String {
    std::env::var("RDM_PRETEND_VERSION").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

pub struct Updater {
    state: Arc<Mutex<State>>,
    started: Instant,
    last_check: Option<Instant>,
    /// The program as it was started (an update renames the running file).
    exe: Option<PathBuf>,
    /// "Later": the bar stays hidden until RDM restarts or a check is asked for.
    pub hidden: bool,
}

impl Updater {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(State::Idle)), started: Instant::now(), last_check: None, exe: std::env::current_exe().ok(), hidden: false }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// Automatic checks (when enabled in Options).
    pub fn tick(&mut self, rt: &tokio::runtime::Handle, enabled: bool) {
        if !enabled || !matches!(self.state(), State::Idle | State::UpToDate | State::Failed(_, None)) {
            return;
        }
        let due = match self.last_check {
            None => self.started.elapsed() >= FIRST_CHECK,
            Some(t) => t.elapsed() >= EVERY,
        };
        if due {
            self.check(rt);
        }
    }

    pub fn check(&mut self, rt: &tokio::runtime::Handle) {
        self.last_check = Some(Instant::now());
        self.hidden = false;
        *self.state.lock().unwrap() = State::Checking;
        let state = self.state.clone();
        rt.spawn(async move {
            let next = match update::latest().await {
                Ok(r) if update::is_newer(&r.version, &current_version()) => State::Available(r),
                Ok(_) => State::UpToDate,
                Err(e) => State::Failed(e, None),
            };
            *state.lock().unwrap() = next;
        });
    }

    /// Updates can be put in place here (not a development build inside a Cargo `target` folder).
    pub fn can_install(&self) -> bool {
        self.exe.as_ref().is_some_and(|e| !e.components().any(|c| c.as_os_str() == "target"))
    }

    /// Download the update and install it (or open it, on macOS).
    pub fn start(&mut self, rt: &tokio::runtime::Handle, data_dir: PathBuf) {
        let State::Available(release) = self.state() else { return };
        let Some(asset) = release.asset.clone() else {
            *self.state.lock().unwrap() = State::Failed("This release has no download for your system.".into(), Some(release));
            return;
        };
        let app_dir = self.exe.as_ref().and_then(|e| e.parent()).map(PathBuf::from);
        let state = self.state.clone();
        rt.spawn(async move {
            let dir = data_dir.join("updates");
            let _ = std::fs::remove_dir_all(&dir);
            let req = DownloadRequest { url: asset.url.clone(), dir: dir.clone(), file_name: Some(asset.name.clone()), ..Default::default() };
            let dl = Download::start(req, EngineConfig::default(), dir.join("parts"), Arc::new(Limiter::default()));
            *state.lock().unwrap() = State::Downloading(release.clone(), dl.progress.clone());
            let file = match dl.wait().await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    *state.lock().unwrap() = State::Failed("The download stopped.".into(), Some(release));
                    return;
                }
                Err(e) => {
                    *state.lock().unwrap() = State::Failed(format!("Downloading the update failed: {e}"), Some(release));
                    return;
                }
            };
            if update::opens_installer() {
                let _ = open::that_detached(&file);
                *state.lock().unwrap() = State::Opened(release);
                return;
            }
            let Some(app_dir) = app_dir else {
                *state.lock().unwrap() = State::Failed("Couldn't find RDM's folder.".into(), Some(release));
                return;
            };
            *state.lock().unwrap() = State::Installing(release.clone());
            let work = dir.join("work");
            let res = tokio::task::spawn_blocking(move || update::install(&file, &app_dir, &work)).await;
            *state.lock().unwrap() = match res {
                Ok(Ok(())) => {
                    // The downloaded package isn't needed any more.
                    let _ = std::fs::remove_dir_all(&dir);
                    State::Ready(release)
                }
                Ok(Err(e)) => State::Failed(e, Some(release)),
                Err(e) => State::Failed(e.to_string(), Some(release)),
            };
        });
    }

    /// Ask for the new version to be started once this one has closed (the caller closes the
    /// window). Starting it only after this copy is fully gone means any version can start
    /// normally, whatever it does when it finds RDM already running.
    pub fn restart(&self) -> bool {
        let Some(exe) = self.exe.clone() else { return false };
        RESTART.set(exe).is_ok() || RESTART.get().is_some()
    }
}

static RESTART: OnceLock<PathBuf> = OnceLock::new();

/// Called by main after RDM has shut down: start the updated program if a restart was asked for.
pub fn start_new_version_if_requested() {
    if let Some(exe) = RESTART.get() {
        let _ = std::process::Command::new(exe).arg("--after-update").spawn();
    }
}
