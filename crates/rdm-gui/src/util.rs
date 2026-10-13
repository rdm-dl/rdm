//! Formatting and OS helpers.

use rdm_core::settings::OnQueueDone;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.2} {}", U[i]) }
}

pub fn speed(bps: f64) -> String {
    if bps < 1.0 { String::new() } else { format!("{}/s", size(bps as u64)) }
}

pub fn duration(d: Option<Duration>) -> String {
    let Some(d) = d else { return String::new() };
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} sec"),
        60..3600 => format!("{} min {} sec", s / 60, s % 60),
        3600..86400 => format!("{} hr {} min", s / 3600, (s % 3600) / 60),
        _ => format!("{} days {} hr", s / 86400, (s % 86400) / 3600),
    }
}

pub fn date(unix: Option<i64>) -> String {
    use chrono::TimeZone;
    unix.and_then(|t| chrono::Local.timestamp_opt(t, 0).single())
        .map(|d| d.format("%b %d %H:%M").to_string())
        .unwrap_or_default()
}

/// Parse "500", "500K", "2M" into bytes.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (n, m) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024.0),
        'M' => (&s[..s.len() - 1], 1024.0 * 1024.0),
        'G' => (&s[..s.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (s, 1.0),
    };
    n.trim().parse::<f64>().ok().map(|v| (v * m) as u64)
}

/// A file shipped with the app: next to the executable, or in `Contents/Resources` when
/// running from a macOS app bundle.
pub fn bundled(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(name), dir.join("../Resources").join(name)].into_iter().find(|p| p.exists())
}

pub fn open_file(p: &Path) {
    let _ = open::that_detached(p);
}

/// Open the containing folder, selecting the file where the OS supports it.
pub fn open_folder(p: &Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // Explorer wants `/select,"path"` exactly; normal argument quoting would wrap the
        // whole thing and make it open the default folder instead.
        let path = p.to_string_lossy().replace('/', "\\");
        if p.exists() {
            let _ = Command::new("explorer").raw_arg(format!("/select,\"{path}\"")).spawn();
        } else if let Some(dir) = p.parent().filter(|d| d.exists()) {
            let _ = Command::new("explorer").raw_arg(format!("\"{}\"", dir.to_string_lossy().replace('/', "\\"))).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(p).spawn();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(d) = p.parent() {
            let _ = open::that_detached(d);
        }
    }
}

/// Perform an end-of-queue power action.
pub fn system_action(a: OnQueueDone) {
    let run = |prog: &str, args: &[&str]| {
        let _ = Command::new(prog).args(args).spawn();
    };
    match a {
        OnQueueDone::Nothing | OnQueueDone::ExitApp => {}
        OnQueueDone::Shutdown => {
            if cfg!(windows) {
                run("shutdown", &["/s", "/t", "0"]);
            } else if cfg!(target_os = "macos") {
                run("osascript", &["-e", "tell app \"System Events\" to shut down"]);
            } else {
                run("systemctl", &["poweroff"]);
            }
        }
        OnQueueDone::Sleep => {
            if cfg!(windows) {
                run("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]);
            } else if cfg!(target_os = "macos") {
                run("pmset", &["sleepnow"]);
            } else {
                run("systemctl", &["suspend"]);
            }
        }
        OnQueueDone::HangUp => {
            // Ends all dial-up, PPPoE and built-in VPN connections.
            if cfg!(windows) {
                run("rasdial", &["/disconnect"]);
            }
        }
        OnQueueDone::Hibernate => {
            if cfg!(windows) {
                run("shutdown", &["/h"]);
            } else if cfg!(target_os = "macos") {
                run("pmset", &["sleepnow"]);
            } else {
                run("systemctl", &["hibernate"]);
            }
        }
    }
}

/// Register / unregister the app to start with the user session.
pub fn set_autostart(enable: bool) {
    let Ok(exe) = std::env::current_exe() else { return };
    #[cfg(target_os = "windows")]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        if enable {
            let _ = Command::new("reg")
                .args(["add", key, "/v", "RDM", "/t", "REG_SZ", "/d", &format!("\"{}\" --minimized", exe.display()), "/f"])
                .output();
        } else {
            let _ = Command::new("reg").args(["delete", key, "/v", "RDM", "/f"]).output();
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(cfg) = dirs::config_dir() {
            let dir = cfg.join("autostart");
            let file = dir.join("rdm.desktop");
            if enable {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(
                    file,
                    format!("[Desktop Entry]\nType=Application\nName=RDM\nExec=\"{}\" --minimized\nX-GNOME-Autostart-enabled=true\n", exe.display()),
                );
            } else {
                let _ = std::fs::remove_file(file);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            let file = home.join("Library/LaunchAgents/com.rdm.app.plist");
            if enable {
                let _ = std::fs::write(
                    file,
                    format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>com.rdm.app</string><key>ProgramArguments</key><array><string>{}</string><string>--minimized</string></array><key>RunAtLoad</key><true/></dict></plist>\n",
                        exe.display()
                    ),
                );
            } else {
                let _ = std::fs::remove_file(file);
            }
        }
    }
}

/// Does the text look like a downloadable link we capture?
pub fn clipboard_link(text: &str, exts: &[String]) -> Option<String> {
    let t = text.trim();
    if t.contains(char::is_whitespace) || !(t.starts_with("http://") || t.starts_with("https://") || t.starts_with("ftp://")) {
        return None;
    }
    let url = url::Url::parse(t).ok()?;
    let last = url.path_segments()?.next_back()?.to_ascii_lowercase();
    let ext = last.rsplit_once('.')?.1.to_string();
    exts.iter()
        .any(|e| rdm_core::settings::wildcard_match(e, &ext))
        .then(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(size(1536), "1.50 KB");
        assert_eq!(duration(Some(Duration::from_secs(125))), "2 min 5 sec");
        assert_eq!(parse_size("2M"), Some(2 * 1024 * 1024));
        let exts = vec!["zip".to_string(), "r0*".to_string()];
        assert!(clipboard_link("https://a.com/x.zip", &exts).is_some());
        assert!(clipboard_link("https://a.com/x.html", &exts).is_none());
        assert!(clipboard_link("https://a.com/x.r01", &exts).is_some());
    }
}

/// Error text for people: common HTTP statuses and network failures in plain words, with
/// the technical detail kept short.
pub fn friendly_error(e: &str) -> String {
    let code = e.strip_prefix("server returned status ").and_then(|s| s.split_whitespace().next()).and_then(|c| c.parse::<u16>().ok());
    if let Some(c) = code {
        let what = match c {
            401 => "The server wants a login",
            403 => "The server refused access",
            404 => "File not found on the server",
            410 => "The file was removed from the server",
            416 => "The server can't resume this file",
            429 => "The server says too many requests; try again later",
            500..=599 => "The server had a problem",
            _ => "The server refused the download",
        };
        return format!("{what} ({c})");
    }
    let lower = e.to_ascii_lowercase();
    if lower.contains("dns") || lower.contains("failed to lookup") || lower.contains("no such host") {
        return "Couldn't find the server (check the address or your connection)".into();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "The server stopped responding (timed out)".into();
    }
    if lower.contains("connection refused") || lower.contains("error sending request") {
        return "Couldn't connect to the server".into();
    }
    if lower.contains("no space") || lower.contains("disk full") {
        return "Not enough disk space".into();
    }
    e.to_string()
}
