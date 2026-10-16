//! Site grabber window.

use crate::app::App;
use crate::theme::{self, Kind, icon};
use crate::window::{self, Spec};
use eframe::egui::{self, RichText, vec2};
use rdm_core::grabber::{self, GrabberProject, GrabberStatus};
use rdm_core::settings::MAIN_QUEUE;
use rdm_core::{AddOptions, DownloadRequest};
use std::collections::HashSet;
use tokio::sync::watch;

pub struct Grabber {
    project: GrabberProject,
    preset: String,
    custom: String,
    /// Save an offline copy of the explored pages.
    mirror: bool,
    mirror_dir: String,
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
            mirror: false,
            mirror_dir: String::new(),
            status: None,
            cancel: None,
            unchecked: HashSet::new(),
        }
    }
}

impl Grabber {
    pub fn set_start_url(&mut self, url: &str) {
        self.project.start_url = url.to_string();
    }

    pub fn set_mirror_dir(&mut self, dir: std::path::PathBuf) {
        self.mirror_dir = dir.display().to_string();
    }
}

fn preset_label(name: &str) -> &'static str {
    match name {
        "images" => "Images",
        "video" => "Videos",
        "audio" => "Audio",
        "documents" => "Documents",
        "archives" => "Archives",
        "programs" => "Programs",
        _ => "Custom types",
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(g) = app.grabber.as_mut() else { return };
    let mut close = false;
    let mut add: Option<bool> = None;
    let mut start = false;
    let spec = Spec { title: "Site grabber", size: [780.0, 660.0], min: [620.0, 520.0], resizable: true };
    let (_, closed) = window::show(ctx, "grabber", spec, |ui| {
        let p = theme::palette(ui);
        let status = g.status.as_ref().map(|s| s.borrow().clone());
        let running = status.as_ref().is_some_and(|s| !s.done);
        let picked = status.as_ref().map_or(0, |s| s.files.iter().filter(|f| !g.unchecked.contains(&f.url)).count());
        window::footer(ui, |ui| {
            if running {
                if theme::button(ui, Kind::Danger, icon::STOP, "Stop", true).clicked() {
                    if let Some(c) = &g.cancel {
                        let _ = c.send(true);
                    }
                }
            } else if status.as_ref().is_some_and(|s| s.done && !s.files.is_empty()) {
                if theme::button(ui, Kind::Primary, icon::DOWNLOAD_SIMPLE, &format!("Download {picked} files"), picked > 0).clicked() {
                    add = Some(true);
                }
                if theme::button(ui, Kind::Ghost, icon::QUEUE, "Add to queue", picked > 0).clicked() {
                    add = Some(false);
                }
                if theme::button(ui, Kind::Ghost, icon::ARROW_CLOCKWISE, "Crawl again", !g.project.start_url.trim().is_empty()).clicked() {
                    start = true;
                }
            } else if theme::button(ui, Kind::Primary, icon::MAGNIFYING_GLASS, "Start crawling", !g.project.start_url.trim().is_empty()).clicked() {
                start = true;
            }
            if theme::button(ui, Kind::Ghost, icon::X, "Close", true).clicked() {
                close = true;
            }
        });
        window::body(ui, |ui| {
            ui.add_enabled_ui(!running, |ui| {
                theme::group(ui, "Website", |ui| {
                    ui.label("Start page");
                    ui.add(egui::TextEdit::singleline(&mut g.project.start_url).hint_text("https://example.com/gallery/").desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)));
                    theme::divider(ui);
                    theme::setting(ui, "Collect", "The kind of files to look for.", |ui| {
                        if g.preset == "custom" {
                            ui.add(egui::TextEdit::singleline(&mut g.custom).hint_text("*.zip *.pdf").desired_width(130.0).margin(vec2(8.0, 6.0)));
                        }
                        egui::ComboBox::from_id_salt("preset").width(120.0).selected_text(preset_label(&g.preset)).show_ui(ui, |ui| {
                            for name in ["images", "video", "audio", "documents", "archives", "programs", "custom"] {
                                ui.selectable_value(&mut g.preset, name.to_string(), preset_label(name));
                            }
                        });
                    });
                    theme::divider(ui);
                    theme::setting(ui, "Link depth", "How many clicks away from the start page to go.", |ui| {
                        ui.add(egui::DragValue::new(&mut g.project.depth).range(0..=10));
                    });
                    theme::divider(ui);
                    theme::setting(ui, "Page limit", "Stop after exploring this many pages.", |ui| {
                        ui.add(egui::DragValue::new(&mut g.project.max_pages).range(1..=100_000));
                    });
                    theme::divider(ui);
                    theme::switch_setting(ui, "Stay on this site", "", &mut g.project.same_host);
                    theme::switch_setting(ui, "Stay in this folder", "Only pages below the start page's address.", &mut g.project.same_path);
                    theme::switch_setting(ui, "Respect robots.txt", "Skip what the site asks crawlers to skip.", &mut g.project.respect_robots);
                });
                theme::group(ui, "Offline copy", |ui| {
                    theme::switch_setting(ui, "Save the pages for offline browsing", "Links are rewritten so the copy works without internet.", &mut g.mirror);
                    if g.mirror {
                        ui.horizontal(|ui| {
                            let w = ui.available_width() - 44.0;
                            ui.add(egui::TextEdit::singleline(&mut g.mirror_dir).desired_width(w).margin(vec2(10.0, 7.0)));
                            if theme::icon_button(ui, icon::FOLDER_OPEN, "Browse…").clicked() {
                                if let Some(d) = rfd::FileDialog::new().set_directory(&g.mirror_dir).pick_folder() {
                                    g.mirror_dir = d.display().to_string();
                                }
                            }
                        });
                    }
                });
            });
            let Some(s) = status else { return };
            let title = if s.done { "Results" } else { "Exploring…" };
            theme::group(ui, title, |ui| {
                let saved = if s.mirrored > 0 { format!(" · {} pages saved offline", s.mirrored) } else { String::new() };
                ui.label(RichText::new(format!("{} pages explored · {} waiting · {} files found{saved}", s.pages_explored, s.pages_queued, s.files.len())).color(p.text_dim));
                if !s.current.is_empty() && !s.done {
                    let w = ui.available_width();
                    ui.label(RichText::new(theme::elide_middle(ui, &s.current, &egui::FontId::proportional(11.5), w)).size(11.5).color(p.text_dim));
                }
                if let Some(e) = &s.error {
                    ui.colored_label(p.danger, e);
                }
                if let Some(index) = &s.mirror_index {
                    ui.horizontal(|ui| {
                        if theme::button(ui, Kind::Primary, icon::GLOBE, "Open offline copy", true).clicked() {
                            crate::util::open_file(index);
                        }
                        if theme::button(ui, Kind::Ghost, icon::FOLDER_OPEN, "Show folder", true).clicked() {
                            crate::util::open_folder(index);
                        }
                    });
                }
                if !s.files.is_empty() {
                    theme::divider(ui);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{picked} of {} selected", s.files.len())).font(theme::semibold(13.0)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("None").clicked() {
                                g.unchecked = s.files.iter().map(|f| f.url.clone()).collect();
                            }
                            if ui.small_button("All").clicked() {
                                g.unchecked.clear();
                            }
                        });
                    });
                    egui::ScrollArea::vertical().id_salt("found").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                        for f in s.files.iter().take(5000) {
                            let mut on = !g.unchecked.contains(&f.url);
                            if theme::checkbox(ui, &mut on, f.url.clone()).changed() {
                                if on {
                                    g.unchecked.remove(&f.url);
                                } else {
                                    g.unchecked.insert(f.url.clone());
                                }
                            }
                        }
                    });
                }
            });
        });
    });
    close |= closed;
    if start {
        g.project.include = if g.preset == "custom" {
            g.custom.split_whitespace().map(str::to_string).collect()
        } else {
            grabber::preset(&g.preset)
        };
        g.project.mirror_dir = (g.mirror && !g.mirror_dir.trim().is_empty()).then(|| std::path::PathBuf::from(g.mirror_dir.trim()));
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
