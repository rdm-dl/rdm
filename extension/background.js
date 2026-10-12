// RDM browser integration: capture downloads, sniff media, context menus.
// Talks to the desktop app through the native messaging host "com.rdm.bridge".

const api = typeof browser !== "undefined" ? browser : chrome;
const HOST = "com.rdm.bridge";

let settings = {
  enabled: true,
  extensions: [],
  media_extensions: [],
  exception_hosts: [],
  exception_urls: [],
  min_size: 0,
};
let connected = false;

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
  const stored = await api.storage.local.get(["enabled"]);
  const reply = await send({ type: "hello" });
  if (reply.type === "settings" && reply.capture) {
    settings = { ...settings, ...reply.capture };
  }
  if (typeof stored.enabled === "boolean") settings.enabled = stored.enabled;
  updateBadge();
  return reply;
}

// ------------------------------------------------------------ matching helpers

function wildcard(pattern, text) {
  const re = new RegExp("^" + pattern.toLowerCase().replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".") + "$");
  return re.test(text.toLowerCase());
}

function extensionOf(nameOrUrl) {
  try {
    const path = nameOrUrl.includes("://") ? new URL(nameOrUrl).pathname : nameOrUrl;
    const last = decodeURIComponent(path.split("/").pop() || "");
    const dot = last.lastIndexOf(".");
    return dot > 0 ? last.slice(dot + 1).toLowerCase() : "";
  } catch {
    return "";
  }
}

function shouldCapture(item) {
  if (!settings.enabled || !connected) return false;
  const url = item.finalUrl || item.url;
  if (!/^(https?|ftp):/i.test(url)) return false; // blob:, data:, file: stay in the browser
  let host = "";
  try {
    host = new URL(url).hostname;
  } catch {
    return false;
  }
  if (settings.exception_hosts.some((h) => wildcard(h, host))) return false;
  if (settings.exception_urls.some((u) => wildcard(u, url))) return false;
  if (settings.min_size > 0 && item.fileSize > 0 && item.fileSize < settings.min_size) return false;
  const ext = extensionOf(item.filename || "") || extensionOf(url);
  return settings.extensions.some((e) => wildcard(e, ext));
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

// ------------------------------------------------------------ download capture

api.downloads.onCreated.addListener(async (item) => {
  if (!shouldCapture(item)) return;
  try {
    await api.downloads.pause(item.id);
  } catch {
    return; // already finished or not pausable
  }
  const url = item.finalUrl || item.url;
  const name = (item.filename || "").split(/[\\/]/).pop();
  const reply = await send({
    type: "download",
    ...(await payload(url, { referrer: item.referrer || "", filename: name || undefined })),
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
});

// ------------------------------------------------------------ media sniffing

const MEDIA_TYPES = /^(video\/|audio\/|application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml))/i;
const MANIFEST_EXT = /\.(m3u8|mpd)(\?|$)/i;
const media = new Map(); // tabId -> Map(url -> info)

function headerValue(headers, name) {
  const h = (headers || []).find((x) => x.name.toLowerCase() === name);
  return h ? h.value : "";
}

api.webRequest.onHeadersReceived.addListener(
  (d) => {
    if (d.tabId < 0) return;
    const type = headerValue(d.responseHeaders, "content-type").split(";")[0].trim();
    const ext = extensionOf(d.url);
    const isManifest = MANIFEST_EXT.test(d.url) || /mpegurl|dash\+xml/i.test(type);
    const isMedia = MEDIA_TYPES.test(type) || settings.media_extensions.includes(ext);
    if (!isMedia && !isManifest) return;
    // Skip tiny fragments of segmented streams; keep the playlist/manifest instead.
    const size = parseInt(headerValue(d.responseHeaders, "content-length") || "0", 10);
    if (!isManifest && /\.(ts|m4s|aac)(\?|$)/i.test(d.url)) return;
    if (!isManifest && size > 0 && size < 256 * 1024) return;
    let byUrl = media.get(d.tabId);
    if (!byUrl) media.set(d.tabId, (byUrl = new Map()));
    const key = d.url.split("#")[0];
    if (!byUrl.has(key)) {
      byUrl.set(key, {
        url: key,
        type: isManifest ? (/mpd|dash/i.test(d.url + type) ? "DASH stream" : "HLS stream") : type || ext.toUpperCase(),
        size,
        found: Date.now(),
      });
      updateBadge(d.tabId);
    }
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "object"] },
  ["responseHeaders"]
);

api.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status === "loading" && info.url) {
    media.delete(tabId);
    updateBadge(tabId);
  }
});
api.tabs.onRemoved.addListener((tabId) => media.delete(tabId));

function updateBadge(tabId) {
  const set = (id) => {
    const n = media.get(id)?.size || 0;
    api.action.setBadgeText({ tabId: id, text: n ? String(n) : "" });
    api.action.setBadgeBackgroundColor({ tabId: id, color: "#5b7cff" });
  };
  if (typeof tabId === "number") set(tabId);
}

// ------------------------------------------------------------ context menus

function createMenus() {
  api.contextMenus.removeAll(() => {
    api.contextMenus.create({ id: "rdm-link", title: "Download with RDM", contexts: ["link"] });
    api.contextMenus.create({ id: "rdm-media", title: "Download media with RDM", contexts: ["video", "audio", "image"] });
    api.contextMenus.create({ id: "rdm-all", title: "Download all links with RDM", contexts: ["page"] });
  });
}

api.contextMenus.onClicked.addListener(async (info, tab) => {
  const referrer = info.pageUrl || (tab && tab.url) || "";
  if (info.menuItemId === "rdm-link" && info.linkUrl) {
    await send({ type: "download", ...(await payload(info.linkUrl, { referrer })) });
  } else if (info.menuItemId === "rdm-media" && info.srcUrl) {
    await send({ type: "download", ...(await payload(info.srcUrl, { referrer })) });
  } else if (info.menuItemId === "rdm-all" && tab && tab.id != null) {
    const [res] = await api.scripting.executeScript({
      target: { tabId: tab.id },
      func: () => [...new Set([...document.querySelectorAll("a[href]")].map((a) => a.href).filter((h) => /^(https?|ftp):/i.test(h)))],
    });
    const links = [];
    for (const url of res?.result || []) links.push({ url, referrer });
    if (links.length) await send({ type: "links", links });
  }
});

// ------------------------------------------------------------ popup messages

api.runtime.onMessage.addListener((msg, sender, respond) => {
  (async () => {
    if (msg.type === "state") {
      if (!connected) await refreshSettings();
      const list = [...(media.get(msg.tabId)?.values() || [])].sort((a, b) => b.found - a.found);
      respond({ connected, enabled: settings.enabled, media: list });
    } else if (msg.type === "toggle") {
      settings.enabled = msg.enabled;
      await api.storage.local.set({ enabled: msg.enabled });
      respond({ ok: true });
    } else if (msg.type === "download-media") {
      const reply = await send({ type: "download", ...(await payload(msg.url, { referrer: msg.referrer, filename: msg.filename })) });
      respond(reply);
    } else if (msg.type === "show") {
      respond(await send({ type: "show" }));
    } else if (msg.type === "add-url") {
      respond(await send({ type: "download", ...(await payload(msg.url, {})) }));
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
setInterval(refreshSettings, 5 * 60 * 1000);
