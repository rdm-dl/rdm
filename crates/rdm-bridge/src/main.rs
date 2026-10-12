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
    /// Skip the confirmation dialog.
    silent: bool,
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

/// Connect to the app, starting it (minimized) if needed.
async fn connect() -> Option<Client> {
    let dir = data_dir();
    if let Ok(mut c) = Client::connect(&dir).await {
        if c.call(Request::Ping).await.is_ok() {
            return Some(c);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let app = exe.with_file_name(if cfg!(windows) { "rdm-gui.exe" } else { "rdm-gui" });
    std::process::Command::new(app).arg("--minimized").spawn().ok()?;
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
    if client.is_none() {
        *client = connect().await;
    }
    let Some(c) = client.as_mut() else {
        return json!({"type": "error", "message": "RDM is not running"});
    };
    let req = match kind.as_str() {
        "hello" | "settings" => Request::Capture,
        "download" => {
            let d: Download = serde_json::from_value(msg).unwrap_or_default();
            Request::Add { request: d.request(), start_now: d.silent, interactive: !d.silent }
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
        other => return json!({"type": "error", "message": format!("unknown message {other}")}),
    };
    match c.call(req).await {
        Ok(Response::Capture { settings }) => json!({"type": "settings", "version": env!("CARGO_PKG_VERSION"), "capture": settings}),
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
}
