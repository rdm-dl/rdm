# RDM — Rust Download Manager

A fast, modern, cross-platform download manager written in Rust.

## Features

**Download engine**
- Segmented multi-connection downloads (default 8 connections) with dynamic splitting: a free connection always takes over half of the part with the most remaining work, so slow connections get help and every socket stays busy.
- Starts with 4 connections, then doubles while every connection is receiving data.
- A connection that finishes its part takes over the next part directly if that part has not started yet.
- Adapts to servers that limit parallel connections.
- Pause / resume at any time, including after a crash or reboot (progress is saved every 2.5 s).
- Overlap verification between parts; falls back to a single connection if a server returns inconsistent data.
- HLS (`.m3u8`, including AES-128) and MPEG-DASH (`.mpd`) stream downloading, merged with ffmpeg when available.
- HTTP, HTTPS and FTP; HTTP/SOCKS5 and system proxies; Basic authentication; per-site saved logins; cookies and referrers from the browser.
- Smart file naming (Content-Disposition incl. UTF-8 names, `?file=` parameters), never silently overwrites, and strips invisible characters that could disguise a file's real extension.

**Organisation**
- Categories (Compressed, Documents, Music, Programs, Video) with their own folders.
- Queues with "N downloads at a time", a scheduler (start/stop times, weekdays, run once) and end-of-queue actions (exit, sleep, hibernate, shut down).
- Synchronization queue that re-downloads files when the server copy changes.
- Global and per-download speed limits, plus download quotas ("at most X MB every N hours").
- Site grabber: crawl a website and collect all images, videos, audio, documents, archives or custom file types.
- Batch downloads from URL templates (`file*.jpg`, 1–100, a–z) or from the clipboard; import/export lists.
- Clipboard monitoring, optional antivirus scan after completion, completion notifications.

**Integration**
- Browser extension for Chrome, Edge, Brave, Vivaldi, Opera and Firefox: takes over downloads, detects video/audio streams on pages, adds "Download with RDM" context menu entries.
- System tray icon; close-to-tray; start with the system.
- `rdm` command-line tool and headless daemon mode.

## Install (Windows)

1. Unzip `RDM-<version>-windows-x64.zip` anywhere (for example `C:\Program Files\RDM`).
2. Run `rdm-gui.exe`.
3. Browser integration:
   - Chrome / Edge / Brave: open `chrome://extensions`, enable **Developer mode**, click **Load unpacked** and select the `extension-chrome` folder.
   - Firefox: open `about:addons` → gear menu → **Install Add-on From File…** and pick `rdm-firefox.xpi` (Developer/Nightly, or after signing).
   - In RDM: **Settings → General → Register browser bridge**.

On Linux and macOS, unpack the archive and run `rdm-gui` (macOS: `RDM.app`).

## Command line

```
rdm get <URL> [-p folder] [-f name] [-c connections] [-l 500K] [--referer URL] [--cookie "a=b"] [-H "Name: value"]
rdm add <URL> [--now]       # send to the running app
rdm list                    # downloads in the running app
rdm show                    # bring the window to the front
rdm daemon                  # run without a GUI
```

`rdm get` resumes automatically when run again after an interruption (Ctrl+C pauses).

## Build from source

Requires Rust 1.85+ (edition 2024).

```
cargo build --release
python tools/package.py
```

Linux needs: `libgtk-3-dev libxkbcommon-dev libwayland-dev libxrandr-dev libxi-dev libgl1-mesa-dev libxcb1-dev`.

Project layout:

| Crate | Purpose |
|---|---|
| `crates/rdm-core` | Download engine, HLS/DASH, manager (queues, scheduler, settings), site grabber, local IPC |
| `crates/rdm-gui` | Desktop app (egui) |
| `crates/rdm-cli` | `rdm` command-line tool |
| `crates/rdm-bridge` | Native-messaging host for the browser extension |
| `extension/` | Browser extension (Manifest V3) |

## License

MIT. The bundled Inter font is licensed under the SIL Open Font License (see `crates/rdm-gui/assets/fonts/Inter-LICENSE.txt`).
