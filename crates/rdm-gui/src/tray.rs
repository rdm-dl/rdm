//! System tray icon (Windows and macOS).

use eframe::egui;
use std::sync::mpsc::Receiver;
#[cfg(not(target_os = "linux"))]
use std::sync::mpsc::channel;

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub enum TrayAction {
    Show,
    NewDownload,
    PauseAll,
    ResumeAll,
    Exit,
}

pub struct Tray {
    #[cfg(not(target_os = "linux"))]
    _icon: tray_icon::TrayIcon,
    pub rx: Receiver<TrayAction>,
}

#[cfg(not(target_os = "linux"))]
pub fn create(ctx: egui::Context) -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let png = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon256.png")).ok()?;
    let icon = tray_icon::Icon::from_rgba(png.rgba, png.width, png.height).ok()?;
    let menu = Menu::new();
    let show = MenuItem::new("Show RDM", true, None);
    let add = MenuItem::new("New download…", true, None);
    let pause = MenuItem::new("Pause all", true, None);
    let resume = MenuItem::new("Resume all", true, None);
    let exit = MenuItem::new("Exit", true, None);
    for item in [&show as &dyn tray_icon::menu::IsMenuItem, &add, &PredefinedMenuItem::separator(), &pause, &resume, &PredefinedMenuItem::separator(), &exit] {
        let _ = menu.append(item);
    }
    let ids = [
        (show.id().clone(), TrayAction::Show),
        (add.id().clone(), TrayAction::NewDownload),
        (pause.id().clone(), TrayAction::PauseAll),
        (resume.id().clone(), TrayAction::ResumeAll),
        (exit.id().clone(), TrayAction::Exit),
    ];
    let (tx, rx) = channel();
    let (tx2, ctx2) = (tx.clone(), ctx.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, a)) = ids.iter().find(|(id, _)| *id == e.id) {
            let _ = tx.send(*a);
            ctx.request_repaint();
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        let show = match e {
            TrayIconEvent::DoubleClick { .. } => true,
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => true,
            _ => false,
        };
        if show {
            let _ = tx2.send(TrayAction::Show);
            ctx2.request_repaint();
        }
    }));
    let icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("RDM Download Manager")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .ok()?;
    Some(Tray { _icon: icon, rx })
}

#[cfg(target_os = "linux")]
pub fn create(_ctx: egui::Context) -> Option<Tray> {
    None
}
