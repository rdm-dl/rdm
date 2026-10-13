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

// True once the app's lists are known (from the app, or remembered from an earlier session),
// so downloads can be captured even while RDM is closed: the capture itself starts it.
let settingsKnown = false;

async function refreshSettings() {
  lastRefresh = Date.now();
  const stored = await api.storage.local.get(["enabled", "captureSettings"]);
  // Only checks whether RDM is running; the bridge doesn't start it for this.
  const reply = await send({ type: "hello" });
  const before = JSON.stringify(settings);
  if (reply.type === "settings" && reply.capture) {
    settings = { ...settings, ...reply.capture };
    settingsKnown = true;
    api.storage.local.set({ captureSettings: reply.capture });
  } else if (stored.captureSettings) {
    settings = { ...settings, ...stored.captureSettings };
    settingsKnown = true;
  }
  if (typeof stored.enabled === "boolean") settings.enabled = stored.enabled;
  if (JSON.stringify(settings) !== before) broadcast({ type: "rdm-settings" });
  return reply;
}

async function freshSettings() {
  const age = Date.now() - lastRefresh;
  if (age > 60 * 1000 || (!connected && age > 10 * 1000)) await refreshSettings();
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
  if (!settings.enabled || !settingsKnown) return false;
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
        const s = await api.storage.session.get(["media", "yt", "diag"]);
        if (Array.isArray(s.diag)) diagLog.unshift(...s.diag.slice(-300));
        for (const [tab, list] of Object.entries(s.yt || {})) {
          const videos = ytVideos(+tab);
          for (const v of list) if (!videos.has(v.videoId)) videos.set(v.videoId, v);
        }
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
    const ytOut = {};
    for (const [tab, videos] of yt) ytOut[tab] = [...videos.values()].sort((a, b) => b.found - a.found).slice(0, 5);
    try {
      api.storage.session.set({ media: out, yt: ytOut, diag: diagLog.slice(-300) });
    } catch {}
  }, 300);
}

function forgetTab(tabId) {
  media.delete(tabId);
  children.delete(tabId);
  yt.delete(tabId);
  ytPending.delete(tabId);
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
  const n = mediaCount(tabId);
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

// The Referer / Origin the browser actually sent for each media request. Players embedded
// from another site send their own origin, and stream servers often insist on it.
const sentHeaders = new Map(); // url -> { referer, origin, t }

function onSendHeaders(d) {
  if (d.tabId < 0) return;
  const h = d.requestHeaders || [];
  const referer = headerValue(h, "referer");
  const origin = headerValue(h, "origin");
  if (!referer && !origin) return;
  sentHeaders.set(d.url.split("#")[0], { referer, origin, t: Date.now() });
  if (sentHeaders.size > 2000) {
    for (const [u, v] of sentHeaders) if (Date.now() - v.t > 10 * 60 * 1000) sentHeaders.delete(u);
  }
}
try {
  api.webRequest.onSendHeaders.addListener(onSendHeaders, { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "object"] }, ["requestHeaders", "extraHeaders"]);
} catch {
  // Firefox has no "extraHeaders" (and doesn't hide these headers).
  api.webRequest.onSendHeaders.addListener(onSendHeaders, { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "object"] }, ["requestHeaders"]);
}

function requestContext(url) {
  const h = sentHeaders.get(url.split("#")[0]);
  return h ? { referer: h.referer || "", origin: h.origin || "" } : {};
}

// Classify one response (from webRequest or from the page hooks) and remember it.
async function consider(tabId, url, type, size, total) {
  if (tabId == null || tabId < 0) return;
  if (/\.googlevideo\.com\//i.test(url)) return;
  await loadState();
  const key = url.split("#")[0];
  const ctx = requestContext(key);
  if (media.get(tabId)?.has(key) || children.get(tabId)?.has(key)) return;
  const ext = extensionOf(url);
  const isManifest = MANIFEST_EXT.test(url) || /mpegurl|dash\+xml/i.test(type);
  const isSub = SUB_TYPES.test(type) || SUB_EXT.includes(ext);
  const isMedia = MEDIA_TYPES.test(type) || settings.media_extensions.includes(ext);
  if (isSub) {
    addMedia(tabId, { url: key, kind: "subtitle", type: type || ext, size: total || size, found: Date.now(), ...ctx });
    return;
  }
  if (!isMedia && !isManifest) return;
  if (!isManifest) {
    // Skip fragments of segmented streams and tiny files; keep the playlist/manifest instead.
    if (SEGMENT_EXT.test(url) || /[?&](range|bytes)=/i.test(url)) return;
    if (size > 0 && size < 256 * 1024 && !(total > size)) return;
    addMedia(tabId, { url: key, kind: "file", type: type || ext.toUpperCase(), size: total || size, found: Date.now(), ...ctx });
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
    ...ctx,
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
  const n = mediaCount(tabId);
  try {
    api.action.setBadgeText({ tabId, text: n ? String(n) : "" });
    api.action.setBadgeBackgroundColor({ tabId, color: "#2a86ff" });
  } catch {}
}

function mediaList(tabId, pageUrl) {
  const rank = { stream: 0, file: 1, subtitle: 2 };
  const own = [...(media.get(tabId)?.values() || [])].sort((a, b) => rank[a.kind] - rank[b.kind] || b.found - a.found);
  return [...ytList(tabId, pageUrl), ...own];
}

function mediaCount(tabId) {
  return (media.get(tabId)?.size || 0) + ytList(tabId).filter((e) => e.kind === "yt").length;
}

function safeName(title, ext) {
  const t = (title || "").replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_").replace(/\s+/g, " ").trim().slice(0, 150);
  return t ? `${t}.${ext}` : undefined;
}

function pairUrl(video, audio) {
  return `${video.split("#")[0]}#rdm-audio=${encodeURIComponent(audio)}`;
}

async function downloadMedia(msg, tab) {
  if (msg.kind === "yt") {
    // RDM makes its own player request with this page's session (the way the page does),
    // unlocks the links with the player script, and downloads video + audio.
    const v = tab && tab.id != null ? yt.get(tab.id)?.get(msg.videoId) : null;
    const job = {
      video_id: msg.videoId,
      itag_video: msg.itagV || 0,
      itag_audio: msg.itagA || 0,
      size: msg.size || 0,
      title: msg.title || (v && v.title) || "",
      visitor_data: (v && v.visitorData) || "",
      player_js_url: (v && v.jsUrl) || "",
      client_version: (v && v.clientVersion) || "",
      session_index: (v && v.sessionIndex) || "",
      logged_in: !!(v && v.loggedIn),
      pot: (v && v.pot) || (tab && tab.id != null && tabPot.get(tab.id)) || "",
    };
    const watch = `https://www.youtube.com/watch?v=${msg.videoId}#rdm-yt`;
    return send({
      type: "download",
      ...(await payload(watch, { referrer: tab?.url || "", filename: safeName(job.title || tab?.title, msg.ext || "mp4"), postData: JSON.stringify(job) })),
    });
  }
  const isStream = /\.(m3u8|mpd)(\?|$)/i.test(msg.url) || msg.kind === "stream";
  // Tell RDM it is a stream (playlist URLs often have no .m3u8/.mpd) and which quality.
  const known = tab && tab.id != null ? media.get(tab.id)?.get(msg.url.split("#")[0]) : null;
  const kind = isStream ? (/dash/i.test((known && known.type) || "") || /\.mpd(\?|$)/i.test(msg.url) ? "dash" : "hls") : "";
  const hints = [msg.quality ? `rdm-quality=${msg.quality}` : "", kind ? `rdm-kind=${kind}` : ""].filter(Boolean).join("&");
  const quality = hints ? `#${hints}` : "";
  const title = msg.title || tab?.title;
  let filename = msg.filename;
  if (!filename && isStream) filename = safeName(title, "mp4");
  if (!filename && msg.kind === "subtitle") filename = safeName(title, extensionOf(msg.url) || "vtt");
  // Send what the browser sent: the embedded player's Referer and Origin, not the page's.
  const key = msg.url.split("#")[0];
  const item = (tab && tab.id != null && media.get(tab.id)?.get(key)) || {};
  const ctx = { ...requestContext(key), ...Object.fromEntries(Object.entries({ referer: item.referer, origin: item.origin }).filter(([, v]) => v)) };
  const headers = ctx.origin ? { Origin: ctx.origin } : undefined;
  return send({
    type: "download",
    ...(await payload(msg.url + quality, { referrer: ctx.referer || msg.referrer || tab?.url || "", filename, headers })),
  });
}

// ------------------------------------------------------------ site players (YouTube)
//
// The page script (document.js) reports the player's own data: every format with its
// itag, size, quality and (when the player has it) its URL. Stream URLs need the player's
// own unlocking (the `n` / signature parameters); instead of re-implementing that, the
// requests the player itself makes to /videoplayback are recorded here: they carry the
// unlocked parameters. From one unlocked request the shared `n` value maps every other
// format URL of the same response; signature-protected formats are usable once the
// player has requested them (switch quality in the player to add more).

const yt = new Map(); // tabId -> Map(videoId -> info)
const diagLog = []; // recent events, for diagnostics

// Diagnostics are off unless switched on for troubleshooting:
// chrome.storage.local.set({ diagnostics: true }) in the extension's service worker console.
let diagnosticsOn = false;
api.storage.local.get("diagnostics").then((s) => (diagnosticsOn = s.diagnostics === true), () => {});
api.storage.onChanged.addListener((c) => {
  if (c.diagnostics) diagnosticsOn = c.diagnostics.newValue === true;
});

function diag(event, detail) {
  if (!diagnosticsOn) return;
  diagLog.push({ t: new Date().toISOString().slice(11, 23), event, ...detail });
  if (diagLog.length > 300) diagLog.splice(0, diagLog.length - 300);
  scheduleDiag();
  saveState();
}

let diagTimer = 0;
function scheduleDiag() {
  clearTimeout(diagTimer);
  diagTimer = setTimeout(writeDiag, 1500);
}

async function writeDiag() {
  await loadState();
  const tabs = {};
  for (const [tabId, videos] of yt) {
    tabs[tabId] = [...videos.values()].map((v) => ({
      videoId: v.videoId,
      title: v.title,
      formats: v.formats.length,
      withUrl: v.formats.filter((f) => f.url).length,
      withCipher: v.formats.filter((f) => f.cipher).length,
      sabr: v.sabr,
      sabrSeen: v.sabrSeen,
      hls: !!v.hls,
      observed: Object.keys(v.observed),
      nMapped: Object.keys(v.nMap).length,
      pot: !!v.pot,
      itags: v.formats.map((f) => `${f.itag}:${f.mime.split(";")[0]}:${f.h || ""}${f.url ? ":url" : ""}${f.cipher ? ":cipher" : ""}`),
      entries: ytEntries(v).map((e) => e.kind === "yt" ? `${e.label} ${e.container}` : `note: ${e.text}`),
      sabrUrlKeys: v.sabrUrl ? [...new URL(v.sabrUrl).searchParams.keys()].join(",") : "",
      sabrReq: v.sabrReq ? { url: v.sabrReq.url, body: v.sabrReq.body } : null,
      ustreamer: v.ustreamer || "",
      clientVersion: v.clientVersion || "",
      formatsFull: v.formats.map((f) => ({ itag: f.itag, mime: f.mime, h: f.h, fps: f.fps, clen: f.clen, lmt: f.lmt, xtags: f.xtags, durMs: f.durMs, audioId: f.audioId, audioDefault: f.audioDefault })),
    }));
  }
  const pending = {};
  for (const [tabId, list] of ytPending) pending[tabId] = list.length;
  await send({
    type: "diag",
    data: { version: api.runtime.getManifest().version, at: new Date().toISOString(), connected, tabs, pending, sabrSamples, media: Object.fromEntries([...media].map(([k, v]) => [k, [...v.keys()]])), log: diagLog },
  });
}

const YT_DROP = ["range", "rn", "rbuf", "alr", "ump", "srfvp", "cpn"];

function ytVideos(tabId) {
  let m = yt.get(tabId);
  if (!m) yt.set(tabId, (m = new Map()));
  return m;
}

function cleanStreamUrl(u) {
  try {
    const url = new URL(u);
    for (const k of YT_DROP) url.searchParams.delete(k);
    url.hash = "";
    return url.href;
  } catch {
    return "";
  }
}

const sabrSamples = []; // first response samples, for diagnostics

// The player's latest streaming request: its URL carries the unlocked n/sig values, its
// body the session (PO token, playback cookie). Kept per video as the template to replay.
function ytOnSabr(tabId, data) {
  if (tabId == null || tabId < 0 || !data || !data.url) return;
  const videos = yt.get(tabId);
  if (!videos || !videos.size) return;
  const v = [...videos.values()].sort((a, b) => b.found - a.found)[0];
  const first = !v.sabrReq;
  v.sabrReq = { url: data.url, body: data.body, t: Date.now() };
  v.sabrSeen = true;
  try {
    const n = new URL(data.url).searchParams.get("n");
    const orig = v.sabrUrl ? new URL(v.sabrUrl).searchParams.get("n") : "";
    if (n && orig && n !== orig) v.nMap[orig] = n;
  } catch {}
  diag("sabr-request", { tabId, videoId: v.videoId, bodyBytes: Math.round(((data.body || "").length * 3) / 4) });
  saveState();
  if (first) notifyTab(tabId);
}

function ytOnPlayer(tabId, data) {
  diag("player-data", { tabId, videoId: data && data.videoId, formats: data && data.formats && data.formats.length, sabr: data && data.sabr });
  if (tabId == null || tabId < 0 || !data || !data.videoId || !Array.isArray(data.formats)) return;
  const videos = ytVideos(tabId);
  const prev = videos.get(data.videoId);
  const info = {
    ...data,
    observed: prev ? prev.observed : {}, // itag -> unlocked URL
    nMap: prev ? prev.nMap : {}, // original n -> unlocked n
    pot: prev ? prev.pot : "",
    sabrSeen: prev ? prev.sabrSeen : false,
    sabrReq: prev ? prev.sabrReq : null,
    found: Date.now(),
  };
  videos.set(data.videoId, info);
  for (const r of ytPending.get(tabId) || []) ytOnRequest(r, true);
  saveState();
  if (data.hls && !data.live) consider(tabId, data.hls, "application/x-mpegurl", 0, 0);
  updateBadge(tabId);
  notifyTab(tabId);
}

const tabPot = new Map(); // tabId -> latest token from that tab's player
const ytPending = new Map(); // tabId -> recent requests not matched yet

function ytOnRequest(d, replay) {
  if (d.tabId < 0) return;
  const videos = yt.get(d.tabId);
  let u;
  try {
    u = new URL(d.url);
  } catch {
    return;
  }
  const q = u.searchParams;
  const itag = +q.get("itag");
  const clen = +q.get("clen") || 0;
  if (!replay) diag("videoplayback", { tabId: d.tabId, method: d.method, type: d.type, itag, clen, sabr: q.get("sabr"), ump: q.get("ump"), keys: [...q.keys()].join(",").slice(0, 300) });
  // Which video: match the content length against the formats the player listed.
  let target = null;
  if (videos) {
    for (const v of videos.values()) {
      if (v.formats.some((f) => f.itag === itag && (!clen || !f.clen || f.clen === clen))) {
        if (!target || v.found > target.found) target = v;
      }
    }
  }
  // The player's streaming request (POST, sabr=1): its body carries the session's
  // proof-of-origin token, which RDM adds to the stream links it builds.
  if (d.method === "POST" && q.get("sabr") === "1") {
    const latest = videos && videos.size ? [...videos.values()].sort((a, b) => b.found - a.found)[0] : null;
    const raw = d.requestBody && d.requestBody.raw && d.requestBody.raw[0] && d.requestBody.raw[0].bytes;
    let pot = "";
    let shape = "";
    if (raw) {
      const bytes = new Uint8Array(raw);
      const top = protoFields(bytes);
      shape = top.map((f) => `${f.field}:${f.bytes ? f.bytes.length : f.value}`).join(",").slice(0, 300);
      const ctx = top.find((f) => f.field === 19 && f.bytes);
      const tok = ctx && protoFields(ctx.bytes).find((f) => f.field === 2 && f.bytes && f.bytes.length >= 16);
      if (tok) pot = b64url(tok.bytes);
    }
    diag("sabr-body", { tabId: d.tabId, videoId: latest && latest.videoId, bodyBytes: raw ? raw.byteLength : 0, shape, pot: pot ? `${pot.length} chars` : "" });
    if (pot) tabPot.set(d.tabId, pot);
    if (latest) {
      latest.sabrSeen = true;
      if (pot) {
        const fresh = latest.pot !== pot;
        latest.pot = pot;
        latest.potAt = Date.now();
        if (fresh) saveState();
      }
    }
    return;
  }
  if (!target) {
    if (!replay) {
      const list = ytPending.get(d.tabId) || [];
      list.push({ tabId: d.tabId, url: d.url, method: d.method, t: Date.now() });
      ytPending.set(d.tabId, list.filter((x) => Date.now() - x.t < 60000).slice(-60));
    }
    return;
  }
  if (d.method !== "GET" || !itag) {
    target.sabrSeen = true;
    return;
  }
  const clean = cleanStreamUrl(d.url);
  if (!clean) return;
  const fresh = !target.observed[itag];
  target.observed[itag] = clean;
  const f = target.formats.find((x) => x.itag === itag);
  const n = q.get("n");
  if (f && f.url && n) {
    try {
      const orig = new URL(f.url).searchParams.get("n");
      if (orig && orig !== n) target.nMap[orig] = n;
    } catch {}
  }
  if (q.get("pot")) target.pot = q.get("pot");
  if (fresh) {
    saveState();
    notifyTab(d.tabId);
  }
}

api.webRequest.onBeforeRequest.addListener((d) => ytOnRequest(d, false), { urls: ["*://*.googlevideo.com/videoplayback*"] }, ["requestBody"]);

// Minimal protobuf reader: top-level fields of a message (varints and length-delimited).
function protoFields(b) {
  const out = [];
  let i = 0;
  const varint = () => {
    let v = 0;
    let mul = 1;
    for (let k = 0; k < 10 && i < b.length; k++) {
      const x = b[i++];
      v += (x & 0x7f) * mul;
      mul *= 128;
      if (!(x & 0x80)) return v;
    }
    return v;
  };
  while (i < b.length) {
    const key = varint();
    const field = Math.floor(key / 8);
    const wire = key % 8;
    if (wire === 0) out.push({ field, value: varint() });
    else if (wire === 2) {
      const len = varint();
      if (i + len > b.length) break;
      out.push({ field, bytes: b.subarray(i, i + len) });
      i += len;
    } else if (wire === 1) i += 8;
    else if (wire === 5) i += 4;
    else break;
  }
  return out;
}

function b64url(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

// Unlocked URL for a format, if one can be built.
function ytUrl(v, f) {
  if (v.observed[f.itag]) return v.observed[f.itag];
  if (!f.url || f.drm) return "";
  try {
    const url = new URL(f.url);
    const n = url.searchParams.get("n");
    if (n) {
      if (!v.nMap[n]) return ""; // the player hasn't unlocked this response yet
      url.searchParams.set("n", v.nMap[n]);
    } else if (!Object.keys(v.observed).length) {
      return "";
    }
    if (v.pot && !url.searchParams.get("pot")) url.searchParams.set("pot", v.pot);
    return url.href;
  } catch {
    return "";
  }
}

const isVideoOnly = (f) => f.mime.startsWith("video/") && !/mp4a|opus|vorbis|ac-3|ec-3/i.test(f.mime);
const isAudioOnly = (f) => f.mime.startsWith("audio/");
const isMuxed = (f) => f.mime.startsWith("video/") && /mp4a|opus|vorbis/i.test(f.mime);
const containerOf = (f) => (/webm/i.test(f.mime) ? "webm" : "mp4");

// Every quality the page lists. RDM itself unlocks the stream links (own player request +
// the player script's signature / n functions), so no URL is needed here.
function ytEntries(v) {
  const fmts = v.formats.filter((f) => !f.drm && f.itag);
  const videos = fmts.filter(isVideoOnly);
  const audios = fmts.filter(isAudioOnly);
  const bestAudio = (container) =>
    audios.filter((a) => containerOf(a) === container).sort((x, y) => (y.audioDefault - x.audioDefault) || y.bitrate - x.bitrate)[0];
  const base = { kind: "yt", videoId: v.videoId, title: v.title, duration: v.duration, found: v.found };
  const out = [];
  const codec = (f) => (/avc1/i.test(f.mime) ? "H.264" : /av01/i.test(f.mime) ? "AV1" : /vp0?9/i.test(f.mime) ? "VP9" : "");
  for (const h of [...new Set(videos.map((f) => f.h))].sort((a, b) => b - a)) {
    const at = videos.filter((f) => f.h === h).sort((x, y) => y.fps - x.fps || y.bitrate - x.bitrate);
    // Each format offered at this quality: MP4 (H.264 preferred — plays everywhere — else
    // AV1) and WebM (VP9), each with the matching audio.
    const mp4 = at.find((f) => containerOf(f) === "mp4" && codec(f) === "H.264") || at.find((f) => containerOf(f) === "mp4");
    const webm = at.find((f) => containerOf(f) === "webm");
    for (const vid of [mp4, webm].filter(Boolean)) {
      const container = containerOf(vid);
      const aud = bestAudio(container) || bestAudio(container === "mp4" ? "webm" : "mp4");
      if (!aud) continue;
      const kind = [container.toUpperCase(), codec(vid)].filter(Boolean).join(" · ");
      out.push({ ...base, label: vid.label || `${h}p`, container: kind, ext: containerOf(aud) === container ? container : "mkv", fps: vid.fps, size: vid.clen + aud.clen, itagV: vid.itag, itagA: aud.itag });
    }
  }
  for (const a of [bestAudio("mp4"), bestAudio("webm")].filter(Boolean)) {
    const mp4 = containerOf(a) === "mp4";
    out.push({ ...base, label: "Audio", container: mp4 ? "M4A · AAC" : "WEBM · Opus", ext: mp4 ? "m4a" : "webm", fps: 0, size: a.clen, itagV: 0, itagA: a.itag });
  }
  if (!out.length) out.push({ kind: "yt-note", title: v.title, text: "Play the video for a few seconds, then open this menu again." });
  return out;
}

function ytEntriesFromLinks(v) {
  const usable = v.formats.filter((f) => !f.drm).map((f) => ({ ...f, live: ytUrl(v, f) }));
  const audios = usable.filter((f) => isAudioOnly(f) && f.live);
  const bestAudio = (container) =>
    audios
      .filter((a) => containerOf(a) === container)
      .sort((x, y) => (y.audioDefault - x.audioDefault) || y.bitrate - x.bitrate)[0];
  const out = [];
  const heights = [...new Set(usable.filter((f) => isVideoOnly(f) && f.live).map((f) => f.h))].sort((a, b) => b - a);
  for (const h of heights) {
    const at = usable.filter((f) => isVideoOnly(f) && f.live && f.h === h).sort((x, y) => y.fps - x.fps || y.bitrate - x.bitrate);
    // Prefer MP4 (plays everywhere); WebM when that's all there is at this height.
    for (const container of ["mp4", "webm"]) {
      const vid = at.find((f) => containerOf(f) === container);
      const aud = vid && bestAudio(container);
      if (vid && aud) {
        out.push({
          kind: "yt",
          label: vid.label || `${h}p`,
          container: container.toUpperCase(),
          ext: container,
          fps: vid.fps,
          size: vid.clen + aud.clen,
          duration: v.duration,
          video: vid.live,
          audio: aud.live,
          title: v.title,
          found: v.found,
        });
        break;
      }
    }
  }
  for (const f of usable.filter((f) => isMuxed(f) && f.live)) {
    if (!out.some((e) => e.label === (f.label || `${f.h}p`))) {
      out.push({ kind: "yt", label: f.label || `${f.h}p`, container: containerOf(f).toUpperCase(), ext: containerOf(f), fps: f.fps, size: f.clen, duration: v.duration, video: f.live, audio: "", title: v.title, found: v.found });
    }
  }
  const a = bestAudio("mp4") || bestAudio("webm");
  if (a) {
    out.push({ kind: "yt", label: "Audio", container: containerOf(a) === "mp4" ? "M4A" : "WEBM", ext: containerOf(a) === "mp4" ? "m4a" : "webm", fps: 0, size: a.clen, duration: v.duration, video: a.live, audio: "", title: v.title, found: v.found });
  }
  const locked = usable.filter((f) => (isVideoOnly(f) || isMuxed(f)) && !f.live).length;
  if (!out.length) {
    out.push({
      kind: "yt-note",
      title: v.title,
      text: v.sabrSeen
        ? "The player is streaming in a mode RDM can't read yet. Try switching quality in the player, then open this menu again."
        : "Play the video for a few seconds, then open this menu again.",
    });
  } else if (locked) {
    out.push({ kind: "yt-note", title: v.title, text: "Want a quality that isn't listed? Pick it in the player's settings, let it play a moment, then reopen this menu." });
  }
  return out;
}

function videoIdOf(pageUrl) {
  try {
    const u = new URL(pageUrl);
    return u.searchParams.get("v") || (/^\/(?:shorts|embed|live|v)\/([\w-]{11})/.exec(u.pathname) || [])[1] || "";
  } catch {
    return "";
  }
}

function ytList(tabId, pageUrl) {
  const videos = yt.get(tabId);
  if (!videos || !videos.size) return [];
  const id = pageUrl ? videoIdOf(pageUrl) : "";
  const v = (id && videos.get(id)) || [...videos.values()].sort((a, b) => b.found - a.found)[0];
  return v ? ytEntries(v) : [];
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
    const ctx = requestContext(info.srcUrl);
    await send({ type: "download", ...(await payload(info.srcUrl, { referrer: ctx.referer || referrer, headers: ctx.origin ? { Origin: ctx.origin } : undefined })) });
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
        respond({ connected, enabled: settings.enabled, show_button: b.show_button, blocked_hosts: b.blocked_hosts, media: mediaList(msg.tabId, msg.pageUrl) });
        break;
      }
      case "content-hello":
        diag("content", { tabId: sender.tab?.id, frame: sender.frameId, url: (msg.url || "").slice(0, 120), docHook: msg.docHook });
        respond({ ok: true });
        break;
      case "page-media":
        diag("menu-open", { tabId: sender.tab?.id, pageUrl: (msg.pageUrl || "").slice(0, 120) });
        await freshSettings();
        respond({ connected, media: mediaList(sender.tab?.id, msg.pageUrl || sender.tab?.url) });
        break;
      case "button-settings":
        if (!lastRefresh) await refreshSettings();
        respond({ ...(await buttonSettings()), count: mediaCount(sender.tab?.id) });
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
      case "yt-sabr":
        ytOnSabr(sender.tab?.id, msg.data);
        respond({ ok: true });
        break;
      case "yt-sabr-sample":
        if (sabrSamples.length < 3) sabrSamples.push({ ...msg.data, tabId: sender.tab?.id });
        diag("sabr-sample", { tabId: sender.tab?.id, status: msg.data && msg.data.status, bytes: msg.data && msg.data.data ? msg.data.data.length : 0 });
        respond({ ok: true });
        break;
      case "yt-player":
        ytOnPlayer(sender.tab?.id, msg.data);
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
