//! "Install the browser extension" helper: per-browser steps with buttons that do the
//! fiddly parts (register the bridge, open the browser's extensions page, show the
//! extension folder ready to drag onto that page) and a live connection status.

use crate::app::App;
use crate::theme::{self, Kind, icon};
use crate::window::{self, Spec};
use eframe::egui::{self, RichText};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Chromium,
    Firefox,
}

#[derive(Clone)]
struct Browser {
    name: &'static str,
    exe: Option<PathBuf>,
    /// Page that lists extensions (opened by the browser itself from the command line).
    page: &'static str,
    family: Family,
    /// Where the "Developer mode" switch is on that page.
    dev_mode: &'static str,
}

pub struct ExtensionHelp {
    browsers: Vec<Browser>,
    selected: usize,
    message: Option<String>,
}

fn find(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.exists()).cloned()
}

#[cfg(target_os = "windows")]
fn exe_candidates(rel: &[&str]) -> Vec<PathBuf> {
    let mut out = vec![];
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            for r in rel {
                out.push(PathBuf::from(&base).join(r));
            }
        }
    }
    out
}

fn detect() -> Vec<Browser> {
    let mut v = vec![];
    #[cfg(target_os = "windows")]
    {
        let b = |name, rel: &[&str], page, family, dev_mode| Browser { name, exe: find(&exe_candidates(rel)), page, family, dev_mode };
        v.push(b("Chrome", &[r"Google\Chrome\Application\chrome.exe"], "chrome://extensions/", Family::Chromium, "top-right corner"));
        v.push(b("Edge", &[r"Microsoft\Edge\Application\msedge.exe"], "edge://extensions/", Family::Chromium, "left side panel"));
        v.push(b("Brave", &[r"BraveSoftware\Brave-Browser\Application\brave.exe"], "brave://extensions/", Family::Chromium, "top-right corner"));
        v.push(b("Vivaldi", &[r"Vivaldi\Application\vivaldi.exe"], "vivaldi://extensions/", Family::Chromium, "top-right corner"));
        v.push(b("Opera", &[r"Programs\Opera\opera.exe", r"Opera\opera.exe"], "opera://extensions/", Family::Chromium, "top-right corner"));
        v.push(b("Firefox", &[r"Mozilla Firefox\firefox.exe"], "about:debugging#/runtime/this-firefox", Family::Firefox, ""));
    }
    #[cfg(target_os = "macos")]
    {
        let app = |p: &str| find(&[PathBuf::from(p)]);
        v.push(Browser { name: "Chrome", exe: app("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"), page: "chrome://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Edge", exe: app("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"), page: "edge://extensions/", family: Family::Chromium, dev_mode: "left side panel" });
        v.push(Browser { name: "Brave", exe: app("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"), page: "brave://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Vivaldi", exe: app("/Applications/Vivaldi.app/Contents/MacOS/Vivaldi"), page: "vivaldi://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Opera", exe: app("/Applications/Opera.app/Contents/MacOS/Opera"), page: "opera://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Firefox", exe: app("/Applications/Firefox.app/Contents/MacOS/firefox"), page: "about:debugging#/runtime/this-firefox", family: Family::Firefox, dev_mode: "" });
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let which = |names: &[&str]| {
            let path = std::env::var_os("PATH").unwrap_or_default();
            names.iter().find_map(|n| std::env::split_paths(&path).map(|d| d.join(n)).find(|p| p.exists()))
        };
        v.push(Browser { name: "Chrome", exe: which(&["google-chrome", "google-chrome-stable"]), page: "chrome://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Chromium", exe: which(&["chromium", "chromium-browser"]), page: "chrome://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Edge", exe: which(&["microsoft-edge", "microsoft-edge-stable"]), page: "edge://extensions/", family: Family::Chromium, dev_mode: "left side panel" });
        v.push(Browser { name: "Brave", exe: which(&["brave-browser", "brave"]), page: "brave://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Vivaldi", exe: which(&["vivaldi", "vivaldi-stable"]), page: "vivaldi://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Opera", exe: which(&["opera"]), page: "opera://extensions/", family: Family::Chromium, dev_mode: "top-right corner" });
        v.push(Browser { name: "Firefox", exe: which(&["firefox"]), page: "about:debugging#/runtime/this-firefox", family: Family::Firefox, dev_mode: "" });
    }
    // Installed browsers first.
    v.sort_by_key(|b| b.exe.is_none());
    v
}

/// The unpacked Chromium extension folder shipped next to the app (or the source folder
/// in a development build).
fn chromium_folder() -> Option<PathBuf> {
    if let Some(packaged) = crate::util::bundled("extension-chrome").filter(|p| p.join("manifest.json").exists()) {
        return Some(packaged);
    }
    let exe = std::env::current_exe().ok()?;
    exe.parent()?.ancestors().map(|a| a.join("extension")).find(|p| p.join("manifest.json").exists())
}

fn firefox_package() -> Option<PathBuf> {
    crate::util::bundled("rdm-firefox.xpi")
}

impl ExtensionHelp {
    pub fn new() -> Self {
        Self { browsers: detect(), selected: 0, message: None }
    }
}

/// Open the browser and copy its extensions-page address: browsers refuse to open their
/// internal pages when asked from outside, so the user pastes it into the address bar.
fn open_page(b: &Browser) -> Result<String, String> {
    let exe = b.exe.as_ref().ok_or_else(|| format!("{} isn't installed.", b.name))?;
    if let Some(mut c) = app_clipboard() {
        let _ = c.set_text(b.page.to_string());
    }
    std::process::Command::new(exe).arg(b.page).spawn().map_err(|e| e.to_string())?;
    Ok(format!("{} is opening. The address {} is copied — click the address bar, press Ctrl+V, then Enter.", b.name, b.page))
}

fn step(ui: &mut egui::Ui, n: usize, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette(ui);
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 13.0, p.accent_soft);
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, n.to_string(), theme::semibold(13.0), p.accent);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.add_space(3.0);
            ui.label(RichText::new(title).font(theme::semibold(14.0)));
            body(ui);
        });
    });
    ui.add_space(10.0);
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(h) = app.ext_help.as_mut() else { return };
    let connected_once = app.manager.settings().extension_connected_once;
    ctx.request_repaint_after(Duration::from_secs(1));
    let mut done = false;
    let spec = Spec { title: "Install the browser extension", size: [700.0, 700.0], min: [580.0, 520.0], resizable: true };
    let (_, closed) = window::show(ctx, "ext-help", spec, |ui| {
        window::footer(ui, |ui| {
            if theme::button(ui, Kind::Primary, icon::CHECK, "Done", true).clicked() {
                done = true;
            }
            if let Some(m) = &h.message {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let p = theme::palette(ui);
                    ui.label(theme::ic(icon::INFO).color(p.accent));
                    ui.add(egui::Label::new(RichText::new(m).size(12.5)).wrap());
                });
            }
        });
        window::body(ui, |ui| {
            let p = theme::palette(ui);
            // Live status.
            let recent = rdm_core::ipc::last_extension_contact().map(|t| chrono::Utc::now().timestamp() - t);
            let (ic, text, color) = match recent {
                Some(s) if s < 600 => (icon::CHECK_CIRCLE, "Extension connected — downloads and videos from your browser go to RDM.", p.success),
                _ if connected_once => (icon::INFO, "The extension is installed but hasn't checked in yet. Open your browser and it will connect.", p.text_dim),
                _ => (icon::WARNING_CIRCLE, "Not connected yet. Follow the steps for your browser.", p.warning),
            };
            egui::Frame::new().fill(color.gamma_multiply(0.12)).corner_radius(egui::CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(theme::ic(ic).size(18.0).color(color));
                    ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
                });
            });
            ui.add_space(14.0);

            // Browser choice.
            ui.label(RichText::new("YOUR BROWSER").font(theme::semibold(11.0)).color(p.text_dim));
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                for (i, b) in h.browsers.iter().enumerate() {
                    let label = if b.exe.is_some() { b.name.to_string() } else { format!("{} (not installed)", b.name) };
                    if ui.selectable_label(h.selected == i, RichText::new(label).font(theme::semibold(13.0))).clicked() {
                        h.selected = i;
                        h.message = None;
                    }
                }
            });
            ui.add_space(14.0);
            let Some(b) = h.browsers.get(h.selected).cloned() else { return };

            step(ui, 1, "Connect RDM to your browsers", |ui| {
                ui.label(RichText::new("Lets the extension talk to RDM. Safe to run again.").color(p.text_dim));
                if theme::button(ui, Kind::Ghost, icon::LINK, "Connect", true).clicked() {
                    h.message = Some(match crate::bridge::register() {
                        Ok(m) => m,
                        Err(e) => format!("Couldn't connect: {e}"),
                    });
                }
            });

            match b.family {
                Family::Chromium => {
                    let folder = chromium_folder();
                    step(ui, 2, &format!("Open {}'s extensions page", b.name), |ui| {
                        ui.label(RichText::new(format!("Opens {} and copies the address {} — paste it into the address bar (Ctrl+V) and press Enter.", b.name, b.page)).color(p.text_dim));
                        if theme::button(ui, Kind::Ghost, icon::ARROW_SQUARE_OUT, &format!("Open {}", b.name), b.exe.is_some()).clicked() {
                            h.message = Some(open_page(&b).unwrap_or_else(|e| e));
                        }
                    });
                    step(ui, 3, "Turn on “Developer mode”", |ui| {
                        ui.label(RichText::new(format!("It's a switch in the {} of the extensions page.", b.dev_mode)).color(p.text_dim));
                    });
                    step(ui, 4, "Drag the RDM extension folder onto the page", |ui| {
                        ui.label(
                            RichText::new("“Show folder” opens it with the extension-chrome folder selected — drag that folder onto the extensions page. (Or click “Load unpacked” there and pick it; “Copy path” lets you paste it.)")
                                .color(p.text_dim),
                        );
                        ui.horizontal(|ui| {
                            if theme::button(ui, Kind::Primary, icon::FOLDER_OPEN, "Show folder", folder.is_some()).clicked() {
                                if let Some(f) = &folder {
                                    crate::util::open_folder(f);
                                }
                            }
                            if theme::button(ui, Kind::Ghost, icon::COPY, "Copy path", folder.is_some()).clicked() {
                                if let (Some(f), Some(c)) = (&folder, app_clipboard()) {
                                    let mut c = c;
                                    let _ = c.set_text(f.display().to_string());
                                    h.message = Some("Folder path copied.".into());
                                }
                            }
                        });
                        if folder.is_none() {
                            ui.label(RichText::new("The extension folder wasn't found next to RDM.").color(p.danger));
                        }
                    });
                    step(ui, 5, "Done", |ui| {
                        ui.label(
                            RichText::new("The status above turns green within a few seconds. Tip: pin RDM from the puzzle-piece menu so its button stays in the toolbar.")
                                .color(p.text_dim),
                        );
                    });
                }
                Family::Firefox => {
                    let xpi = firefox_package();
                    step(ui, 2, "Open Firefox's add-on debugging page", |ui| {
                        ui.label(RichText::new(format!("Opens Firefox and copies the address {} — paste it into the address bar (Ctrl+V) and press Enter.", b.page)).color(p.text_dim));
                        if theme::button(ui, Kind::Ghost, icon::ARROW_SQUARE_OUT, "Open Firefox", b.exe.is_some()).clicked() {
                            h.message = Some(open_page(&b).unwrap_or_else(|e| e));
                        }
                    });
                    step(ui, 3, "Click “Load Temporary Add-on…” and pick rdm-firefox.xpi", |ui| {
                        if theme::button(ui, Kind::Primary, icon::FOLDER_OPEN, "Show file", xpi.is_some()).clicked() {
                            if let Some(f) = &xpi {
                                crate::util::open_folder(f);
                            }
                        }
                        ui.label(
                            RichText::new("Firefox removes temporary add-ons when it restarts. The permanent install comes with RDM's signed Firefox add-on.")
                                .color(p.text_dim),
                        );
                    });
                }
            }
        });
    });
    if done || closed {
        app.ext_help = None;
    }
}

fn app_clipboard() -> Option<arboard::Clipboard> {
    arboard::Clipboard::new().ok()
}
