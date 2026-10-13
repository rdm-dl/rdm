#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! RDM desktop app.

mod app;
mod bridge;
mod dialogs;
mod extension_help;
mod grabber_ui;
mod theme;
mod tray;
mod util;

use eframe::egui;
use rdm_core::ipc::{self, Client, Request};
use rdm_core::settings::data_dir;
use rdm_core::{DownloadRequest, Manager};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;

fn main() -> eframe::Result<()> {
    // Any panic is written to <data dir>/crash.log (the window has no console).
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("[{}] RDM {} crashed: {info}
{bt}

", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), env!("CARGO_PKG_VERSION"));
        eprintln!("{msg}");
        let path = data_dir().join("crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = f.write_all(msg.as_bytes());
        }
    }));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let minimized = args.iter().any(|a| a == "--minimized");
    let urls: Vec<String> = args.iter().filter(|a| a.contains("://")).cloned().collect();
    let dir = data_dir();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");

    // Single instance: hand the URLs to the running app and quit. The lock file closes the
    // window where several launches (e.g. parallel browser requests) start at once.
    let forward = |show: bool| {
        rt.block_on(async {
            let Ok(mut c) = Client::connect(&dir).await else { return false };
            if c.call(Request::Ping).await.is_err() {
                return false;
            }
            for u in &urls {
                let _ = c
                    .call(Request::Add { request: DownloadRequest { url: u.clone(), ..Default::default() }, start_now: false, interactive: true })
                    .await;
            }
            if show {
                let _ = c.call(Request::Show).await;
            }
            true
        })
    };
    let show = !minimized || !urls.is_empty();
    if forward(show) {
        return Ok(());
    }
    let _ = std::fs::create_dir_all(&dir);
    let instance_lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock"));
    if let Ok(f) = &instance_lock {
        if f.try_lock().is_err() {
            // Another instance holds the lock and is still starting: wait for it, then forward.
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(200));
                if forward(show) {
                    break;
                }
            }
            return Ok(());
        }
    }

    // IPC requests are relayed to the UI and wake it up (it may be hidden in the tray).
    let (ipc_tx, mut ipc_rx) = mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let waker: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    let manager = {
        let _guard = rt.enter();
        let w = waker.clone();
        tokio::spawn(async move {
            while let Some(r) = ipc_rx.recv().await {
                let _ = ui_tx.send(r);
                if let Some(ctx) = w.get() {
                    ctx.request_repaint();
                }
            }
        });
        Manager::open(&dir)
    };
    rt.block_on(ipc::serve(manager.clone(), Some(ipc_tx))).expect("ipc server");
    let dark = manager.settings().dark_mode;
    let initial: Vec<DownloadRequest> = urls.into_iter().map(|url| DownloadRequest { url, ..Default::default() }).collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RDM — Download Manager")
            .with_inner_size([1100.0, 640.0])
            .with_min_inner_size([720.0, 400.0])
            .with_visible(!minimized)
            .with_icon(Arc::new(
                eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon256.png")).unwrap_or_default(),
            )),
        ..Default::default()
    };
    let handle = rt.handle().clone();
    let m2 = manager.clone();
    let result = eframe::run_native(
        "RDM",
        options,
        Box::new(move |cc| {
            let _ = waker.set(cc.egui_ctx.clone());
            theme::setup(&cc.egui_ctx, dark);
            Ok(Box::new(app::App::new(&cc.egui_ctx, handle, m2, ui_rx, initial)))
        }),
    );
    rt.block_on(manager.shutdown());
    let _ = std::fs::remove_file(dir.join("ipc.json"));
    result
}
