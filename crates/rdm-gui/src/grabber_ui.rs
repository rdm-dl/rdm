//! Site grabber window.

use crate::app::App;
use crate::theme::{self, Kind, icon};
use eframe::egui::{self, RichText};
use rdm_core::grabber::{self, GrabberProject, GrabberStatus};
use rdm_core::settings::MAIN_QUEUE;
use rdm_core::{AddOptions, DownloadRequest};
use std::collections::HashSet;
use tokio::sync::watch;

pub struct Grabber {
    project: GrabberProject,
    preset: String,
    custom: String,
    status: Option<watch::Receiver<GrabberStatus>>,
    cancel: Option<watch::Sender<bool>>,
    unchecked: HashSet<String>,
}

impl Default for Grabber {
    fn default() -> Self {
        Self {
            project: GrabberProject::default(),
            preset: "images".into(),
            custom: "*.zip *.pdf".into(),
            status: None,
            cancel: None,
            unchecked: HashSet::new(),
        }
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(g) = app.grabber.as_mut() else { return };
    let mut close = false;
    let mut add: Option<bool> = None;
    let mut start = false;
    egui::Window::new("Site grabber").default_size([640.0, 520.0]).show(ctx, |ui| {
        let p = theme::palette(ui);
        let running = g.status.as_ref().is_some_and(|s| !s.borrow().done);
        ui.add_enabled_ui(!running, |ui| {
            egui::Grid::new("grab").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Start page");
                ui.add(egui::TextEdit::singleline(&mut g.project.start_url).hint_text("https://example.com/gallery/").desired_width(440.0));
                ui.end_row();
                ui.label("Collect");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("preset").selected_text(g.preset.as_str()).show_ui(ui, |ui| {
                        for name in ["images", "video", "audio", "documents", "archives", "programs", "custom"] {
                            ui.selectable_value(&mut g.preset, name.to_string(), name);
                        }
                    });
                    if g.preset == "custom" {
                        ui.add(egui::TextEdit::singleline(&mut g.custom).hint_text("*.zip *.pdf").desired_width(260.0));
                    }
                });
                ui.end_row();
                ui.label("Link depth");
                ui.add(egui::DragValue::new(&mut g.project.depth).range(0..=10));
                ui.end_row();
                ui.label("Page limit");
                ui.add(egui::DragValue::new(&mut g.project.max_pages).range(1..=100_000));
                ui.end_row();
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut g.project.same_host, "Stay on this site");
                ui.checkbox(&mut g.project.same_path, "Stay in this folder");
                ui.checkbox(&mut g.project.respect_robots, "Respect robots.txt");
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if running {
                if theme::button(ui, Kind::Danger, icon::STOP, "Stop", true).clicked() {
                    if let Some(c) = &g.cancel {
                        let _ = c.send(true);
                    }
                }
            } else if theme::button(ui, Kind::Primary, icon::MAGNIFYING_GLASS, "Start crawling", !g.project.start_url.trim().is_empty()).clicked() {
                start = true;
            }
        });
        if let Some(st) = &g.status {
            let s = st.borrow().clone();
            ui.add_space(6.0);
            let finished = if s.done { " · finished" } else { "" };
            ui.label(
                RichText::new(format!("{} pages explored · {} queued · {} files found{finished}", s.pages_explored, s.pages_queued, s.files.len()))
                    .color(p.text_dim),
            );
            if let Some(e) = &s.error {
                ui.colored_label(p.danger, e);
            }
            if !s.current.is_empty() {
                ui.add(egui::Label::new(RichText::new(&s.current).size(11.5).color(p.text_dim)).truncate());
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                for f in s.files.iter().take(5000) {
                    let mut on = !g.unchecked.contains(&f.url);
                    if ui.checkbox(&mut on, &f.url).changed() {
                        if on {
                            g.unchecked.remove(&f.url);
                        } else {
                            g.unchecked.insert(f.url.clone());
                        }
                    }
                }
            });
            if s.done && !s.files.is_empty() {
                ui.horizontal(|ui| {
                    if theme::button(ui, Kind::Primary, icon::DOWNLOAD_SIMPLE, "Download now", true).clicked() {
                        add = Some(true);
                    }
                    if theme::button(ui, Kind::Ghost, icon::QUEUE, "Add to queue", true).clicked() {
                        add = Some(false);
                    }
                });
            }
        }
        ui.separator();
        if ui.button("Close").clicked() {
            close = true;
        }
    });
    if start {
        g.project.include = if g.preset == "custom" {
            g.custom.split_whitespace().map(str::to_string).collect()
        } else {
            grabber::preset(&g.preset)
        };
        let (tx, rx) = watch::channel(GrabberStatus::default());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        g.status = Some(rx);
        g.cancel = Some(cancel_tx);
        g.unchecked.clear();
        app.rt.spawn(grabber::run(g.project.clone(), tx, cancel_rx));
    }
    if let Some(now) = add {
        let files = g.status.as_ref().map(|s| s.borrow().files.clone()).unwrap_or_default();
        let picked: Vec<_> = files.into_iter().filter(|f| !g.unchecked.contains(&f.url)).collect();
        let n = picked.len();
        for f in picked {
            let r = DownloadRequest { url: f.url, referer: Some(f.referer), ..Default::default() };
            app.manager.add(r, AddOptions::default());
        }
        if now {
            app.manager.start_queue(MAIN_QUEUE);
        }
        app.flash(format!("Added {n} files from the site grabber"));
        close = true;
    }
    if close {
        if let Some(g) = &app.grabber {
            if let Some(c) = &g.cancel {
                let _ = c.send(true);
            }
        }
        app.grabber = None;
    }
}
