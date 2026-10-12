#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! RDM desktop app.

mod app;
mod bridge;
mod dialogs;
mod theme;
mod util;

use eframe::egui;
use rdm_core::ipc::{self, Client, Request};
use rdm_core::settings::data_dir;
use rdm_core::{DownloadRequest, Manager};
use tokio::sync::mpsc;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let minimized = args.iter().any(|a| a == "--minimized");
    let urls: Vec<String> = args.iter().filter(|a| a.contains("://")).cloned().collect();
    let dir = data_dir();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");

    // Single instance: hand the URLs to the running app and quit.
    let forwarded = rt.block_on(async {
        let Ok(mut c) = Client::connect(&dir).await else { return false };
        if c.call(Request::Ping).await.is_err() {
            return false;
        }
        for u in &urls {
            let _ = c
                .call(Request::Add { request: DownloadRequest { url: u.clone(), ..Default::default() }, start_now: false, interactive: true })
                .await;
        }
        let _ = c.call(Request::Show).await;
        true
    });
    if forwarded {
        return Ok(());
    }

    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let manager = {
        let _guard = rt.enter();
        Manager::open(&dir)
    };
    rt.block_on(ipc::serve(manager.clone(), Some(ui_tx))).expect("ipc server");
    let dark = manager.settings().dark_mode;
    let initial: Vec<DownloadRequest> = urls.into_iter().map(|url| DownloadRequest { url, ..Default::default() }).collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RDM — Download Manager")
            .with_inner_size([1100.0, 640.0])
            .with_min_inner_size([720.0, 400.0])
            .with_visible(!minimized),
        ..Default::default()
    };
    let handle = rt.handle().clone();
    let m2 = manager.clone();
    let result = eframe::run_native(
        "RDM",
        options,
        Box::new(move |cc| {
            theme::setup(&cc.egui_ctx, dark);
            Ok(Box::new(app::App::new(handle, m2, ui_rx, initial)))
        }),
    );
    rt.block_on(manager.shutdown());
    let _ = std::fs::remove_file(dir.join("ipc.json"));
    result
}
