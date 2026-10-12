//! Registers the native-messaging bridge (`rdm-bridge`) with installed browsers.
//!
//! Chromium browsers find hosts through a JSON manifest (on Windows referenced from the
//! registry, elsewhere placed in a well-known folder). Firefox uses a separate manifest
//! listing allowed extension ids.

use serde_json::json;
use std::path::PathBuf;

pub const HOST_NAME: &str = "com.rdm.bridge";
/// Chromium extension id derived from the public key in `extension/manifest.json`.
pub const CHROME_EXTENSION_ID: &str = include_str!("../../../extension/chrome-id.txt");
pub const FIREFOX_EXTENSION_ID: &str = "bridge@rdm.app";

fn bridge_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let name = if cfg!(windows) { "rdm-bridge.exe" } else { "rdm-bridge" };
    let p = exe.with_file_name(name);
    if p.exists() { Ok(p) } else { Err(std::io::Error::other(format!("{} not found next to the app", name))) }
}

pub fn register() -> std::io::Result<String> {
    let exe = bridge_exe()?;
    let dir = rdm_core::settings::data_dir().join("native-messaging");
    std::fs::create_dir_all(&dir)?;
    let chrome = json!({
        "name": HOST_NAME,
        "description": "RDM browser bridge",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{}/", CHROME_EXTENSION_ID.trim())],
    });
    let firefox = json!({
        "name": HOST_NAME,
        "description": "RDM browser bridge",
        "path": exe,
        "type": "stdio",
        "allowed_extensions": [FIREFOX_EXTENSION_ID],
    });
    let chrome_file = dir.join("chrome.json");
    let firefox_file = dir.join("firefox.json");
    std::fs::write(&chrome_file, serde_json::to_vec_pretty(&chrome)?)?;
    std::fs::write(&firefox_file, serde_json::to_vec_pretty(&firefox)?)?;

    let mut done = vec![];
    #[cfg(target_os = "windows")]
    {
        let keys = [
            (r"HKCU\Software\Google\Chrome\NativeMessagingHosts", &chrome_file, "Chrome"),
            (r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts", &chrome_file, "Edge"),
            (r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts", &chrome_file, "Brave"),
            (r"HKCU\Software\Vivaldi\NativeMessagingHosts", &chrome_file, "Vivaldi"),
            (r"HKCU\Software\Chromium\NativeMessagingHosts", &chrome_file, "Chromium"),
            (r"HKCU\Software\Mozilla\NativeMessagingHosts", &firefox_file, "Firefox"),
        ];
        for (k, f, name) in keys {
            let ok = std::process::Command::new("reg")
                .args(["add", &format!(r"{k}\{HOST_NAME}"), "/ve", "/t", "REG_SZ", "/d", &f.display().to_string(), "/f"])
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                done.push(name);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = dirs::home_dir().unwrap_or_default();
        let targets: Vec<(PathBuf, &PathBuf, &str)> = if cfg!(target_os = "macos") {
            let base = home.join("Library/Application Support");
            vec![
                (base.join("Google/Chrome/NativeMessagingHosts"), &chrome_file, "Chrome"),
                (base.join("Microsoft Edge/NativeMessagingHosts"), &chrome_file, "Edge"),
                (base.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"), &chrome_file, "Brave"),
                (base.join("Chromium/NativeMessagingHosts"), &chrome_file, "Chromium"),
                (base.join("Mozilla/NativeMessagingHosts"), &firefox_file, "Firefox"),
            ]
        } else {
            let cfg = home.join(".config");
            vec![
                (cfg.join("google-chrome/NativeMessagingHosts"), &chrome_file, "Chrome"),
                (cfg.join("microsoft-edge/NativeMessagingHosts"), &chrome_file, "Edge"),
                (cfg.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"), &chrome_file, "Brave"),
                (cfg.join("chromium/NativeMessagingHosts"), &chrome_file, "Chromium"),
                (home.join(".mozilla/native-messaging-hosts"), &firefox_file, "Firefox"),
            ]
        };
        for (d, f, name) in targets {
            if std::fs::create_dir_all(&d).is_ok() && std::fs::copy(f, d.join(format!("{HOST_NAME}.json"))).is_ok() {
                done.push(name);
            }
        }
    }
    Ok(format!("Registered for: {}", done.join(", ")))
}
