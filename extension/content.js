// RDM content script (every frame, document_start):
//  - floating "Download this video/audio" button over playing or hovered media, with a
//    menu of the streams, files and subtitles the extension detected on the tab
//  - relays media requests seen by document.js (page fetch/XHR hooks) to the background
//  - reports held keys on clicks so the background can force or skip a capture

(() => {
  if (window.__rdmContent) return;
  window.__rdmContent = true;
  const api = typeof browser !== "undefined" ? browser : chrome;
  const MAGIC = 0x52444d34; // "RDM4", see document.js

  const CSS = `
    :host { all: initial; }
    .wrap { position: relative; font: 500 13px/1.2 "Inter", "Segoe UI", system-ui, -apple-system, sans-serif; color: #f2f4f8; }
    .btn {
      display: flex; align-items: center; gap: 8px; height: 34px; padding: 0 6px 0 5px;
      background: rgba(18, 21, 28, 0.86); border: 1px solid rgba(255, 255, 255, 0.14); border-radius: 999px;
      color: inherit; font: inherit; cursor: pointer; box-shadow: 0 6px 24px rgba(0, 0, 0, 0.35);
      backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px); transition: background .15s, transform .15s;
      white-space: nowrap;
    }
    .btn:hover { background: rgba(28, 33, 44, 0.94); transform: translateY(-1px); }
    .ico { display: grid; place-items: center; width: 24px; height: 24px; border-radius: 50%; background: #5b7cff; color: #fff; flex: none; }
    .label { padding-right: 2px; }
    .count { min-width: 18px; height: 18px; padding: 0 5px; border-radius: 9px; background: rgba(255,255,255,.14); font-size: 11px; display: grid; place-items: center; }
    .count:empty { display: none; }
    .x { display: grid; place-items: center; width: 22px; height: 22px; border-radius: 50%; color: #aeb5c4; font-size: 15px; line-height: 1; }
    .x:hover { background: rgba(255,255,255,.12); color: #fff; }
    .menu {
      position: absolute; top: 40px; right: 0; width: 310px; max-height: 360px; overflow: auto; padding: 6px;
      background: rgba(18, 21, 28, 0.96); border: 1px solid rgba(255, 255, 255, 0.12); border-radius: 14px;
      box-shadow: 0 16px 40px rgba(0, 0, 0, 0.45); backdrop-filter: blur(14px); -webkit-backdrop-filter: blur(14px);
      scrollbar-width: thin; scrollbar-color: rgba(255,255,255,.22) transparent;
    }
    .menu::-webkit-scrollbar { width: 8px; }
    .menu::-webkit-scrollbar-thumb { background: rgba(255,255,255,.22); border-radius: 4px; }
    .menu.up { top: auto; bottom: 40px; }
    .menu[hidden] { display: none; }
    .head { padding: 8px 10px 6px; font-size: 11px; letter-spacing: .04em; text-transform: uppercase; color: #8b93a5; }
    .item { display: flex; align-items: center; gap: 10px; width: 100%; padding: 9px 10px; border: 0; border-radius: 10px; background: transparent; color: inherit; font: inherit; text-align: left; cursor: pointer; }
    .item:hover { background: rgba(91, 124, 255, 0.18); }
    .q { flex: none; min-width: 52px; padding: 3px 7px; border-radius: 6px; background: rgba(91,124,255,.22); color: #c9d4ff; font-size: 12px; font-weight: 600; text-align: center; }
    .meta { flex: 1; min-width: 0; color: #c3c8d4; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .note { padding: 10px; color: #8b93a5; line-height: 1.45; }
    .bad { color: #f58b8c; }
    .sep { height: 1px; margin: 4px 6px; background: rgba(255,255,255,.08); }
    .link { display: block; width: 100%; padding: 8px 10px; border: 0; border-radius: 10px; background: transparent; color: #8b93a5; font: inherit; font-size: 12px; text-align: left; cursor: pointer; }
    .link:hover { background: rgba(255,255,255,.06); color: #dfe3ec; }
  `;
  const ICON = '<svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true"><path d="M12 4v11m0 0-4.5-4.5M12 15l4.5-4.5M5 20h14" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>';

  // Settings: local toggle (popup) + lists pushed from the app.
  let cfg = { show_button: true, show_video_button: true, show_audio_button: true, no_button_hosts: [], blocked_hosts: [] };
  let host = null;
  let root, btn, label, count, menu;
  let current = null; // media element the button is attached to
  let visibleUntil = 0;
  let menuOpen = false;
  let rafId = 0;
  let mediaCount = 0;
  const dismissed = new WeakSet();

  function send(msg) {
    return new Promise((resolve) => {
      try {
        api.runtime.sendMessage(msg, (r) => {
          void api.runtime.lastError;
          resolve(r);
        });
      } catch {
        resolve(null);
      }
    });
  }

  function wildcard(pattern, text) {
    const re = new RegExp("^" + pattern.toLowerCase().replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".") + "$");
    return re.test(text.toLowerCase());
  }

  function siteAllowed() {
    const h = location.hostname;
    return ![...cfg.no_button_hosts, ...cfg.blocked_hosts].some((p) => wildcard(p, h));
  }

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  // ------------------------------------------------------------ button

  function build() {
    host = document.createElement("rdm-media-button");
    host.style.cssText = "all:initial;position:fixed;z-index:2147483647;top:0;left:0;display:none;";
    root = host.attachShadow({ mode: "closed" });
    try {
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(CSS);
      root.adoptedStyleSheets = [sheet];
    } catch {
      root.append(el("style", null, CSS));
    }
    const wrap = el("div", "wrap");
    btn = el("button", "btn");
    btn.type = "button";
    const ico = el("span", "ico");
    ico.innerHTML = ICON;
    label = el("span", "label", "Download this video");
    count = el("span", "count", mediaCount ? String(mediaCount) : "");
    const x = el("span", "x", "×");
    x.title = "Hide";
    btn.append(ico, label, count, x);
    menu = el("div", "menu");
    menu.hidden = true;
    wrap.append(btn, menu);
    root.append(wrap);

    x.addEventListener("click", (e) => {
      e.stopPropagation();
      if (current) dismissed.add(current);
      hide(true);
    });
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      menuOpen ? closeMenu() : openMenu();
    });
    for (const t of ["mousedown", "pointerdown", "mouseup", "dblclick", "contextmenu", "wheel"]) {
      host.addEventListener(t, (e) => e.stopPropagation());
    }
    attachHost();
  }

  function attachHost() {
    const fs = document.fullscreenElement || document.webkitFullscreenElement;
    const parent = fs && fs.tagName !== "VIDEO" ? fs : document.documentElement;
    if (host && parent && host.parentNode !== parent) parent.appendChild(host);
  }

  function isAudio(m) {
    return m instanceof HTMLAudioElement;
  }

  function usable(m) {
    if (!m || !m.isConnected || dismissed.has(m)) return false;
    if (isAudio(m) ? !cfg.show_audio_button : !cfg.show_video_button) return false;
    const r = m.getBoundingClientRect();
    const [minW, minH] = isAudio(m) ? [180, 20] : [200, 110];
    if (r.width < minW || r.height < minH) return false;
    if (r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
    const cs = getComputedStyle(m);
    return cs.visibility !== "hidden" && cs.display !== "none" && +cs.opacity > 0.05;
  }

  function enabled() {
    return cfg.show_button && (cfg.show_video_button || cfg.show_audio_button) && siteAllowed();
  }

  function mediaAt(x, y) {
    let best = null;
    let area = 0;
    for (const m of document.querySelectorAll("video, audio")) {
      const r = m.getBoundingClientRect();
      if (x < r.left || x > r.right || y < r.top || y > r.bottom || !usable(m)) continue;
      if (r.width * r.height > area) {
        area = r.width * r.height;
        best = m;
      }
    }
    return best;
  }

  function place() {
    if (!host || !current) return;
    if (!usable(current) && !menuOpen) return hide(true);
    const r = current.getBoundingClientRect();
    const w = host.offsetWidth || 220;
    const left = Math.max(4, Math.min(r.right - w - 10, innerWidth - w - 4));
    // Audio players are short: sit above them.
    const top = isAudio(current) ? r.top - 42 : r.top + 10;
    const y = Math.max(4, Math.min(top, innerHeight - 44));
    host.style.left = `${Math.round(left)}px`;
    host.style.top = `${Math.round(y)}px`;
    menu.classList.toggle("up", y > innerHeight - 380 && y > 380);
  }

  function loop() {
    rafId = 0;
    if (!host || host.style.display === "none") return;
    if (!menuOpen && Date.now() > visibleUntil) return hide(false);
    place();
    rafId = requestAnimationFrame(loop);
  }

  function show(m, ms) {
    if (!enabled() || !m) return;
    if (!host) build();
    attachHost();
    if (current !== m) {
      closeMenu();
      current = m;
      label.textContent = isAudio(m) ? "Download this audio" : "Download this video";
    }
    visibleUntil = Math.max(visibleUntil, Date.now() + ms);
    host.style.display = "block";
    place();
    if (!rafId) rafId = requestAnimationFrame(loop);
  }

  function hide(force) {
    if (!host) return;
    if (!force && menuOpen) return;
    closeMenu();
    host.style.display = "none";
    visibleUntil = 0;
  }

  function closeMenu() {
    menuOpen = false;
    if (menu) {
      menu.hidden = true;
      menu.replaceChildren();
    }
  }

  function fmtSize(n) {
    if (!n) return "";
    const u = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    while (n >= 1024 && i < u.length - 1) {
      n /= 1024;
      i++;
    }
    return `${n.toFixed(i > 1 ? 1 : 0)} ${u[i]}`;
  }

  function fmtDur(s) {
    if (!s) return "";
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = String(Math.floor(s % 60)).padStart(2, "0");
    return h ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
  }

  function nameOf(url) {
    try {
      return decodeURIComponent(new URL(url).pathname.split("/").pop()) || url;
    } catch {
      return url;
    }
  }

  function tagOf(f) {
    const ext = (nameOf(f.url).split(".").pop() || "").toUpperCase();
    if (ext && ext.length <= 4 && /^[A-Z0-9]+$/.test(ext)) return ext;
    return (f.type || "").split("/").pop().toUpperCase().slice(0, 4) || "FILE";
  }

  function entry(q, meta, onClick) {
    const b = el("button", "item");
    b.type = "button";
    b.append(el("span", "q", q), el("span", "meta", meta));
    b.title = meta;
    b.addEventListener("click", async (e) => {
      e.stopPropagation();
      closeMenu();
      const normal = label.textContent;
      label.textContent = "Sending…";
      const reply = await onClick();
      label.textContent = reply && reply.type === "accepted" ? "Sent to RDM ✓" : reply && reply.type === "ok" ? "Opened in RDM ✓" : "RDM not reachable";
      visibleUntil = Date.now() + 2500;
      setTimeout(() => (label.textContent = normal), 2500);
    });
    return b;
  }

  async function openMenu() {
    menuOpen = true;
    menu.hidden = false;
    menu.replaceChildren(el("div", "note", "Looking for media…"));
    const state = (await send({ type: "page-media", pageUrl: location.href })) || { media: [], connected: false };
    if (!menuOpen) return;
    menu.replaceChildren();
    const items = state.media || [];
    if (!state.connected) menu.append(el("div", "note bad", "RDM is not running. Start the app, then try again."));
    // Site player streams (e.g. YouTube): one entry per quality, video + audio joined by RDM.
    const site = items.filter((m) => m.kind === "yt");
    const siteNotes = items.filter((m) => m.kind === "yt-note");
    if (site.length || siteNotes.length) {
      const t = (site[0] || siteNotes[0]).title;
      menu.append(el("div", "head", t ? `Video · ${t}` : "Video"));
      for (const s of site) {
        const meta = [s.container, s.fps > 30 ? `${s.fps}fps` : "", fmtSize(s.size), fmtDur(s.duration)].filter(Boolean).join(" · ");
        menu.append(entry(s.label, meta, () => send({ type: "download-media", kind: "yt", videoId: s.videoId, itagV: s.itagV, itagA: s.itagA, title: s.title, ext: s.ext })));
      }
      for (const n of siteNotes) menu.append(el("div", "note", n.text));
    }
    const rest = items.filter((m) => m.kind !== "yt" && m.kind !== "yt-note");
    items.length = 0;
    items.push(...rest);
    const direct = current && /^https?:/i.test(current.currentSrc || "") ? current.currentSrc.split("#")[0] : "";
    if (direct && !items.some((m) => m.url === direct)) {
      items.push({ url: direct, kind: "file", type: isAudio(current) ? "audio" : "video", size: 0 });
    }
    const streams = items.filter((m) => m.kind === "stream");
    const files = items.filter((m) => m.kind === "file");
    const subs = items.filter((m) => m.kind === "subtitle");
    const title = document.title;
    if (streams.length) {
      if (site.length) menu.append(el("div", "sep"));
      menu.append(el("div", "head", "Streams"));
      for (const s of streams) {
        const kind = s.type.startsWith("DASH") ? "DASH" : "HLS";
        const meta = [kind, fmtDur(s.duration)].filter(Boolean).join(" · ");
        if (s.qualities && s.qualities.length) {
          for (const q of s.qualities) {
            menu.append(entry(`${q}p`, meta, () => send({ type: "download-media", url: s.url, kind: "stream", quality: q, title })));
          }
        } else {
          menu.append(entry("Best", meta, () => send({ type: "download-media", url: s.url, kind: "stream", title })));
        }
      }
    }
    if (files.length) {
      if (streams.length) menu.append(el("div", "sep"));
      menu.append(el("div", "head", "Files"));
      for (const f of files) {
        const meta = [fmtSize(f.size), nameOf(f.url)].filter(Boolean).join(" · ");
        menu.append(entry(tagOf(f), meta, () => send({ type: "download-media", url: f.url, kind: "file", title })));
      }
    }
    if (subs.length) {
      menu.append(el("div", "sep"), el("div", "head", "Subtitles"));
      for (const f of subs) {
        menu.append(entry(tagOf(f), nameOf(f.url), () => send({ type: "download-media", url: f.url, kind: "subtitle", title })));
      }
    }
    if (!site.length && !siteNotes.length && !streams.length && !files.length && !subs.length) {
      menu.append(el("div", "note", "Nothing downloadable detected yet. Let it play for a few seconds, then click again. If you want another quality, pick it in the player first."));
    }
    menu.append(el("div", "sep"));
    const block = el("button", "link", `Don't show on ${location.hostname}`);
    block.type = "button";
    block.addEventListener("click", async (e) => {
      e.stopPropagation();
      await send({ type: "block-host", host: location.hostname });
      cfg.blocked_hosts.push(location.hostname);
      hide(true);
    });
    menu.append(block);
  }

  async function refreshSettings() {
    const r = await send({ type: "button-settings" });
    if (r) {
      cfg = { ...cfg, ...r };
      if (typeof r.count === "number") mediaCount = r.count;
    }
    if (!enabled()) hide(true);
  }

  // ------------------------------------------------------------ events

  // Hover over a large video or an audio player.
  let pending = false;
  let lastX = 0;
  let lastY = 0;
  document.addEventListener(
    "mousemove",
    (e) => {
      lastX = e.clientX;
      lastY = e.clientY;
      if (pending || !enabled()) return;
      pending = true;
      requestAnimationFrame(() => {
        pending = false;
        if (host && host.style.display !== "none") {
          const r = host.getBoundingClientRect();
          if (lastX >= r.left && lastX <= r.right && lastY >= r.top && lastY <= r.bottom) {
            visibleUntil = Date.now() + 2000;
            return;
          }
        }
        const m = mediaAt(lastX, lastY);
        if (m) show(m, 2500);
      });
    },
    { capture: true, passive: true }
  );

  // Show for a few seconds when playback starts.
  document.addEventListener(
    "play",
    (e) => {
      const m = e.target;
      if ((m instanceof HTMLVideoElement || m instanceof HTMLAudioElement) && usable(m)) show(m, 4000);
    },
    true
  );

  document.addEventListener("click", () => menuOpen && closeMenu(), true);
  document.addEventListener("fullscreenchange", attachHost);
  document.addEventListener("webkitfullscreenchange", attachHost);

  // Held keys, reported on each click so a following download can be forced or skipped.
  const held = new Set();
  const keyName = (e) => ({ Insert: "insert", Delete: "delete" })[e.key];
  window.addEventListener("keydown", (e) => keyName(e) && held.add(keyName(e)), true);
  window.addEventListener("keyup", (e) => keyName(e) && held.delete(keyName(e)), true);
  window.addEventListener("blur", () => held.clear());
  window.addEventListener(
    "mousedown",
    (e) => {
      if (e.button > 1) return;
      const keys = [...held];
      if (e.altKey) keys.push("alt");
      if (e.ctrlKey || e.metaKey) keys.push("ctrl");
      if (e.shiftKey) keys.push("shift");
      send({ type: "keys", keys });
    },
    true
  );

  // Media requests seen inside the page (document.js).
  window.addEventListener("message", (e) => {
    const d = e.data;
    if (e.source !== window || !d || d.__rdm !== MAGIC) return;
    if (d.yt && typeof d.yt === "object") {
      send({ type: "yt-player", data: d.yt });
      return;
    }
    if (d.sabr && typeof d.sabr === "object") {
      send({ type: "yt-sabr", data: d.sabr });
      return;
    }
    if (d.sabrSample && typeof d.sabrSample === "object") {
      send({ type: "yt-sabr-sample", data: d.sabrSample });
      return;
    }
    if (typeof d.url !== "string") return;
    send({ type: "page-request", url: d.url, mime: String(d.type || ""), size: +d.size || 0 });
  });

  api.runtime.onMessage.addListener((msg) => {
    if (!msg) return;
    if (msg.type === "rdm-media") {
      mediaCount = msg.count || 0;
      if (count) count.textContent = mediaCount ? String(mediaCount) : "";
    }
    if (msg.type === "rdm-settings") refreshSettings();
  });
  refreshSettings();
  if (window.top === window && /(^|\.)youtube\.com$/.test(location.hostname)) {
    setTimeout(() => send({ type: "content-hello", url: location.href, docHook: document.documentElement.dataset.rdmHook === "1" }), 3000);
  }
})();
