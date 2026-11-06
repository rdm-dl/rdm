//! Local control channel between the running app and helper processes (CLI, browser bridge).
//!
//! The app listens on `127.0.0.1:<random port>` and writes `{port, token}` to `ipc.json`
//! in the data folder. Every request is one JSON line carrying the token; every reply is
//! one JSON line.

use crate::download::DownloadRequest;
use crate::manager::{AddOptions, ItemId, ItemState, Manager};
use crate::settings::CaptureSettings;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

/// When the browser extension last talked to RDM (unix seconds; 0 = not this session).
static LAST_EXTENSION_CONTACT: AtomicI64 = AtomicI64::new(0);

/// Unix time of the browser extension's last contact in this session, if any.
pub fn last_extension_contact() -> Option<i64> {
    let v = LAST_EXTENSION_CONTACT.load(Ordering::Relaxed);
    (v > 0).then_some(v)
}
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const IPC_FILE: &str = "ipc.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Add a download. `interactive` = it came from the browser; the UI may ask first.
    Add { request: DownloadRequest, start_now: bool, interactive: bool },
    /// Add several links (batch / "download all links").
    AddMany { requests: Vec<DownloadRequest> },
    List,
    /// Bring the main window to the front.
    Show,
    /// Lists the browser extension needs.
    Capture,
    /// Media found in a page, offered to the user (video panel).
    Media { request: DownloadRequest, title: Option<String>, kind: String },
    /// What a site with its own code offers for a page ([`crate::sites`]).
    Site { url: String, cookies: Option<String> },
    /// The episodes of a show's page, for the extension to play one after another.
    Episodes { url: String },
    /// Start what's waiting in the main queue.
    StartQueue,
    /// The browser's sign-in cookies for sites with their own code: (domain, cookie).
    SignIn { list: Vec<(String, String)> },
    /// Close the app, even when it would stay in the tray. Setup uses this.
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemSummary {
    pub id: ItemId,
    pub file_name: String,
    pub url: String,
    pub state: String,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub speed: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong { version: String },
    Ok,
    Added { id: Option<ItemId> },
    Items { items: Vec<ItemSummary> },
    Capture {
        settings: CaptureSettings,
        /// Domains the extension should ask about, with [`Request::Site`].
        #[serde(default)]
        sites: Vec<String>,
        /// Which sign-in cookie to hand over for which domain, with [`Request::SignIn`].
        #[serde(default)]
        sign_ins: Vec<(String, String)>,
        /// Domains where the extension plays a show's episodes itself.
        #[serde(default)]
        browser_sites: Vec<String>,
    },
    /// (address, folder, file name) of each episode.
    Episodes { list: Vec<(String, String, String)> },
    Site { url: String, title: Option<String>, duration: Option<u64> },
    Error { message: String },
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

#[derive(Serialize, Deserialize)]
struct IpcInfo {
    port: u16,
    token: String,
    pid: u32,
}

/// Requests the server forwards to the UI.
#[derive(Debug, Clone)]
pub enum UiRequest {
    Show,
    Quit,
    /// Ask the user (file-info dialog) before adding.
    ConfirmAdd(DownloadRequest),
    ConfirmMany(Vec<DownloadRequest>),
    Media { request: DownloadRequest, title: Option<String>, kind: String },
}

fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut s = String::new();
    for i in 0..4u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i ^ std::process::id() as u64);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        s.push_str(&format!("{:016x}", h.finish()));
    }
    s
}

/// Start the server. UI-bound requests are sent to `ui` (if any); without a UI,
/// downloads are added directly.
pub async fn serve(manager: Manager, ui: Option<mpsc::UnboundedSender<UiRequest>>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let info = IpcInfo { port, token: token.clone(), pid: std::process::id() };
    let path = manager.data_dir().join(IPC_FILE);
    std::fs::create_dir_all(manager.data_dir())?;
    std::fs::write(&path, serde_json::to_vec(&info)?)?;
    tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = listener.accept().await else { break };
            if !peer.ip().is_loopback() {
                continue;
            }
            let (m, ui, token) = (manager.clone(), ui.clone(), token.clone());
            tokio::spawn(async move {
                let _ = handle(sock, m, ui, token).await;
            });
        }
    });
    Ok(())
}

async fn handle(sock: TcpStream, m: Manager, ui: Option<mpsc::UnboundedSender<UiRequest>>, token: String) -> std::io::Result<()> {
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let resp = match serde_json::from_str::<Envelope>(&line) {
            Ok(Envelope { token: t, request: Request::Site { url, cookies } }) if t == token => {
                // Takes a few requests to the site, so it runs here and not with the quick ones.
                match crate::sites::find_for_page(&url, cookies, &m.settings().engine).await {
                    Ok(f) => Response::Site { url: f.url, title: f.title, duration: f.duration },
                    Err(e) => Response::Error { message: e.to_string() },
                }
            }
            Ok(Envelope { token: t, request: Request::Episodes { url } }) if t == token => match crate::sites::episodes(&url, None, &m.settings().engine).await {
                Ok(list) => Response::Episodes { list: list.iter().map(|e| (e.url.clone(), e.folder().to_string_lossy().replace('\\', "/"), e.file_name())).collect() },
                Err(e) => Response::Error { message: e.to_string() },
            },
            Ok(env) if env.token == token => dispatch(&m, ui.as_ref(), env.request),
            Ok(_) => Response::Error { message: "bad token".into() },
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_vec(&resp).unwrap_or_default();
        out.push(b'\n');
        w.write_all(&out).await?;
    }
    Ok(())
}

fn dispatch(m: &Manager, ui: Option<&mpsc::UnboundedSender<UiRequest>>, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong { version: env!("CARGO_PKG_VERSION").into() },
        Request::Show => {
            if let Some(ui) = ui {
                let _ = ui.send(UiRequest::Show);
            }
            Response::Ok
        }
        Request::Quit => {
            if let Some(ui) = ui {
                let _ = ui.send(UiRequest::Quit);
            }
            Response::Ok
        }
        Request::Add { request, start_now, interactive } => {
            let ask = interactive && m.settings().confirm_captured;
            match (ask, ui) {
                (true, Some(ui)) => {
                    let _ = ui.send(UiRequest::ConfirmAdd(request));
                    Response::Added { id: None }
                }
                _ => {
                    let id = m.add(request, AddOptions { start_now, ..Default::default() });
                    Response::Added { id: Some(id) }
                }
            }
        }
        Request::AddMany { requests } => {
            match ui {
                Some(ui) => {
                    let _ = ui.send(UiRequest::ConfirmMany(requests));
                }
                None => {
                    for r in requests {
                        m.add(r, AddOptions::default());
                    }
                }
            }
            Response::Ok
        }
        Request::List => Response::Items {
            items: m
                .items()
                .into_iter()
                .map(|i| ItemSummary {
                    id: i.id,
                    file_name: i.file_name,
                    url: i.request.url,
                    state: match i.state {
                        ItemState::Queued => "queued".into(),
                        ItemState::Stopped => "stopped".into(),
                        ItemState::Running => "downloading".into(),
                        ItemState::Complete => "complete".into(),
                        ItemState::Failed(e) => format!("failed: {e}"),
                    },
                    total: i.total,
                    downloaded: i.downloaded,
                    speed: i.speed,
                })
                .collect(),
        },
        Request::Capture => {
            // The extension asks for its settings when it starts and every few minutes.
            LAST_EXTENSION_CONTACT.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
            let mut s = m.settings();
            if !s.extension_connected_once {
                s.extension_connected_once = true;
                m.set_settings(s.clone());
            }
            let sign_ins = crate::sites::sign_in_cookies().into_iter().map(|(d, n)| (d.to_string(), n.to_string())).collect();
            let browser_sites = crate::sites::browser_hosts().into_iter().map(String::from).collect();
            Response::Capture { settings: s.capture, sites: crate::sites::hosts().into_iter().map(String::from).collect(), sign_ins, browser_sites }
        }
        Request::Media { request, title, kind } => {
            if let Some(ui) = ui {
                let _ = ui.send(UiRequest::Media { request, title, kind });
            }
            Response::Ok
        }
        Request::Site { .. } | Request::Episodes { .. } => Response::Error { message: "handled above".into() },
        Request::StartQueue => {
            m.start_queue(crate::settings::MAIN_QUEUE);
            Response::Ok
        }
        Request::SignIn { list } => {
            for (domain, cookie) in list {
                crate::sites::remember_sign_in(&domain, &cookie);
            }
            Response::Ok
        }
    }
}

/// Client side, used by the CLI and the browser bridge.
pub struct Client {
    stream: BufReader<TcpStream>,
    token: String,
}

impl Client {
    /// Connect to the running app, if any.
    pub async fn connect(data_dir: &Path) -> std::io::Result<Self> {
        let info: IpcInfo = serde_json::from_slice(&std::fs::read(data_dir.join(IPC_FILE))?)?;
        let stream = TcpStream::connect(("127.0.0.1", info.port)).await?;
        Ok(Self { stream: BufReader::new(stream), token: info.token })
    }

    pub async fn call(&mut self, request: Request) -> std::io::Result<Response> {
        let mut out = serde_json::to_vec(&Envelope { token: self.token.clone(), request })?;
        out.push(b'\n');
        self.stream.get_mut().write_all(&out).await?;
        let mut line = String::new();
        self.stream.read_line(&mut line).await?;
        Ok(serde_json::from_str(&line)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let dir = std::env::temp_dir().join(format!("rdm_ipc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = crate::settings::Settings::default();
        s.temp_dir = dir.join("temp");
        s.save(&dir.join("settings.json")).unwrap();
        let m = Manager::open(&dir);
        serve(m.clone(), None).await.unwrap();
        let mut c = Client::connect(&dir).await.unwrap();
        assert!(matches!(c.call(Request::Ping).await.unwrap(), Response::Pong { .. }));
        let req = DownloadRequest { url: "http://127.0.0.1:9/never.bin".into(), ..Default::default() };
        let r = c.call(Request::Add { request: req, start_now: false, interactive: false }).await.unwrap();
        assert!(matches!(r, Response::Added { id: Some(_) }));
        match c.call(Request::List).await.unwrap() {
            Response::Items { items } => assert_eq!(items[0].file_name, "never.bin"),
            other => panic!("unexpected {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
