//! Main window: sidebar, header, download list, progress windows, popups.

use crate::dialogs::{self, AddUrl, Batch, FileInfo, Options, Scheduler, SpeedLimit};
use crate::grabber_ui::Grabber;
use crate::theme::{self, Kind, icon};
use crate::tray::{Tray, TrayAction};
use crate::util;
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, Rect, RichText, Sense, Stroke, pos2, vec2};
use rdm_core::ipc::UiRequest;
use rdm_core::settings::{MAIN_QUEUE, OnQueueDone};
use rdm_core::{DownloadRequest, Event, Item, ItemId, ItemState, Manager};
use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    All,
    Category(Option<String>),
    Unfinished,
    Finished,
    Queue(u32),
}

pub struct MediaOffer {
    pub request: DownloadRequest,
    pub title: Option<String>,
    pub kind: String,
}

pub struct App {
    /// The app icon for the sidebar header, at the pixel size of the current display scale.
    logo: Option<(u32, egui::TextureHandle)>,
    /// The logo artwork for the sidebar header: (width px, dark theme, texture).
    brand: Option<(u32, bool, egui::TextureHandle)>,
    pub rt: tokio::runtime::Handle,
    pub manager: Manager,
    ui_rx: mpsc::UnboundedReceiver<UiRequest>,
    events: broadcast::Receiver<Event>,
    filter: Filter,
    search: String,
    selected: BTreeSet<ItemId>,
    anchor: Option<ItemId>,
    pub add_url: Option<AddUrl>,
    pub file_infos: VecDeque<FileInfo>,
    pub batch: Option<Batch>,
    pub options: Option<Options>,
    pub scheduler: Option<Scheduler>,
    pub speed_limit: Option<SpeedLimit>,
    pub grabber: Option<Grabber>,
    /// "Install the browser extension" helper window.
    pub ext_help: Option<crate::extension_help::ExtensionHelp>,
    /// The setup banner was closed this session.
    ext_banner_hidden: bool,
    /// RDM_SCREENSHOT=<window>: open that window once at start-up (documentation screenshots).
    screenshot: Option<String>,
    progress_windows: BTreeSet<ItemId>,
    completed: VecDeque<ItemId>,
    confirm_delete: Option<Vec<ItemId>>,
    media: VecDeque<MediaOffer>,
    queue_action: Option<(OnQueueDone, Instant)>,
    clipboard: Option<arboard::Clipboard>,
    last_clip: String,
    last_clip_check: Instant,
    status_msg: Option<(String, Instant)>,
    tray: Option<Tray>,
    exiting: bool,
}

fn category_icon(name: &str) -> &'static str {
    match name {
        "Compressed" => icon::FILE_ZIP,
        "Documents" => icon::FILE_TEXT,
        "Music" => icon::MUSIC_NOTES,
        "Programs" => icon::APP_WINDOW,
        "Video" => icon::FILM_STRIP,
        _ => icon::FOLDER_OPEN,
    }
}

impl App {
    pub fn new(
        ctx: &egui::Context,
        rt: tokio::runtime::Handle,
        manager: Manager,
        ui_rx: mpsc::UnboundedReceiver<UiRequest>,
        initial: Vec<DownloadRequest>,
    ) -> Self {
        let events = manager.subscribe();
        let _ = ctx;
        let logo = None;
        let mut app = Self {
            logo,
            brand: None,
            ext_help: None,
            ext_banner_hidden: false,
            screenshot: std::env::var("RDM_SCREENSHOT").ok().filter(|s| !s.is_empty()),
            rt,
            manager,
            ui_rx,
            events,
            filter: Filter::All,
            search: String::new(),
            selected: BTreeSet::new(),
            anchor: None,
            add_url: None,
            file_infos: VecDeque::new(),
            batch: None,
            options: None,
            scheduler: None,
            speed_limit: None,
            grabber: None,
            progress_windows: BTreeSet::new(),
            completed: VecDeque::new(),
            confirm_delete: None,
            media: VecDeque::new(),
            queue_action: None,
            clipboard: arboard::Clipboard::new().ok(),
            last_clip: String::new(),
            last_clip_check: Instant::now(),
            status_msg: None,
            tray: crate::tray::create(ctx.clone()),
            exiting: false,
        };
        if let Some(cb) = app.clipboard.as_mut() {
            app.last_clip = cb.get_text().unwrap_or_default();
        }
        for r in initial {
            app.open_file_info(r);
        }
        app
    }

    pub fn open_file_info(&mut self, req: DownloadRequest) {
        let fi = FileInfo::new(req, &self.manager, &self.rt);
        self.file_infos.push_back(fi);
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }

    /// Screenshot mode: open the requested window once its content is available.
    fn open_screenshot_window(&mut self) {
        let Some(which) = self.screenshot.clone() else { return };
        let done = match which.as_str() {
            "options" => {
                self.options = Some(Options::new(&self.manager));
                true
            }
            "scheduler" => {
                self.scheduler = Some(Scheduler::new(&self.manager));
                true
            }
            "grabber" => {
                let mut g = Grabber::default();
                g.set_mirror_dir(self.manager.settings().download_dir.join("Websites"));
                g.set_start_url("https://example.com/gallery/");
                self.grabber = Some(g);
                true
            }
            "extension" => {
                self.ext_help = Some(crate::extension_help::ExtensionHelp::new());
                true
            }
            "new" => {
                self.new_download();
                if let Some(d) = self.add_url.as_mut() {
                    d.url = "https://releases.example.com/linux/distro-26.04-desktop-amd64.iso".into();
                }
                true
            }
            "details" => match self.manager.items().into_iter().find(|i| i.state == ItemState::Running && i.downloaded > 0) {
                Some(it) => {
                    self.progress_windows.insert(it.id);
                    true
                }
                None => false,
            },
            _ => true,
        };
        if done {
            self.screenshot = None;
        }
    }

    fn new_download(&mut self) {
        self.add_url = Some(AddUrl::from_clipboard(self.clipboard.as_mut()));
    }

    fn poll_external(&mut self, ctx: &egui::Context) {
        while let Ok(req) = self.ui_rx.try_recv() {
            match req {
                UiRequest::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiRequest::ConfirmAdd(r) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.open_file_info(r);
                }
                UiRequest::ConfirmMany(rs) => {
                    let mut b = Batch::default();
                    b.links = rs.into_iter().map(|r| (true, r)).collect();
                    self.batch = Some(b);
                }
                UiRequest::Media { request, title, kind } => self.media.push_back(MediaOffer { request, title, kind }),
            }
        }
        loop {
            match self.events.try_recv() {
                Ok(Event::Completed(id)) => {
                    if self.manager.settings().show_complete_dialog {
                        self.completed.push_back(id);
                    }
                    self.progress_windows.remove(&id);
                }
                Ok(Event::Failed(id, e)) => {
                    let name = self.manager.item(id).map(|i| i.file_name).unwrap_or_default();
                    self.flash(format!("{name}: {e}"));
                }
                Ok(Event::QueueDone(_, action)) if action != OnQueueDone::Nothing => {
                    self.queue_action = Some((action, Instant::now()));
                }
                Ok(Event::QuotaExceeded(until)) => {
                    self.flash(format!("Download quota reached; downloads resume at {}", util::date(Some(until))));
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        if self.last_clip_check.elapsed() >= Duration::from_secs(1) {
            self.last_clip_check = Instant::now();
            let s = self.manager.settings();
            if s.monitor_clipboard {
                if let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
                    if text != self.last_clip {
                        self.last_clip = text.clone();
                        if let Some(link) = util::clipboard_link(&text, &s.capture.extensions) {
                            self.open_file_info(DownloadRequest { url: link, ..Default::default() });
                        }
                    }
                }
            }
        }
    }

    fn visible_items(&self, all: &[Item]) -> Vec<Item> {
        let q = self.search.to_lowercase();
        let mut v: Vec<Item> = all
            .iter()
            .filter(|i| match &self.filter {
                Filter::All => true,
                Filter::Category(c) => &i.category == c,
                Filter::Unfinished => i.state != ItemState::Complete,
                Filter::Finished => i.state == ItemState::Complete,
                Filter::Queue(qid) => i.queue == *qid && i.state != ItemState::Complete,
            })
            .filter(|i| q.is_empty() || i.file_name.to_lowercase().contains(&q) || i.request.url.to_lowercase().contains(&q))
            .cloned()
            .collect();
        v.sort_by_key(|i| std::cmp::Reverse((i.added, i.id)));
        v
    }

    fn filter_title(&self) -> String {
        match &self.filter {
            Filter::All => "All downloads".into(),
            Filter::Category(Some(c)) => c.clone(),
            Filter::Category(None) => "General".into(),
            Filter::Unfinished => "Unfinished".into(),
            Filter::Finished => "Completed".into(),
            Filter::Queue(q) => self.manager.settings().queues.into_iter().find(|x| x.id == *q).map(|x| x.name).unwrap_or_default(),
        }
    }

    // ------------------------------------------------------------- sidebar

    /// Sidebar header: the logo artwork (icon + lettering), rendered from the SVG at the
    /// exact pixel size of the display; a light-theme variant keeps the lettering readable.
    fn sidebar_logo(&mut self, ui: &mut egui::Ui) {
        let ppp = ui.ctx().pixels_per_point();
        let dark = ui.visuals().dark_mode;
        let w_pt = (ui.available_width() - 4.0).clamp(120.0, 260.0);
        let (w_px, h_px) = ((w_pt * ppp).round() as u32, (w_pt * ppp / LOGO_ASPECT).round() as u32);
        if self.brand.as_ref().map(|(w, d, _)| (*w, *d)) != Some((w_px, dark)) {
            let svg = if dark { LOGO_WIDE_DARK } else { LOGO_WIDE_LIGHT };
            self.brand = render_svg(ui.ctx(), svg, w_px, h_px, &format!("rdm-brand-{w_px}-{dark}")).map(|t| (w_px, dark, t));
        }
        let size = vec2(w_px as f32 / ppp, h_px as f32 / ppp);
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), size.y), Sense::hover());
        let snap = |v: f32| (v * ppp).round() / ppp;
        let rect = Rect::from_min_size(pos2(snap(row.left() + 2.0), snap(row.top())), size);
        match &self.brand {
            Some((_, _, tex)) => {
                ui.painter().image(tex.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => self.sidebar_header_text(ui),
        }
    }

    /// Previous sidebar header: app icon + "RDM" (blue M) + tagline. Not used now.
    #[allow(dead_code)]
    fn sidebar_header_text(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        // Brand: glowing logo tile with a soft highlight, name and tagline centred on it.
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
        // The icon is rendered from the SVG at the display's exact pixel size and drawn 1:1 on
        // the pixel grid, so it stays sharp at any scale.
        let ppp = ui.ctx().pixels_per_point();
        let px = logo_pixels(50.0 * ppp);
        if self.logo.as_ref().map(|(s, _)| *s) != Some(px) {
            self.logo = load_logo(ui.ctx(), px).map(|t| (px, t));
        }
        let side = px as f32 / ppp;
        let snap = |v: f32| (v * ppp).round() / ppp;
        let logo = Rect::from_min_size(pos2(snap(row.left() + 2.0), snap(row.center().y - side / 2.0)), vec2(side, side));
        let painter = ui.painter();
        match &self.logo {
            // The app icon (it carries its own glow).
            Some((_, tex)) => {
                painter.image(tex.id(), logo, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                let radius = CornerRadius::same(13);
                painter.rect_filled(logo.shrink(4.0), radius, p.accent);
                painter.text(logo.center(), Align2::CENTER_CENTER, icon::DOWNLOAD_SIMPLE, theme::icon_font(22.0), Color32::WHITE);
            }
        }
        // "RD" + "M" in the accent blue, as in the logo artwork.
        let tx = logo.right() + 10.0;
        let mut job = egui::text::LayoutJob::default();
        job.append("RD", 0.0, egui::TextFormat { font_id: theme::semibold(21.0), color: p.text, ..Default::default() });
        job.append("M", 0.0, egui::TextFormat { font_id: theme::semibold(21.0), color: p.accent, ..Default::default() });
        let galley = painter.layout_job(job);
        painter.galley(pos2(tx, row.center().y + 1.0 - galley.size().y), galley, p.text);
        painter.text(pos2(tx + 1.0, row.center().y + 4.0), Align2::LEFT_TOP, "Download Manager", FontId::proportional(11.5), p.text_dim);
    }

    /// Until the browser extension has connected once: a slim card pointing to the helper.
    fn extension_banner(&mut self, ui: &mut egui::Ui) {
        if self.ext_banner_hidden || self.manager.settings().extension_connected_once || rdm_core::ipc::last_extension_contact().is_some() {
            return;
        }
        let p = theme::palette(ui);
        egui::Frame::new().fill(p.accent_soft).corner_radius(CornerRadius::same(12)).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::ic(icon::PUZZLE_PIECE).size(18.0).color(p.accent));
                ui.label(RichText::new("Add the RDM extension to your browser to catch downloads and videos automatically.").color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, icon::X, "Hide").clicked() {
                        self.ext_banner_hidden = true;
                    }
                    if theme::button(ui, Kind::Primary, icon::PUZZLE_PIECE, "Set up", true).clicked() {
                        self.ext_help = Some(crate::extension_help::ExtensionHelp::new());
                    }
                });
            });
        });
        ui.add_space(10.0);
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, items: &[Item]) {
        let p = theme::palette(ui);
        ui.add_space(16.0);
        // Brand: the RDM logo artwork. (The previous icon + "RDM" text header is kept in
        // `sidebar_header_text`; call that here instead to switch back.)
        self.sidebar_logo(ui);
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let w = ui.available_width() - 6.0;
            let resp = ui.scope(|ui| {
                ui.set_max_width(w);
                theme::cta_button(ui, icon::PLUS, "New download")
            });
            if resp.inner.clicked() {
                self.new_download();
            }
        });
        ui.add_space(12.0);

        // Speed card pinned to the bottom.
        let s = self.manager.settings();
        let running: Vec<&Item> = items.iter().filter(|i| i.state == ItemState::Running).collect();
        let total: f64 = running.iter().map(|i| i.speed).sum();
        egui::Panel::bottom("speed_card").frame(egui::Frame::new().inner_margin(Margin { left: 4, right: 4, top: 8, bottom: 12 })).show(ui, |ui| {
            theme::card(ui).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(theme::ic(icon::GAUGE).size(20.0).color(p.accent));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(if total >= 1.0 { util::speed(total) } else { "Idle".into() }).font(theme::semibold(15.0)));
                        let lim = match s.speed_limit {
                            Some(l) => format!("{} active · limit {}/s", running.len(), util::size(l)),
                            None => format!("{} active · no limit", running.len()),
                        };
                        ui.label(RichText::new(lim).size(11.5).color(p.text_dim));
                    });
                });
            });
        });
        let count = |f: &dyn Fn(&Item) -> bool| items.iter().filter(|i| f(i)).count();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width() - 10.0);
            theme::caption(ui, "Library");
            let mut nav = |ui: &mut egui::Ui, f: Filter, ic: &str, label: &str, n: usize| {
                if theme::nav_item(ui, ic, label, Some(n), self.filter == f).clicked() {
                    self.filter = f;
                    self.selected.clear();
                }
            };
            nav(ui, Filter::All, icon::SQUARES_FOUR, "All downloads", items.len());
            nav(ui, Filter::Unfinished, icon::HOURGLASS_MEDIUM, "Unfinished", count(&|i| i.state != ItemState::Complete));
            nav(ui, Filter::Finished, icon::CHECK_CIRCLE, "Completed", count(&|i| i.state == ItemState::Complete));
            theme::caption(ui, "Categories");
            for c in &s.categories {
                let n = count(&|i| i.category.as_deref() == Some(&c.name));
                nav(ui, Filter::Category(Some(c.name.clone())), category_icon(&c.name), &c.name, n);
            }
            nav(ui, Filter::Category(None), icon::FOLDER_OPEN, "General", count(&|i| i.category.is_none()));
            theme::caption(ui, "Queues");
            for q in &s.queues {
                let n = count(&|i| i.queue == q.id && i.state != ItemState::Complete);
                let ic = if self.manager.is_queue_active(q.id) { icon::PLAY_CIRCLE } else { icon::QUEUE };
                nav(ui, Filter::Queue(q.id), ic, &q.name, n);
            }
        });

    }

    // ------------------------------------------------------------- header

    fn header(&mut self, ui: &mut egui::Ui, visible: &[Item]) {
        let p = theme::palette(ui);
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(self.filter_title()).font(theme::semibold(22.0)));
                let active = visible.iter().filter(|i| i.state == ItemState::Running).count();
                ui.label(RichText::new(format!("{} items · {} downloading", visible.len(), active)).size(12.5).color(p.text_dim));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Overflow menu.
                let more = theme::icon_button(ui, icon::DOTS_THREE, "More");
                egui::Popup::menu(&more).show(|ui| {
                    ui.set_min_width(220.0);
                    if ui.button(theme::mi(icon::GLOBE, "Site grabber…")).clicked() {
                        let mut g = Grabber::default();
                        g.set_mirror_dir(self.manager.settings().download_dir.join("Websites"));
                        self.grabber = Some(g);
                    }
                    if ui.button(theme::mi(icon::PUZZLE_PIECE, "Install browser extension…")).clicked() {
                        self.ext_help = Some(crate::extension_help::ExtensionHelp::new());
                    }
                    if ui.button(theme::mi(icon::LIST_CHECKS, "Batch download…")).clicked() {
                        self.batch = Some(Batch::default());
                    }
                    if ui.button(theme::mi(icon::COPY, "Batch from clipboard")).clicked() {
                        let text = self.clipboard.as_mut().and_then(|c| c.get_text().ok()).unwrap_or_default();
                        self.batch = Some(Batch::from_text(&text));
                    }
                    ui.separator();
                    if ui.button(theme::mi(icon::TRAY_ARROW_DOWN, "Import list…")).clicked() {
                        let n = dialogs::import_list(&self.manager);
                        self.flash(format!("Imported {n} links"));
                    }
                    if ui.button(theme::mi(icon::ARROW_SQUARE_OUT, "Export list…")).clicked() {
                        dialogs::export_list(&self.manager);
                    }
                    ui.separator();
                    let dark = ui.visuals().dark_mode;
                    let (ic, label) = if dark { (icon::SUN, "Light theme") } else { (icon::MOON, "Dark theme") };
                    if ui.button(theme::mi(ic, label)).clicked() {
                        let mut s = self.manager.settings();
                        s.dark_mode = !dark;
                        ui.ctx().set_theme(if s.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
                        self.manager.set_settings(s);
                    }
                    if ui.button(theme::mi(icon::FOLDER_OPEN, "Open data folder")).clicked() {
                        util::open_file(self.manager.data_dir());
                    }
                });
                if theme::icon_button(ui, icon::GEAR_SIX, "Settings").clicked() {
                    self.options = Some(Options::new(&self.manager));
                }
                if theme::icon_button(ui, icon::GAUGE, "Speed limiter").clicked() {
                    self.speed_limit = Some(SpeedLimit::new(&self.manager));
                }
                if theme::icon_button(ui, icon::CALENDAR_DOTS, "Scheduler").clicked() {
                    self.scheduler = Some(Scheduler::new(&self.manager));
                }
                let q_active = self.manager.is_queue_active(MAIN_QUEUE);
                if q_active {
                    if theme::icon_button(ui, icon::STOP_CIRCLE, "Stop queue").clicked() {
                        self.manager.stop_queue(MAIN_QUEUE);
                    }
                } else if theme::icon_button(ui, icon::PLAY_CIRCLE, "Start queue").clicked() {
                    self.manager.start_queue(MAIN_QUEUE);
                }
                ui.add_space(6.0);
                // Search.
                let search = egui::TextEdit::singleline(&mut self.search).hint_text("Search downloads").desired_width(210.0).margin(vec2(10.0, 7.0));
                ui.add(search);
                ui.label(theme::ic(icon::MAGNIFYING_GLASS).color(p.text_dim));
            });
        });
        ui.add_space(10.0);
        // Action strip for the selection. In a narrow window the buttons show icons only
        // (the name appears on hover), so nothing gets cut off.
        let compact = ui.available_width() < 760.0;
        let bt = |ui: &mut egui::Ui, kind: Kind, ic: &str, label: &str, on: bool| {
            if compact { theme::button(ui, kind, ic, "", on).on_hover_text(label) } else { theme::button(ui, kind, ic, label, on) }
        };
        ui.horizontal(|ui| {
            let has_sel = !self.selected.is_empty();
            if bt(ui, Kind::Ghost, icon::PLAY, "Resume", has_sel).clicked() {
                for id in self.selected.clone() {
                    self.manager.start(id);
                }
            }
            if bt(ui, Kind::Ghost, icon::PAUSE, "Pause", has_sel).clicked() {
                for id in self.selected.clone() {
                    self.manager.stop(id);
                }
            }
            if bt(ui, Kind::Ghost, icon::STOP, "Pause all", true).clicked() {
                self.manager.stop_all();
            }
            if bt(ui, Kind::Ghost, icon::ARROW_CLOCKWISE, "Resume all", true).clicked() {
                self.manager.start_all();
            }
            ui.add_space(6.0);
            if bt(ui, Kind::Danger, icon::TRASH, "Delete", has_sel).clicked() {
                self.confirm_delete = Some(self.selected.iter().copied().collect());
            }
            if bt(ui, Kind::Ghost, icon::BROOM, "Clear completed", true).clicked() {
                for i in self.manager.items() {
                    if i.state == ItemState::Complete {
                        self.manager.remove(i.id, false);
                    }
                }
            }
        });
        ui.add_space(8.0);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.border));
    }

    // ------------------------------------------------------------- list

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.28);
            let (r, _) = ui.allocate_exact_size(vec2(72.0, 72.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(20), p.accent_soft);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, icon::CLOUD_ARROW_DOWN, theme::icon_font(36.0), p.accent);
            ui.add_space(14.0);
            ui.label(RichText::new("No downloads here yet").font(theme::semibold(17.0)));
            ui.label(RichText::new("Paste a link, drop it from your browser, or start a new download.").color(p.text_dim));
            ui.add_space(12.0);
            if theme::button(ui, Kind::Primary, icon::PLUS, "New download", true).clicked() {
                self.new_download();
            }
        });
    }

    fn list(&mut self, ui: &mut egui::Ui, items: &[Item]) {
        if items.is_empty() {
            self.empty_state(ui);
            return;
        }
        let ids: Vec<ItemId> = items.iter().map(|i| i.id).collect();
        let mut clicked: Option<(ItemId, egui::Modifiers)> = None;
        let mut double: Option<ItemId> = None;
        let mut action: Option<(ItemId, &'static str)> = None;
        let row_h = 68.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h + 6.0, items.len(), |ui, range| {
            for it in &items[range] {
                let (resp, act) = self.row(ui, it, row_h);
                if resp.clicked() {
                    clicked = Some((it.id, ui.input(|i| i.modifiers)));
                }
                if resp.double_clicked() {
                    double = Some(it.id);
                }
                if let Some(a) = act {
                    action = Some((it.id, a));
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(200.0);
                    if !self.selected.contains(&it.id) {
                        clicked = Some((it.id, egui::Modifiers::NONE));
                    }
                    let complete = it.state == ItemState::Complete;
                    let mut entry = |ui: &mut egui::Ui, enabled: bool, ic: &str, label: &str, a: &'static str| {
                        if ui.add_enabled(enabled, egui::Button::new(theme::mi(ic, label))).clicked() {
                            action = Some((it.id, a));
                            ui.close();
                        }
                    };
                    entry(ui, complete, icon::ARROW_SQUARE_OUT, "Open", "open");
                    entry(ui, complete, icon::FOLDER_OPEN, "Show in folder", "folder");
                    ui.separator();
                    entry(ui, !complete && it.state != ItemState::Running, icon::PLAY, "Resume", "resume");
                    entry(ui, it.state == ItemState::Running, icon::PAUSE, "Pause", "stop");
                    entry(ui, true, icon::GAUGE, "Details", "progress");
                    entry(ui, true, icon::ARROW_CLOCKWISE, "Download again", "redownload");
                    ui.menu_button(theme::mi(icon::QUEUE, "Move to queue"), |ui| {
                        for q in self.manager.settings().queues {
                            if ui.button(&q.name).clicked() {
                                self.manager.set_queue(it.id, q.id);
                                ui.close();
                            }
                        }
                    });
                    if ui.button(theme::mi(icon::LINK, "Copy link")).clicked() {
                        ui.ctx().copy_text(it.request.url.clone());
                        ui.close();
                    }
                    ui.separator();
                    entry(ui, true, icon::TRASH, "Delete", "delete");
                });
            }
        });

        if let Some((id, m)) = clicked {
            if m.command || m.ctrl {
                if !self.selected.remove(&id) {
                    self.selected.insert(id);
                }
            } else if m.shift {
                if let (Some(a), Some(b)) = (self.anchor.and_then(|a| ids.iter().position(|x| *x == a)), ids.iter().position(|x| *x == id)) {
                    let (lo, hi) = (a.min(b), a.max(b));
                    self.selected = ids[lo..=hi].iter().copied().collect();
                }
            } else {
                self.selected = [id].into();
            }
            if !m.shift {
                self.anchor = Some(id);
            }
        }
        if let Some(id) = double {
            match self.manager.item(id) {
                Some(it) if it.state == ItemState::Complete => {
                    if let Some(p) = it.final_path {
                        util::open_file(&p);
                    }
                }
                Some(_) => {
                    self.progress_windows.insert(id);
                }
                None => {}
            }
        }
        if let Some((id, a)) = action {
            let it = self.manager.item(id);
            match a {
                "open" => {
                    if let Some(p) = it.and_then(|i| i.final_path) {
                        util::open_file(&p);
                    }
                }
                "folder" => {
                    if let Some(p) = it.and_then(|i| i.final_path) {
                        util::open_folder(&p);
                    }
                }
                "resume" => self.manager.start(id),
                "stop" => self.manager.stop(id),
                "progress" => {
                    self.progress_windows.insert(id);
                }
                "redownload" => self.manager.redownload(id, None),
                "delete" => self.confirm_delete = Some(if self.selected.contains(&id) { self.selected.iter().copied().collect() } else { vec![id] }),
                _ => {}
            }
        }
        let (del, all) = ui.input(|i| (i.key_pressed(egui::Key::Delete), i.modifiers.command && i.key_pressed(egui::Key::A)));
        if del && !self.selected.is_empty() && self.confirm_delete.is_none() {
            self.confirm_delete = Some(self.selected.iter().copied().collect());
        }
        if all {
            self.selected = ids.into_iter().collect();
        }
    }

    /// One download row. Returns the row response and an inline action, if clicked.
    fn row(&self, ui: &mut egui::Ui, it: &Item, h: f32) -> (egui::Response, Option<&'static str>) {
        let p = theme::palette(ui);
        // Leave a gutter so the cards never touch the scrollbar.
        let width = (ui.available_width() - 14.0).max(240.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
        ui.add_space(6.0);
        let selected = self.selected.contains(&it.id);
        let hovered = resp.hovered();
        let bg = if selected { p.accent_soft } else if hovered { p.surface_hi } else { p.surface };
        ui.painter().rect(rect, CornerRadius::same(12), bg, Stroke::new(1.0, if selected { p.accent.gamma_multiply(0.6) } else { p.border }), egui::StrokeKind::Inside);

        let tile = Rect::from_min_size(pos2(rect.left() + 14.0, rect.center().y - 20.0), vec2(40.0, 40.0));
        theme::file_tile(ui, tile, &it.file_name);

        let (status_text, color) = match &it.state {
            ItemState::Running if it.joining.is_some() => ("Joining", p.accent),
            ItemState::Running => ("Downloading", p.accent),
            ItemState::Complete => ("Completed", p.success),
            ItemState::Failed(_) => ("Error", p.danger),
            ItemState::Queued => ("Queued", p.warning),
            ItemState::Stopped if it.downloaded > 0 => ("Paused", p.text_dim),
            ItemState::Stopped => ("Stopped", p.text_dim),
        };

        // Right side: actions, then speed/ETA, then pill.
        let mut act = None;
        // Narrow window: keep room for the name; speed/ETA move out of the row.
        let narrow = width < 720.0;
        let right_w = if narrow { 190.0 } else { 340.0 };
        let right = Rect::from_min_max(pos2(rect.right() - right_w, rect.top()), pos2(rect.right() - 10.0, rect.bottom()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::right_to_left(egui::Align::Center)));
        {
            let ui = &mut child;
            match it.state {
                ItemState::Complete => {
                    if theme::icon_button(ui, icon::FOLDER_OPEN, "Show in folder").clicked() {
                        act = Some("folder");
                    }
                    if theme::icon_button(ui, icon::ARROW_SQUARE_OUT, "Open").clicked() {
                        act = Some("open");
                    }
                }
                ItemState::Running => {
                    if theme::icon_button(ui, icon::GAUGE, "Details").clicked() {
                        act = Some("progress");
                    }
                    if theme::icon_button(ui, icon::PAUSE, "Pause").clicked() {
                        act = Some("stop");
                    }
                }
                _ => {
                    if theme::icon_button(ui, icon::TRASH, "Delete").clicked() {
                        act = Some("delete");
                    }
                    if theme::icon_button(ui, icon::PLAY, "Resume").clicked() {
                        act = Some("resume");
                    }
                }
            }
            ui.add_space(8.0);
            theme::pill(ui, status_text, color);
            ui.add_space(10.0);
            if it.state == ItemState::Running && it.joining.is_none() && !narrow {
                ui.vertical(|ui| {
                    ui.add_space(14.0);
                    ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                        ui.label(RichText::new(util::speed(it.speed)).font(theme::semibold(13.0)));
                        ui.label(RichText::new(util::duration(it.eta())).size(11.5).color(p.text_dim));
                    });
                });
            }
        }

        // Middle: name, details, progress.
        let left = tile.right() + 14.0;
        let text_right = right.left() - 10.0;
        let name_galley = ui.painter().layout(
            it.file_name.clone(),
            theme::semibold(14.5),
            p.text,
            (text_right - left).max(50.0),
        );
        let name_pos = pos2(left, rect.top() + 12.0);
        ui.painter().with_clip_rect(Rect::from_min_max(pos2(left, rect.top()), pos2(text_right, rect.bottom()))).galley(name_pos, name_galley, p.text);
        let sub = match &it.state {
            ItemState::Running if it.joining.is_some() => {
                format!("Putting the file together  ·  {:.0}%", it.joining.unwrap_or(0.0) * 100.0)
            }
            ItemState::Running | ItemState::Stopped | ItemState::Queued => match it.total {
                Some(t) => format!("{} of {}  ·  {:.0}%", util::size(it.downloaded), util::size(t), it.progress_fraction().unwrap_or(0.0) * 100.0),
                None if it.downloaded > 0 => util::size(it.downloaded),
                None => host(&it.request.url),
            },
            ItemState::Complete => format!("{}  ·  {}", it.total.map(util::size).unwrap_or_default(), util::date(it.finished)),
            ItemState::Failed(e) => util::friendly_error(e),
        };
        let sub_color = if matches!(it.state, ItemState::Failed(_)) { p.danger } else { p.text_dim };
        ui.painter()
            .with_clip_rect(Rect::from_min_max(pos2(left, rect.top()), pos2(text_right, rect.bottom())))
            .text(pos2(left, rect.top() + 33.0), Align2::LEFT_TOP, sub, FontId::proportional(12.5), sub_color);
        if matches!(it.state, ItemState::Running | ItemState::Stopped | ItemState::Queued) && it.total.is_some() {
            let bar = Rect::from_min_size(pos2(left, rect.bottom() - 13.0), vec2((text_right - left).max(40.0), 5.0));
            let c = if it.state == ItemState::Running { p.accent } else { p.text_dim.gamma_multiply(0.7) };
            let f = it.joining.or(it.progress_fraction()).unwrap_or(0.0);
            theme::progress(ui, bar, f, c);
        }
        let resp = resp.on_hover_text(&it.request.url);
        (resp, act)
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(theme::ic(icon::LIGHTNING).size(12.0).color(p.text_dim));
            ui.label(RichText::new(format!("RDM {}", env!("CARGO_PKG_VERSION"))).size(11.5).color(p.text_dim));
            if let Some((m, t)) = &self.status_msg {
                if t.elapsed() < Duration::from_secs(8) {
                    ui.separator();
                    ui.label(RichText::new(m).size(11.5).color(p.warning));
                }
            }
        });
    }

    // ------------------------------------------------------------- windows

    fn progress_window(&mut self, ctx: &egui::Context, id: ItemId) -> bool {
        let Some(it) = self.manager.item(id) else { return false };
        let mut open = true;
        egui::Window::new(RichText::new(&it.file_name).font(theme::semibold(15.0)))
            .id(egui::Id::new(("progress", id)))
            .open(&mut open)
            .collapsible(false)
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .default_width(540.0)
            .resizable(true)
            .show(ctx, |ui| {
                let p = theme::palette(ui);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                    theme::file_tile(ui, r, &it.file_name);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(match it.total {
                            Some(t) => format!("{} of {}", util::size(it.downloaded), util::size(t)),
                            None => util::size(it.downloaded),
                        }).font(theme::semibold(18.0)));
                        ui.label(RichText::new(&it.request.url).size(11.5).color(p.text_dim));
                    });
                });
                ui.add_space(10.0);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
                theme::progress(ui, bar, it.progress_fraction().unwrap_or(0.0), if it.state == ItemState::Complete { p.success } else { p.accent });
                ui.add_space(12.0);
                ui.columns(4, |c| {
                    stat(&mut c[0], "Speed", &util::speed(it.speed));
                    stat(&mut c[1], "Time left", &util::duration(it.eta()));
                    stat(&mut c[2], "Connections", &it.connections.to_string());
                    stat(&mut c[3], "Resume", if it.resumable { "Supported" } else { "No" });
                });
                ui.add_space(12.0);
                ui.label(RichText::new("Connections").font(theme::semibold(13.0)));
                segment_bar(ui, &it);
                ui.add_space(10.0);
                let mut limit_on = it.request.speed_limit.is_some();
                let mut kb = it.request.speed_limit.map_or(500, |l| l / 1024);
                ui.horizontal(|ui| {
                    let c1 = ui.checkbox(&mut limit_on, "Limit this download to").changed();
                    let c2 = ui.add_enabled(limit_on, egui::DragValue::new(&mut kb).range(1..=1_000_000).suffix(" KB/s")).changed();
                    if c1 || c2 {
                        self.manager.set_item_speed_limit(id, limit_on.then_some(kb * 1024));
                    }
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    match it.state {
                        ItemState::Running => {
                            if theme::button(ui, Kind::Primary, icon::PAUSE, "Pause", true).clicked() {
                                self.manager.stop(id);
                            }
                        }
                        ItemState::Complete => {
                            if theme::button(ui, Kind::Primary, icon::ARROW_SQUARE_OUT, "Open", true).clicked() {
                                if let Some(p) = &it.final_path {
                                    util::open_file(p);
                                }
                            }
                        }
                        _ => {
                            if theme::button(ui, Kind::Primary, icon::PLAY, "Resume", true).clicked() {
                                self.manager.start(id);
                            }
                        }
                    }
                    if it.state != ItemState::Complete && theme::button(ui, Kind::Danger, icon::X, "Cancel", true).clicked() {
                        self.confirm_delete = Some(vec![id]);
                    }
                });
            });
        open
    }

    fn popups(&mut self, ctx: &egui::Context) {
        if let Some(&id) = self.completed.front() {
            let mut close = false;
            if let Some(it) = self.manager.item(id) {
                egui::Window::new("Download complete").id(egui::Id::new(("done", id))).collapsible(false).resizable(false).anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, -20.0)).show(ctx, |ui| {
                    let p = theme::palette(ui);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                        theme::file_tile(ui, r, &it.file_name);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&it.file_name).font(theme::semibold(14.0)));
                            ui.label(RichText::new(format!("{}  ·  saved", util::size(it.downloaded))).size(12.0).color(p.text_dim));
                        });
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if theme::button(ui, Kind::Primary, icon::ARROW_SQUARE_OUT, "Open", true).clicked() {
                            if let Some(p) = &it.final_path {
                                util::open_file(p);
                            }
                            close = true;
                        }
                        if theme::button(ui, Kind::Ghost, icon::FOLDER_OPEN, "Folder", true).clicked() {
                            if let Some(p) = &it.final_path {
                                util::open_folder(p);
                            }
                            close = true;
                        }
                        if theme::button(ui, Kind::Ghost, icon::X, "", true).clicked() {
                            close = true;
                        }
                    });
                    let mut s = self.manager.settings();
                    let mut dont = !s.show_complete_dialog;
                    if ui.checkbox(&mut dont, "Don't show again").changed() {
                        s.show_complete_dialog = !dont;
                        self.manager.set_settings(s);
                    }
                });
            } else {
                close = true;
            }
            if close {
                self.completed.pop_front();
            }
        }

        if let Some(ids) = self.confirm_delete.clone() {
            let mut done = false;
            egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(format!("Delete {} download{}?", ids.len(), if ids.len() == 1 { "" } else { "s" })).font(theme::semibold(16.0)));
                ui.label(RichText::new("You can keep the downloaded files or remove them from disk too.").color(theme::palette(ui).text_dim));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if theme::button(ui, Kind::Ghost, icon::LIST_CHECKS, "Remove from list", true).clicked() {
                        for id in &ids {
                            self.manager.remove(*id, false);
                            self.selected.remove(id);
                        }
                        done = true;
                    }
                    if theme::button(ui, Kind::Danger, icon::TRASH, "Delete files too", true).clicked() {
                        for id in &ids {
                            self.manager.remove(*id, true);
                            self.selected.remove(id);
                        }
                        done = true;
                    }
                    if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                        done = true;
                    }
                });
            });
            if done {
                self.confirm_delete = None;
            }
        }

        if let Some(m) = self.media.front() {
            let mut take: Option<bool> = None;
            egui::Window::new("Media found").collapsible(false).anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, -20.0)).show(ctx, |ui| {
                ui.label(RichText::new(m.title.clone().unwrap_or_else(|| "Media on this page".into())).font(theme::semibold(14.0)));
                ui.label(RichText::new(format!("{} · {}", m.kind, host(&m.request.url))).size(12.0).color(theme::palette(ui).text_dim));
                ui.horizontal(|ui| {
                    if theme::button(ui, Kind::Primary, icon::DOWNLOAD_SIMPLE, "Download", true).clicked() {
                        take = Some(true);
                    }
                    if theme::button(ui, Kind::Ghost, icon::X, "Dismiss", true).clicked() {
                        take = Some(false);
                    }
                });
            });
            if let Some(t) = take {
                let m = self.media.pop_front().unwrap();
                if t {
                    let mut r = m.request;
                    if r.file_name.is_none() {
                        r.file_name = m.title.map(|t| rdm_core::naming::sanitize(&t));
                    }
                    self.open_file_info(r);
                }
            }
        }

        if let Some((action, since)) = self.queue_action {
            let left = 30u64.saturating_sub(since.elapsed().as_secs());
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("queue_action")).show(ctx, |ui| {
                ui.label(RichText::new("Queue finished").font(theme::semibold(16.0)));
                ui.label(format!("The queue has finished. {} in {left} seconds.", action.label()));
                if theme::button(ui, Kind::Ghost, icon::X, "Cancel", true).clicked() {
                    cancel = true;
                }
            });
            if cancel {
                self.queue_action = None;
            } else if left == 0 {
                self.queue_action = None;
                if action == OnQueueDone::ExitApp {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    util::system_action(action);
                }
            }
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

fn host(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| url.to_string())
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str) {
    let p = theme::palette(ui);
    ui.label(RichText::new(label).size(11.5).color(p.text_dim));
    ui.label(RichText::new(if value.is_empty() { "—" } else { value }).font(theme::semibold(14.0)));
}

/// Chunk map: each connection's start (marker) and downloaded part.
fn segment_bar(ui: &mut egui::Ui, it: &Item) {
    let p = theme::palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(4), p.track);
    let Some(total) = it.total.filter(|&t| t > 0) else { return };
    let x = |v: u64| rect.left() + rect.width() * (v.min(total) as f32 / total as f32);
    if it.state == ItemState::Complete || (it.chunks.is_empty() && it.downloaded >= total) {
        ui.painter().rect_filled(rect, CornerRadius::same(4), p.success);
        return;
    }
    for (i, c) in it.chunks.iter().enumerate() {
        let next_start = it.chunks.get(i + 1).map_or(total, |n| n.start);
        let done_to = c.pos.min(next_start);
        let r = Rect::from_min_max(pos2(x(c.start), rect.top()), pos2(x(done_to), rect.bottom()));
        ui.painter().rect_filled(r, CornerRadius::ZERO, if c.active { p.accent } else { p.accent.gamma_multiply(0.55) });
        let sx = x(c.start);
        ui.painter().line_segment([pos2(sx, rect.top()), pos2(sx, rect.bottom())], Stroke::new(1.5, p.warning));
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_external(ctx);
        let actions: Vec<TrayAction> = self.tray.as_ref().map(|t| t.rx.try_iter().collect()).unwrap_or_default();
        for a in actions {
            match a {
                TrayAction::Show | TrayAction::NewDownload => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    if a == TrayAction::NewDownload {
                        self.new_download();
                    }
                }
                TrayAction::PauseAll => self.manager.stop_all(),
                TrayAction::ResumeAll => self.manager.start_all(),
                TrayAction::Exit => {
                    self.exiting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        // Closing the window hides it to the tray instead of quitting.
        if ctx.input(|i| i.viewport().close_requested()) && !self.exiting && self.tray.is_some() && self.manager.settings().minimize_to_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Keep polling while hidden so tray and browser requests are handled.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.open_screenshot_window();
        let ctx = ui.ctx().clone();
        let p = theme::palette(ui);
        let all = self.manager.items();
        let visible = self.visible_items(&all);

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(236.0)
            .frame(egui::Frame::new().fill(p.sidebar).inner_margin(Margin::symmetric(10, 0)))
            .show(ui, |ui| self.sidebar(ui, &all));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(14, 6)))
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(22, 0)))
            .show(ui, |ui| self.header(ui, &visible));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin { left: 22, right: 22, top: 12, bottom: 4 }))
            .show(ui, |ui| {
                self.extension_banner(ui);
                self.list(ui, &visible)
            });

        let ids: Vec<ItemId> = self.progress_windows.iter().copied().collect();
        for id in ids {
            if !self.progress_window(&ctx, id) {
                self.progress_windows.remove(&id);
            }
        }
        dialogs::show_all(self, &ctx);
        crate::grabber_ui::show(self, &ctx);
        crate::extension_help::show(self, &ctx);
        self.popups(&ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

/// The app icon (vector), rendered at exactly the pixel size the display needs.
const LOGO_SVG: &[u8] = include_bytes!("../assets/rdm_icon.svg");

/// Pixel size for the sidebar icon at the current display scale.
fn logo_pixels(want: f32) -> u32 {
    want.round().clamp(16.0, 1024.0) as u32
}

fn load_logo(ctx: &egui::Context, px: u32) -> Option<egui::TextureHandle> {
    render_svg(ctx, LOGO_SVG, px, px, &format!("rdm-logo-{px}"))
}

/// The logo artwork (icon + lettering); the light copy has darker lettering for light theme.
const LOGO_WIDE_DARK: &[u8] = include_bytes!("../assets/rdm_logo.svg");
const LOGO_WIDE_LIGHT: &[u8] = include_bytes!("../assets/rdm_logo_light.svg");
/// Width / height of the logo artwork.
const LOGO_ASPECT: f32 = 1320.0 / 500.0;

/// Rasterize an SVG to exactly w x h pixels (fit, centred) as a texture.
fn render_svg(ctx: &egui::Context, svg: &[u8], w: u32, h: u32, name: &str) -> Option<egui::TextureHandle> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(w.max(1), h.max(1))?;
    let size = tree.size();
    let scale = (w as f32 / size.width()).min(h as f32 / size.height());
    let (dx, dy) = ((w as f32 - size.width() * scale) / 2.0, (h as f32 - size.height() * scale) / 2.0);
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy), &mut pixmap.as_mut());
    // tiny-skia pixmaps are premultiplied RGBA, like egui textures.
    let img = egui::ColorImage::from_rgba_premultiplied([w.max(1) as usize, h.max(1) as usize], pixmap.data());
    Some(ctx.load_texture(name, img, egui::TextureOptions::LINEAR))
}
