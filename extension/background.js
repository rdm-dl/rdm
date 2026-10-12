// RDM browser integration: download capture (with POST bodies and force/skip keys), media
// detection (network + page hooks), the in-page media button, context menus and popup.
// Talks to the desktop app through the native messaging host "com.rdm.bridge".

const api = typeof browser !== "undefined" ? browser : chrome;
const HOST = "com.rdm.bridge";

// Lists pushed by the app (Options > File types / Browser); these are the fallbacks.
let settings = {
  enabled: true,
  extensions: [],
  media_extensions: [],
  exception_hosts: [],
  exception_urls: [],
  min_size: 0,
  ignore_extensions: ["txt", "htm", "html", "css", "js", "json", "apng", "gif", "ico", "jpeg", "jpg", "png", "svg", "webp"],
  download_mimes: ["application/download", "application/force-download", "application/x-msdownload", "application/octet-stream", "binary/octet-stream"],
  show_video_button: true,
  show_audio_button: true,
  no_button_hosts: [],
  force_key: "alt",
  skip_key: "ctrl",
};
let connected = false;
let lastRefresh = 0;

// ------------------------------------------------------------ native messaging

function send(message) {
  return new Promise((resolve) => {
    try {
      api.runtime.sendNativeMessage(HOST, message, (reply) => {
        const err = api.runtime.lastError;
        if (err || !reply) {
          connected = false;
          resolve({ type: "error", message: err ? err.message : "no reply" });
        } else {
          connected = true;
          resolve(reply);
        }
      });
    } catch (e) {
      connected = false;
      resolve({ type: "error", message: String(e) });
    }
  });
}

async function refreshSettings() {
  lastRefresh = Date.now();
  const stored = await api.storage.local.get(["enabled"]);
  const reply = await send({ type: "hello" });
  if (reply.type === "settings" && reply.capture) {
    const before = JSON.stringify(settings);
    settings = { ...settings, ...reply.capture };
    if (typeof stored.enabled === "boolean") settings.enabled = stored.enabled;
    if (JSON.stringify(settings) !== before) broadcast({ type: "rdm-settings" });
  } else if (typeof stored.enabled === "boolean") {
    settings.enabled = stored.enabled;
  }
  return reply;
}

async function freshSettings() {
  if (!connected || Date.now() - lastRefresh > 60 * 1000) await refreshSettings();
}

async function broadcast(msg) {
  try {
    for (const t of await api.tabs.query({})) {
      if (t.id != null) api.tabs.sendMessage(t.id, msg, () => void api.runtime.lastError);
    }
  } catch {}
}

// ------------------------------------------------------------ helpers

function wildcard(pattern, text) {
  const re = new RegExp("^" + pattern.toLowerCase().replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".") + "$");
  return re.test(text.toLowerCase());
}

function extensionOf(nameOrUrl) {
  try {
    const path = nameOrUrl.includes("://") ? new URL(nameOrUrl).pathname : nameOrUrl;
    const last = decodeURIComponent(path.split(/[\\/]/).pop() || "");
    const dot = last.lastIndexOf(".");
    return dot > 0 ? last.slice(dot + 1).toLowerCase() : "";
  } catch {
    return "";
  }
}

function hostOf(url) {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
}

async function cookiesFor(url) {
  try {
    const list = await api.cookies.getAll({ url });
    return list.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

async function payload(url, extra = {}) {
  return {
    url,
    cookies: await cookiesFor(url),
    userAgent: navigator.userAgent,
    ...extra,
  };
}

// ------------------------------------------------------------ keys and POST bodies

// Keys held during the last click in any page (reported by content.js).
let lastKeys = { keys: [], t: 0 };

function heldKeys() {
  return Date.now() - lastKeys.t < 5000 ? lastKeys.keys : [];
}

// Bodies of recent form POSTs, so POST-generated downloads can be repeated by the app.
const postBodies = new Map(); // url -> { body, t }

function encodeForm(formData) {
  const parts = [];
  for (const [k, values] of Object.entries(formData)) {
    for (const v of values) parts.push(`${encodeURIComponent(k)}=${encodeURIComponent(v)}`);
  }
  return parts.join("&");
}

api.webRequest.onBeforeRequest.addListener(
  (d) => {
    if (d.method !== "POST" || !d.requestBody) return;
    let body = "";
    if (d.requestBody.formData) body = encodeForm(d.requestBody.formData);
    else if (d.requestBody.raw && d.requestBody.raw[0] && d.requestBody.raw[0].bytes) {
      try {
        body = new TextDecoder().decode(d.requestBody.raw[0].bytes);
      } catch {}
    }
    if (!body) return;
    postBodies.set(d.url, { body, t: Date.now() });
    for (const [u, v] of postBodies) if (Date.now() - v.t > 120000) postBodies.delete(u);
  },
  { urls: ["<all_urls>"], types: ["main_frame", "sub_frame"] },
  ["requestBody"]
);

function postBodyFor(...urls) {
  for (const u of urls) {
    const v = u && postBodies.get(u);
    if (v && Date.now() - v.t < 120000) return v.body;
  }
  return undefined;
}

// ------------------------------------------------------------ download capture

// "force" (key held), "skip", or a normal yes/no decision.
function captureDecision(item, name) {
  const url = item.finalUrl || item.url;
  if (!settings.enabled || !connected) return false;
  if (!/^(https?|ftp):/i.test(url)) return false; // blob:, data:, file: stay in the browser
  const keys = heldKeys();
  if (settings.skip_key !== "none" && keys.includes(settings.skip_key)) return false;
  if (settings.force_key !== "none" && keys.includes(settings.force_key)) return true;
  const host = hostOf(url);
  if (settings.exception_hosts.some((h) => wildcard(h, host))) return false;
  if (settings.exception_urls.some((u) => wildcard(u, url) || wildcard(u, item.url))) return false;
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize;
  if (settings.min_size > 0 && size > 0 && size < settings.min_size) return false;
  const ext = extensionOf(name || "") || extensionOf(url);
  if (ext && settings.ignore_extensions.some((e) => wildcard(e, ext))) return false;
  if (ext) return settings.extensions.some((e) => wildcard(e, ext));
  const mime = (item.mime || "").toLowerCase();
  return settings.download_mimes.some((m) => wildcard(m, mime));
}

const handled = new Set();

async function takeOver(item, name) {
  if (handled.has(item.id)) return;
  handled.add(item.id);
  await freshSettings();
  if (!captureDecision(item, name)) return;
  try {
    await api.downloads.pause(item.id);
  } catch {
    return; // already finished or not pausable
  }
  const url = item.finalUrl || item.url;
  const reply = await send({
    type: "download",
    ...(await payload(url, {
      referrer: item.referrer || "",
      filename: name || undefined,
      postData: postBodyFor(item.url, url),
    })),
  });
  if (reply.type === "accepted") {
    try {
      await api.downloads.cancel(item.id);
      await api.downloads.erase({ id: item.id });
    } catch {}
  } else {
    try {
      await api.downloads.resume(item.id);
    } catch {}
  }
}

if (api.downloads.onDeterminingFilename) {
  // Chromium: the final file name (from Content-Disposition) is known here.
  api.downloads.onDeterminingFilename.addListener((item, suggest) => {
    suggest();
    takeOver(item, (item.filename || "").split(/[\\/]/).pop());
  });
} else {
  // Firefox: onCreated already carries the target path.
  api.downloads.onCreated.addListener((item) => takeOver(item, (item.filename || "").split(/[\\/]/).pop()));
}

// ------------------------------------------------------------ media detection

const MEDIA_TYPES = /^(video\/|audio\/|application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml))/i;
const SUB_TYPES = /^(text\/vtt|application\/(ttml\+xml|x-subrip))/i;
const SUB_EXT = ["vtt", "srt", "ttml", "ttml2", "dfxp"];
const MANIFEST_EXT = /\.(m3u8|mpd)(\?|$)/i;
const SEGMENT_EXT = /\.(ts|m4s|aac|m4f|cmfv|cmfa)(\?|$)/i;

// tabId -> Map(url -> info); kept in session storage so it survives service-worker restarts.
let media = new Map();
let children = new Map(); // tabId -> Set(playlist URLs referenced by a captured master)
let loaded = null;
let saveTimer = 0;

function loadState() {
  if (!loaded) {
    loaded = (async () => {
      try {
        const s = await api.storage.session.get("media");
        for (const [tab, list] of Object.entries(s.media || {})) {
          const id = +tab;
          media.set(id, new Map([...(media.get(id) || new Map()), ...list.items.map((i) => [i.url, i])]));
          children.set(id, new Set([...(children.get(id) || []), ...list.kids]));
        }
      } catch {}
    })();
  }
  return loaded;
}

function saveState() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    const out = {};
    for (const [tab, byUrl] of media) out[tab] = { items: [...byUrl.values()], kids: [...(children.get(tab) || [])] };
    try {
      api.storage.session.set({ media: out });
    } catch {}
  }, 300);
}

function forgetTab(tabId) {
  media.delete(tabId);
  children.delete(tabId);
  saveState();
}

function headerValue(headers, name) {
  const h = (headers || []).find((x) => x.name.toLowerCase() === name);
  return h ? h.value : "";
}

function resolve(base, rel) {
  try {
    return new URL(rel, base).href.split("#")[0];
  } catch {
    return "";
  }
}

// Fetch a playlist/manifest and describe it: qualities, duration, referenced child playlists.
async function inspectManifest(url, dash) {
  const out = { qualities: [], duration: 0, kids: [], master: false };
  let text = "";
  try {
    const r = await fetch(url, { credentials: "include" });
    if (!r.ok) return out;
    text = await r.text();
  } catch {
    return out;
  }
  if (dash) {
    const heights = new Set();
    for (const m of text.matchAll(/<Representation\b[^>]*\bheight="(\d+)"/g)) heights.add(+m[1]);
    if (!heights.size) for (const m of text.matchAll(/<AdaptationSet\b[^>]*\bmaxHeight="(\d+)"/g)) heights.add(+m[1]);
    out.qualities = [...heights].sort((a, b) => b - a);
    const d = /mediaPresentationDuration="PT(?:(\d+(?:\.\d+)?)H)?(?:(\d+(?:\.\d+)?)M)?(?:(\d+(?:\.\d+)?)S)?"/.exec(text);
    if (d) out.duration = (+d[1] || 0) * 3600 + (+d[2] || 0) * 60 + (+d[3] || 0);
    out.master = true;
    return out;
  }
  const lines = text.split(/\r?\n/).map((l) => l.trim());
  if (lines.some((l) => l.startsWith("#EXT-X-STREAM-INF"))) {
    out.master = true;
    const heights = new Set();
    for (let i = 0; i < lines.length; i++) {
      const l = lines[i];
      if (l.startsWith("#EXT-X-STREAM-INF")) {
        const res = /RESOLUTION=\d+x(\d+)/i.exec(l);
        if (res) heights.add(+res[1]);
        const next = lines.slice(i + 1).find((x) => x && !x.startsWith("#"));
        if (next) out.kids.push(resolve(url, next));
      } else if (l.startsWith("#EXT-X-MEDIA") || l.startsWith("#EXT-X-I-FRAME-STREAM-INF")) {
        const uri = /URI="([^"]+)"/.exec(l);
        if (uri) out.kids.push(resolve(url, uri[1]));
      }
    }
    out.qualities = [...heights].sort((a, b) => b - a);
  } else {
    for (const l of lines) {
      const m = /^#EXTINF:([\d.]+)/.exec(l);
      if (m) out.duration += +m[1];
    }
  }
  return out;
}

function notifyTab(tabId) {
  const n = media.get(tabId)?.size || 0;
  try {
    api.tabs.sendMessage(tabId, { type: "rdm-media", count: n }, () => void api.runtime.lastError);
  } catch {}
}

function addMedia(tabId, info) {
  let byUrl = media.get(tabId);
  if (!byUrl) media.set(tabId, (byUrl = new Map()));
  byUrl.set(info.url, info);
  saveState();
  updateBadge(tabId);
  notifyTab(tabId);
}

// Classify one response (from webRequest or from the page hooks) and remember it.
async function consider(tabId, url, type, size, total) {
  if (tabId == null || tabId < 0) return;
  await loadState();
  const key = url.split("#")[0];
  if (media.get(tabId)?.has(key) || children.get(tabId)?.has(key)) return;
  const ext = extensionOf(url);
  const isManifest = MANIFEST_EXT.test(url) || /mpegurl|dash\+xml/i.test(type);
  const isSub = SUB_TYPES.test(type) || SUB_EXT.includes(ext);
  const isMedia = MEDIA_TYPES.test(type) || settings.media_extensions.includes(ext);
  if (isSub) {
    addMedia(tabId, { url: key, kind: "subtitle", type: type || ext, size: total || size, found: Date.now() });
    return;
  }
  if (!isMedia && !isManifest) return;
  if (!isManifest) {
    // Skip fragments of segmented streams and tiny files; keep the playlist/manifest instead.
    if (SEGMENT_EXT.test(url) || /[?&](range|bytes)=/i.test(url)) return;
    if (size > 0 && size < 256 * 1024 && !(total > size)) return;
    addMedia(tabId, { url: key, kind: "file", type: type || ext.toUpperCase(), size: total || size, found: Date.now() });
    return;
  }
  const dash = /\.mpd(\?|$)/i.test(url) || /dash\+xml/i.test(type);
  const info = await inspectManifest(key, dash);
  if (children.get(tabId)?.has(key)) return;
  if (info.kids.length) {
    let kids = children.get(tabId);
    if (!kids) children.set(tabId, (kids = new Set()));
    for (const k of info.kids) {
      kids.add(k);
      media.get(tabId)?.delete(k); // a variant playlist seen before its master
    }
  }
  addMedia(tabId, {
    url: key,
    kind: "stream",
    type: dash ? "DASH stream" : "HLS stream",
    qualities: info.qualities,
    duration: Math.round(info.duration),
    size: 0,
    found: Date.now(),
  });
}

api.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (d.tabId < 0 || d.statusCode >= 400) return;
    const type = headerValue(d.responseHeaders, "content-type").split(";")[0].trim();
    const size = parseInt(headerValue(d.responseHeaders, "content-length") || "0", 10);
    const range = headerValue(d.responseHeaders, "content-range");
    const total = range ? parseInt(range.split("/")[1], 10) || 0 : 0;
    consider(d.tabId, d.url, type, size, total);
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "object"] },
  ["responseHeaders"]
);

api.tabs.onUpdated.addListener(async (tabId, info) => {
  if (info.status === "loading" && info.url) {
    await loadState();
    forgetTab(tabId);
    updateBadge(tabId);
  }
});
api.tabs.onRemoved.addListener(async (tabId) => {
  await loadState();
  forgetTab(tabId);
});

function updateBadge(tabId) {
  if (typeof tabId !== "number") return;
  const n = media.get(tabId)?.size || 0;
  try {
    api.action.setBadgeText({ tabId, text: n ? String(n) : "" });
    api.action.setBadgeBackgroundColor({ tabId, color: "#5b7cff" });
  } catch {}
}

function mediaList(tabId) {
  const rank = { stream: 0, file: 1, subtitle: 2 };
  return [...(media.get(tabId)?.values() || [])].sort((a, b) => rank[a.kind] - rank[b.kind] || b.found - a.found);
}

function safeName(title, ext) {
  const t = (title || "").replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_").replace(/\s+/g, " ").trim().slice(0, 150);
  return t ? `${t}.${ext}` : undefined;
}

async function downloadMedia(msg, tab) {
  const quality = msg.quality ? `#rdm-quality=${msg.quality}` : "";
  const isStream = /\.(m3u8|mpd)(\?|$)/i.test(msg.url) || msg.kind === "stream";
  const title = msg.title || tab?.title;
  let filename = msg.filename;
  if (!filename && isStream) filename = safeName(title, "mp4");
  if (!filename && msg.kind === "subtitle") filename = safeName(title, extensionOf(msg.url) || "vtt");
  return send({
    type: "download",
    ...(await payload(msg.url + quality, { referrer: msg.referrer || tab?.url || "", filename })),
  });
}

// ------------------------------------------------------------ context menus

function createMenus() {
  api.contextMenus.removeAll(() => {
    api.contextMenus.create({ id: "rdm-link", title: "Download with RDM", contexts: ["link"] });
    api.contextMenus.create({ id: "rdm-media", title: "Download media with RDM", contexts: ["video", "audio", "image"] });
    api.contextMenus.create({ id: "rdm-selection", title: "Download selected links with RDM", contexts: ["selection"] });
    api.contextMenus.create({ id: "rdm-all", title: "Download all links with RDM", contexts: ["page", "frame"] });
  });
}

// Runs in the page: links in the current selection (or the whole page), with their text.
function collectLinks(selectionOnly) {
  let anchors = [...document.querySelectorAll("a[href]")];
  if (selectionOnly) {
    const sel = window.getSelection();
    anchors = sel && sel.rangeCount ? anchors.filter((a) => sel.containsNode(a, true)) : [];
  }
  const seen = new Set();
  const out = [];
  for (const a of anchors) {
    if (!/^(https?|ftp):/i.test(a.href) || seen.has(a.href)) continue;
    seen.add(a.href);
    out.push({ url: a.href, text: (a.textContent || "").trim().slice(0, 200) });
  }
  return out;
}

api.contextMenus.onClicked.addListener(async (info, tab) => {
  const referrer = info.frameUrl || info.pageUrl || (tab && tab.url) || "";
  if (info.menuItemId === "rdm-link" && info.linkUrl) {
    await send({ type: "download", ...(await payload(info.linkUrl, { referrer, title: info.selectionText || info.linkText })) });
  } else if (info.menuItemId === "rdm-media" && info.srcUrl) {
    await send({ type: "download", ...(await payload(info.srcUrl, { referrer })) });
  } else if ((info.menuItemId === "rdm-all" || info.menuItemId === "rdm-selection") && tab && tab.id != null) {
    const target = info.frameId ? { tabId: tab.id, frameIds: [info.frameId] } : { tabId: tab.id };
    const [res] = await api.scripting.executeScript({ target, func: collectLinks, args: [info.menuItemId === "rdm-selection"] });
    const links = [];
    for (const l of res?.result || []) links.push({ ...(await payload(l.url, { referrer })), title: l.text });
    if (links.length) await send({ type: "links", links });
  }
});

// ------------------------------------------------------------ messages (popup + content scripts)

async function buttonSettings() {
  const { show_button = true, blocked_hosts = [] } = await api.storage.local.get(["show_button", "blocked_hosts"]);
  return {
    show_button,
    blocked_hosts,
    show_video_button: settings.show_video_button,
    show_audio_button: settings.show_audio_button,
    no_button_hosts: settings.no_button_hosts,
  };
}

api.runtime.onMessage.addListener((msg, sender, respond) => {
  (async () => {
    await loadState();
    switch (msg.type) {
      case "state": {
        if (!connected) await refreshSettings();
        const b = await buttonSettings();
        respond({ connected, enabled: settings.enabled, show_button: b.show_button, blocked_hosts: b.blocked_hosts, media: mediaList(msg.tabId) });
        break;
      }
      case "page-media":
        await freshSettings();
        respond({ connected, media: mediaList(sender.tab?.id) });
        break;
      case "button-settings":
        if (!lastRefresh) await refreshSettings();
        respond({ ...(await buttonSettings()), count: media.get(sender.tab?.id)?.size || 0 });
        break;
      case "show-button":
        await api.storage.local.set({ show_button: msg.enabled });
        broadcast({ type: "rdm-settings" });
        respond({ ok: true });
        break;
      case "block-host": {
        const { blocked_hosts = [] } = await api.storage.local.get("blocked_hosts");
        if (msg.host && !blocked_hosts.includes(msg.host)) blocked_hosts.push(msg.host);
        await api.storage.local.set({ blocked_hosts });
        respond({ ok: true });
        break;
      }
      case "unblock-host": {
        const { blocked_hosts = [] } = await api.storage.local.get("blocked_hosts");
        await api.storage.local.set({ blocked_hosts: blocked_hosts.filter((h) => h !== msg.host) });
        broadcast({ type: "rdm-settings" });
        respond({ ok: true });
        break;
      }
      case "keys":
        lastKeys = { keys: msg.keys || [], t: Date.now() };
        respond({ ok: true });
        break;
      case "page-request":
        consider(sender.tab?.id, msg.url, msg.mime || "", msg.size || 0, 0);
        respond({ ok: true });
        break;
      case "toggle":
        settings.enabled = msg.enabled;
        await api.storage.local.set({ enabled: msg.enabled });
        respond({ ok: true });
        break;
      case "download-media": {
        let tab = sender.tab;
        if (!tab && msg.tabId != null) tab = await api.tabs.get(msg.tabId).catch(() => undefined);
        respond(await downloadMedia(msg, tab));
        break;
      }
      case "show":
        respond(await send({ type: "show" }));
        break;
      case "add-url":
        respond(await send({ type: "download", ...(await payload(msg.url, {})) }));
        break;
      default:
        respond(null);
    }
  })();
  return true;
});

api.runtime.onInstalled.addListener(() => {
  createMenus();
  refreshSettings();
});
api.runtime.onStartup.addListener(() => {
  createMenus();
  refreshSettings();
});
refreshSettings();
