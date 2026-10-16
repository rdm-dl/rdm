//! Visual design: palette, fonts, global style, and custom widgets.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Rect, Response, Sense, Shadow,
    Stroke, TextStyle, Ui, pos2, vec2,
};
pub use egui_phosphor::regular as icon;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub sidebar: Color32,
    pub surface: Color32,
    pub surface_hi: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub track: Color32,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x0f, 0x11, 0x16),
    sidebar: Color32::from_rgb(0x13, 0x16, 0x1d),
    surface: Color32::from_rgb(0x18, 0x1c, 0x24),
    surface_hi: Color32::from_rgb(0x22, 0x27, 0x32),
    border: Color32::from_rgb(0x2a, 0x30, 0x3c),
    text: Color32::from_rgb(0xe8, 0xeb, 0xf1),
    text_dim: Color32::from_rgb(0x8b, 0x93, 0xa5),
    accent: Color32::from_rgb(0x2a, 0x86, 0xff),
    accent_soft: Color32::from_rgba_premultiplied(0x10, 0x27, 0x4a, 0xff),
    success: Color32::from_rgb(0x34, 0xc7, 0x7b),
    warning: Color32::from_rgb(0xf5, 0xa5, 0x24),
    danger: Color32::from_rgb(0xf0, 0x5d, 0x5e),
    track: Color32::from_rgb(0x26, 0x2b, 0x36),
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf6, 0xf7, 0xfb),
    sidebar: Color32::from_rgb(0xee, 0xf0, 0xf6),
    surface: Color32::from_rgb(0xff, 0xff, 0xff),
    surface_hi: Color32::from_rgb(0xf1, 0xf3, 0xf8),
    border: Color32::from_rgb(0xdf, 0xe3, 0xec),
    text: Color32::from_rgb(0x16, 0x1a, 0x23),
    text_dim: Color32::from_rgb(0x6a, 0x72, 0x84),
    accent: Color32::from_rgb(0x16, 0x73, 0xf0),
    accent_soft: Color32::from_rgb(0xe2, 0xee, 0xff),
    success: Color32::from_rgb(0x1e, 0xa8, 0x63),
    warning: Color32::from_rgb(0xd9, 0x8c, 0x0a),
    danger: Color32::from_rgb(0xdc, 0x3f, 0x41),
    track: Color32::from_rgb(0xe4, 0xe7, 0xef),
};

pub fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

pub const SEMIBOLD: &str = "semibold";
/// Font family containing only the icon font (avoids text fonts' private-use glyphs).
pub const ICONS: &str = "icons";

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

/// An icon as rich text.
pub fn ic(i: &str) -> egui::RichText {
    egui::RichText::new(i).family(FontFamily::Name(ICONS.into())).size(15.0)
}

/// Menu entry content: icon + label.
pub fn mi(i: &str, label: &str) -> (egui::RichText, egui::RichText) {
    (ic(i), egui::RichText::new(label))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"))));
    fonts.font_data.insert("inter-sb".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf"))));
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "inter".into());
    let mut sb = vec!["inter-sb".to_string()];
    sb.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), sb);
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    fonts.families.insert(FontFamily::Name(ICONS.into()), vec!["phosphor".into()]);
    ctx.set_fonts(fonts);
}

fn style_for(p: Palette, dark: bool) -> egui::Style {
    let mut s = egui::Style::default();
    s.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Name(SEMIBOLD.into()))),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    s.spacing.item_spacing = vec2(8.0, 7.0);
    s.spacing.button_padding = vec2(12.0, 6.0);
    s.spacing.interact_size = vec2(32.0, 30.0);
    s.spacing.window_margin = Margin::same(16);
    s.spacing.menu_margin = Margin::same(8);
    s.spacing.indent = 16.0;

    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let r = CornerRadius::same(8);
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = if dark { Color32::from_rgb(0x12, 0x15, 0x1b) } else { Color32::WHITE };
    v.faint_bg_color = p.surface_hi;
    v.text_edit_bg_color = Some(v.extreme_bg_color);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(if dark { 140 } else { 40 }) };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(if dark { 120 } else { 30 }) };
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.override_text_color = Some(p.text);
    v.striped = false;
    v.indent_has_left_vline = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.surface_hi;
    w.inactive.weak_bg_fill = p.surface_hi;
    // A hairline outline keeps text fields, drop-downs and number boxes visible on cards.
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.border;
    w.hovered.weak_bg_fill = p.border;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.accent.gamma_multiply(0.5);
    w.active.weak_bg_fill = p.accent.gamma_multiply(0.5);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.hovered;
    s.visuals = v;
    s
}

pub fn setup(ctx: &egui::Context, dark: bool) {
    install_fonts(ctx);
    ctx.set_style_of(egui::Theme::Dark, style_for(DARK, true));
    ctx.set_style_of(egui::Theme::Light, style_for(LIGHT, false));
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
}

// ------------------------------------------------------------------ widgets

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Ghost,
    Danger,
}

/// Rounded button with an icon and optional label.
pub fn button(ui: &mut Ui, kind: Kind, icon_str: &str, label: &str, enabled: bool) -> Response {
    let p = palette(ui);
    let font = FontId::proportional(14.0);
    let icon_g = ui.painter().layout_no_wrap(icon_str.to_string(), icon_font(17.0), p.text);
    let label_g = (!label.is_empty()).then(|| ui.painter().layout_no_wrap(label.to_string(), font.clone(), p.text));
    let pad = vec2(if label.is_empty() { 9.0 } else { 14.0 }, 0.0);
    let gap = if label_g.is_some() { 8.0 } else { 0.0 };
    let w = pad.x * 2.0 + icon_g.size().x + gap + label_g.as_ref().map_or(0.0, |g| g.size().x);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let pressed = enabled && resp.is_pointer_button_down_on();
        let (fill, fg) = match kind {
            Kind::Primary => {
                let base = p.accent;
                (if pressed { base.gamma_multiply(0.85) } else if hovered { lighten(base, 0.12) } else { base }, Color32::WHITE)
            }
            Kind::Ghost => (if pressed { p.border } else if hovered { p.surface_hi } else { Color32::TRANSPARENT }, p.text),
            Kind::Danger => (if hovered { p.danger.gamma_multiply(0.18) } else { Color32::TRANSPARENT }, p.danger),
        };
        let fg = if enabled { fg } else { p.text_dim.gamma_multiply(0.55) };
        ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
        let mut x = rect.left() + pad.x;
        ui.painter().text(pos2(x, rect.center().y), Align2::LEFT_CENTER, icon_str, icon_font(17.0), fg);
        x += icon_g.size().x + gap;
        if !label.is_empty() {
            let f = if kind == Kind::Primary { semibold(14.0) } else { font };
            ui.painter().text(pos2(x, rect.center().y), Align2::LEFT_CENTER, label, f, fg);
        }
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Full-width primary call-to-action (sidebar "New download"): centred icon + label, a
/// soft accent glow and a subtle top highlight, matching the logo tile.
pub fn cta_button(ui: &mut Ui, icon_str: &str, label: &str) -> Response {
    let p = palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let pressed = resp.is_pointer_button_down_on();
        let radius = CornerRadius::same(11);
        let fill = if pressed { p.accent.gamma_multiply(0.85) } else if hovered { lighten(p.accent, 0.10) } else { p.accent };
        let glow = if hovered { 0.5 } else { 0.32 };
        ui.painter().add(egui::epaint::Shadow { offset: [0, 4], blur: 14, spread: 0, color: p.accent.gamma_multiply(glow) }.as_shape(rect, radius));
        ui.painter().rect_filled(rect, radius, fill);
        let top = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.center().y));
        ui.painter().rect_filled(top, CornerRadius { nw: 11, ne: 11, sw: 0, se: 0 }, Color32::from_white_alpha(22));
        let icon_g = ui.painter().layout_no_wrap(icon_str.to_string(), icon_font(17.0), Color32::WHITE);
        let label_g = ui.painter().layout_no_wrap(label.to_string(), semibold(14.0), Color32::WHITE);
        let w = icon_g.size().x + 8.0 + label_g.size().x;
        let x = rect.center().x - w / 2.0;
        ui.painter().galley(pos2(x, rect.center().y - icon_g.size().y / 2.0), icon_g, Color32::WHITE);
        ui.painter().galley(pos2(x + w - label_g.size().x, rect.center().y - label_g.size().y / 2.0), label_g, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small square icon-only button for row actions.
pub fn icon_button(ui: &mut Ui, icon_str: &str, tooltip: &str) -> Response {
    let p = palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.surface_hi);
    }
    let c = if resp.hovered() { p.text } else { p.text_dim };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon_str, icon_font(16.0), c);
    resp.on_hover_text(tooltip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Sidebar navigation row with icon, label, and count badge.
pub fn nav_item(ui: &mut Ui, icon_str: &str, label: &str, count: Option<usize>, selected: bool) -> Response {
    let p = palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if selected { p.accent_soft } else if resp.hovered() { p.surface_hi } else { Color32::TRANSPARENT };
        ui.painter().rect_filled(rect, CornerRadius::same(9), bg);
        if selected {
            let bar = Rect::from_min_size(pos2(rect.left(), rect.top() + 8.0), vec2(3.0, rect.height() - 16.0));
            ui.painter().rect_filled(bar, CornerRadius::same(2), p.accent);
        }
        let fg = if selected { p.text } else { p.text_dim };
        ui.painter().text(pos2(rect.left() + 14.0, rect.center().y), Align2::LEFT_CENTER, icon_str, icon_font(17.0), if selected { p.accent } else { fg });
        let font = if selected { semibold(14.0) } else { FontId::proportional(14.0) };
        ui.painter().text(pos2(rect.left() + 42.0, rect.center().y), Align2::LEFT_CENTER, label, font, fg);
        if let Some(n) = count.filter(|n| *n > 0) {
            let t = n.to_string();
            let g = ui.painter().layout_no_wrap(t.clone(), FontId::proportional(11.5), p.text_dim);
            let bw = g.size().x + 14.0;
            let br = Rect::from_center_size(pos2(rect.right() - 10.0 - bw / 2.0, rect.center().y), vec2(bw, 20.0));
            ui.painter().rect_filled(br, CornerRadius::same(10), if selected { p.accent.gamma_multiply(0.35) } else { p.surface_hi });
            ui.painter().text(br.center(), Align2::CENTER_CENTER, t, FontId::proportional(11.5), if selected { p.text } else { p.text_dim });
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Section caption in the sidebar.
pub fn caption(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(egui::RichText::new(text.to_uppercase()).size(11.0).color(p.text_dim).font(semibold(11.0)));
    });
    ui.add_space(2.0);
}

/// Colored status pill.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), semibold(11.5), color);
    let size = vec2(g.size().x + 18.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(11), color.gamma_multiply(0.16));
    ui.painter().circle_filled(pos2(rect.left() + 9.0, rect.center().y), 3.0, color);
    ui.painter().text(pos2(rect.left() + 15.0, rect.center().y), Align2::LEFT_CENTER, text, semibold(11.5), color);
    resp
}

/// Slim rounded progress bar.
pub fn progress(ui: &mut Ui, rect: Rect, fraction: f32, color: Color32) {
    let p = palette(ui);
    let r = CornerRadius::same((rect.height() / 2.0) as u8);
    ui.painter().rect_filled(rect, r, p.track);
    if fraction > 0.0 {
        let w = (rect.width() * fraction.clamp(0.0, 1.0)).max(rect.height());
        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(w, rect.height())), r, color);
    }
}

/// Icon and color for a file name, by extension.
pub fn file_icon(name: &str) -> (&'static str, Color32) {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "gz" | "tar" | "bz2" | "xz" | "tgz" | "zst" => (icon::FILE_ZIP, Color32::from_rgb(0xf5, 0xa5, 0x24)),
        "iso" | "img" | "dmg" => (icon::PACKAGE, Color32::from_rgb(0xf5, 0xa5, 0x24)),
        "pdf" => (icon::FILE_PDF, Color32::from_rgb(0xf0, 0x5d, 0x5e)),
        "doc" | "docx" | "txt" | "rtf" | "odt" | "md" | "csv" | "xls" | "xlsx" | "ppt" | "pptx" | "epub" => {
            (icon::FILE_TEXT, Color32::from_rgb(0x4f, 0xa3, 0xff))
        }
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "wma" => (icon::MUSIC_NOTES, Color32::from_rgb(0xc0, 0x6c, 0xff)),
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" | "ts" | "mpg" | "mpeg" => {
            (icon::FILM_STRIP, Color32::from_rgb(0xff, 0x6b, 0x9a))
        }
        "exe" | "msi" | "apk" | "deb" | "rpm" | "pkg" | "appimage" | "msix" => (icon::APP_WINDOW, Color32::from_rgb(0x34, 0xc7, 0x7b)),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" => (icon::FILE_IMAGE, Color32::from_rgb(0x2d, 0xc6, 0xc6)),
        _ => (icon::FILE, Color32::from_rgb(0x8b, 0x93, 0xa5)),
    }
}

/// File-type tile: icon on a tinted rounded square.
pub fn file_tile(ui: &mut Ui, rect: Rect, name: &str) {
    let (ic, color) = file_icon(name);
    ui.painter().rect_filled(rect, CornerRadius::same(10), color.gamma_multiply(0.15));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, ic, icon_font(20.0), color);
}

/// A rounded card frame.
pub fn card(ui: &Ui) -> egui::Frame {
    let p = palette(ui);
    egui::Frame::new().fill(p.surface).corner_radius(CornerRadius::same(12)).stroke(Stroke::new(1.0, p.border)).inner_margin(Margin::same(14))
}

// ------------------------------------------------------------------ text fitting

/// Shorten `text` to fit `max_w` by cutting out the middle ("long-name…tail.zip"), so the
/// start and the file extension stay visible.
pub fn elide_middle(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let ext = text.rsplit_once('.').map_or(0, |(_, e)| if e.chars().count() <= 6 { e.chars().count() + 1 } else { 0 });
    let fit = |k: usize| -> String {
        let tail = (k * 2 / 5).max(ext.min(k)).min(k);
        let head = k - tail;
        let mut s: String = chars[..head].iter().collect();
        s.push('…');
        s.extend(&chars[chars.len() - tail..]);
        s
    };
    // Largest number of kept characters that still fits.
    let (mut lo, mut hi) = (0usize, chars.len().saturating_sub(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&fit(mid)) <= max_w { lo = mid } else { hi = mid - 1 }
    }
    fit(lo)
}

/// Shorten `text` to fit `max_w` by cutting the end ("Some long descript…").
pub fn elide_end(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let fit = |k: usize| chars[..k].iter().collect::<String>().trim_end().to_string() + "…";
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&fit(mid)) <= max_w { lo = mid } else { hi = mid - 1 }
    }
    fit(lo)
}

// ------------------------------------------------------------------ form controls

/// Checkbox with a clear rounded box (filled accent with a tick when on).
pub fn checkbox(ui: &mut Ui, on: &mut bool, label: impl Into<String>) -> Response {
    let p = palette(ui);
    let label = label.into();
    let enabled = ui.is_enabled();
    let galley = ui.painter().layout(label, FontId::proportional(14.0), p.text, (ui.available_width() - 30.0).max(40.0));
    let size = vec2(26.0 + galley.size().x, galley.size().y.max(20.0) + 4.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        // Centred on the first line of the label.
        let line_h = galley.size().y / galley.rows.len().max(1) as f32;
        let b = Rect::from_center_size(pos2(rect.left() + 9.0, rect.top() + 2.0 + line_h / 2.0), vec2(18.0, 18.0));
        let hovered = resp.hovered() && enabled;
        if *on {
            ui.painter().rect_filled(b, CornerRadius::same(5), if hovered { lighten(p.accent, 0.1) } else { p.accent });
            ui.painter().text(b.center(), Align2::CENTER_CENTER, icon::CHECK, icon_font(12.5), Color32::WHITE);
        } else {
            let fill = ui.visuals().extreme_bg_color;
            ui.painter().rect(b, CornerRadius::same(5), fill, Stroke::new(1.5, if hovered { p.accent } else { p.text_dim.gamma_multiply(0.7) }), egui::StrokeKind::Inside);
        }
        let color = if enabled { p.text } else { p.text_dim.gamma_multiply(0.7) };
        ui.painter().galley(pos2(rect.left() + 26.0, rect.top() + 2.0), galley, color);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// iOS-style on/off switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let p = palette(ui);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let track = if ui.is_enabled() { lerp_color(p.track, p.accent, t) } else { p.track };
        ui.painter().rect_filled(rect, CornerRadius::same(11), track);
        let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
        ui.painter().circle_filled(pos2(x, rect.center().y), 8.0, if ui.is_enabled() { Color32::WHITE } else { p.text_dim });
    }
    if ui.is_enabled() { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A settings line: title (and optional hint) on the left, the control on the right.
pub fn setting(ui: &mut Ui, title: &str, hint: &str, control: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    ui.horizontal(|ui| {
        let right_w = 220.0f32.min(ui.available_width() * 0.45);
        let left_w = ui.available_width() - right_w - 12.0;
        ui.vertical(|ui| {
            ui.set_width(left_w);
            ui.add(egui::Label::new(egui::RichText::new(title).color(p.text)).wrap());
            if !hint.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(hint).size(12.0).color(p.text_dim)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// A settings line with an on/off switch. Returns true when it was flipped.
pub fn switch_setting(ui: &mut Ui, title: &str, hint: &str, on: &mut bool) -> bool {
    let mut changed = false;
    setting(ui, title, hint, |ui| changed = toggle(ui, on).changed());
    changed
}

/// Heading of a settings page.
pub fn page_title(ui: &mut Ui, title: &str, subtitle: &str) {
    let p = palette(ui);
    ui.label(egui::RichText::new(title).font(semibold(19.0)));
    if !subtitle.is_empty() {
        ui.add(egui::Label::new(egui::RichText::new(subtitle).color(p.text_dim)).wrap());
    }
    ui.add_space(12.0);
}

/// A titled card grouping related settings; rows inside are separated by thin lines.
pub fn group<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette(ui);
    if !title.is_empty() {
        ui.label(egui::RichText::new(title.to_uppercase()).font(semibold(11.0)).color(p.text_dim));
        ui.add_space(2.0);
    }
    let r = card(ui).inner_margin(Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 10.0;
        add(ui)
    });
    ui.add_space(16.0);
    r.inner
}

/// Thin line between rows of a group.
pub fn divider(ui: &mut Ui) {
    let p = palette(ui);
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, p.border.gamma_multiply(0.7)));
}

/// Hint text under a control.
pub fn hint(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(p.text_dim)).wrap());
}

/// Vertical navigation entry for a dialog (Options sections, scheduler queues).
pub fn side_tab(ui: &mut Ui, icon_str: &str, label: &str, selected: bool) -> Response {
    nav_item(ui, icon_str, label, None, selected)
}

/// Icon-only sidebar entry (narrow window): the count shows as a small badge and the name
/// on hover.
pub fn nav_icon(ui: &mut Ui, icon_str: &str, label: &str, count: Option<usize>, selected: bool) -> Response {
    let p = palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let cell = Rect::from_center_size(rect.center(), vec2(40.0, 36.0));
        let bg = if selected { p.accent_soft } else if resp.hovered() { p.surface_hi } else { Color32::TRANSPARENT };
        ui.painter().rect_filled(cell, CornerRadius::same(10), bg);
        ui.painter().text(cell.center(), Align2::CENTER_CENTER, icon_str, icon_font(19.0), if selected { p.accent } else { p.text_dim });
        if let Some(n) = count.filter(|n| *n > 0) {
            let t = if n > 99 { "99+".to_string() } else { n.to_string() };
            let g = ui.painter().layout_no_wrap(t.clone(), semibold(9.5), Color32::WHITE);
            let bw = (g.size().x + 8.0).max(16.0);
            let br = Rect::from_center_size(pos2(cell.right() - 4.0, cell.top() + 5.0), vec2(bw, 16.0));
            ui.painter().rect_filled(br, CornerRadius::same(8), if selected { p.accent } else { p.border });
            ui.painter().text(br.center(), Align2::CENTER_CENTER, t, semibold(9.5), if selected { Color32::WHITE } else { p.text });
        }
    }
    resp.on_hover_text(label).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Round accent button with just an icon (narrow sidebar "New download").
pub fn round_cta(ui: &mut Ui, icon_str: &str, tooltip: &str) -> Response {
    let p = palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
    let r = Rect::from_center_size(rect.center(), vec2(42.0, 42.0));
    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() { p.accent.gamma_multiply(0.85) } else if resp.hovered() { lighten(p.accent, 0.1) } else { p.accent };
        ui.painter().add(egui::epaint::Shadow { offset: [0, 4], blur: 14, spread: 0, color: p.accent.gamma_multiply(0.35) }.as_shape(r, CornerRadius::same(13)));
        ui.painter().rect_filled(r, CornerRadius::same(13), fill);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, icon_str, icon_font(20.0), Color32::WHITE);
    }
    resp.on_hover_text(tooltip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn lighten(c: Color32, f: f32) -> Color32 {
    let l = |v: u8| (v as f32 + (255.0 - v as f32) * f) as u8;
    Color32::from_rgb(l(c.r()), l(c.g()), l(c.b()))
}
