`eframe/` is eframe 0.36.2 (MIT OR Apache-2.0, https://github.com/emilk/egui) with one change:
`new_events` in `src/native/run.rs` runs `check_redraw_requests` with the event loop set, so a
frame of a hidden or minimized window can open a dialog window. See `[patch.crates-io]` in the
root `Cargo.toml`.
