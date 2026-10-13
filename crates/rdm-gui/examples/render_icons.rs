//! Render the app icon SVG to every PNG size the program and the browser extension use.
//! usage: cargo run -p rdm-gui --example render_icons -- <icon.svg> <out-dir> [size...]

use resvg::{tiny_skia, usvg};

fn main() {
    let mut args = std::env::args().skip(1);
    let svg = std::fs::read(args.next().expect("icon.svg")).expect("read svg");
    let out = std::path::PathBuf::from(args.next().expect("out dir"));
    let mut sizes: Vec<u32> = args.filter_map(|a| a.parse().ok()).collect();
    if sizes.is_empty() {
        sizes = vec![16, 20, 24, 32, 40, 48, 64, 96, 128, 256, 512];
    }
    std::fs::create_dir_all(&out).expect("out dir");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("parse svg");
    for px in sizes {
        let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("pixmap");
        let s = tree.size();
        let side = s.width().max(s.height());
        // Small sizes (tabs, taskbar, tray): crop the outer glow so the tile fills the icon.
        // The artwork's tile spans 80..1174 of its 1254-unit canvas.
        let (crop_from, crop_size) = if px <= 48 { (side * 72.0 / 1254.0, side * 1110.0 / 1254.0) } else { (0.0, side) };
        let scale = px as f32 / crop_size;
        let t = tiny_skia::Transform::from_scale(scale, scale).pre_translate(-crop_from, -crop_from);
        resvg::render(&tree, t, &mut pixmap.as_mut());
        let path = out.join(format!("icon-{px}.png"));
        pixmap.save_png(&path).expect("write png");
        println!("{}", path.display());
    }
}
