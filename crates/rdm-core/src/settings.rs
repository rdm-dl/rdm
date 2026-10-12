//! User settings: folders, categories, queues, scheduler, browser-capture lists.

use crate::config::EngineConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Category {
    pub name: String,
    /// Lower-case extensions without dots.
    pub extensions: Vec<String>,
    /// Folder for this category; empty = `<download dir>/<name>`.
    pub folder: Option<PathBuf>,
}

impl Category {
    fn new(name: &str, exts: &str) -> Self {
        Self { name: name.into(), extensions: exts.split_whitespace().map(str::to_string).collect(), folder: None }
    }
}

pub fn default_categories() -> Vec<Category> {
    vec![
        Category::new("Compressed", "zip rar 7z gz gzip tar bz2 xz z lzh arj ace sit sitx sea iso img cab tgz zst"),
        Category::new("Documents", "pdf doc docx xls xlsx ppt pptx pps txt rtf odt ods odp epub csv md"),
        Category::new("Music", "mp3 wav wma aac m4a flac ogg opus aif aiff ra mpa"),
        Category::new("Programs", "exe msi msu msix appx apk dmg pkg deb rpm appimage bin run jar"),
        Category::new("Video", "mp4 mkv avi mov wmv flv webm m4v mpg mpeg mpe 3gp ogv rm rmvb qt ts vob f4v"),
    ]
}

/// What to do when a queue finishes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum OnQueueDone {
    #[default]
    Nothing,
    ExitApp,
    Sleep,
    Hibernate,
    Shutdown,
    /// Disconnect dial-up / VPN connections ("hang up the modem").
    HangUp,
}

impl OnQueueDone {
    /// Every action offered on this system.
    pub fn all() -> Vec<Self> {
        let mut v = vec![Self::Nothing, Self::ExitApp, Self::Sleep, Self::Hibernate, Self::Shutdown];
        if cfg!(windows) {
            v.push(Self::HangUp);
        }
        v
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Nothing => "Do nothing",
            Self::ExitApp => "Exit RDM",
            Self::Sleep => "Put the computer to sleep",
            Self::Hibernate => "Hibernate the computer",
            Self::Shutdown => "Shut down the computer",
            Self::HangUp => "Hang up (disconnect dial-up / VPN)",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    pub enabled: bool,
    /// "HH:MM" local time to start the queue.
    pub start_at: Option<String>,
    /// "HH:MM" local time to stop the queue.
    pub stop_at: Option<String>,
    /// Monday..Sunday; all false = every day.
    pub days: [bool; 7],
    /// Start only once (then disable).
    pub once: bool,
    pub on_done: OnQueueDone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueDef {
    pub id: u32,
    pub name: String,
    /// Downloads running at the same time in this queue.
    pub max_concurrent: usize,
    pub schedule: Schedule,
}

pub const MAIN_QUEUE: u32 = 1;
pub const SYNC_QUEUE: u32 = 2;

pub fn default_queues() -> Vec<QueueDef> {
    vec![
        QueueDef { id: MAIN_QUEUE, name: "Main queue".into(), max_concurrent: 4, schedule: Schedule::default() },
        QueueDef { id: SYNC_QUEUE, name: "Synchronization queue".into(), max_concurrent: 1, schedule: Schedule::default() },
    ]
}

/// Lists pushed to the browser extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CaptureSettings {
    pub enabled: bool,
    /// File types taken over automatically.
    pub extensions: Vec<String>,
    /// Media types that show the "download this video" button.
    pub media_extensions: Vec<String>,
    /// Hosts never captured (`*.example.com` wildcards).
    pub exception_hosts: Vec<String>,
    /// URL patterns never captured.
    pub exception_urls: Vec<String>,
    /// Don't capture files smaller than this (bytes, 0 = no minimum).
    pub min_size: u64,
    /// Never treated as downloads, even when the server says "attachment".
    pub ignore_extensions: Vec<String>,
    /// MIME types that mean "this is a download" (combined with the extension list).
    pub download_mimes: Vec<String>,
    /// Show the floating "Download this video" button over videos.
    pub show_video_button: bool,
    /// Also show it over audio players.
    pub show_audio_button: bool,
    /// Hosts where the video button never appears.
    pub no_button_hosts: Vec<String>,
    /// Key that forces a capture while clicking a link: none, alt, ctrl, shift, insert.
    pub force_key: String,
    /// Key that lets the browser download instead: none, alt, ctrl, shift, delete.
    pub skip_key: String,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        let list = |s: &str| s.split_whitespace().map(str::to_string).collect();
        Self {
            enabled: true,
            extensions: list(
                "3gp 7z aac ace aif apk arj asf avi bin bz2 doc docx exe flv gz gzip img iso lzh m4a m4v mkv mov mp3 mp4 \
                 mpa mpe mpeg mpg msi msu ogg ogv pdf pps ppt qt r0* r1* ra rar rm rmvb sea sit sitx tar tif tiff wav \
                 webm wma wmv z zip",
            ),
            media_extensions: list("flv mp3 mp4 m4v f4v m4a mpg mpeg avi wmv wma wav asf rm ogg ogv mov 3gp qt webm ts mkv aac m3u8 mpd"),
            exception_hosts: list("*.update.microsoft.com download.windowsupdate.com *.download.windowsupdate.com"),
            exception_urls: vec![],
            min_size: 0,
            ignore_extensions: list("txt htm html css js json apng gif ico jpeg jpg png svg webp"),
            download_mimes: list("application/download application/force-download application/x-msdownload application/octet-stream binary/octet-stream"),
            show_video_button: true,
            show_audio_button: true,
            no_button_hosts: list("*.gstatic.com media.tenor.co media*.giphy.com"),
            force_key: "alt".into(),
            skip_key: "ctrl".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProxySettings {
    /// `none`, `system`, or a proxy URL like `http://host:port` / `socks5://host:port`.
    pub mode: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Hosts that bypass the proxy.
    pub bypass: Vec<String>,
}

/// A saved login for a site or path prefix (`https://example.com/private/`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SiteLogin {
    pub prefix: String,
    pub username: String,
    pub password: String,
}

/// Download quota: stop when more than `megabytes` were downloaded within `hours`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Quota {
    pub enabled: bool,
    pub megabytes: u64,
    pub hours: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub categories: Vec<Category>,
    pub queues: Vec<QueueDef>,
    pub capture: CaptureSettings,
    pub proxy: ProxySettings,
    pub logins: Vec<SiteLogin>,
    pub quota: Quota,
    /// Global speed limit in bytes/s.
    pub speed_limit: Option<u64>,
    pub engine: EngineConfig,
    /// Show the "download complete" dialog.
    pub show_complete_dialog: bool,
    /// Ask before starting downloads captured from the browser.
    pub confirm_captured: bool,
    pub start_with_system: bool,
    pub minimize_to_tray: bool,
    pub monitor_clipboard: bool,
    /// Antivirus command run after completion; `{file}` is replaced by the path.
    pub virus_scan_command: Option<String>,
    pub language: String,
    pub dark_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let download_dir = dirs::download_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"));
        Self {
            temp_dir: data_dir().join("temp"),
            download_dir,
            categories: default_categories(),
            queues: default_queues(),
            capture: CaptureSettings::default(),
            proxy: ProxySettings { mode: "system".into(), ..Default::default() },
            logins: vec![],
            quota: Quota { enabled: false, megabytes: 1024, hours: 4 },
            speed_limit: None,
            engine: EngineConfig::default(),
            show_complete_dialog: true,
            confirm_captured: true,
            start_with_system: false,
            minimize_to_tray: true,
            monitor_clipboard: false,
            virus_scan_command: None,
            language: "en".into(),
            dark_mode: true,
        }
    }
}

impl Settings {
    /// Category for a file name, by extension (`None` = General).
    pub fn category_for(&self, file_name: &str) -> Option<&Category> {
        let ext = file_name.rsplit_once('.')?.1.to_ascii_lowercase();
        self.categories.iter().find(|c| c.extensions.iter().any(|e| e == &ext))
    }

    /// Folder for a category name (`General` -> the download dir itself).
    pub fn folder_for(&self, category: Option<&str>) -> PathBuf {
        match category.and_then(|n| self.categories.iter().find(|c| c.name == n)) {
            Some(c) => c.folder.clone().unwrap_or_else(|| self.download_dir.join(&c.name)),
            None => self.download_dir.clone(),
        }
    }

    pub fn login_for(&self, url: &str) -> Option<&SiteLogin> {
        self.logins.iter().filter(|l| url.starts_with(&l.prefix)).max_by_key(|l| l.prefix.len())
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }
}

/// Per-user data folder (`%APPDATA%\RDM`, `~/.local/share/rdm`, `~/Library/Application Support/RDM`).
pub fn data_dir() -> PathBuf {
    // A separate profile (tests, portable use): RDM_DATA_DIR=<folder>.
    if let Some(d) = std::env::var_os("RDM_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "linux") { base.join("rdm") } else { base.join("RDM") }
}

/// Simple wildcard match (`*` any run, `?` one char), case-insensitive.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_by_extension() {
        let s = Settings::default();
        assert_eq!(s.category_for("a.ZIP").unwrap().name, "Compressed");
        assert_eq!(s.category_for("movie.mkv").unwrap().name, "Video");
        assert!(s.category_for("noext").is_none());
        assert_eq!(s.folder_for(Some("Video")), s.download_dir.join("Video"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.example.com", "dl.example.com"));
        assert!(wildcard_match("r0*", "r01"));
        assert!(!wildcard_match("*.example.com", "example.org"));
    }

    #[test]
    fn longest_login_prefix_wins() {
        let mut s = Settings::default();
        s.logins = vec![
            SiteLogin { prefix: "https://a.com/".into(), username: "u1".into(), password: "p".into() },
            SiteLogin { prefix: "https://a.com/private/".into(), username: "u2".into(), password: "p".into() },
        ];
        assert_eq!(s.login_for("https://a.com/private/x.zip").unwrap().username, "u2");
    }
}
