//! Updates: find the latest release on GitHub and put its files in place of this copy's.
//!
//! Installing works on the files next to the running program: each one that is replaced is
//! first renamed to `<name>.old` (Windows allows renaming a running program, not writing
//! over it), then the new file is copied in. If anything fails, the old files are put back.
//! The `.old` files are removed the next time RDM starts ([`remove_leftovers`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// GitHub repository the releases come from.
pub const REPO: &str = "rdm-dl/rdm";

#[derive(Debug, Clone)]
pub struct Release {
    /// Version without the leading "v", e.g. "0.1.2".
    pub version: String,
    /// Release notes (Markdown).
    pub notes: String,
    /// The release's web page.
    pub page: String,
    /// The download for this system, if the release has one.
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// The `<name>.sha256` file published next to it (releases before 0.1.3 have none).
    pub sha256_url: Option<String>,
}

/// End of the release file name for this build ("…-windows-x64.zip" and so on).
pub fn platform_suffix() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("windows-x64.zip")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x64.tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-arm64.dmg")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("macos-x64.dmg")
    } else {
        None
    }
}

/// End of the name of the Windows setup program.
const SETUP_SUFFIX: &str = "windows-x64-setup.exe";

/// Whether this copy came from the setup program, as opposed to being unpacked somewhere. Such a
/// copy is updated by the next setup, which also keeps its entry in Windows' app list right.
pub fn from_setup() -> bool {
    cfg!(windows) && std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("unins000.exe").exists())).unwrap_or(false)
}

/// What to pass the setup program for a quiet update. It finds this copy by itself, whatever
/// folder it's in, and keeps its install mode, asking for admin rights if that was for all users.
pub const SETUP_ARGS: [&str; 4] = ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/relaunch=1"];

/// True when this system's update is a disk image to open (macOS) rather than files to swap in.
pub fn opens_installer() -> bool {
    cfg!(target_os = "macos")
}

/// The latest published release.
pub async fn latest() -> Result<Release, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("RDM/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Couldn't reach GitHub: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    parse_release_for(&v, if from_setup() { Some(SETUP_SUFFIX) } else { platform_suffix() }).filter(|r| r.asset.is_some()).or_else(|| parse_release(&v)).ok_or_else(|| "Unexpected answer from GitHub".into())
}

fn parse_release(v: &serde_json::Value) -> Option<Release> {
    parse_release_for(v, platform_suffix())
}

/// The release, with the file ending in `suffix`.
fn parse_release_for(v: &serde_json::Value, suffix: Option<&str>) -> Option<Release> {
    let tag = v["tag_name"].as_str()?;
    let asset = v["assets"].as_array().and_then(|list| {
        list.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            suffix.filter(|s| name.ends_with(s))?;
            let checksum = format!("{name}.sha256");
            let sha256_url = list.iter().find(|c| c["name"].as_str() == Some(&checksum)).and_then(|c| c["browser_download_url"].as_str()).map(str::to_string);
            Some(Asset { name: name.to_string(), url: a["browser_download_url"].as_str()?.to_string(), size: a["size"].as_u64().unwrap_or(0), sha256_url })
        })
    });
    Some(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes: v["body"].as_str().unwrap_or_default().to_string(),
        page: v["html_url"].as_str().unwrap_or_default().to_string(),
        asset,
    })
}

/// `candidate` is a later version than `current` ("0.1.10" > "0.1.9").
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v').split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// The checksum published for a download.
pub async fn fetch_sha256(url: &str) -> Result<String, String> {
    let fail = |e: String| format!("Couldn't get the update's checksum: {e}");
    let client = reqwest::Client::builder().user_agent(concat!("RDM/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(20)).build().map_err(|e| fail(e.to_string()))?;
    let resp = client.get(url).send().await.map_err(|e| fail(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(fail(resp.status().to_string()));
    }
    let text = resp.text().await.map_err(|e| fail(e.to_string()))?;
    let hash = text.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) { Ok(hash) } else { Err(fail("unreadable file".into())) }
}

/// Check a downloaded file against its checksum. Run off the async workers: this reads the file.
pub fn verify_sha256(file: &Path, expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    std::io::copy(&mut f, &mut hash).map_err(|e| e.to_string())?;
    let got = hash.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got == expected { Ok(()) } else { Err("The downloaded update is damaged. Please try again.".into()) }
}

/// Unpack `archive` (the release .zip / .tar.gz) in `work` and put its files into `app_dir`.
pub fn install(archive: &Path, app_dir: &Path, work: &Path) -> Result<(), String> {
    let unpacked = work.join("unpacked");
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    extract(archive, &unpacked)?;
    // The archive holds one folder, RDM-<version>-<platform>/.
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&unpacked).map_err(|e| e.to_string())?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    let root = if entries.len() == 1 && entries[0].is_dir() { entries.remove(0) } else { unpacked.clone() };
    if !root.join(exe_name("rdm-gui")).exists() {
        return Err("The update package doesn't contain RDM.".into());
    }
    let mut swapped: Vec<(PathBuf, Option<PathBuf>)> = vec![];
    let result = copy_tree(&root, app_dir, &mut swapped);
    if let Err(e) = result {
        // Put everything back as it was.
        for (target, old) in swapped.into_iter().rev() {
            let _ = std::fs::remove_file(&target);
            if let Some(old) = old {
                let _ = std::fs::rename(&old, &target);
            }
        }
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&unpacked);
    Ok(())
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) { format!("{stem}.exe") } else { stem.to_string() }
}

fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    let status = if archive.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let script = format!(
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                archive.display().to_string().replace('\'', "''"),
                into.display().to_string().replace('\'', "''")
            );
            std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).creation_flags(CREATE_NO_WINDOW).status()
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("unzip").arg("-oq").arg(archive).arg("-d").arg(into).status()
        }
    } else {
        std::process::Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(into).status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("Unpacking the update failed ({s})")),
        Err(e) => Err(format!("Unpacking the update failed: {e}")),
    }
}

/// Copy `from` into `to`, moving each existing file aside first. Records what was swapped.
fn copy_tree(from: &Path, to: &Path, swapped: &mut Vec<(PathBuf, Option<PathBuf>)>) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst, swapped)?;
            continue;
        }
        let old = if dst.exists() {
            let old = old_name(&dst);
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&dst, &old).map_err(|e| permission_hint(&dst, e))?;
            Some(old)
        } else {
            None
        };
        swapped.push((dst.clone(), old));
        std::fs::copy(&src, &dst).map_err(|e| permission_hint(&dst, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(&src) {
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(m.permissions().mode()));
            }
        }
    }
    Ok(())
}

fn old_name(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

fn permission_hint(p: &Path, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        format!("RDM isn't allowed to change {} (the folder may need administrator rights). Download the update from the website instead.", p.display())
    } else {
        format!("{}: {e}", p.display())
    }
}

/// Remove the `.old` files a previous update left next to the program.
pub fn remove_leftovers(app_dir: &Path) {
    fn walk(dir: &Path, depth: u32) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && depth < 3 {
                walk(&p, depth + 1);
            } else if p.extension().is_some_and(|x| x == "old") {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    walk(app_dir, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_from_setup_takes_the_setup_and_others_the_archive() {
        let v = serde_json::json!({"tag_name": "v0.2.0", "assets": [
            {"name": "RDM-0.2.0-windows-x64.zip", "browser_download_url": "https://x/z"},
            {"name": "RDM-0.2.0-windows-x64-setup.exe", "browser_download_url": "https://x/s"},
            {"name": "RDM-0.2.0-windows-x64-setup.exe.sha256", "browser_download_url": "https://x/s.sha256"},
        ]});
        let setup = parse_release_for(&v, Some(SETUP_SUFFIX)).unwrap().asset.unwrap();
        assert_eq!(setup.url, "https://x/s");
        assert_eq!(setup.sha256_url.as_deref(), Some("https://x/s.sha256"));
        assert_eq!(parse_release_for(&v, Some("windows-x64.zip")).unwrap().asset.unwrap().url, "https://x/z");
    }

    #[test]
    fn checksums_are_found_and_verified() {
        let assets: Vec<serde_json::Value> = ["windows-x64.zip", "linux-x64.tar.gz", "macos-arm64.dmg", "macos-x64.dmg"]
            .iter()
            .flat_map(|s| {
                [
                    serde_json::json!({"name": format!("RDM-{s}"), "browser_download_url": format!("https://x/{s}")}),
                    serde_json::json!({"name": format!("RDM-{s}.sha256"), "browser_download_url": format!("https://x/{s}.sha256")}),
                ]
            })
            .collect();
        let release = parse_release(&serde_json::json!({"tag_name": "v0.2.0", "assets": assets})).unwrap();
        if let Some(asset) = release.asset {
            assert!(!asset.url.ends_with(".sha256"));
            assert_eq!(asset.sha256_url, Some(format!("{}.sha256", asset.url)));
        }
        let file = std::env::temp_dir().join(format!("rdm_sha_{}", std::process::id()));
        std::fs::write(&file, b"abc").unwrap();
        assert!(verify_sha256(&file, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").is_ok());
        assert!(verify_sha256(&file, &"0".repeat(64)).is_err());
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.2", "0.1.5"));
    }

    #[test]
    fn release_json_picks_this_systems_file() {
        let v = serde_json::json!({
            "tag_name": "v0.2.0",
            "body": "Notes",
            "html_url": "https://github.com/rdm-dl/rdm/releases/tag/v0.2.0",
            "assets": [
                {"name": "RDM-0.2.0-windows-x64.zip", "browser_download_url": "https://x/w.zip", "size": 10},
                {"name": "RDM-0.2.0-windows-x64.zip.sha256", "browser_download_url": "https://x/w.zip.sha256", "size": 90},
                {"name": "RDM-0.2.0-linux-x64.tar.gz.sha256", "browser_download_url": "https://x/l.tar.gz.sha256", "size": 90},
                {"name": "RDM-0.2.0-macos-arm64.dmg.sha256", "browser_download_url": "https://x/a.dmg.sha256", "size": 90},
                {"name": "RDM-0.2.0-macos-x64.dmg.sha256", "browser_download_url": "https://x/i.dmg.sha256", "size": 90},
                {"name": "RDM-0.2.0-linux-x64.tar.gz", "browser_download_url": "https://x/l.tar.gz", "size": 11},
                {"name": "RDM-0.2.0-macos-arm64.dmg", "browser_download_url": "https://x/a.dmg", "size": 12},
                {"name": "RDM-0.2.0-macos-x64.dmg", "browser_download_url": "https://x/i.dmg", "size": 13}
            ]
        });
        let r = parse_release(&v).unwrap();
        assert_eq!(r.version, "0.2.0");
        if let Some(suffix) = platform_suffix() {
            assert!(r.asset.unwrap().name.ends_with(suffix));
        }
    }

    #[test]
    fn install_swaps_files_and_keeps_the_old_ones_aside() {
        let base = std::env::temp_dir().join(format!("rdm_update_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (app, pkg, work) = (base.join("app"), base.join("pkg"), base.join("work"));
        let root = pkg.join("RDM-9.9.9-test");
        std::fs::create_dir_all(root.join("extension-chrome")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(root.join(exe_name("rdm-gui")), "new gui").unwrap();
        std::fs::write(root.join("extension-chrome").join("manifest.json"), "new ext").unwrap();
        std::fs::write(app.join(exe_name("rdm-gui")), "old gui").unwrap();
        // Pack it the way releases are packed.
        let archive = base.join(if cfg!(windows) { "u.zip" } else { "u.tar.gz" });
        let ok = if cfg!(windows) {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &format!("Compress-Archive -Path '{}' -DestinationPath '{}'", root.display(), archive.display())])
                .status()
        } else {
            std::process::Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&pkg).arg("RDM-9.9.9-test").status()
        };
        assert!(ok.unwrap().success());
        install(&archive, &app, &work).unwrap();
        assert_eq!(std::fs::read_to_string(app.join(exe_name("rdm-gui"))).unwrap(), "new gui");
        assert_eq!(std::fs::read_to_string(app.join("extension-chrome").join("manifest.json")).unwrap(), "new ext");
        assert!(old_name(&app.join(exe_name("rdm-gui"))).exists());
        remove_leftovers(&app);
        assert!(!old_name(&app.join(exe_name("rdm-gui"))).exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
