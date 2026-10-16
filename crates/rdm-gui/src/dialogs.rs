//! Dialog windows.

use crate::app::App;
use crate::theme::{self, Kind, icon};
use crate::util;
use crate::window::{self, Spec};
use eframe::egui::{self, RichText, vec2};
use std::collections::HashMap;
use rdm_core::settings::{Category, OnQueueDone, QueueDef, Schedule, Settings, SiteLogin, MAIN_QUEUE};
use rdm_core::{AddOptions, DownloadRequest, Manager, ProbeInfo};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- Add URL

pub struct AddUrl {
    pub url: String,
    use_auth: bool,
    user: String,
    pass: String,
    /// The address box got the keyboard focus once.
    focused: bool,
}

impl AddUrl {
    pub fn from_clipboard(cb: Option<&mut arboard::Clipboard>) -> Self {
        let clip = cb.and_then(|c| c.get_text().ok()).unwrap_or_default();
        let clip = clip.trim();
        let url = if (clip.starts_with("http://") || clip.starts_with("https://") || clip.starts_with("ftp://")) && !clip.contains(char::is_whitespace) {
            clip.to_string()
        } else {
            String::new()
        };
        Self { url, use_auth: false, user: String::new(), pass: String::new(), focused: false }
    }
}

// ---------------------------------------------------------------- File info

pub struct FileInfo {
    req: DownloadRequest,
    probe: Arc<Mutex<Option<Result<ProbeInfo, String>>>>,
    name: String,
    name_edited: bool,
    category: Option<String>,
    dir: String,
    dir_edited: bool,
    description: String,
    queue: u32,
    remember_dir: bool,
}

impl FileInfo {
    pub fn new(req: DownloadRequest, m: &Manager, rt: &tokio::runtime::Handle) -> Self {
        let s = m.settings();
        let guess = req.file_name.clone().unwrap_or_else(|| {
            url::Url::parse(&req.url).ok().and_then(|u| rdm_core::naming::from_url(&u)).map(|n| rdm_core::naming::sanitize(&n)).unwrap_or_default()
        });
        let category = s.category_for(&guess).map(|c| c.name.clone());
        let dir = if req.dir.as_os_str().is_empty() { s.folder_for(category.as_deref()) } else { req.dir.clone() };
        let probe = Arc::new(Mutex::new(None));
        let (slot, r, cfg) = (probe.clone(), req.clone(), s.engine.clone());
        rt.spawn(async move {
            let res = rdm_core::probe_info(&r, &cfg).await.map_err(|e| e.to_string());
            *slot.lock().unwrap() = Some(res);
        });
        Self {
            name: guess,
            name_edited: req.file_name.is_some(),
            category,
            dir: dir.display().to_string(),
            dir_edited: !req.dir.as_os_str().is_empty(),
            description: String::new(),
            queue: MAIN_QUEUE,
            remember_dir: false,
            req,
            probe,
        }
    }

    fn request(&self) -> DownloadRequest {
        let mut r = self.req.clone();
        r.dir = PathBuf::from(&self.dir);
        r.file_name = Some(self.name.clone()).filter(|n| !n.trim().is_empty());
        r
    }
}

// ---------------------------------------------------------------- Batch

#[derive(Default)]
pub struct Batch {
    template: String,
    from: String,
    to: String,
    pad: u32,
    pub links: Vec<(bool, DownloadRequest)>,
}

impl Batch {
    pub fn from_text(text: &str) -> Self {
        let links = text
            .split_whitespace()
            .filter(|t| t.starts_with("http://") || t.starts_with("https://") || t.starts_with("ftp://"))
            .map(|u| (true, DownloadRequest { url: u.to_string(), ..Default::default() }))
            .collect();
        Self { from: "1".into(), to: "10".into(), links, ..Default::default() }
    }

    /// Expand `http://x/file*.jpg` over the numeric or letter range.
    fn expand(&mut self) {
        self.links.clear();
        if !self.template.contains('*') {
            return;
        }
        let mk = |s: String| (true, DownloadRequest { url: s, ..Default::default() });
        if let (Ok(a), Ok(b)) = (self.from.trim().parse::<u64>(), self.to.trim().parse::<u64>()) {
            for n in a.min(b)..=a.max(b).min(a.min(b) + 100_000) {
                let v = format!("{:0width$}", n, width = self.pad as usize);
                self.links.push(mk(self.template.replacen('*', &v, 1)));
            }
        } else if let (Some(a), Some(b)) = (self.from.trim().chars().next(), self.to.trim().chars().next()) {
            for c in a.min(b)..=a.max(b) {
                if c.is_ascii_alphanumeric() {
                    self.links.push(mk(self.template.replacen('*', &c.to_string(), 1)));
                }
            }
        }
    }
}

// ---------------------------------------------------------------- Speed limit

pub struct SpeedLimit {
    on: bool,
    kb: u64,
}

impl SpeedLimit {
    pub fn new(m: &Manager) -> Self {
        let l = m.settings().speed_limit;
        Self { on: l.is_some(), kb: l.map_or(500, |v| v / 1024) }
    }
}

// ---------------------------------------------------------------- Options

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    General,
    Browser,
    FileTypes,
    Connection,
    Folders,
    Proxy,
    Logins,
    Advanced,
}

impl Tab {
    const ALL: [(Tab, &'static str, &'static str); 8] = [
        (Tab::General, icon::GEAR_SIX, "General"),
        (Tab::Browser, icon::PUZZLE_PIECE, "Browser"),
        (Tab::FileTypes, icon::FILE, "File types"),
        (Tab::Connection, icon::LIGHTNING, "Connection"),
        (Tab::Folders, icon::FOLDER_OPEN, "Folders"),
        (Tab::Proxy, icon::GLOBE, "Proxy"),
        (Tab::Logins, icon::KEY, "Site logins"),
        (Tab::Advanced, icon::SLIDERS, "Advanced"),
    ];
}

pub struct Options {
    s: Settings,
    tab: Tab,
    exts: String,
    media_exts: String,
    hosts: String,
    urls: String,
    ignore_exts: String,
    no_button_hosts: String,
    bypass: String,
    new_login: SiteLogin,
    bridge_msg: Option<String>,
}

impl Options {
    pub fn new(m: &Manager) -> Self {
        let s = m.settings();
        Self {
            exts: s.capture.extensions.join(" "),
            media_exts: s.capture.media_extensions.join(" "),
            hosts: s.capture.exception_hosts.join("\n"),
            urls: s.capture.exception_urls.join("\n"),
            ignore_exts: s.capture.ignore_extensions.join(" "),
            no_button_hosts: s.capture.no_button_hosts.join("\n"),
            bypass: s.proxy.bypass.join("\n"),
            s,
            tab: Tab::General,
            new_login: SiteLogin { prefix: String::new(), username: String::new(), password: String::new() },
            bridge_msg: None,
        }
    }

    fn apply(&mut self, m: &Manager) {
        let words = |t: &str| t.split_whitespace().map(|w| w.trim_start_matches('.').to_ascii_lowercase()).collect::<Vec<_>>();
        let lines = |t: &str| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>();
        self.s.capture.extensions = words(&self.exts);
        self.s.capture.media_extensions = words(&self.media_exts);
        self.s.capture.exception_hosts = lines(&self.hosts);
        self.s.capture.exception_urls = lines(&self.urls);
        self.s.capture.ignore_extensions = words(&self.ignore_exts);
        self.s.capture.no_button_hosts = lines(&self.no_button_hosts);
        self.s.proxy.bypass = lines(&self.bypass);
        let before = m.settings();
        if before.start_with_system != self.s.start_with_system {
            util::set_autostart(self.s.start_with_system);
        }
        m.set_settings(self.s.clone());
    }
}

fn key_label(k: &str) -> &'static str {
    match k {
        "alt" => "Alt",
        "ctrl" => "Ctrl",
        "shift" => "Shift",
        "insert" => "Insert",
        "delete" => "Delete",
        _ => "None",
    }
}

// ---------------------------------------------------------------- Scheduler

pub struct Scheduler {
    queues: Vec<QueueDef>,
    sel: usize,
    /// Start/stop times as typed, per (queue, is_start); saved once they are valid.
    times: HashMap<(u32, bool), String>,
}

impl Scheduler {
    pub fn new(m: &Manager) -> Self {
        Self { queues: m.settings().queues, sel: 0, times: HashMap::new() }
    }
}

// ---------------------------------------------------------------- Import / export

pub fn export_list(m: &Manager) {
    let Some(path) = rfd::FileDialog::new().set_file_name("downloads.txt").add_filter("Text", &["txt"]).save_file() else { return };
    let text: String = m.items().into_iter().map(|i| i.request.url + "\n").collect();
    let _ = std::fs::write(path, text);
}

pub fn import_list(m: &Manager) -> usize {
    let Some(path) = rfd::FileDialog::new().add_filter("Text", &["txt"]).pick_file() else { return 0 };
    let Ok(text) = std::fs::read_to_string(path) else { return 0 };
    let mut n = 0;
    for line in text.lines().map(str::trim).filter(|l| l.starts_with("http") || l.starts_with("ftp")) {
        m.add(DownloadRequest { url: line.to_string(), ..Default::default() }, AddOptions::default());
        n += 1;
    }
    n
}

// ---------------------------------------------------------------- rendering

pub fn show_all(app: &mut App, ctx: &egui::Context) {
    add_url(app, ctx);
    file_info(app, ctx);
    batch(app, ctx);
    speed_limit(app, ctx);
    options(app, ctx);
    scheduler(app, ctx);
}

/// Label above a field.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::semibold(13.0)));
}

/// Full-width single-line text box.
fn text_field(ui: &mut egui::Ui, value: &mut String, hint: &str) -> egui::Response {
    ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)))
}

/// Folder path box with a browse button.
fn folder_field(ui: &mut egui::Ui, path: &mut String) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let w = ui.available_width() - 44.0;
        changed |= ui.add(egui::TextEdit::singleline(path).desired_width(w).margin(vec2(10.0, 7.0))).changed();
        if theme::icon_button(ui, icon::FOLDER_OPEN, "Browse…").clicked() {
            if let Some(p) = rfd::FileDialog::new().set_directory(&*path).pick_folder() {
                *path = p.display().to_string();
                changed = true;
            }
        }
    });
    changed
}

/// Small rounded on/off chip (weekdays).
fn chip(ui: &mut egui::Ui, text: &str, on: &mut bool) {
    let p = theme::palette(ui);
    let g = ui.painter().layout_no_wrap(text.to_string(), theme::semibold(12.5), p.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 22.0, 30.0), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let enabled = ui.is_enabled();
    let (fill, fg) = if *on && enabled { (p.accent, egui::Color32::WHITE) } else if resp.hovered() && enabled { (p.border, p.text) } else { (p.surface_hi, if enabled { p.text } else { p.text_dim }) };
    ui.painter().rect_filled(rect, egui::CornerRadius::same(15), fill);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, theme::semibold(12.5), fg);
}

// ---------------------------------------------------------------- New download

fn add_url(app: &mut App, ctx: &egui::Context) {
    let Some(d) = app.add_url.as_mut() else { return };
    let mut ok = false;
    let mut cancel = false;
    let h = if d.use_auth { 330.0 } else { 232.0 };
    let spec = Spec { title: "New download", size: [560.0, h], min: [440.0, h], resizable: false };
    let (_, closed) = window::show(ctx, "add-url", spec, |ui| {
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::ARROW_RIGHT, "Continue", !d.url.trim().is_empty()).clicked() {
                ok = true;
            }
            if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                cancel = true;
            }
        });
        window::body_fixed(ui, |ui| {
            field_label(ui, "Address");
            let r = text_field(ui, &mut d.url, "https://example.com/file.zip");
            if !d.focused {
                r.request_focus();
                d.focused = true;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                ok = true;
            }
            theme::hint(ui, "A link to a file, a video page or a stream (.m3u8 / .mpd).");
            ui.add_space(6.0);
            theme::checkbox(ui, &mut d.use_auth, "This site needs a login");
            if d.use_auth {
                ui.add_space(4.0);
                egui::Grid::new("auth").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("User name");
                    ui.add(egui::TextEdit::singleline(&mut d.user).desired_width(300.0).margin(vec2(10.0, 7.0)));
                    ui.end_row();
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut d.pass).password(true).desired_width(300.0).margin(vec2(10.0, 7.0)));
                    ui.end_row();
                });
            }
        });
    });
    if ok && !d.url.trim().is_empty() {
        let mut r = DownloadRequest { url: d.url.trim().to_string(), ..Default::default() };
        if d.use_auth {
            r.username = Some(d.user.clone());
            r.password = Some(d.pass.clone());
        }
        app.add_url = None;
        app.open_file_info(r);
    } else if cancel || closed {
        app.add_url = None;
    }
}

// ---------------------------------------------------------------- File info

fn file_info(app: &mut App, ctx: &egui::Context) {
    let Some(d) = app.file_infos.front_mut() else { return };
    let s = app.manager.settings();
    // Fill in the probe result once it arrives.
    let probe = d.probe.lock().unwrap().clone();
    if let Some(Ok(p)) = &probe {
        if !d.name_edited && d.name != p.file_name {
            d.name = p.file_name.clone();
            let cat = s.category_for(&d.name).map(|c| c.name.clone());
            if cat != d.category {
                d.category = cat;
                if !d.dir_edited {
                    d.dir = s.folder_for(d.category.as_deref()).display().to_string();
                }
            }
        }
    }
    let mut result: Option<Option<bool>> = None; // Some(Some(true)) start now, Some(Some(false)) later, Some(None) cancel
    let pending = app.file_infos.len() - 1;
    let d = app.file_infos.front_mut().unwrap();
    let spec = Spec { title: "Download file info", size: [640.0, 470.0], min: [520.0, 440.0], resizable: true };
    let (_, closed) = window::show(ctx, "file-info", spec, |ui| {
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::DOWNLOAD_SIMPLE, "Start download", true).clicked() {
                result = Some(Some(true));
            }
            let later = theme::button(ui, Kind::Ghost, icon::QUEUE, "Download later", true);
            egui::Popup::menu(&later).show(|ui| {
                ui.set_min_width(180.0);
                for q in &s.queues {
                    if ui.button(&q.name).clicked() {
                        d.queue = q.id;
                        result = Some(Some(false));
                        ui.close();
                    }
                }
            });
            if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                result = Some(None);
            }
            if pending > 0 {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{pending} more waiting")).size(12.0).color(theme::palette(ui).text_dim));
                });
            }
        });
        window::body(ui, |ui| {
            let p = theme::palette(ui);
            // What is being downloaded.
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(46.0, 46.0), egui::Sense::hover());
                theme::file_tile(ui, r, &d.name);
                ui.vertical(|ui| {
                    let size = match &probe {
                        None => "Checking size…".to_string(),
                        Some(Ok(p)) => match p.total {
                            Some(t) => format!("{}{}", util::size(t), if p.resumable { "" } else { "  ·  can't be resumed" }),
                            None => "Size unknown".into(),
                        },
                        Some(Err(_)) => "Size unknown".into(),
                    };
                    let r = ui.label(RichText::new(size).font(theme::semibold(16.0)));
                    if let Some(Err(e)) = &probe {
                        // The size check failing doesn't stop the download; say so quietly.
                        r.on_hover_text(format!("Couldn't check the size: {e}"));
                    }
                    let w = ui.available_width();
                    let url = d.req.url.split('#').next().unwrap_or_default().to_string();
                    ui.label(RichText::new(theme::elide_middle(ui, &url, &egui::FontId::proportional(12.0), w)).size(12.0).color(p.text_dim)).on_hover_text(&url);
                });
            });
            ui.add_space(14.0);
            field_label(ui, "File name");
            if text_field(ui, &mut d.name, "").changed() {
                d.name_edited = true;
            }
            ui.add_space(8.0);
            ui.columns(2, |c| {
                field_label(&mut c[0], "Category");
                let before = d.category.clone();
                egui::ComboBox::from_id_salt("cat").width(c[0].available_width()).selected_text(d.category.clone().unwrap_or_else(|| "General".into())).show_ui(&mut c[0], |ui| {
                    ui.selectable_value(&mut d.category, None, "General");
                    for cat in &s.categories {
                        ui.selectable_value(&mut d.category, Some(cat.name.clone()), &cat.name);
                    }
                });
                if before != d.category && !d.dir_edited {
                    d.dir = s.folder_for(d.category.as_deref()).display().to_string();
                }
                field_label(&mut c[1], "Description");
                text_field(&mut c[1], &mut d.description, "Optional");
            });
            ui.add_space(8.0);
            field_label(ui, "Save to");
            if folder_field(ui, &mut d.dir) {
                d.dir_edited = true;
            }
            if let Some(cat) = d.category.clone() {
                ui.add_space(4.0);
                theme::checkbox(ui, &mut d.remember_dir, format!("Always save {cat} files to this folder"));
            }
        });
    });
    if closed && result.is_none() {
        result = Some(None);
    }
    if let Some(r) = result {
        let d = app.file_infos.pop_front().unwrap();
        if let Some(now) = r {
            if d.remember_dir {
                let mut s = app.manager.settings();
                if let Some(c) = s.categories.iter_mut().find(|c| Some(&c.name) == d.category.as_ref()) {
                    c.folder = Some(PathBuf::from(&d.dir));
                }
                app.manager.set_settings(s);
            }
            if let Some(existing) = app.manager.find_by_url(&d.req.url) {
                app.flash(format!("Note: this address is already in the list (#{existing})"));
            }
            let id = app.manager.add(
                d.request(),
                AddOptions { start_now: now, queue: Some(d.queue), category: d.category.clone(), description: d.description.clone() },
            );
            if !now {
                app.flash(format!("Added #{id} to the queue"));
            }
        }
    }
}

// ---------------------------------------------------------------- Batch

fn batch(app: &mut App, ctx: &egui::Context) {
    let Some(b) = app.batch.as_mut() else { return };
    let mut cancel = false;
    let mut add: Option<bool> = None;
    let spec = Spec { title: "Batch download", size: [680.0, 580.0], min: [520.0, 460.0], resizable: true };
    let (_, closed) = window::show(ctx, "batch", spec, |ui| {
        let picked = b.links.iter().filter(|(on, _)| *on).count();
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::DOWNLOAD_SIMPLE, "Download now", picked > 0).clicked() {
                add = Some(true);
            }
            if theme::button(ui, Kind::Ghost, icon::QUEUE, "Add to queue", picked > 0).clicked() {
                add = Some(false);
            }
            if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                cancel = true;
            }
        });
        window::body_fixed(ui, |ui| {
            let p = theme::palette(ui);
            field_label(ui, "Address with a * where the number or letter goes");
            let mut changed = text_field(ui, &mut b.template, "https://example.com/images/photo*.jpg").changed();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("From");
                changed |= ui.add(egui::TextEdit::singleline(&mut b.from).desired_width(70.0).margin(vec2(8.0, 6.0))).changed();
                ui.add_space(6.0);
                ui.label("to");
                changed |= ui.add(egui::TextEdit::singleline(&mut b.to).desired_width(70.0).margin(vec2(8.0, 6.0))).changed();
                ui.add_space(6.0);
                ui.label("Digits");
                changed |= ui.add(egui::DragValue::new(&mut b.pad).range(0..=10)).on_hover_text("Pad numbers with zeros: 3 digits → 001, 002, …").changed();
            });
            if changed {
                b.expand();
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} links", b.links.len())).font(theme::semibold(13.0)));
                if !b.links.is_empty() {
                    ui.label(RichText::new(format!("·  {picked} selected")).color(p.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("None").clicked() {
                            b.links.iter_mut().for_each(|(on, _)| *on = false);
                        }
                        if ui.small_button("All").clicked() {
                            b.links.iter_mut().for_each(|(on, _)| *on = true);
                        }
                    });
                }
            });
            ui.add_space(4.0);
            theme::card(ui).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.set_min_height(ui.available_height() - 20.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if b.links.is_empty() {
                        ui.label(RichText::new("The links appear here.").color(p.text_dim));
                    }
                    for (on, r) in b.links.iter_mut().take(2000) {
                        theme::checkbox(ui, on, r.url.clone());
                    }
                });
            });
        });
    });
    if let Some(now) = add {
        let links = std::mem::take(&mut b.links);
        let mut n = 0;
        for (on, r) in links {
            if on {
                app.manager.add(r, AddOptions { start_now: false, ..Default::default() });
                n += 1;
            }
        }
        if now {
            app.manager.start_queue(MAIN_QUEUE);
        }
        app.flash(format!("Added {n} links"));
        cancel = true;
    }
    if cancel || closed {
        app.batch = None;
    }
}

// ---------------------------------------------------------------- Speed limiter

fn speed_limit(app: &mut App, ctx: &egui::Context) {
    let Some(d) = app.speed_limit.as_mut() else { return };
    let mut close = false;
    let spec = Spec { title: "Speed limiter", size: [440.0, 240.0], min: [400.0, 240.0], resizable: false };
    let (_, closed) = window::show(ctx, "speed-limit", spec, |ui| {
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::CHECK, "Apply", true).clicked() {
                app.manager.set_speed_limit(d.on.then_some(d.kb * 1024));
                close = true;
            }
            if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                close = true;
            }
        });
        window::body_fixed(ui, |ui| {
            theme::switch_setting(ui, "Limit the total download speed", "Leaves room for browsing and calls while RDM downloads.", &mut d.on);
            ui.add_space(8.0);
            ui.add_enabled_ui(d.on, |ui| {
                theme::setting(ui, "Maximum speed", "", |ui| {
                    ui.add(egui::DragValue::new(&mut d.kb).range(1..=10_000_000).speed(10).suffix(" KB/s"));
                });
            });
        });
    });
    if close || closed {
        app.speed_limit = None;
    }
}

// ---------------------------------------------------------------- Options

fn options(app: &mut App, ctx: &egui::Context) {
    let Some(o) = app.options.as_mut() else { return };
    let mut close = false;
    let mut apply = false;
    let mut open_ext_help = false;
    let mut check_now = false;
    let update_state = app.updater.state();
    let spec = Spec { title: "Options", size: [860.0, 600.0], min: [700.0, 480.0], resizable: true };
    let (_, closed) = window::show(ctx, "options", spec, |ui| {
        let p = theme::palette(ui);
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::CHECK, "OK", true).clicked() {
                apply = true;
                close = true;
            }
            if theme::button(ui, Kind::Ghost, icon::CHECK_CIRCLE, "Apply", true).clicked() {
                apply = true;
            }
            if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                close = true;
            }
        });
        egui::Panel::left("options-nav")
            .resizable(false)
            .exact_size(210.0)
            .frame(egui::Frame::new().fill(p.sidebar).inner_margin(egui::Margin { left: 10, right: 10, top: 16, bottom: 10 }))
            .show(ui, |ui| {
                for (t, ic, n) in Tab::ALL {
                    if theme::side_tab(ui, ic, n, o.tab == t).clicked() {
                        o.tab = t;
                    }
                }
            });
        window::body(ui, |ui| match o.tab {
            Tab::General => {
                theme::page_title(ui, "General", "How RDM starts and keeps you informed.");
                theme::group(ui, "Startup", |ui| {
                    theme::switch_setting(ui, "Launch RDM when you sign in", "Starts quietly in the tray, ready for downloads from your browser.", &mut o.s.start_with_system);
                    theme::divider(ui);
                    theme::switch_setting(ui, "Keep running in the tray", "Closing the window hides RDM instead of quitting it.", &mut o.s.minimize_to_tray);
                    theme::divider(ui);
                    theme::switch_setting(ui, "Watch the clipboard for links", "Offers to download file links you copy.", &mut o.s.monitor_clipboard);
                });
                theme::group(ui, "Notifications", |ui| {
                    theme::switch_setting(ui, "Ask before downloading from the browser", "Shows the file info window, where you can change the name and folder.", &mut o.s.confirm_captured);
                    theme::divider(ui);
                    theme::switch_setting(ui, "Tell me when a download completes", "A small card with Open and Show in folder.", &mut o.s.show_complete_dialog);
                });
                theme::group(ui, "Updates", |ui| {
                    use crate::updater::State;
                    theme::switch_setting(ui, "Check for updates automatically", "RDM tells you when a new version is out. Nothing installs without your OK.", &mut o.s.check_updates);
                    theme::divider(ui);
                    let status = match &update_state {
                        State::Checking => "Checking…".to_string(),
                        State::UpToDate => "You have the latest version.".into(),
                        State::Available(r) => format!("RDM {} is available — see the bar in the main window.", r.version),
                        State::Downloading(r, _) | State::Installing(r) => format!("Updating to RDM {}…", r.version),
                        State::Ready(r) => format!("RDM {} is installed. Restart RDM to use it.", r.version),
                        State::Opened(r) => format!("RDM {} was downloaded.", r.version),
                        State::Failed(e, _) => e.clone(),
                        State::Idle => String::new(),
                    };
                    theme::setting(ui, &format!("RDM {}", env!("CARGO_PKG_VERSION")), &status, |ui| {
                        let busy = matches!(update_state, State::Checking | State::Downloading(..) | State::Installing(_));
                        if theme::button(ui, Kind::Ghost, icon::ARROW_CLOCKWISE, "Check now", !busy).clicked() {
                            check_now = true;
                        }
                    });
                });
                theme::group(ui, "Appearance", |ui| {
                    theme::setting(ui, "Theme", "", |ui| {
                        let mut dark = o.s.dark_mode;
                        if ui.selectable_label(!dark, theme::mi(icon::SUN, "Light")).clicked() {
                            dark = false;
                        }
                        if ui.selectable_label(dark, theme::mi(icon::MOON, "Dark")).clicked() {
                            dark = true;
                        }
                        o.s.dark_mode = dark;
                    });
                });
            }
            Tab::Browser => {
                theme::page_title(ui, "Browser", "The RDM extension sends downloads and videos from your browser to RDM.");
                theme::group(ui, "Extension", |ui| {
                    let recent = rdm_core::ipc::last_extension_contact().map(|t| chrono::Utc::now().timestamp() - t);
                    let (text, color) = match recent {
                        Some(s) if s < 600 => ("Connected — the extension is talking to RDM.", p.success),
                        _ if o.s.extension_connected_once => ("Installed — it connects when your browser is open.", p.text_dim),
                        _ => ("Not installed yet.", p.warning),
                    };
                    ui.horizontal(|ui| {
                        ui.label(theme::ic(icon::PUZZLE_PIECE).size(18.0).color(color));
                        ui.label(RichText::new(text).color(color));
                    });
                    ui.horizontal_wrapped(|ui| {
                        if theme::button(ui, Kind::Primary, icon::PUZZLE_PIECE, "Install browser extension…", true).clicked() {
                            open_ext_help = true;
                        }
                        if theme::button(ui, Kind::Ghost, icon::LINK, "Reconnect", true).on_hover_text("Registers RDM with your browsers again").clicked() {
                            o.bridge_msg = Some(match crate::bridge::register() {
                                Ok(m) => m,
                                Err(e) => format!("Failed: {e}"),
                            });
                        }
                        if theme::button(ui, Kind::Danger, icon::PLUG, "Disconnect", true).on_hover_text("Removes RDM's browser registration").clicked() {
                            o.bridge_msg = Some(crate::bridge::unregister());
                        }
                    });
                    if let Some(m) = &o.bridge_msg {
                        theme::hint(ui, m);
                    }
                });
                theme::group(ui, "Download button on videos", |ui| {
                    theme::switch_setting(ui, "Show the button over videos", "Appears on the player when RDM finds a video or stream.", &mut o.s.capture.show_video_button);
                    theme::divider(ui);
                    theme::switch_setting(ui, "Show it over audio players too", "", &mut o.s.capture.show_audio_button);
                    theme::divider(ui);
                    ui.label("Never show the button on these sites");
                    theme::hint(ui, "One per line. * matches anything, e.g. *.example.com");
                    ui.add(egui::TextEdit::multiline(&mut o.no_button_hosts).desired_rows(3).desired_width(f32::INFINITY));
                });
                theme::group(ui, "Keys", |ui| {
                    let keys = |ui: &mut egui::Ui, id: &str, v: &mut String, opts: &[&str]| {
                        egui::ComboBox::from_id_salt(id.to_string()).width(110.0).selected_text(key_label(v)).show_ui(ui, |ui| {
                            for k in opts {
                                ui.selectable_value(v, k.to_string(), key_label(k));
                            }
                        });
                    };
                    theme::setting(ui, "Always download with RDM", "Hold this key while clicking a link.", |ui| keys(ui, "force_key", &mut o.s.capture.force_key, &["none", "alt", "shift", "insert"]));
                    theme::divider(ui);
                    theme::setting(ui, "Let the browser download", "Hold this key while clicking a link.", |ui| keys(ui, "skip_key", &mut o.s.capture.skip_key, &["none", "ctrl", "shift", "delete"]));
                });
            }
            Tab::FileTypes => {
                theme::page_title(ui, "File types", "Which downloads RDM takes over from the browser.");
                theme::group(ui, "Take over", |ui| {
                    ui.label("Downloads of these types");
                    theme::hint(ui, "Extensions separated by spaces.");
                    ui.add(egui::TextEdit::multiline(&mut o.exts).desired_rows(4).desired_width(f32::INFINITY));
                    theme::divider(ui);
                    ui.label("Media that shows the video download button");
                    ui.add(egui::TextEdit::multiline(&mut o.media_exts).desired_rows(2).desired_width(f32::INFINITY));
                    theme::divider(ui);
                    theme::setting(ui, "Skip files smaller than", "Small files stay with the browser.", |ui| {
                        let mut kb = o.s.capture.min_size / 1024;
                        if ui.add(egui::DragValue::new(&mut kb).suffix(" KB")).changed() {
                            o.s.capture.min_size = kb * 1024;
                        }
                    });
                });
                theme::group(ui, "Exceptions", |ui| {
                    ui.label("Never take over downloads from these sites");
                    theme::hint(ui, "One per line. * matches anything.");
                    ui.add(egui::TextEdit::multiline(&mut o.hosts).desired_rows(3).desired_width(f32::INFINITY));
                    theme::divider(ui);
                    ui.label("Never take over these addresses");
                    ui.add(egui::TextEdit::multiline(&mut o.urls).desired_rows(3).desired_width(f32::INFINITY));
                    theme::divider(ui);
                    ui.label("Never treat these types as downloads");
                    ui.add(egui::TextEdit::multiline(&mut o.ignore_exts).desired_rows(2).desired_width(f32::INFINITY));
                });
            }
            Tab::Connection => {
                theme::page_title(ui, "Connection", "How RDM splits and fetches each file.");
                let e = &mut o.s.engine;
                theme::group(ui, "Connections", |ui| {
                    theme::setting(ui, "Connections per download", "More connections are usually faster; some servers limit them.", |ui| {
                        ui.add(egui::DragValue::new(&mut e.max_connections).range(1..=32));
                    });
                    theme::divider(ui);
                    theme::setting(ui, "Start with", "RDM adds more while every connection is busy.", |ui| {
                        ui.add(egui::DragValue::new(&mut e.initial_connections).range(1..=32));
                    });
                    theme::divider(ui);
                    theme::setting(ui, "Retries per part", "0 keeps trying.", |ui| {
                        ui.add(egui::DragValue::new(&mut e.max_retries).range(0..=1000));
                    });
                    theme::divider(ui);
                    theme::setting(ui, "Timeout", "Give up on a silent connection after this long.", |ui| {
                        let mut t = e.timeout.as_secs();
                        if ui.add(egui::DragValue::new(&mut t).range(5..=600).suffix(" s")).changed() {
                            e.timeout = std::time::Duration::from_secs(t);
                        }
                    });
                });
                theme::group(ui, "Browser identity", |ui| {
                    ui.label("User agent");
                    theme::hint(ui, "Sent to servers. Downloads from the extension use your browser's own.");
                    ui.add(egui::TextEdit::singleline(&mut e.user_agent).desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)));
                });
                theme::group(ui, "Download quota", |ui| {
                    theme::switch_setting(ui, "Limit how much is downloaded", "Useful on metered connections. Downloads pause when the limit is reached.", &mut o.s.quota.enabled);
                    ui.add_enabled_ui(o.s.quota.enabled, |ui| {
                        theme::divider(ui);
                        theme::setting(ui, "At most", "", |ui| {
                            ui.add(egui::DragValue::new(&mut o.s.quota.hours).range(1..=23).suffix(" hours"));
                            ui.label("every");
                            ui.add(egui::DragValue::new(&mut o.s.quota.megabytes).suffix(" MB"));
                        });
                    });
                });
            }
            Tab::Folders => {
                theme::page_title(ui, "Folders", "Where downloads are saved.");
                theme::group(ui, "Folders", |ui| {
                    ui.label("Downloads");
                    let mut d = o.s.download_dir.display().to_string();
                    if folder_field(ui, &mut d) {
                        o.s.download_dir = PathBuf::from(d);
                    }
                    theme::divider(ui);
                    ui.label("Unfinished parts");
                    theme::hint(ui, "Pieces are kept here until a download is complete.");
                    let mut t = o.s.temp_dir.display().to_string();
                    if folder_field(ui, &mut t) {
                        o.s.temp_dir = PathBuf::from(t);
                    }
                });
                ui.label(RichText::new("CATEGORIES").font(theme::semibold(11.0)).color(p.text_dim));
                theme::hint(ui, "Files are sorted by extension. A category without a folder uses Downloads\\<category>.");
                ui.add_space(6.0);
                let mut remove = None;
                for (i, c) in o.s.categories.iter_mut().enumerate() {
                    theme::card(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(theme::ic(icon::TAG).color(p.accent));
                            ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(180.0).font(theme::semibold(14.0)));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::icon_button(ui, icon::TRASH, "Remove category").clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                        let mut exts = c.extensions.join(" ");
                        if ui.add(egui::TextEdit::singleline(&mut exts).hint_text("zip rar 7z").desired_width(f32::INFINITY).margin(vec2(10.0, 6.0))).changed() {
                            c.extensions = exts.split_whitespace().map(str::to_ascii_lowercase).collect();
                        }
                        let mut f = c.folder.clone().map(|p| p.display().to_string()).unwrap_or_default();
                        if folder_field(ui, &mut f) {
                            c.folder = Some(PathBuf::from(&f)).filter(|p| !p.as_os_str().is_empty());
                        }
                    });
                    ui.add_space(8.0);
                }
                if let Some(i) = remove {
                    o.s.categories.remove(i);
                }
                if theme::button(ui, Kind::Ghost, icon::PLUS, "Add category", true).clicked() {
                    o.s.categories.push(Category { name: "New category".into(), extensions: vec![], folder: None });
                }
            }
            Tab::Proxy => {
                theme::page_title(ui, "Proxy", "Send RDM's connections through a proxy server.");
                theme::group(ui, "Proxy server", |ui| {
                    let mode = match o.s.proxy.mode.as_str() {
                        "" | "system" => 0,
                        "none" => 1,
                        _ => 2,
                    };
                    theme::setting(ui, "Use", "", |ui| {
                        let mut m = mode;
                        egui::ComboBox::from_id_salt("proxy-mode").width(200.0).selected_text(["System settings", "No proxy", "This proxy"][m]).show_ui(ui, |ui| {
                            ui.selectable_value(&mut m, 0, "System settings");
                            ui.selectable_value(&mut m, 1, "No proxy");
                            ui.selectable_value(&mut m, 2, "This proxy");
                        });
                        if m != mode {
                            o.s.proxy.mode = match m {
                                0 => "system".into(),
                                1 => "none".into(),
                                _ => "http://".into(),
                            };
                        }
                    });
                    if mode == 2 {
                        theme::divider(ui);
                        ui.label("Address");
                        text_field(ui, &mut o.s.proxy.mode, "http://host:8080 or socks5://host:1080");
                        let mut u = o.s.proxy.username.clone().unwrap_or_default();
                        let mut pw = o.s.proxy.password.clone().unwrap_or_default();
                        ui.columns(2, |c| {
                            c[0].label("User name (optional)");
                            text_field(&mut c[0], &mut u, "");
                            c[1].label("Password");
                            c[1].add(egui::TextEdit::singleline(&mut pw).password(true).desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)));
                        });
                        o.s.proxy.username = Some(u).filter(|x| !x.is_empty());
                        o.s.proxy.password = Some(pw).filter(|x| !x.is_empty());
                        theme::divider(ui);
                        ui.label("Connect directly to these hosts");
                        theme::hint(ui, "One per line.");
                        ui.add(egui::TextEdit::multiline(&mut o.bypass).desired_rows(3).desired_width(f32::INFINITY));
                    }
                });
            }
            Tab::Logins => {
                theme::page_title(ui, "Site logins", "User names and passwords RDM sends to sites that ask for them.");
                theme::group(ui, "Saved logins", |ui| {
                    if o.s.logins.is_empty() {
                        ui.label(RichText::new("No saved logins.").color(p.text_dim));
                    }
                    let mut remove = None;
                    for (i, l) in o.s.logins.iter().enumerate() {
                        if i > 0 {
                            theme::divider(ui);
                        }
                        ui.horizontal(|ui| {
                            ui.label(theme::ic(icon::KEY).color(p.text_dim));
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&l.prefix).font(theme::semibold(13.5)));
                                ui.label(RichText::new(&l.username).size(12.0).color(p.text_dim));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::icon_button(ui, icon::TRASH, "Remove").clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                    }
                    if let Some(i) = remove {
                        o.s.logins.remove(i);
                    }
                });
                theme::group(ui, "Add a login", |ui| {
                    ui.label("Site or folder address");
                    text_field(ui, &mut o.new_login.prefix, "https://example.com/members/");
                    ui.columns(2, |c| {
                        c[0].label("User name");
                        text_field(&mut c[0], &mut o.new_login.username, "");
                        c[1].label("Password");
                        c[1].add(egui::TextEdit::singleline(&mut o.new_login.password).password(true).desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)));
                    });
                    if theme::button(ui, Kind::Ghost, icon::PLUS, "Add login", !o.new_login.prefix.trim().is_empty()).clicked() {
                        o.s.logins.push(o.new_login.clone());
                        o.new_login = SiteLogin { prefix: String::new(), username: String::new(), password: String::new() };
                    }
                });
            }
            Tab::Advanced => {
                theme::page_title(ui, "Advanced", "");
                theme::group(ui, "Scan completed downloads", |ui| {
                    ui.label("Antivirus command");
                    theme::hint(ui, "Runs after each download finishes. {file} is replaced by the downloaded file.");
                    let mut v = o.s.virus_scan_command.clone().unwrap_or_default();
                    ui.add(egui::TextEdit::singleline(&mut v).hint_text("\"C:\\Program Files\\AV\\scan.exe\" \"{file}\"").desired_width(f32::INFINITY).margin(vec2(10.0, 7.0)));
                    o.s.virus_scan_command = Some(v).filter(|x| !x.trim().is_empty());
                });
            }
        });
    });
    if apply {
        o.apply(&app.manager);
        ctx.set_theme(if o.s.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
    }
    if close || closed {
        app.options = None;
    }
    if open_ext_help {
        app.ext_help = Some(crate::extension_help::ExtensionHelp::new());
    }
    if check_now {
        app.updater.check(&app.rt);
    }
}

// ---------------------------------------------------------------- Scheduler

fn scheduler(app: &mut App, ctx: &egui::Context) {
    let Some(sc) = app.scheduler.as_mut() else { return };
    let mut close = false;
    let mut save = false;
    let spec = Spec { title: "Scheduler", size: [820.0, 600.0], min: [680.0, 480.0], resizable: true };
    let (_, closed) = window::show(ctx, "scheduler", spec, |ui| {
        let p = theme::palette(ui);
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::CHECK, "Done", true).clicked() {
                save = true;
                close = true;
            }
            if theme::button(ui, Kind::Ghost, icon::CHECK_CIRCLE, "Apply", true).clicked() {
                save = true;
            }
        });
        egui::Panel::left("queues")
            .resizable(false)
            .exact_size(210.0)
            .frame(egui::Frame::new().fill(p.sidebar).inner_margin(egui::Margin { left: 10, right: 10, top: 16, bottom: 10 }))
            .show(ui, |ui| {
                ui.label(RichText::new("QUEUES").font(theme::semibold(11.0)).color(p.text_dim));
                ui.add_space(4.0);
                for (i, q) in sc.queues.iter().enumerate() {
                    let ic = if app.manager.is_queue_active(q.id) { icon::PLAY_CIRCLE } else { icon::QUEUE };
                    let name = theme::elide_end(ui, &q.name, &egui::FontId::proportional(14.0), ui.available_width() - 56.0);
                    if theme::side_tab(ui, ic, &name, sc.sel == i).clicked() {
                        sc.sel = i;
                    }
                }
                ui.add_space(8.0);
                if theme::button(ui, Kind::Ghost, icon::PLUS, "New queue", true).clicked() {
                    let id = sc.queues.iter().map(|q| q.id).max().unwrap_or(2) + 1;
                    sc.queues.push(QueueDef { id, name: format!("Queue {id}"), max_concurrent: 2, schedule: Schedule::default() });
                    sc.sel = sc.queues.len() - 1;
                }
            });
        window::body(ui, |ui| {
            let Some(q) = sc.queues.get_mut(sc.sel) else { return };
            let id = q.id;
            let active = app.manager.is_queue_active(id);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&q.name).font(theme::semibold(19.0)));
                    ui.label(RichText::new(if active { "Running" } else { "Stopped" }).color(if active { p.success } else { p.text_dim }));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if active {
                        if theme::button(ui, Kind::Ghost, icon::STOP, "Stop", true).clicked() {
                            app.manager.stop_queue(id);
                        }
                    } else if theme::button(ui, Kind::Primary, icon::PLAY, "Start now", true).clicked() {
                        app.manager.start_queue(id);
                    }
                });
            });
            ui.add_space(12.0);
            theme::group(ui, "Queue", |ui| {
                theme::setting(ui, "Name", "", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut q.name).desired_width(200.0).margin(vec2(10.0, 6.0)));
                });
                theme::divider(ui);
                theme::setting(ui, "Files at the same time", "How many downloads of this queue run together.", |ui| {
                    ui.add(egui::DragValue::new(&mut q.max_concurrent).range(1..=32));
                });
            });
            let s = &mut q.schedule;
            theme::group(ui, "Schedule", |ui| {
                theme::switch_setting(ui, "Run this queue on a schedule", "Starts and stops it by itself.", &mut s.enabled);
                ui.add_enabled_ui(s.enabled, |ui| {
                    theme::divider(ui);
                    for (is_start, title, hint) in [(true, "Start at", "24-hour time, e.g. 01:30"), (false, "Stop at", "Optional")] {
                        let saved = if is_start { &mut s.start_at } else { &mut s.stop_at };
                        let text = sc.times.entry((id, is_start)).or_insert_with(|| saved.clone().unwrap_or_default());
                        let mut bad = false;
                        theme::setting(ui, title, hint, |ui| {
                            ui.add(egui::TextEdit::singleline(text).hint_text("HH:MM").desired_width(70.0).margin(vec2(8.0, 6.0)));
                            bad = !text.trim().is_empty() && valid_hhmm(text).is_none();
                            if bad {
                                ui.label(theme::ic(icon::WARNING_CIRCLE).color(p.warning)).on_hover_text("Use HH:MM, e.g. 23:30");
                            }
                        });
                        if !bad {
                            *saved = valid_hhmm(text);
                        }
                    }
                    theme::divider(ui);
                    ui.label("Days");
                    ui.horizontal_wrapped(|ui| {
                        for (i, d) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
                            chip(ui, d, &mut s.days[i]);
                        }
                    });
                    theme::hint(ui, "No days selected means every day.");
                    theme::divider(ui);
                    theme::switch_setting(ui, "Run only once", "Turns the schedule off after it runs.", &mut s.once);
                    theme::divider(ui);
                    theme::setting(ui, "When the queue finishes", "", |ui| {
                        egui::ComboBox::from_id_salt("on_done").width(180.0).selected_text(s.on_done.label()).show_ui(ui, |ui| {
                            for a in OnQueueDone::all() {
                                ui.selectable_value(&mut s.on_done, a, a.label());
                            }
                        });
                    });
                });
            });
            let files: Vec<rdm_core::Item> = app.manager.items().into_iter().filter(|i| i.queue == id && i.state != rdm_core::ItemState::Complete).collect();
            theme::group(ui, &format!("Files in this queue ({})", files.len()), |ui| {
                if files.is_empty() {
                    ui.label(RichText::new("Nothing waiting. Use “Download later” or right-click a download → Move to queue.").color(p.text_dim));
                }
                for f in files.iter().take(200) {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), egui::Sense::hover());
                        theme::file_tile(ui, r, &f.file_name);
                        let w = ui.available_width();
                        ui.label(theme::elide_middle(ui, &f.file_name, &egui::FontId::proportional(14.0), w));
                    });
                }
            });
            if id > 2 && theme::button(ui, Kind::Danger, icon::TRASH, "Delete this queue", true).clicked() {
                sc.queues.remove(sc.sel);
                sc.sel = 0;
            }
        });
    });
    if save {
        let mut s = app.manager.settings();
        s.queues = sc.queues.clone();
        app.manager.set_settings(s);
    }
    if close || closed {
        app.scheduler = None;
    }
}

fn valid_hhmm(s: &str) -> Option<String> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}
