#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Native messaging host: browser extension <-> running RDM app.
//!
//! Messages are JSON objects framed with a 4-byte little-endian length on stdin/stdout.

use rdm_core::DownloadRequest;
use rdm_core::ipc::{Client, Request, Response};
use rdm_core::settings::data_dir;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::time::Duration;

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Download {
    url: String,
    referrer: Option<String>,
    cookies: Option<String>,
    filename: Option<String>,
    user_agent: Option<String>,
    post_data: Option<String>,
    title: Option<String>,
    kind: Option<String>,
    /// Extra request headers the browser sent for this URL (e.g. `Origin` of an embedded
    /// player), as `{name: value}`.
    headers: Option<std::collections::BTreeMap<String, String>>,
    /// Skip the confirmation dialog.
    silent: bool,
    /// Wait in the queue: a show's episodes shouldn't all start at once.
    queued: bool,
    /// A folder inside the default one.
    folder: Option<String>,
}

impl Download {
    fn request(&self) -> DownloadRequest {
        DownloadRequest {
            url: self.url.clone(),
            referer: self.referrer.clone().filter(|s| !s.is_empty()),
            cookies: self.cookies.clone().filter(|s| !s.is_empty()),
            file_name: self.filename.clone().filter(|s| !s.is_empty()),
            user_agent: self.user_agent.clone().filter(|s| !s.is_empty()),
            post_data: self.post_data.clone(),
            subfolder: self.folder.clone().filter(|s| !s.is_empty()),
            headers: self
                .headers
                .iter()
                .flatten()
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            ..Default::default()
        }
    }
}

fn read_message(stdin: &mut impl Read) -> Option<Value> {
    let mut len = [0u8; 4];
    stdin.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    if n > 64 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; n];
    stdin.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_message(stdout: &mut impl Write, v: &Value) {
    let data = serde_json::to_vec(v).unwrap_or_default();
    let _ = stdout.write_all(&(data.len() as u32).to_le_bytes());
    let _ = stdout.write_all(&data);
    let _ = stdout.flush();
}

/// Start the app so that it outlives this helper, which ends after every message.
///
/// Firefox and browsers built on it (Zen, LibreWolf, Waterfox) run a helper inside a job on
/// Windows and end the whole job when the helper exits, and signal its process group
/// elsewhere. So the app leaves the job, or gets a group of its own. It also doesn't keep
/// this helper's pipes to the browser.
fn start_app(app: &std::path::Path, args: &[&str]) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut c = Command::new(app);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        keep_pipes_from_children();
        c.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = c.spawn() {
            return Ok(child);
        }
        // A job that doesn't allow leaving refuses the start: go without.
        c.creation_flags(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    c.spawn()
}

/// New processes inherit every inheritable handle, whatever their own stdin and stdout are.
#[cfg(windows)]
fn keep_pipes_from_children() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    for h in [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()] {
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
    }
}

/// Connect to the app; start it (minimized) only when `launch` is set, i.e. for something
/// the user did (a download, "Open RDM"), never for the extension's background checks.
async fn connect(launch: bool) -> Option<Client> {
    let dir = data_dir();
    if let Ok(mut c) = Client::connect(&dir).await {
        if c.call(Request::Ping).await.is_ok() {
            return Some(c);
        }
    }
    if !launch {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let app = exe.with_file_name(if cfg!(windows) { "rdm-gui.exe" } else { "rdm-gui" });
    start_app(&app, &["--minimized"]).ok()?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(mut c) = Client::connect(&dir).await {
            if c.call(Request::Ping).await.is_ok() {
                return Some(c);
            }
        }
    }
    None
}

async fn handle(client: &mut Option<Client>, msg: Value) -> Value {
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    if kind == "diag" {
        let dir = data_dir().join("diagnostics");
        let ok = std::fs::create_dir_all(&dir).is_ok()
            && std::fs::write(dir.join("extension.json"), serde_json::to_vec_pretty(msg.get("data").unwrap_or(&Value::Null)).unwrap_or_default()).is_ok();
        return json!({"type": if ok { "ok" } else { "error" }});
    }
    if client.is_none() {
        let user_action = matches!(kind.as_str(), "download" | "links" | "media" | "show");
        *client = connect(user_action).await;
    }
    let Some(c) = client.as_mut() else {
        return json!({"type": "error", "message": "RDM is not running"});
    };
    let req = match kind.as_str() {
        "hello" | "settings" => Request::Capture,
        "download" => {
            let d: Download = serde_json::from_value(msg).unwrap_or_default();
            Request::Add { request: d.request(), start_now: d.silent && !d.queued, interactive: !d.silent && !d.queued }
        }
        "links" => {
            let links: Vec<Download> = msg.get("links").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
            Request::AddMany { requests: links.iter().map(Download::request).collect() }
        }
        "media" => {
            let d: Download = serde_json::from_value(msg).unwrap_or_default();
            Request::Media { request: d.request(), title: d.title.clone(), kind: d.kind.clone().unwrap_or_else(|| "video".into()) }
        }
        "show" => Request::Show,
        "episodes" => Request::Episodes { url: msg.get("url").and_then(Value::as_str).unwrap_or_default().to_string() },
        "start-queue" => Request::StartQueue,
        "sign-in" => Request::SignIn { list: msg.get("list").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default() },
        "site" => Request::Site {
            url: msg.get("url").and_then(Value::as_str).unwrap_or_default().to_string(),
            cookies: msg.get("cookies").and_then(Value::as_str).map(String::from),
        },
        other => return json!({"type": "error", "message": format!("unknown message {other}")}),
    };
    match c.call(req).await {
        Ok(Response::Episodes { list }) => json!({"type": "episodes", "list": list}),
        Ok(Response::Capture { settings, sites, sign_ins, browser_sites }) => {
            let mut capture = json!(settings);
            capture["sites"] = json!(sites);
            capture["sign_ins"] = json!(sign_ins);
            capture["browser_sites"] = json!(browser_sites);
            json!({"type": "settings", "version": env!("CARGO_PKG_VERSION"), "capture": capture})
        }
        Ok(Response::Site { url, title, duration }) => json!({"type": "site", "url": url, "title": title, "duration": duration}),
        Ok(Response::Added { id }) => json!({"type": "accepted", "id": id}),
        Ok(Response::Ok) => json!({"type": "ok"}),
        Ok(Response::Error { message }) => json!({"type": "error", "message": message}),
        Ok(other) => json!({"type": "ok", "detail": format!("{other:?}")}),
        Err(e) => {
            *client = None;
            json!({"type": "error", "message": e.to_string()})
        }
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut client: Option<Client> = None;
    while let Some(msg) = read_message(&mut stdin) {
        let id = msg.get("id").cloned();
        let mut reply = rt.block_on(handle(&mut client, msg));
        if let (Some(id), Some(obj)) = (id, reply.as_object_mut()) {
            obj.insert("id".into(), id);
        }
        write_message(&mut stdout, &reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"type": "hello"}));
        let back = read_message(&mut buf.as_slice()).unwrap();
        assert_eq!(back["type"], "hello");
    }

    /// A Firefox-based browser ends its job when the helper exits. An app left in that job
    /// dies a moment after it starts.
    #[cfg(windows)]
    #[test]
    fn app_starts_outside_the_browsers_job() {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        // A helper in a job set up the way Firefox sets it up, which then starts the app.
        let cmd = std::path::PathBuf::from(std::env::var_os("ComSpec").unwrap());
        let in_job = |child: &std::process::Child, job| {
            let mut yes = 0;
            assert_ne!(unsafe { IsProcessInJob(child.as_raw_handle(), job, &mut yes) }, 0);
            yes != 0
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            let size = std::mem::size_of_val(&info) as u32;
            assert_ne!(SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const info).cast(), size), 0);
            assert_ne!(AssignProcessToJobObject(job, GetCurrentProcess()), 0);

            let mut ordinary = std::process::Command::new(&cmd).args(["/c", "ping -n 2 127.0.0.1 >nul"]).creation_flags(0).spawn().unwrap();
            let mut app = start_app(&cmd, &["/c", "ping -n 2 127.0.0.1 >nul"]).unwrap();
            assert!(in_job(&ordinary, job), "the test's job isn't working");
            assert!(!in_job(&app, job), "the app stayed in the browser's job");
            let _ = (ordinary.wait(), app.wait());
            CloseHandle(job);
        }
    }
}
