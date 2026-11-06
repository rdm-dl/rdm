//! System tray icon: the OS tray on Windows and macOS; on Linux a StatusNotifierItem, the
//! D-Bus protocol the panels of Cinnamon, KDE, XFCE, MATE and others host (GNOME needs its
//! AppIndicator extension).

use eframe::egui;
use std::sync::mpsc::{Receiver, channel};

#[derive(Debug, Clone, Copy, PartialEq)]
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
    #[cfg(target_os = "linux")]
    _handle: ksni::Handle<linux::Item>,
    /// A panel is hosting the icon right now (on Linux it can come and go).
    #[cfg(target_os = "linux")]
    online: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub rx: Receiver<TrayAction>,
}

impl Tray {
    /// The icon is showing, so a hidden window can be brought back from it.
    pub fn available(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.online.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(target_os = "linux"))]
        true
    }
}

#[cfg(not(target_os = "linux"))]
pub fn create(ctx: egui::Context, _rt: &tokio::runtime::Handle) -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let png = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/tray32.png")).ok()?;
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
mod linux {
    use super::TrayAction;
    use eframe::egui;
    use ksni::menu::{MenuItem, StandardItem};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;

    pub struct Item {
        pub tx: Sender<TrayAction>,
        pub ctx: egui::Context,
        pub online: Arc<AtomicBool>,
        pub icons: Vec<ksni::Icon>,
    }

    impl Item {
        fn send(&self, a: TrayAction) {
            let _ = self.tx.send(a);
            self.ctx.request_repaint();
        }
    }

    fn entry(label: &str, action: TrayAction) -> MenuItem<Item> {
        StandardItem { label: label.into(), activate: Box::new(move |t: &mut Item| t.send(action)), ..Default::default() }.into()
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            "rdm-download-manager".into()
        }
        fn title(&self) -> String {
            "RDM".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icons.clone()
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "RDM Download Manager".into(), ..Default::default() }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(TrayAction::Show);
        }
        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                entry("Show RDM", TrayAction::Show),
                entry("New download…", TrayAction::NewDownload),
                MenuItem::Separator,
                entry("Pause all", TrayAction::PauseAll),
                entry("Resume all", TrayAction::ResumeAll),
                MenuItem::Separator,
                entry("Exit", TrayAction::Exit),
            ]
        }
        fn watcher_online(&self) {
            self.online.store(true, Ordering::Relaxed);
            self.ctx.request_repaint();
        }
        // Keep the item alive: the panel may simply not have started yet, or be restarting.
        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            self.online.store(false, Ordering::Relaxed);
            self.ctx.request_repaint();
            true
        }
    }

    /// RGBA pixels to the ARGB32 (network byte order) the protocol carries.
    pub fn icon(png: &[u8]) -> Option<ksni::Icon> {
        let i = eframe::icon_data::from_png_bytes(png).ok()?;
        let data = i.rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
        Some(ksni::Icon { width: i.width as i32, height: i.height as i32, data })
    }
}

/// An X server answers (natively or through XWayland). A local display is tried for real,
/// on its socket file or abstract socket: a stale DISPLAY must not stop RDM from starting.
#[cfg(target_os = "linux")]
pub fn x11_available() -> bool {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};

    let Ok(display) = std::env::var("DISPLAY") else { return false };
    match display.rsplit_once(':') {
        Some(("" | "unix", rest)) => {
            let path = format!("/tmp/.X11-unix/X{}", rest.split('.').next().unwrap_or(rest));
            UnixStream::connect(&path).is_ok() || SocketAddr::from_abstract_name(&path).and_then(|a| UnixStream::connect_addr(&a)).is_ok()
        }
        // A remote display (SSH forwarding); leave it to the connection itself.
        Some(_) => true,
        None => false,
    }
}

#[cfg(target_os = "linux")]
pub fn create(ctx: egui::Context, rt: &tokio::runtime::Handle) -> Option<Tray> {
    use ksni::TrayMethods;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    // Pure Wayland (no X11 to fall back on): a window can't be hidden, so closing has to quit.
    if !x11_available() {
        return None;
    }
    let (tx, rx) = channel();
    let online = Arc::new(AtomicBool::new(true));
    let icons = [&include_bytes!("../../../assets/tray32.png")[..], &include_bytes!("../../../assets/icon256.png")[..]].into_iter().filter_map(linux::icon).collect();
    let item = linux::Item { tx, ctx, online: online.clone(), icons };
    // Not giving up when no panel answers yet: at sign-in RDM can start before the panel.
    let spawn = item.assume_sni_available(true).spawn();
    let handle = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(5), spawn).await }).ok()?.ok()?;
    Some(Tray { _handle: handle, online, rx })
}
