//! Dialogs as real windows of their own: separate OS windows that open centred on the main
//! window. On Windows they are owned by the main window, like standard dialogs: they stay
//! in front of it, have no taskbar button of their own and minimize with it. Where the
//! platform can't open more windows, egui shows them inside the main window instead.

use crate::theme;
use eframe::egui::{self, Id, Margin, Pos2, Stroke, Ui, Vec2, ViewportBuilder, ViewportClass, ViewportId};
use std::hash::Hash;
use std::sync::{Arc, OnceLock};

/// The main window's title (also used to find it among the app's windows).
pub const MAIN_TITLE: &str = "RDM — Download Manager";

/// Size and behaviour of a dialog window.
pub struct Spec<'a> {
    pub title: &'a str,
    pub size: [f32; 2],
    pub min: [f32; 2],
    pub resizable: bool,
}

pub fn app_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    ICON.get_or_init(|| Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon256.png")).unwrap_or_default())).clone()
}

/// Show a dialog window identified by `key`. `add` draws the contents (call [`footer`]
/// first, then [`body`]). Returns what `add` returned and whether the user closed the
/// window with its close button or Esc.
pub fn show<R>(ctx: &egui::Context, key: impl Hash + std::fmt::Debug, spec: Spec, mut add: impl FnMut(&mut Ui) -> R) -> (R, bool) {
    let vid = ViewportId::from_hash_of(("rdm-dialog", key));
    let mut b = ViewportBuilder::default()
        .with_title(spec.title)
        .with_inner_size(spec.size)
        .with_min_inner_size(spec.min)
        .with_resizable(spec.resizable)
        .with_maximize_button(spec.resizable)
        .with_minimize_button(false)
        .with_icon(app_icon());
    if cfg!(windows) {
        b = b.with_taskbar(false);
    }
    if let Some(p) = placement(ctx, vid, spec.size) {
        b = b.with_position(p);
    }
    ctx.show_viewport_immediate(vid, b, |ui, class| {
        if class == ViewportClass::Immediate {
            own_new_windows();
        }
        let closed = ui.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape));
        (add(ui), closed)
    })
}

/// Where a dialog opens: centred on the main window when it (re)opens, then left where the
/// user puts it. The position is kept while the dialog is shown on consecutive passes.
fn placement(ctx: &egui::Context, vid: ViewportId, size: [f32; 2]) -> Option<Pos2> {
    let id = Id::new(("rdm-dialog-pos", vid));
    let pass = ctx.cumulative_pass_nr();
    let prev: Option<(Pos2, u64)> = ctx.data(|d| d.get_temp(id));
    let pos = match prev {
        Some((p, last)) if pass <= last + 2 => p,
        _ => {
            let main = ctx.input(|i| i.viewport().outer_rect)?;
            let p = main.center() - Vec2::from(size) / 2.0;
            Pos2::new(p.x.max(main.left()), p.y.max(main.top()))
        }
    };
    ctx.data_mut(|d| d.insert_temp(id, (pos, pass)));
    Some(pos)
}

/// Button bar along the bottom of a dialog; contents are laid out right to left, so add
/// the main action first.
pub fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let p = theme::palette(ui);
    egui::Panel::bottom(ui.id().with("footer"))
        .frame(egui::Frame::new().fill(p.sidebar).inner_margin(Margin::symmetric(20, 12)).stroke(Stroke::NONE))
        .show(ui, |ui| {
            let r = ui.max_rect();
            ui.painter().hline(r.x_range().expand(20.0), r.top() - 12.0, Stroke::new(1.0, p.border));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
        });
}

/// The scrollable main area of a dialog.
pub fn body<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette(ui);
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin { left: 22, right: 12, top: 18, bottom: 12 }))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Room for the scrollbar so it never sits on the content.
                    ui.set_max_width(ui.available_width() - 10.0);
                    add(ui)
                })
                .inner
        })
        .inner
}

/// Like [`body`], without scrolling (the contents manage their own space).
pub fn body_fixed<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette(ui);
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(22, 18)))
        .show(ui, add)
        .inner
}

/// Make the app's dialog windows owned by the main window (Windows): new top-level windows
/// of this process that are visible, have a title bar and no owner yet.
#[cfg(windows)]
fn own_new_windows() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GWL_STYLE, GWLP_HWNDPARENT, GetWindow, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, SetWindowLongPtrW, WS_CAPTION,
    };

    // A few times a second is plenty: a new dialog is owned within its first moments.
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 150 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);

    struct Found {
        pid: u32,
        main: HWND,
        dialogs: Vec<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lp: LPARAM) -> i32 {
        unsafe {
            let f = &mut *(lp as *mut Found);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != f.pid || IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
                return 1;
            }
            if GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_CAPTION != WS_CAPTION {
                return 1;
            }
            let mut buf = [0u16; 64];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            if String::from_utf16_lossy(&buf[..n]) == MAIN_TITLE {
                f.main = hwnd;
            } else {
                f.dialogs.push(hwnd);
            }
            1
        }
    }
    let mut f = Found { pid: unsafe { GetCurrentProcessId() }, main: std::ptr::null_mut(), dialogs: vec![] };
    unsafe {
        EnumWindows(Some(visit), &mut f as *mut Found as LPARAM);
        if !f.main.is_null() {
            for d in f.dialogs {
                SetWindowLongPtrW(d, GWLP_HWNDPARENT, f.main as isize);
            }
        }
    }
}

#[cfg(not(windows))]
fn own_new_windows() {}
