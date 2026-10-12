//! Registers the native-messaging bridge (`rdm-bridge`) with installed browsers.
//!
//! Chromium browsers find hosts through a JSON manifest (on Windows referenced from the
//! registry, elsewhere placed in a well-known folder). Firefox uses a separate manifest
//! listing allowed extension ids. `add_extension` asks browsers to install the extension
//! itself (store entries for Chromium browsers, the bundled signed .xpi for Firefox).

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
        "allowed_origins": chromium_ids().iter().map(|id| format!("chrome-extension://{id}/")).collect::<Vec<_>>(),
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
    let added = add_extension();
    let mut msg = format!("Bridge registered for: {}.", done.join(", "));
    if !added.is_empty() {
        msg.push_str(&format!(" Extension offered to: {} (the browser asks you to enable it).", added.join(", ")));
    }
    Ok(msg)
}

/// Store ids filled in after publishing (`extension/store-ids.json`).
fn store_ids() -> (Option<String>, Option<String>) {
    let v: serde_json::Value = serde_json::from_str(include_str!("../../../extension/store-ids.json")).unwrap_or_default();
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    (get("chrome_web_store"), get("edge_addons"))
}

/// Every Chromium extension id the bridge accepts: the self-signed build plus store builds.
fn chromium_ids() -> Vec<String> {
    let (cws, edge) = store_ids();
    let mut ids = vec![CHROME_EXTENSION_ID.trim().to_string()];
    ids.extend(cws);
    ids.extend(edge);
    ids.dedup();
    ids
}

const CWS_UPDATE: &str = "https://clients2.google.com/service/update2/crx";
const EDGE_UPDATE: &str = "https://edge.microsoft.com/extensionwebstorebase/v1/crx";
#[cfg_attr(windows, allow(dead_code))]
const FIREFOX_APP_ID: &str = "{ec8030f7-c20a-464f-9b0e-13a3a9e97384}";

fn firefox_xpi() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.with_file_name("rdm-firefox.xpi");
    p.exists().then_some(p)
}

/// Ask installed browsers to add the extension, the way installers do: Chromium browsers
/// get an "external extension" entry pointing at the store (the browser downloads it and
/// asks the user to enable it), Firefox gets the bundled signed .xpi.
pub fn add_extension() -> Vec<&'static str> {
    let (cws, edge) = store_ids();
    let mut done = vec![];
    #[cfg(target_os = "windows")]
    {
        let reg = |args: &[&str]| std::process::Command::new("reg").args(args).output().is_ok_and(|o| o.status.success());
        if let Some(id) = &cws {
            for (base, name) in [
                (r"HKCU\Software\Google\Chrome\Extensions", "Chrome"),
                (r"HKCU\Software\BraveSoftware\Brave-Browser\Extensions", "Brave"),
                (r"HKCU\Software\Vivaldi\Extensions", "Vivaldi"),
                (r"HKCU\Software\Chromium\Extensions", "Chromium"),
            ] {
                if reg(&["add", &format!(r"{base}\{id}"), "/v", "update_url", "/t", "REG_SZ", "/d", CWS_UPDATE, "/f"]) {
                    done.push(name);
                }
            }
        }
        if let Some(id) = &edge {
            if reg(&["add", &format!(r"HKCU\Software\Microsoft\Edge\Extensions\{id}"), "/v", "update_url", "/t", "REG_SZ", "/d", EDGE_UPDATE, "/f"]) {
                done.push("Edge");
            }
        }
        if let Some(xpi) = firefox_xpi() {
            if reg(&["add", r"HKCU\Software\Mozilla\Firefox\Extensions", "/v", FIREFOX_EXTENSION_ID, "/t", "REG_SZ", "/d", &xpi.display().to_string(), "/f"]) {
                done.push("Firefox");
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = dirs::home_dir().unwrap_or_default();
        let write_json = |dir: PathBuf, id: &str, url: &str| -> bool {
            std::fs::create_dir_all(&dir).is_ok()
                && std::fs::write(dir.join(format!("{id}.json")), json!({ "external_update_url": url }).to_string()).is_ok()
        };
        let (chrome_dirs, edge_dir, ff_dir): (Vec<(PathBuf, &'static str)>, PathBuf, PathBuf) = if cfg!(target_os = "macos") {
            let base = home.join("Library/Application Support");
            (
                vec![
                    (base.join("Google/Chrome/External Extensions"), "Chrome"),
                    (base.join("BraveSoftware/Brave-Browser/External Extensions"), "Brave"),
                    (base.join("Chromium/External Extensions"), "Chromium"),
                ],
                base.join("Microsoft Edge/External Extensions"),
                base.join("Mozilla/Extensions").join(FIREFOX_APP_ID),
            )
        } else {
            // Linux browsers read external extensions only from system folders.
            (
                vec![(PathBuf::from("/usr/share/google-chrome/extensions"), "Chrome"), (PathBuf::from("/usr/share/chromium/extensions"), "Chromium")],
                PathBuf::from("/usr/share/microsoft-edge/extensions"),
                home.join(".mozilla/extensions").join(FIREFOX_APP_ID),
            )
        };
        if let Some(id) = &cws {
            for (d, name) in chrome_dirs {
                if write_json(d, id, CWS_UPDATE) {
                    done.push(name);
                }
            }
        }
        if let Some(id) = &edge {
            if write_json(edge_dir, id, EDGE_UPDATE) {
                done.push("Edge");
            }
        }
        if let Some(xpi) = firefox_xpi() {
            if std::fs::create_dir_all(&ff_dir).is_ok() && std::fs::copy(&xpi, ff_dir.join(format!("{FIREFOX_EXTENSION_ID}.xpi"))).is_ok() {
                done.push("Firefox");
            }
        }
    }
    done
}

/// Undo `register` and `add_extension`.
pub fn unregister() -> String {
    let (cws, edge) = store_ids();
    #[cfg(target_os = "windows")]
    {
        let del = |k: &str, v: Option<&str>| {
            let mut args = vec!["delete", k];
            if let Some(v) = v {
                args.extend(["/v", v]);
            }
            args.push("/f");
            let _ = std::process::Command::new("reg").args(&args).output();
        };
        for base in [
            r"HKCU\Software\Google\Chrome",
            r"HKCU\Software\Microsoft\Edge",
            r"HKCU\Software\BraveSoftware\Brave-Browser",
            r"HKCU\Software\Vivaldi",
            r"HKCU\Software\Chromium",
            r"HKCU\Software\Mozilla",
        ] {
            del(&format!(r"{base}\NativeMessagingHosts\{HOST_NAME}"), None);
            for id in cws.iter().chain(edge.iter()) {
                del(&format!(r"{base}\Extensions\{id}"), None);
            }
        }
        del(r"HKCU\Software\Mozilla\Firefox\Extensions", Some(FIREFOX_EXTENSION_ID));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = dirs::home_dir().unwrap_or_default();
        let (base, ff) = if cfg!(target_os = "macos") {
            let b = home.join("Library/Application Support");
            (b.clone(), b.join("Mozilla/Extensions").join(FIREFOX_APP_ID))
        } else {
            (home.join(".config"), home.join(".mozilla/extensions").join(FIREFOX_APP_ID))
        };
        for sub in [
            "Google/Chrome", "google-chrome", "Microsoft Edge", "microsoft-edge", "BraveSoftware/Brave-Browser", "Chromium", "chromium", "Mozilla",
        ] {
            let _ = std::fs::remove_file(base.join(sub).join("NativeMessagingHosts").join(format!("{HOST_NAME}.json")));
            for id in cws.iter().chain(edge.iter()) {
                let _ = std::fs::remove_file(base.join(sub).join("External Extensions").join(format!("{id}.json")));
            }
        }
        let _ = std::fs::remove_file(home.join(".mozilla/native-messaging-hosts").join(format!("{HOST_NAME}.json")));
        let _ = std::fs::remove_file(ff.join(format!("{FIREFOX_EXTENSION_ID}.xpi")));
    }
    "Browser integration removed.".into()
}
