//! Dialog windows.

use crate::app::App;
use crate::util;
use eframe::egui::{self, RichText};
use rdm_core::settings::{Category, OnQueueDone, QueueDef, Schedule, Settings, SiteLogin, MAIN_QUEUE};
use rdm_core::{AddOptions, DownloadRequest, Manager, ProbeInfo};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- Add URL

pub struct AddUrl {
    url: String,
    use_auth: bool,
    user: String,
    pass: String,
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
        Self { url, use_auth: false, user: String::new(), pass: String::new() }
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
    FileTypes,
    Connection,
    SaveTo,
    Proxy,
    Logins,
    Downloads,
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
}

impl Scheduler {
    pub fn new(m: &Manager) -> Self {
        Self { queues: m.settings().queues, sel: 0 }
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

fn add_url(app: &mut App, ctx: &egui::Context) {
    let Some(d) = app.add_url.as_mut() else { return };
    let mut close = false;
    let mut ok = false;
    egui::Window::new("Enter new address to download").collapsible(false).resizable(false).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label("Address:");
            let r = ui.add(egui::TextEdit::singleline(&mut d.url).desired_width(420.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                ok = true;
            }
            r.request_focus();
        });
        ui.checkbox(&mut d.use_auth, "Use authorization");
        ui.add_enabled_ui(d.use_auth, |ui| {
            egui::Grid::new("auth").show(ui, |ui| {
                ui.label("Login");
                ui.text_edit_singleline(&mut d.user);
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut d.pass).password(true));
                ui.end_row();
            });
        });
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() {
                ok = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
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
    } else if close {
        app.add_url = None;
    }
}

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
    egui::Window::new("Download file info").collapsible(false).resizable(false).default_width(560.0).show(ctx, |ui| {
        egui::Grid::new("fi").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("URL");
            ui.add(egui::Label::new(&d.req.url).truncate());
            ui.end_row();
            ui.label("Category");
            let before = d.category.clone();
            egui::ComboBox::from_id_salt("cat").selected_text(d.category.clone().unwrap_or_else(|| "General".into())).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.category, None, "General");
                for c in &s.categories {
                    ui.selectable_value(&mut d.category, Some(c.name.clone()), &c.name);
                }
            });
            if before != d.category && !d.dir_edited {
                d.dir = s.folder_for(d.category.as_deref()).display().to_string();
            }
            ui.end_row();
            ui.label("Save to");
            ui.horizontal(|ui| {
                if ui.add(egui::TextEdit::singleline(&mut d.dir).desired_width(360.0)).changed() {
                    d.dir_edited = true;
                }
                if ui.button("…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_directory(&d.dir).pick_folder() {
                        d.dir = p.display().to_string();
                        d.dir_edited = true;
                    }
                }
            });
            ui.end_row();
            ui.label("File name");
            if ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(400.0)).changed() {
                d.name_edited = true;
            }
            ui.end_row();
            ui.label("Size");
            match &probe {
                None => ui.label("checking…"),
                Some(Ok(p)) => ui.label(match p.total {
                    Some(t) => format!("{}{}", util::size(t), if p.resumable { "" } else { "  (no resume support)" }),
                    None => "unknown".into(),
                }),
                Some(Err(e)) => ui.label(RichText::new(e).color(egui::Color32::from_rgb(210, 90, 90))),
            };
            ui.end_row();
            ui.label("Description");
            ui.text_edit_singleline(&mut d.description);
            ui.end_row();
        });
        if d.category.is_some() {
            ui.checkbox(&mut d.remember_dir, format!("Remember this path for \"{}\" category", d.category.clone().unwrap_or_default()));
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Start download").strong()).clicked() {
                result = Some(Some(true));
            }
            ui.menu_button("Download later ⏷", |ui| {
                for q in &s.queues {
                    if ui.button(&q.name).clicked() {
                        d.queue = q.id;
                        result = Some(Some(false));
                        ui.close();
                    }
                }
            });
            if ui.button("Cancel").clicked() {
                result = Some(None);
            }
        });
    });
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

fn batch(app: &mut App, ctx: &egui::Context) {
    let Some(b) = app.batch.as_mut() else { return };
    let mut close = false;
    let mut add: Option<bool> = None;
    egui::Window::new("Batch download").default_width(600.0).show(ctx, |ui| {
        ui.label("Address with a * wildcard, e.g. https://example.com/images/img*.jpg");
        let mut changed = ui.add(egui::TextEdit::singleline(&mut b.template).desired_width(560.0)).changed();
        ui.horizontal(|ui| {
            ui.label("From");
            changed |= ui.add(egui::TextEdit::singleline(&mut b.from).desired_width(60.0)).changed();
            ui.label("To");
            changed |= ui.add(egui::TextEdit::singleline(&mut b.to).desired_width(60.0)).changed();
            ui.label("Wildcard size");
            changed |= ui.add(egui::DragValue::new(&mut b.pad).range(0..=10)).changed();
        });
        if changed {
            b.expand();
        }
        ui.label(format!("{} links", b.links.len()));
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for (on, r) in b.links.iter_mut().take(2000) {
                ui.checkbox(on, &r.url);
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Add to queue").clicked() {
                add = Some(false);
            }
            if ui.button("Download now").clicked() {
                add = Some(true);
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
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
        close = true;
    }
    if close {
        app.batch = None;
    }
}

fn speed_limit(app: &mut App, ctx: &egui::Context) {
    let Some(d) = app.speed_limit.as_mut() else { return };
    let mut close = false;
    egui::Window::new("Speed limiter").collapsible(false).resizable(false).show(ctx, |ui| {
        ui.checkbox(&mut d.on, "Limit total download speed");
        ui.add_enabled(d.on, egui::DragValue::new(&mut d.kb).range(1..=10_000_000).suffix(" KB/s"));
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                app.manager.set_speed_limit(d.on.then_some(d.kb * 1024));
                close = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if close {
        app.speed_limit = None;
    }
}

fn options(app: &mut App, ctx: &egui::Context) {
    let Some(o) = app.options.as_mut() else { return };
    let mut close = false;
    let mut apply = false;
    egui::Window::new("Options").default_size([640.0, 460.0]).show(ctx, |ui| {
        ui.horizontal_wrapped(|ui| {
            for (t, n) in [
                (Tab::General, "General"),
                (Tab::FileTypes, "File types"),
                (Tab::Connection, "Connection"),
                (Tab::SaveTo, "Save to"),
                (Tab::Proxy, "Proxy"),
                (Tab::Logins, "Site logins"),
                (Tab::Downloads, "Downloads"),
            ] {
                ui.selectable_value(&mut o.tab, t, n);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| match o.tab {
            Tab::General => {
                ui.checkbox(&mut o.s.start_with_system, "Launch RDM on startup");
                ui.checkbox(&mut o.s.monitor_clipboard, "Monitor clipboard for download links");
                ui.checkbox(&mut o.s.confirm_captured, "Show the file-info dialog for downloads captured from the browser");
                ui.checkbox(&mut o.s.show_complete_dialog, "Show the download-complete dialog");
                ui.checkbox(&mut o.s.dark_mode, "Dark theme");
                ui.add_space(8.0);
                ui.label(RichText::new("Browser integration").strong());
                ui.label("Registers the browser bridge and offers the RDM extension to installed browsers.");
                ui.horizontal(|ui| {
                    if ui.button("Set up browser integration").clicked() {
                        o.bridge_msg = Some(match crate::bridge::register() {
                            Ok(m) => m,
                            Err(e) => format!("Failed: {e}"),
                        });
                    }
                    if ui.button("Remove browser integration").clicked() {
                        o.bridge_msg = Some(crate::bridge::unregister());
                    }
                });
                if let Some(m) = &o.bridge_msg {
                    ui.label(m);
                }
                ui.add_space(8.0);
                ui.label(RichText::new("Download button").strong());
                ui.checkbox(&mut o.s.capture.show_video_button, "Show the download button over videos");
                ui.checkbox(&mut o.s.capture.show_audio_button, "Show it over audio players too");
                ui.label("Never show the button on these sites (one per line, * wildcards):");
                ui.add(egui::TextEdit::multiline(&mut o.no_button_hosts).desired_rows(3).desired_width(f32::INFINITY));
                ui.add_space(8.0);
                ui.label(RichText::new("Keys").strong());
                let keys = |ui: &mut egui::Ui, id: &str, v: &mut String, opts: &[&str]| {
                    egui::ComboBox::from_id_salt(id.to_string()).selected_text(key_label(v)).show_ui(ui, |ui| {
                        for k in opts {
                            ui.selectable_value(v, k.to_string(), key_label(k));
                        }
                    });
                };
                egui::Grid::new("keys").num_columns(2).show(ui, |ui| {
                    ui.label("Hold while clicking to always download with RDM");
                    keys(ui, "force_key", &mut o.s.capture.force_key, &["none", "alt", "shift", "insert"]);
                    ui.end_row();
                    ui.label("Hold while clicking to let the browser download");
                    keys(ui, "skip_key", &mut o.s.capture.skip_key, &["none", "ctrl", "shift", "delete"]);
                    ui.end_row();
                });
            }
            Tab::FileTypes => {
                ui.label("Automatically take over downloads of these file types:");
                ui.add(egui::TextEdit::multiline(&mut o.exts).desired_rows(4).desired_width(f32::INFINITY));
                ui.label("Media types that show the video download button:");
                ui.add(egui::TextEdit::multiline(&mut o.media_exts).desired_rows(2).desired_width(f32::INFINITY));
                ui.label("Never take over downloads from these sites (one per line, * wildcards):");
                ui.add(egui::TextEdit::multiline(&mut o.hosts).desired_rows(4).desired_width(f32::INFINITY));
                ui.label("Never take over these addresses:");
                ui.add(egui::TextEdit::multiline(&mut o.urls).desired_rows(3).desired_width(f32::INFINITY));
                ui.label("Never treat these types as downloads:");
                ui.add(egui::TextEdit::multiline(&mut o.ignore_exts).desired_rows(2).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    ui.label("Don't take over files smaller than");
                    let mut kb = o.s.capture.min_size / 1024;
                    if ui.add(egui::DragValue::new(&mut kb).suffix(" KB")).changed() {
                        o.s.capture.min_size = kb * 1024;
                    }
                });
            }
            Tab::Connection => {
                let e = &mut o.s.engine;
                egui::Grid::new("conn").num_columns(2).show(ui, |ui| {
                    ui.label("Max. connections per download");
                    ui.add(egui::DragValue::new(&mut e.max_connections).range(1..=32));
                    ui.end_row();
                    ui.label("Initial connections");
                    ui.add(egui::DragValue::new(&mut e.initial_connections).range(1..=32));
                    ui.end_row();
                    ui.label("Retries per segment (0 = unlimited)");
                    ui.add(egui::DragValue::new(&mut e.max_retries).range(0..=1000));
                    ui.end_row();
                    ui.label("Timeout (seconds)");
                    let mut t = e.timeout.as_secs();
                    if ui.add(egui::DragValue::new(&mut t).range(5..=600)).changed() {
                        e.timeout = std::time::Duration::from_secs(t);
                    }
                    ui.end_row();
                    ui.label("User agent");
                    ui.add(egui::TextEdit::singleline(&mut e.user_agent).desired_width(360.0));
                    ui.end_row();
                });
                ui.separator();
                ui.label(RichText::new("Download limits").strong());
                ui.checkbox(&mut o.s.quota.enabled, "Limit how much is downloaded");
                ui.add_enabled_ui(o.s.quota.enabled, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Download at most");
                        ui.add(egui::DragValue::new(&mut o.s.quota.megabytes).suffix(" MB"));
                        ui.label("every");
                        ui.add(egui::DragValue::new(&mut o.s.quota.hours).range(1..=23).suffix(" hours"));
                    });
                });
            }
            Tab::SaveTo => {
                folder_row(ui, "Default download folder", &mut o.s.download_dir);
                folder_row(ui, "Temporary folder", &mut o.s.temp_dir);
                ui.separator();
                ui.label(RichText::new("Categories").strong());
                let mut remove = None;
                for (i, c) in o.s.categories.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(120.0));
                            if ui.button("Remove").clicked() {
                                remove = Some(i);
                            }
                        });
                        let mut exts = c.extensions.join(" ");
                        if ui.add(egui::TextEdit::singleline(&mut exts).desired_width(f32::INFINITY)).changed() {
                            c.extensions = exts.split_whitespace().map(str::to_ascii_lowercase).collect();
                        }
                        let mut f = c.folder.clone().map(|p| p.display().to_string()).unwrap_or_default();
                        ui.horizontal(|ui| {
                            ui.label("Folder");
                            if ui.add(egui::TextEdit::singleline(&mut f).hint_text("(default)").desired_width(380.0)).changed() {
                                c.folder = Some(PathBuf::from(&f)).filter(|p| !p.as_os_str().is_empty());
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    o.s.categories.remove(i);
                }
                if ui.button("Add category").clicked() {
                    o.s.categories.push(Category { name: "New".into(), extensions: vec![], folder: None });
                }
            }
            Tab::Proxy => {
                ui.label("Proxy: \"system\", \"none\", or a URL such as http://host:8080 or socks5://host:1080");
                ui.text_edit_singleline(&mut o.s.proxy.mode);
                let mut u = o.s.proxy.username.clone().unwrap_or_default();
                let mut p = o.s.proxy.password.clone().unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label("User");
                    ui.text_edit_singleline(&mut u);
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut p).password(true));
                });
                o.s.proxy.username = Some(u).filter(|x| !x.is_empty());
                o.s.proxy.password = Some(p).filter(|x| !x.is_empty());
            }
            Tab::Logins => {
                let mut remove = None;
                for (i, l) in o.s.logins.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}  —  {}", l.prefix, l.username));
                        if ui.button("Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    o.s.logins.remove(i);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut o.new_login.prefix).hint_text("https://site.com/path/").desired_width(220.0));
                    ui.add(egui::TextEdit::singleline(&mut o.new_login.username).hint_text("user").desired_width(100.0));
                    ui.add(egui::TextEdit::singleline(&mut o.new_login.password).hint_text("password").password(true).desired_width(100.0));
                    if ui.button("Add").clicked() && !o.new_login.prefix.is_empty() {
                        o.s.logins.push(o.new_login.clone());
                        o.new_login = SiteLogin { prefix: String::new(), username: String::new(), password: String::new() };
                    }
                });
            }
            Tab::Downloads => {
                ui.label("Antivirus program to run when a download completes ({file} = downloaded file):");
                let mut v = o.s.virus_scan_command.clone().unwrap_or_default();
                ui.add(egui::TextEdit::singleline(&mut v).hint_text("\"C:\\Program Files\\AV\\scan.exe\" \"{file}\"").desired_width(f32::INFINITY));
                o.s.virus_scan_command = Some(v).filter(|x| !x.trim().is_empty());
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() {
                apply = true;
                close = true;
            }
            if ui.button("Apply").clicked() {
                apply = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if apply {
        o.apply(&app.manager);
        ctx.set_theme(if o.s.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
    }
    if close {
        app.options = None;
    }
}

fn folder_row(ui: &mut egui::Ui, label: &str, p: &mut PathBuf) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut t = p.display().to_string();
        if ui.add(egui::TextEdit::singleline(&mut t).desired_width(360.0)).changed() {
            *p = PathBuf::from(t);
        }
        if ui.button("…").clicked() {
            if let Some(d) = rfd::FileDialog::new().set_directory(&*p).pick_folder() {
                *p = d;
            }
        }
    });
}

fn scheduler(app: &mut App, ctx: &egui::Context) {
    let Some(sc) = app.scheduler.as_mut() else { return };
    let mut close = false;
    let mut save = false;
    egui::Window::new("Scheduler").default_size([620.0, 420.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(170.0);
                for (i, q) in sc.queues.iter().enumerate() {
                    ui.selectable_value(&mut sc.sel, i, &q.name);
                }
                if ui.button("New queue").clicked() {
                    let id = sc.queues.iter().map(|q| q.id).max().unwrap_or(2) + 1;
                    sc.queues.push(QueueDef { id, name: format!("Queue {id}"), max_concurrent: 2, schedule: Schedule::default() });
                    sc.sel = sc.queues.len() - 1;
                }
                if sc.queues.get(sc.sel).is_some_and(|q| q.id > 2) && ui.button("Delete queue").clicked() {
                    sc.queues.remove(sc.sel);
                    sc.sel = 0;
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                let Some(q) = sc.queues.get_mut(sc.sel) else { return };
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut q.name);
                });
                ui.horizontal(|ui| {
                    ui.label("Download");
                    ui.add(egui::DragValue::new(&mut q.max_concurrent).range(1..=32));
                    ui.label("files at the same time");
                });
                ui.separator();
                let s = &mut q.schedule;
                ui.checkbox(&mut s.enabled, "Run this queue on a schedule");
                ui.add_enabled_ui(s.enabled, |ui| {
                    let mut start = s.start_at.clone().unwrap_or_default();
                    let mut stop = s.stop_at.clone().unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label("Start at (HH:MM)");
                        ui.add(egui::TextEdit::singleline(&mut start).desired_width(60.0));
                        ui.label("Stop at");
                        ui.add(egui::TextEdit::singleline(&mut stop).desired_width(60.0));
                    });
                    s.start_at = valid_hhmm(&start);
                    s.stop_at = valid_hhmm(&stop);
                    ui.horizontal(|ui| {
                        for (i, d) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
                            ui.checkbox(&mut s.days[i], *d);
                        }
                    });
                    ui.label(RichText::new("No days selected = every day").small().weak());
                    ui.checkbox(&mut s.once, "Run only once");
                    egui::ComboBox::from_label("When the queue finishes").selected_text(format!("{:?}", s.on_done)).show_ui(ui, |ui| {
                        for a in [OnQueueDone::Nothing, OnQueueDone::ExitApp, OnQueueDone::Sleep, OnQueueDone::Hibernate, OnQueueDone::Shutdown] {
                            ui.selectable_value(&mut s.on_done, a, format!("{a:?}"));
                        }
                    });
                });
                ui.separator();
                let id = q.id;
                ui.horizontal(|ui| {
                    if ui.button("Start now").clicked() {
                        app.manager.start_queue(id);
                    }
                    if ui.button("Stop").clicked() {
                        app.manager.stop_queue(id);
                    }
                });
                let files: Vec<String> = app.manager.items().into_iter().filter(|i| i.queue == id && i.state != rdm_core::ItemState::Complete).map(|i| i.file_name).collect();
                ui.label(format!("{} file(s) in this queue", files.len()));
                egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                    for f in files {
                        ui.label(f);
                    }
                });
            });
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                save = true;
            }
            if ui.button("Close").clicked() {
                save = true;
                close = true;
            }
        });
    });
    if save {
        let mut s = app.manager.settings();
        s.queues = sc.queues.clone();
        app.manager.set_settings(s);
    }
    if close {
        app.scheduler = None;
    }
}

fn valid_hhmm(s: &str) -> Option<String> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}
