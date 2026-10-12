// Runs in the page's own JavaScript context. Watches the page's fetch/XMLHttpRequest
// traffic for stream manifests and media responses (including ones a service worker
// serves, which the extension's webRequest listener can't attribute to the tab) and
// reports them to content.js with window.postMessage.

(() => {
  if (window.__rdmDocHook) return;
  window.__rdmDocHook = true;
  const MAGIC = 0x52444d34; // "RDM4"
  const URL_RE = /\.(m3u8|mpd|mp4|m4v|m4a|webm|mkv|mp3|aac|flac|ogg|ogv|mov|flv|vtt|srt|ttml|dfxp)(\?|#|$)/i;
  const TYPE_RE = /^(video\/|audio\/|application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml|ttml\+xml)|text\/vtt)/i;
  const SEGMENT_RE = /\.(ts|m4s|m4f|cmfv|cmfa)(\?|#|$)/i;
  const seen = new Set();

  function report(url, type, size) {
    try {
      if (!url || seen.has(url) || !/^https?:/i.test(url)) return;
      if (/\.googlevideo\.com\//i.test(url)) return; // handled from the player data
      const t = (type || "").split(";")[0].trim();
      if (SEGMENT_RE.test(url) && !/mpegurl|dash/i.test(t)) return;
      if (!URL_RE.test(url) && !TYPE_RE.test(t)) return;
      seen.add(url);
      window.postMessage({ __rdm: MAGIC, url, type: t, size: size || 0 }, "*");
    } catch {}
  }

  function abs(u) {
    try {
      return new URL(u, location.href).href;
    } catch {
      return "";
    }
  }

  // ---- YouTube: the player's own data (formats, qualities, title).
  const YT = /(^|\.)(youtube\.com|youtube-nocookie\.com|youtube\.googleapis\.com)$/.test(location.hostname);
  const PLAYER_API = /\/youtubei\/v1\/player\b/;
  let lastYt = "";

  function ytReport(resp) {
    try {
      if (!resp || !resp.streamingData || !resp.videoDetails) return;
      const sd = resp.streamingData;
      const vd = resp.videoDetails;
      const fmt = (f) => ({
        itag: f.itag,
        url: f.url || "",
        cipher: !!(f.signatureCipher || f.cipher),
        mime: f.mimeType || "",
        w: f.width || 0,
        h: f.height || 0,
        fps: f.fps || 0,
        label: f.qualityLabel || "",
        bitrate: f.bitrate || 0,
        clen: +f.contentLength || 0,
        drm: !!(f.drmFamilies && f.drmFamilies.length),
        audioDefault: f.audioTrack ? !!f.audioTrack.audioIsDefault : true,
        audioName: f.audioTrack ? f.audioTrack.displayName || "" : "",
      });
      const data = {
        videoId: vd.videoId,
        title: vd.title || document.title,
        duration: +vd.lengthSeconds || 0,
        live: !!vd.isLive,
        formats: [...(sd.formats || []).map(fmt), ...(sd.adaptiveFormats || []).map(fmt)],
        hls: sd.hlsManifestUrl || "",
        sabr: !!sd.serverAbrStreamingUrl,
      };
      const key = data.videoId + ":" + data.formats.length + ":" + data.formats.filter((f) => f.url).length;
      if (key === lastYt) return;
      lastYt = key;
      window.postMessage({ __rdm: MAGIC, yt: data }, "*");
    } catch {}
  }

  if (YT) {
    const poll = () => {
      try {
        const p = document.getElementById("movie_player");
        const resp = (p && typeof p.getPlayerResponse === "function" && p.getPlayerResponse()) || window.ytInitialPlayerResponse;
        ytReport(resp);
      } catch {}
    };
    document.addEventListener("yt-navigate-finish", () => setTimeout(poll, 500));
    document.addEventListener("yt-page-data-updated", () => setTimeout(poll, 500));
    window.addEventListener("load", poll);
    setInterval(poll, 2000);
  }

  const origFetch = window.fetch;
  if (typeof origFetch === "function") {
    const hooked = function (input, init) {
      const p = origFetch.apply(this, arguments);
      try {
        const url = abs(typeof input === "string" ? input : input && input.url);
        p.then(
          (r) => {
            try {
              if (YT && PLAYER_API.test(r.url || url)) {
                r.clone().json().then(ytReport, () => {});
                return;
              }
              report(r.url || url, r.headers.get("content-type"), +r.headers.get("content-length") || 0);
            } catch {}
          },
          () => {}
        );
      } catch {}
      return p;
    };
    try {
      Object.defineProperty(hooked, "toString", { value: () => origFetch.toString() });
    } catch {}
    window.fetch = hooked;
  }

  const XHR = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
  if (XHR) {
    const origOpen = XHR.open;
    XHR.open = function (method, url) {
      try {
        const target = abs(url);
        this.addEventListener("load", () => {
          try {
            const u = this.responseURL || target;
            if (YT && PLAYER_API.test(u)) {
              const body = this.responseType === "json" ? this.response : this.responseType === "" || this.responseType === "text" ? JSON.parse(this.responseText) : null;
              ytReport(body);
              return;
            }
            report(u, this.getResponseHeader("content-type"), +this.getResponseHeader("content-length") || 0);
          } catch {}
        });
      } catch {}
      return origOpen.apply(this, arguments);
    };
  }
})();
